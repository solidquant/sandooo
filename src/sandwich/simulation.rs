use anyhow::Result;
use eth_encode_packed::ethabi::ethereum_types::{H160 as eH160, U256 as eU256};
use eth_encode_packed::{SolidityDataType, TakeLastXBytes};
use ethers::abi::ParamType;
use ethers::prelude::*;
use ethers::providers::{Provider, Ws};
use ethers::types::{transaction::eip2930::AccessList, Bytes, H160, H256, I256, U256, U64};
use log::info;
use revm::primitives::{Bytecode, U256 as rU256};
use std::{collections::HashMap, default::Default, str::FromStr, sync::Arc};

use crate::common::bytecode::SANDOOO_BYTECODE;
use crate::common::constants::{USDC, USDT};
use crate::common::evm::{EvmSimulator, Tx, VictimTx};
use crate::common::pools::Pool;
use crate::common::streams::{NewBlock, NewPendingTx};
use crate::common::utils::{
    create_new_wallet, is_weth, return_main_and_target_currency, MainCurrency,
};

#[derive(Debug, Clone, Default)]
pub struct PendingTxInfo {
    pub pending_tx: NewPendingTx,
    pub touched_pairs: Vec<SwapInfo>,
}

#[derive(Debug, Clone)]
pub enum SwapDirection {
    Buy,
    Sell,
}

#[derive(Debug, Clone)]
pub struct SwapInfo {
    pub tx_hash: H256,
    pub target_pair: H160,
    pub main_currency: H160,
    pub target_token: H160,
    pub version: u8,
    pub token0_is_main: bool,
    pub direction: SwapDirection,
}

#[derive(Debug, Clone)]
pub struct Sandwich {
    pub amount_in: U256,
    pub swap_info: SwapInfo,
    pub victim_tx: VictimTx,
    pub optimized_sandwich: Option<OptimizedSandwich>,
}

#[derive(Debug, Default, Clone)]
pub struct BatchSandwich {
    pub sandwiches: Vec<Sandwich>,
}

#[derive(Debug, Default, Clone)]
pub struct SimulatedSandwich {
    pub revenue: i128,
    pub profit: i128,
    pub gas_cost: i128,
    pub front_gas_used: u64,
    pub back_gas_used: u64,
    pub front_access_list: AccessList,
    pub back_access_list: AccessList,
    pub front_calldata: Bytes,
    pub back_calldata: Bytes,
}

#[derive(Debug, Default, Clone)]
pub struct OptimizedSandwich {
    pub amount_in: U256,
    pub max_revenue: U256,
    pub front_gas_used: u64,
    pub back_gas_used: u64,
    pub front_access_list: AccessList,
    pub back_access_list: AccessList,
    pub front_calldata: Bytes,
    pub back_calldata: Bytes,
}

pub static V2_SWAP_EVENT_ID: &str = "0xd78ad95f";

pub async fn debug_trace_call(
    provider: &Arc<Provider<Ws>>,
    new_block: &NewBlock,
    pending_tx: &NewPendingTx,
) -> Result<Option<CallFrame>> {
    let mut opts = GethDebugTracingCallOptions::default();
    let mut call_config = CallConfig::default();
    call_config.with_log = Some(true);

    opts.tracing_options.tracer = Some(GethDebugTracerType::BuiltInTracer(
        GethDebugBuiltInTracerType::CallTracer,
    ));
    opts.tracing_options.tracer_config = Some(GethDebugTracerConfig::BuiltInTracer(
        GethDebugBuiltInTracerConfig::CallTracer(call_config),
    ));

    let block_number = new_block.block_number;
    let mut tx = pending_tx.tx.clone();
    let nonce = provider
        .get_transaction_count(tx.from, Some(block_number.into()))
        .await
        .unwrap_or_default();
    tx.nonce = nonce;

    let trace = provider
        .debug_trace_call(&tx, Some(block_number.into()), opts)
        .await;

    match trace {
        Ok(trace) => match trace {
            GethTrace::Known(call_tracer) => match call_tracer {
                GethTraceFrame::CallTracer(frame) => Ok(Some(frame)),
                _ => Ok(None),
            },
            _ => Ok(None),
        },
        _ => Ok(None),
    }
}

