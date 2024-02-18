use anyhow::{anyhow, Result};
use ethers::prelude::abi;
use ethers::providers::Middleware;
use ethers::types::{transaction::eip2930::AccessList, H160, H256, U256, U64};
use foundry_evm_mini::evm::executor::fork::{BlockchainDb, BlockchainDbMeta, SharedBackend};
use foundry_evm_mini::evm::executor::inspector::{get_precompiles_for, AccessListTracer};
use revm::primitives::bytes::Bytes as rBytes;
use revm::primitives::{Bytes, Log, B160};
use revm::{
    db::{CacheDB, Database},
    primitives::{
        keccak256, AccountInfo, Bytecode, ExecutionResult, Output, TransactTo, B256, U256 as rU256,
    },
    EVM,
};
use std::{collections::BTreeSet, default::Default, str::FromStr, sync::Arc};

use crate::common::abi::Abi;
use crate::common::constants::COINBASE;
use crate::common::utils::{access_list_to_revm, create_new_wallet};

#[derive(Debug, Clone, Default)]
pub struct VictimTx {
    pub tx_hash: H256,
    pub from: H160,
    pub to: H160,
    pub data: Bytes,
    pub value: U256,
    pub gas_price: U256,
    pub gas_limit: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct Tx {
    pub caller: H160,
    pub transact_to: H160,
    pub data: rBytes,
    pub value: U256,
    pub gas_price: U256,
    pub gas_limit: u64,
}

impl Tx {
    pub fn from(tx: VictimTx) -> Self {
        let gas_limit = match tx.gas_limit {
            Some(gas_limit) => gas_limit,
            None => 5000000,
        };
        Self {
            caller: tx.from,
            transact_to: tx.to,
            data: tx.data,
            value: tx.value,
            gas_price: tx.gas_price,
            gas_limit,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TxResult {
    pub output: rBytes,
    pub logs: Option<Vec<Log>>,
    pub gas_used: u64,
    pub gas_refunded: u64,
}

#[derive(Clone)]
pub struct EvmSimulator<M> {
    pub provider: Arc<M>,
    pub owner: H160,
    pub evm: EVM<CacheDB<SharedBackend>>,
    pub block_number: U64,
    pub abi: Abi,
}

impl<M: Middleware + 'static> EvmSimulator<M> {
    pub fn new(provider: Arc<M>, owner: Option<H160>, block_number: U64) -> Self {
        let shared_backend = SharedBackend::spawn_backend_thread(
            provider.clone(),
            BlockchainDb::new(
                BlockchainDbMeta {
                    cfg_env: Default::default(),
                    block_env: Default::default(),
                    hosts: BTreeSet::from(["".to_string()]),
                },
                None,
            ),
            Some(block_number.into()),
        );
        let db = CacheDB::new(shared_backend);
        EvmSimulator::new_with_db(provider, owner, block_number, db)
    }

    pub fn new_with_db(
        provider: Arc<M>,
        owner: Option<H160>,
        block_number: U64,
        db: CacheDB<SharedBackend>,
    ) -> Self {
        let owner = match owner {
            Some(owner) => owner,
            None => create_new_wallet().1,
        };

        let mut evm = EVM::new();
        evm.database(db);

        evm.env.block.number = rU256::from(block_number.as_u64() + 1);
        evm.env.block.coinbase = H160::from_str(COINBASE).unwrap().into();

        Self {
            provider,
            owner,
            evm,
            block_number,
            abi: Abi::new(),
        }
    }

    pub fn clone_db(&mut self) -> CacheDB<SharedBackend> {
        self.evm.db.as_mut().unwrap().clone()
    }

    pub fn insert_db(&mut self, db: CacheDB<SharedBackend>) {
        let mut evm = EVM::new();
        evm.database(db);

        self.evm = evm;
    }

    pub fn get_block_number(&mut self) -> U256 {
        self.evm.env.block.number.into()
    }

    pub fn get_coinbase(&mut self) -> H160 {
        self.evm.env.block.coinbase.into()
    }

    pub fn get_base_fee(&mut self) -> U256 {
        self.evm.env.block.basefee.into()
    }

    pub fn set_base_fee(&mut self, base_fee: U256) {
        self.evm.env.block.basefee = base_fee.into();
    }

    pub fn get_access_list(&mut self, tx: Tx) -> Result<AccessList> {
        self.evm.env.tx.caller = tx.caller.into();
        self.evm.env.tx.transact_to = TransactTo::Call(tx.transact_to.into());
        self.evm.env.tx.data = tx.data;
        self.evm.env.tx.value = tx.value.into();
        self.evm.env.tx.gas_price = tx.gas_price.into();
        self.evm.env.tx.gas_limit = tx.gas_limit;
        let mut access_list_tracer = AccessListTracer::new(
            Default::default(),
            tx.caller.into(),
            tx.transact_to.into(),
            get_precompiles_for(self.evm.env.cfg.spec_id),
        );
        let access_list = match self.evm.inspect_ref(&mut access_list_tracer) {
            Ok(_) => access_list_tracer.access_list(),
            Err(_) => AccessList::default(),
        };
        Ok(access_list)
    }

    pub fn set_access_list(&mut self, access_list: AccessList) {
        self.evm.env.tx.access_list = access_list_to_revm(access_list);
    }

    pub fn staticcall(&mut self, tx: Tx) -> Result<TxResult> {
        self._call(tx, false)
    }

    pub fn call(&mut self, tx: Tx) -> Result<TxResult> {
        self._call(tx, true)
    }

    pub fn _call(&mut self, tx: Tx, commit: bool) -> Result<TxResult> {
        self.evm.env.tx.caller = tx.caller.into();
        self.evm.env.tx.transact_to = TransactTo::Call(tx.transact_to.into());
        self.evm.env.tx.data = tx.data;
        self.evm.env.tx.value = tx.value.into();
        self.evm.env.tx.gas_price = tx.gas_price.into();
        self.evm.env.tx.gas_limit = tx.gas_limit;

        let result;

        if commit {
            result = match self.evm.transact_commit() {
                Ok(result) => result,
                Err(e) => return Err(anyhow!("EVM call failed: {:?}", e)),
            };
        } else {
            let ref_tx = self
                .evm
                .transact_ref()
                .map_err(|e| anyhow!("EVM staticcall failed: {:?}", e))?;
            result = ref_tx.result;
        }

        let output = match result {
            ExecutionResult::Success {
                gas_used,
                gas_refunded,
                output,
                logs,
                ..
            } => match output {
                Output::Call(o) => TxResult {
                    output: o,
                    logs: Some(logs),
                    gas_used,
                    gas_refunded,
                },
                Output::Create(o, _) => TxResult {
                    output: o,
                    logs: Some(logs),
                    gas_used,
                    gas_refunded,
                },
            },
            ExecutionResult::Revert { gas_used, output } => {
                return Err(anyhow!(
                    "EVM REVERT: {:?} / Gas used: {:?}",
                    output,
                    gas_used
                ))
            }
            ExecutionResult::Halt { reason, .. } => return Err(anyhow!("EVM HALT: {:?}", reason)),
        };

        Ok(output)
    }

    pub fn basic(&mut self, target: H160) -> Result<Option<AccountInfo>> {
        self.evm
            .db
            .as_mut()
            .unwrap()
            .basic(target.into())
            .map_err(|e| anyhow!("Basic error: {e:?}"))
    }

    pub fn insert_account_info(&mut self, target: H160, account_info: AccountInfo) {
        self.evm
            .db
            .as_mut()
            .unwrap()
            .insert_account_info(target.into(), account_info);
    }

    pub fn insert_account_storage(
        &mut self,
        target: H160,
        slot: rU256,
        value: rU256,
    ) -> Result<()> {
        self.evm
            .db
            .as_mut()
            .unwrap()
            .insert_account_storage(target.into(), slot, value)?;
        Ok(())
    }

    pub fn deploy(&mut self, target: H160, bytecode: Bytecode) {
        let contract_info = AccountInfo::new(rU256::ZERO, 0, B256::zero(), bytecode);
        self.insert_account_info(target, contract_info);
    }

    pub fn get_eth_balance_of(&mut self, target: H160) -> U256 {
        let acc = self.basic(target).unwrap().unwrap();
        acc.balance.into()
    }

    pub fn set_eth_balance(&mut self, target: H160, amount: U256) {
        let user_balance = amount.into();
        let user_info = AccountInfo::new(user_balance, 0, B256::zero(), Bytecode::default());
        self.insert_account_info(target.into(), user_info);
    }

    pub fn get_token_balance(&mut self, token_address: H160, owner: H160) -> Result<U256> {
        let calldata = self.abi.token.encode("balanceOf", owner)?;
        let value = self.staticcall(Tx {
            caller: self.owner,
            transact_to: token_address,
            data: calldata.0,
            value: U256::zero(),
            gas_price: U256::zero(),
            gas_limit: 5000000,
        })?;
        let out = self.abi.token.decode_output("balanceOf", value.output)?;
        Ok(out)
    }

    pub fn set_token_balance(
        &mut self,
        token_address: H160,
        to: H160,
        slot: i32,
        amount: rU256,
    ) -> Result<()> {
        let balance_slot = keccak256(&abi::encode(&[
            abi::Token::Address(to.into()),
            abi::Token::Uint(U256::from(slot)),
        ]));
        self.insert_account_storage(token_address, balance_slot.into(), amount)?;
        Ok(())
    }

    pub fn get_pair_reserves(&mut self, pair_address: H160) -> Result<(U256, U256)> {
        let calldata = self.abi.pair.encode("getReserves", ())?;
        let value = self.staticcall(Tx {
            caller: self.owner,
            transact_to: pair_address,
            data: calldata.0,
            value: U256::zero(),
            gas_price: U256::zero(),
            gas_limit: 5000000,
        })?;
        let out: (U256, U256, U256) = self.abi.pair.decode_output("getReserves", value.output)?;
        Ok((out.0, out.1))
    }

    pub fn get_balance_slot(&mut self, token_address: H160) -> Result<i32> {
        let calldata = self.abi.token.encode("balanceOf", token_address)?;
        self.evm.env.tx.caller = self.owner.into();
        self.evm.env.tx.transact_to = TransactTo::Call(token_address.into());
        self.evm.env.tx.data = calldata.0;
        let result = match self.evm.transact_ref() {
            Ok(result) => result,
            Err(e) => return Err(anyhow!("EVM ref call failed: {e:?}")),
        };
        let token_b160: B160 = token_address.into();
        let token_acc = result.state.get(&token_b160).unwrap();
        let token_touched_storage = token_acc.storage.clone();
        for i in 0..30 {
            let slot = keccak256(&abi::encode(&[
                abi::Token::Address(token_address.into()),
                abi::Token::Uint(U256::from(i)),
            ]));
            let slot: rU256 = U256::from(slot).into();
            match token_touched_storage.get(&slot) {
                Some(_) => {
                    return Ok(i);
                }
                None => {}
            }
        }

        Ok(-1)
    }
}
