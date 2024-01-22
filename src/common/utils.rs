use anyhow::Result;
use ethers::core::rand::thread_rng;
use ethers::prelude::*;
use ethers::{
    self,
    types::{
        transaction::eip2930::{AccessList, AccessListItem},
        U256,
    },
};
use fern::colors::{Color, ColoredLevelConfig};
use foundry_evm_mini::evm::utils::{b160_to_h160, h160_to_b160, ru256_to_u256, u256_to_ru256};
use log::LevelFilter;
use rand::Rng;
use revm::primitives::{B160, U256 as rU256};
use std::str::FromStr;
use std::sync::Arc;

use crate::common::constants::{PROJECT_NAME, WETH};

pub fn setup_logger() -> Result<()> {
    let colors = ColoredLevelConfig {
        trace: Color::Cyan,
        debug: Color::Magenta,
        info: Color::Green,
        warn: Color::Red,
        error: Color::BrightRed,
        ..ColoredLevelConfig::new()
    };

    fern::Dispatch::new()
        .format(move |out, message, record| {
            out.finish(format_args!(
                "{}[{}] {}",
                chrono::Local::now().format("[%H:%M:%S]"),
                colors.color(record.level()),
                message
            ))
        })
        .chain(std::io::stdout())
        .level(log::LevelFilter::Error)
        .level_for(PROJECT_NAME, LevelFilter::Info)
        .apply()?;

    Ok(())
}

pub fn calculate_next_block_base_fee(
    gas_used: U256,
    gas_limit: U256,
    base_fee_per_gas: U256,
) -> U256 {
    let gas_used = gas_used;

    let mut target_gas_used = gas_limit / 2;
    target_gas_used = if target_gas_used == U256::zero() {
        U256::one()
    } else {
        target_gas_used
    };

    let new_base_fee = {
        if gas_used > target_gas_used {
            base_fee_per_gas
                + ((base_fee_per_gas * (gas_used - target_gas_used)) / target_gas_used)
                    / U256::from(8u64)
        } else {
            base_fee_per_gas
                - ((base_fee_per_gas * (target_gas_used - gas_used)) / target_gas_used)
                    / U256::from(8u64)
        }
    };

    let seed = rand::thread_rng().gen_range(0..9);
    new_base_fee + seed
}

pub fn access_list_to_ethers(access_list: Vec<(B160, Vec<rU256>)>) -> AccessList {
    AccessList::from(
        access_list
            .into_iter()
            .map(|(address, slots)| AccessListItem {
                address: b160_to_h160(address),
                storage_keys: slots
                    .into_iter()
                    .map(|y| H256::from_uint(&ru256_to_u256(y)))
                    .collect(),
            })
            .collect::<Vec<AccessListItem>>(),
    )
}

pub fn access_list_to_revm(access_list: AccessList) -> Vec<(B160, Vec<rU256>)> {
    access_list
        .0
        .into_iter()
        .map(|x| {
            (
                h160_to_b160(x.address),
                x.storage_keys
                    .into_iter()
                    .map(|y| u256_to_ru256(y.0.into()))
                    .collect(),
            )
        })
        .collect()
}

abigen!(
    IERC20,
    r#"[
        function balanceOf(address) external view returns (uint256)
    ]"#,
);

pub async fn get_token_balance(
    provider: Arc<Provider<Ws>>,
    owner: H160,
    token: H160,
) -> Result<U256> {
    let contract = IERC20::new(token, provider);
    let token_balance = contract.balance_of(owner).call().await?;
    Ok(token_balance)
}

pub fn create_new_wallet() -> (LocalWallet, H160) {
    let wallet = LocalWallet::new(&mut thread_rng());
    let address = wallet.address();
    (wallet, address)
}

pub fn to_h160(str_address: &'static str) -> H160 {
    H160::from_str(str_address).unwrap()
}

pub fn is_weth(token_address: H160) -> bool {
    token_address == to_h160(WETH)
}