pub async fn extract_swap_info(
    provider: &Arc<Provider<Ws>>,
    new_block: &NewBlock,
    pending_tx: &NewPendingTx,
    pools_map: &HashMap<H160, Pool>,
) -> Result<Vec<SwapInfo>> {
    let tx_hash = pending_tx.tx.hash;
    let mut swap_info_vec = Vec::new();

    let frame = debug_trace_call(provider, new_block, pending_tx).await?;
    if frame.is_none() {
        return Ok(swap_info_vec);
    }
    let frame = frame.unwrap();

    let mut logs = Vec::new();
    extract_logs(&frame, &mut logs);

    for log in &logs {
        match &log.topics {
            Some(topics) => {
                if topics.len() > 1 {
                    let selector = &format!("{:?}", topics[0])[0..10];
                    let is_v2_swap = selector == V2_SWAP_EVENT_ID;
                    if is_v2_swap {
                        let pair_address = log.address.unwrap();

                        // filter out the pools we have in memory only
                        let pool = pools_map.get(&pair_address);
                        if pool.is_none() {
                            continue;
                        }
                        let pool = pool.unwrap();

                        let token0 = pool.token0;
                        let token1 = pool.token1;

                        let (main_currency, target_token, token0_is_main) =
                            match return_main_and_target_currency(token0, token1) {
                                Some(out) => (out.0, out.1, out.0 == token0),
                                None => continue,
                            };

                        let (in0, _, _, out1) = match ethers::abi::decode(
                            &[
                                ParamType::Uint(256),
                                ParamType::Uint(256),
                                ParamType::Uint(256),
                                ParamType::Uint(256),
                            ],
                            log.data.as_ref().unwrap(),
                        ) {
                            Ok(input) => {
                                let uints: Vec<U256> = input
                                    .into_iter()
                                    .map(|i| i.to_owned().into_uint().unwrap())
                                    .collect();
                                (uints[0], uints[1], uints[2], uints[3])
                            }
                            _ => {
                                let zero = U256::zero();
                                (zero, zero, zero, zero)
                            }
                        };

                        let zero_for_one = (in0 > U256::zero()) && (out1 > U256::zero());

                        let direction = if token0_is_main {
                            if zero_for_one {
                                SwapDirection::Buy
                            } else {
                                SwapDirection::Sell
                            }
                        } else {
                            if zero_for_one {
                                SwapDirection::Sell
                            } else {
                                SwapDirection::Buy
                            }
                        };

                        let swap_info = SwapInfo {
                            tx_hash,
                            target_pair: pair_address,
                            main_currency,
                            target_token,
                            version: 2,
                            token0_is_main,
                            direction,
                        };
                        swap_info_vec.push(swap_info);
                    }
                }
            }
            _ => {}
        }
    }

    Ok(swap_info_vec)
}

pub fn extract_logs(call_frame: &CallFrame, logs: &mut Vec<CallLogFrame>) {
    if let Some(ref logs_vec) = call_frame.logs {
        logs.extend(logs_vec.iter().cloned());
    }

    if let Some(ref calls_vec) = call_frame.calls {
        for call in calls_vec {
            extract_logs(call, logs);
        }
    }
}

pub fn get_v2_amount_out(amount_in: U256, reserve_in: U256, reserve_out: U256) -> U256 {
    let amount_in_with_fee = amount_in * U256::from(997);
    let numerator = amount_in_with_fee * reserve_out;
    let denominator = (reserve_in * U256::from(1000)) + amount_in_with_fee;
    let amount_out = numerator.checked_div(denominator);
    amount_out.unwrap_or_default()
}

pub fn convert_usdt_to_weth(
    simulator: &mut EvmSimulator<Provider<Ws>>,
    amount: U256,
) -> Result<U256> {
    let conversion_pair = H160::from_str("0x0d4a11d5EEaaC28EC3F61d100daF4d40471f1852").unwrap();
    // token0: WETH / token1: USDT
    let reserves = simulator.get_pair_reserves(conversion_pair)?;
    let (reserve_in, reserve_out) = (reserves.1, reserves.0);
    let weth_out = get_v2_amount_out(amount, reserve_in, reserve_out);
    Ok(weth_out)
}

pub fn convert_usdc_to_weth(
    simulator: &mut EvmSimulator<Provider<Ws>>,
    amount: U256,
) -> Result<U256> {
    let conversion_pair = H160::from_str("0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc").unwrap();
    // token0: USDC / token1: WETH
    let reserves = simulator.get_pair_reserves(conversion_pair)?;
    let (reserve_in, reserve_out) = (reserves.0, reserves.1);
    let weth_out = get_v2_amount_out(amount, reserve_in, reserve_out);
    Ok(weth_out)
}

impl BatchSandwich {
    pub fn bundle_id(&self) -> String {
        let mut tx_hashes = Vec::new();
        for sandwich in &self.sandwiches {
            let tx_hash = sandwich.victim_tx.tx_hash;
            let tx_hash_4_bytes = &format!("{:?}", tx_hash)[0..10];
            tx_hashes.push(String::from_str(tx_hash_4_bytes).unwrap());
        }
        tx_hashes.sort();
        tx_hashes.dedup();
        tx_hashes.join("-")
    }

    pub fn victim_tx_hashes(&self) -> Vec<H256> {
        self.sandwiches
            .iter()
            .map(|s| s.victim_tx.tx_hash)
            .collect()
    }

    pub fn target_tokens(&self) -> Vec<H160> {
        self.sandwiches
            .iter()
            .map(|s| s.swap_info.target_token)
            .collect()
    }

    pub fn target_v2_pairs(&self) -> Vec<H160> {
        self.sandwiches
            .iter()
            .filter(|s| s.swap_info.version == 2)
            .map(|s| s.swap_info.target_pair)
            .collect()
    }

    pub fn encode_frontrun_tx(
        &self,
        block_number: U256,
        pair_reserves: &HashMap<H160, (U256, U256)>,
    ) -> Result<(Bytes, Vec<Tx>, HashMap<H160, U256>)> {
        let mut starting_mc_values = HashMap::new();

        let mut added_tx_hash = HashMap::new();
        let mut victim_txs = Vec::new();

        let mut frontrun_swap_params = Vec::new();

        let block_number_u256 = eU256::from_dec_str(&block_number.to_string())?;
        frontrun_swap_params.push(
            SolidityDataType::NumberWithShift(block_number_u256, TakeLastXBytes(64)), // blockNumber (uint64)
        );

        for sandwich in &self.sandwiches {
            let tx_hash = sandwich.victim_tx.tx_hash;
            if !added_tx_hash.contains_key(&tx_hash) {
                added_tx_hash.insert(tx_hash, true);
                victim_txs.push(Tx::from(sandwich.victim_tx.clone()));
            }

            // Token swap 0 -> 1
            // Frontrun tx is a main_currency -> target_token BUY tx
            // thus, if token0_is_main, then it is zero_for_one swap
            let zero_for_one = sandwich.swap_info.token0_is_main;

            let new_amount_in = sandwich
                .amount_in
                .checked_sub(U256::from(1))
                .unwrap_or(U256::zero());
            let amount_in_u256 = eU256::from_dec_str(&new_amount_in.to_string())?;
            let amount_out_u256 = if sandwich.swap_info.version == 2 {
                let reserves = pair_reserves.get(&sandwich.swap_info.target_pair).unwrap();
                let (reserve_in, reserve_out) = if zero_for_one {
                    (reserves.0, reserves.1)
                } else {
                    (reserves.1, reserves.0)
                };
                let amount_out = get_v2_amount_out(new_amount_in, reserve_in, reserve_out);
                eU256::from_dec_str(&amount_out.to_string())?
            } else {
                eU256::zero()
            };

            let pair = eH160::from_str(&format!("{:?}", sandwich.swap_info.target_pair)).unwrap();
            let token_in =
                eH160::from_str(&format!("{:?}", sandwich.swap_info.main_currency)).unwrap();

            let main_currency = sandwich.swap_info.main_currency;
            if starting_mc_values.contains_key(&main_currency) {
                let prev_mc_value = *starting_mc_values.get(&main_currency).unwrap();
                starting_mc_values.insert(main_currency, prev_mc_value + new_amount_in);
            } else {
                starting_mc_values.insert(main_currency, new_amount_in);
            }

            frontrun_swap_params.extend(vec![
                SolidityDataType::NumberWithShift(
                    eU256::from(zero_for_one as u8),
                    TakeLastXBytes(8),
                ), // zeroForOne (uint8)
                SolidityDataType::Address(pair),     // pair (address)
                SolidityDataType::Address(token_in), // tokenIn (address)
                SolidityDataType::NumberWithShift(amount_in_u256, TakeLastXBytes(256)), // amountIn (uint256)
                SolidityDataType::NumberWithShift(amount_out_u256, TakeLastXBytes(256)), // amountOut (uint256)
            ]);
        }

        let frontrun_calldata = eth_encode_packed::abi::encode_packed(&frontrun_swap_params);
        let frontrun_calldata_bytes = Bytes::from_str(&frontrun_calldata.1).unwrap_or_default();

        Ok((frontrun_calldata_bytes, victim_txs, starting_mc_values))
    }

    pub fn encode_backrun_tx(
        &self,
        block_number: U256,
        pair_reserves: &HashMap<H160, (U256, U256)>,
        token_balances: &HashMap<H160, U256>,
    ) -> Result<Bytes> {
        let mut backrun_swap_params = Vec::new();

        let block_number_u256 = eU256::from_dec_str(&block_number.to_string())?;
        backrun_swap_params.push(
            SolidityDataType::NumberWithShift(block_number_u256, TakeLastXBytes(64)), // blockNumber (uint64)
        );

        for sandwich in &self.sandwiches {
            let amount_in = *token_balances
                .get(&sandwich.swap_info.target_token)
                .unwrap_or(&U256::zero());
            let new_amount_in = amount_in.checked_sub(U256::from(1)).unwrap_or(U256::zero());
            let amount_in_u256 = eU256::from_dec_str(&new_amount_in.to_string())?;

            // this means that the buy order is token0 -> token1
            let zero_for_one = sandwich.swap_info.token0_is_main;

            // in backrun tx we sell tokens we bought in our frontrun tx
            // so it's important to flip the boolean value of zero_for_one
            let amount_out_u256 = if sandwich.swap_info.version == 2 {
                let reserves = pair_reserves.get(&sandwich.swap_info.target_pair).unwrap();
                let (reserve_in, reserve_out) = if zero_for_one {
                    // token0 is main_currency
                    (reserves.1, reserves.0)
                } else {
                    // token1 is main_currency
                    (reserves.0, reserves.1)
                };
                let amount_out = get_v2_amount_out(new_amount_in, reserve_in, reserve_out);
                eU256::from_dec_str(&amount_out.to_string())?
            } else {
                eU256::zero()
            };

            let pair = eH160::from_str(&format!("{:?}", sandwich.swap_info.target_pair)).unwrap();
            let token_in =
                eH160::from_str(&format!("{:?}", sandwich.swap_info.target_token)).unwrap();

            backrun_swap_params.extend(vec![
                SolidityDataType::NumberWithShift(
                    eU256::from(!zero_for_one as u8), // <-- make sure to flip boolean value (it's a sell now, not buy)
                    TakeLastXBytes(8),
                ), // zeroForOne (uint8)
                SolidityDataType::Address(pair),     // pair (address)
                SolidityDataType::Address(token_in), // tokenIn (address)
                SolidityDataType::NumberWithShift(amount_in_u256, TakeLastXBytes(256)), // amountIn (uint256)
                SolidityDataType::NumberWithShift(amount_out_u256, TakeLastXBytes(256)), // amountOut (uint256)
            ]);
        }

        let backrun_calldata = eth_encode_packed::abi::encode_packed(&backrun_swap_params);
        let backrun_calldata_bytes = Bytes::from_str(&backrun_calldata.1).unwrap_or_default();

        Ok(backrun_calldata_bytes)
    }

    pub async fn simulate(
        &self,
        provider: Arc<Provider<Ws>>,
        owner: Option<H160>,
        block_number: U64,
        base_fee: U256,
        max_fee: U256,
        front_access_list: Option<AccessList>,
        back_access_list: Option<AccessList>,
        bot_address: Option<H160>,
    ) -> Result<SimulatedSandwich> {
        let mut simulator = EvmSimulator::new(provider.clone(), owner, block_number);

        // set ETH balance so that it's enough to cover gas fees
        match owner {
            None => {
                let initial_eth_balance = U256::from(100) * U256::from(10).pow(U256::from(18));
                simulator.set_eth_balance(simulator.owner, initial_eth_balance);
            }
            _ => {}
        }

        // get reserves for v2 pairs and target tokens
        let target_v2_pairs = self.target_v2_pairs();
        let target_tokens = self.target_tokens();

        let mut reserves_before = HashMap::new();

        for v2_pair in &target_v2_pairs {
            let reserves = simulator.get_pair_reserves(*v2_pair)?;
            reserves_before.insert(*v2_pair, reserves);
        }

        let next_block_number = simulator.get_block_number();

        // create frontrun tx calldata and inject main_currency token balance to bot contract
        let (frontrun_calldata, victim_txs, starting_mc_values) =
            self.encode_frontrun_tx(next_block_number, &reserves_before)?;

        // deploy Sandooo bot
        let bot_address = match bot_address {
            Some(bot_address) => bot_address,
            None => {
                let bot_address = create_new_wallet().1;
                simulator.deploy(bot_address, Bytecode::new_raw((*SANDOOO_BYTECODE.0).into()));

                // override owner slot
                let owner_ru256 = rU256::from_str(&format!("{:?}", simulator.owner)).unwrap();
                simulator.insert_account_storage(bot_address, rU256::from(0), owner_ru256)?;

                for (main_currency, starting_value) in &starting_mc_values {
                    let mc = MainCurrency::new(*main_currency);
                    let balance_slot = mc.balance_slot();
                    simulator.set_token_balance(
                        *main_currency,
                        bot_address,
                        balance_slot,
                        (*starting_value).into(),
                    )?;
                }

                bot_address
            }
        };

        // check ETH, MC balance before any txs are run
        let eth_balance_before = simulator.get_eth_balance_of(simulator.owner);
        let mut mc_balances_before = HashMap::new();
        for (main_currency, _) in &starting_mc_values {
            let balance_before = simulator.get_token_balance(*main_currency, bot_address)?;
            mc_balances_before.insert(main_currency, balance_before);
        }

        // set base fee so that gas fees are taken into account
        simulator.set_base_fee(base_fee);

        // Frontrun
        let front_tx = Tx {
            caller: simulator.owner,
            transact_to: bot_address,
            data: frontrun_calldata.0.clone(),
            value: U256::zero(),
            gas_price: base_fee,
            gas_limit: 5000000,
        };
        let front_access_list = match front_access_list {
            Some(access_list) => access_list,
            None => match simulator.get_access_list(front_tx.clone()) {
                Ok(access_list) => access_list,
                _ => AccessList::default(),
            },
        };
        simulator.set_access_list(front_access_list.clone());
        let front_gas_used = match simulator.call(front_tx) {
            Ok(result) => result.gas_used,
            Err(_) => 0,
        };

        // Victim Txs
        for victim_tx in victim_txs {
            match simulator.call(victim_tx) {
                _ => {}
            }
        }

        simulator.set_base_fee(U256::zero());

        // get reserves after frontrun / victim tx
        let mut reserves_after = HashMap::new();
        let mut token_balances = HashMap::new();

        for v2_pair in &target_v2_pairs {
            let reserves = simulator
                .get_pair_reserves(*v2_pair)
                .unwrap_or((U256::zero(), U256::zero()));
            reserves_after.insert(*v2_pair, reserves);
        }

        for token in &target_tokens {
            let token_balance = simulator
                .get_token_balance(*token, bot_address)
                .unwrap_or_default();
            token_balances.insert(*token, token_balance);
        }

        simulator.set_base_fee(base_fee);

        let backrun_calldata =
            self.encode_backrun_tx(next_block_number, &reserves_after, &token_balances)?;

        // Backrun
        let back_tx = Tx {
            caller: simulator.owner,
            transact_to: bot_address,
            data: backrun_calldata.0.clone(),
            value: U256::zero(),
            gas_price: max_fee,
            gas_limit: 5000000,
        };
        let back_access_list = match back_access_list.clone() {
            Some(access_list) => access_list,
            None => match simulator.get_access_list(back_tx.clone()) {
                Ok(access_list) => access_list,
                _ => AccessList::default(),
            },
        };
        let back_access_list = back_access_list.clone();
        simulator.set_access_list(back_access_list.clone());
        let back_gas_used = match simulator.call(back_tx) {
            Ok(result) => result.gas_used,
            Err(_) => 0,
        };

        simulator.set_base_fee(U256::zero());

        let eth_balance_after = simulator.get_eth_balance_of(simulator.owner);
        let mut mc_balances_after = HashMap::new();
        for (main_currency, _) in &starting_mc_values {
            let balance_after = simulator.get_token_balance(*main_currency, bot_address)?;
            mc_balances_after.insert(main_currency, balance_after);
        }

        let eth_used_as_gas = eth_balance_before
            .checked_sub(eth_balance_after)
            .unwrap_or(eth_balance_before);
        let eth_used_as_gas_i256 = I256::from_dec_str(&eth_used_as_gas.to_string())?;

        let usdt = H160::from_str(USDT).unwrap();
        let usdc = H160::from_str(USDC).unwrap();

        let mut weth_before_i256 = I256::zero();
        let mut weth_after_i256 = I256::zero();

        for (main_currency, _) in &starting_mc_values {
            let mc_balance_before = *mc_balances_before.get(&main_currency).unwrap();
            let mc_balance_after = *mc_balances_after.get(&main_currency).unwrap();

            let (mc_balance_before, mc_balance_after) = if *main_currency == usdt {
                let before =
                    convert_usdt_to_weth(&mut simulator, mc_balance_before).unwrap_or_default();
                let after =
                    convert_usdt_to_weth(&mut simulator, mc_balance_after).unwrap_or_default();
                (before, after)
            } else if *main_currency == usdc {
                let before =
                    convert_usdc_to_weth(&mut simulator, mc_balance_before).unwrap_or_default();
                let after =
                    convert_usdc_to_weth(&mut simulator, mc_balance_after).unwrap_or_default();
                (before, after)
            } else {
                (mc_balance_before, mc_balance_after)
            };

            let mc_balance_before_i256 = I256::from_dec_str(&mc_balance_before.to_string())?;
            let mc_balance_after_i256 = I256::from_dec_str(&mc_balance_after.to_string())?;

            weth_before_i256 += mc_balance_before_i256;
            weth_after_i256 += mc_balance_after_i256;
        }

        let profit = (weth_after_i256 - weth_before_i256).as_i128();
        let gas_cost = eth_used_as_gas_i256.as_i128();
        let revenue = profit - gas_cost;

        let simulated_sandwich = SimulatedSandwich {
            revenue,
            profit,
            gas_cost,
            front_gas_used,
            back_gas_used,
            front_access_list,
            back_access_list,
            front_calldata: frontrun_calldata,
            back_calldata: backrun_calldata,
        };

        Ok(simulated_sandwich)
    }
}

impl Sandwich {
    pub fn is_optimized(&mut self) -> bool {
        self.optimized_sandwich.is_some()
    }

    pub fn pretty_print(&self) {
        println!("\n");
        info!("🥪 SANDWICH: [{:?}]", self.victim_tx.tx_hash);
        info!("- Target token: {:?}", self.swap_info.target_token);
        info!(
            "- Target V{:?} pair: {:?}",
            self.swap_info.version, self.swap_info.target_pair
        );

        match &self.optimized_sandwich {
            Some(optimized_sandwich) => {
                info!("----- Optimized -----");
                info!("- Amount in: {:?}", optimized_sandwich.amount_in);
                info!("- Profit: {:?}", optimized_sandwich.max_revenue);
                info!(
                    "- Front gas: {:?} / Back gas: {:?}",
                    optimized_sandwich.front_gas_used, optimized_sandwich.back_gas_used
                );
            }
            _ => {}
        }
    }

    pub async fn optimize(
        &mut self,
        provider: Arc<Provider<Ws>>,
        block_number: U64,
        amount_in_ceiling: U256,
        base_fee: U256,
        max_fee: U256,
        front_access_list: AccessList,
        back_access_list: AccessList,
    ) -> Result<OptimizedSandwich> {
        let main_currency = self.swap_info.main_currency;

        let mut min_amount_in = U256::zero();
        let mut max_amount_in = amount_in_ceiling;
        let tolerance = if is_weth(main_currency) {
            U256::from(1) * U256::from(10).pow(U256::from(14))
        } else {
            U256::from(1) * U256::from(10).pow(U256::from(3))
        };

        if max_amount_in < min_amount_in {
            return Ok(OptimizedSandwich {
                amount_in: U256::zero(),
                max_revenue: U256::zero(),
                front_gas_used: 0,
                back_gas_used: 0,
                front_access_list: AccessList::default(),
                back_access_list: AccessList::default(),
                front_calldata: Bytes::default(),
                back_calldata: Bytes::default(),
            });
        }

        let mut optimized_in = U256::zero();
        let mut max_revenue = U256::zero();
        let mut max_front_gas_used = 0;
        let mut max_back_gas_used = 0;
        let mut max_front_calldata = Bytes::default();
        let mut max_back_calldata = Bytes::default();

        let intervals = U256::from(10);

        loop {
            let diff = max_amount_in - min_amount_in;
            let step = diff.checked_div(intervals).unwrap();

            if step <= tolerance {
                break;
            }

            let mut inputs = Vec::new();
            for i in 0..intervals.as_u64() + 1 {
                let _i = U256::from(i);
                let input = min_amount_in + (_i * step);
                inputs.push(input);
            }

            let mut simulations = Vec::new();

            for (idx, input) in inputs.iter().enumerate() {
                let sim = tokio::task::spawn(simulate_sandwich(
                    idx,
                    provider.clone(),
                    block_number,
                    self.clone(),
                    *input,
                    base_fee,
                    max_fee,
                    front_access_list.clone(),
                    back_access_list.clone(),
                ));
                simulations.push(sim);
            }

            let results = futures::future::join_all(simulations).await;
            let revenue: Vec<(usize, U256, i128, u64, u64, Bytes, Bytes)> =
                results.into_iter().map(|res| res.unwrap()).collect();

            let mut max_idx = 0;

            for (
                idx,
                amount_in,
                profit,
                front_gas_used,
                back_gas_used,
                front_calldata,
                back_calldata,
            ) in &revenue
            {
                if *profit > max_revenue.as_u128() as i128 {
                    optimized_in = *amount_in;
                    max_revenue = U256::from(*profit);
                    max_front_gas_used = *front_gas_used;
                    max_back_gas_used = *back_gas_used;
                    max_front_calldata = front_calldata.clone();
                    max_back_calldata = back_calldata.clone();

                    max_idx = *idx;
                }
            }

            min_amount_in = if max_idx == 0 {
                U256::zero()
            } else {
                revenue[max_idx - 1].1
            };
            max_amount_in = if max_idx == revenue.len() - 1 {
                revenue[max_idx].1
            } else {
                revenue[max_idx + 1].1
            };
        }

        let optimized_sandwich = OptimizedSandwich {
            amount_in: optimized_in,
            max_revenue,
            front_gas_used: max_front_gas_used,
            back_gas_used: max_back_gas_used,
            front_access_list,
            back_access_list,
            front_calldata: max_front_calldata,
            back_calldata: max_back_calldata,
        };

        self.optimized_sandwich = Some(optimized_sandwich.clone());
        Ok(optimized_sandwich)
    }
}

pub async fn simulate_sandwich(
    idx: usize,
    provider: Arc<Provider<Ws>>,
    block_number: U64,
    sandwich: Sandwich,
    amount_in: U256,
    base_fee: U256,
    max_fee: U256,
    front_access_list: AccessList,
    back_access_list: AccessList,
) -> (usize, U256, i128, u64, u64, Bytes, Bytes) {
    let mut sandwich = sandwich;
    sandwich.amount_in = amount_in;

    let batch_sandwich = BatchSandwich {
        sandwiches: vec![sandwich],
    };
    match batch_sandwich
        .simulate(
            provider,
            None,
            block_number,
            base_fee,
            max_fee,
            Some(front_access_list),
            Some(back_access_list),
            None,
        )
        .await
    {
        Ok(simulated_sandwich) => (
            idx,
            amount_in,
            simulated_sandwich.revenue,
            simulated_sandwich.front_gas_used,
            simulated_sandwich.back_gas_used,
            simulated_sandwich.front_calldata,
            simulated_sandwich.back_calldata,
        ),
        _ => (idx, amount_in, 0, 0, 0, Bytes::default(), Bytes::default()),
    }
}
