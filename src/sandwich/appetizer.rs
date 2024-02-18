use anyhow::Result;
use ethers::{
    providers::{Provider, Ws},
    types::{H256, U256},
};
use log::warn;
use std::{collections::HashMap, sync::Arc};

use crate::common::evm::VictimTx;
use crate::common::streams::NewBlock;
use crate::common::utils::{is_weth, MainCurrency};
use crate::sandwich::simulation::{BatchSandwich, PendingTxInfo, Sandwich, SwapDirection};

pub async fn appetizer(
    provider: &Arc<Provider<Ws>>,
    new_block: &NewBlock,
    tx_hash: H256,
    victim_gas_price: U256,
    pending_txs: &HashMap<H256, PendingTxInfo>,
    promising_sandwiches: &mut HashMap<H256, Vec<Sandwich>>,
) -> Result<()> {
    let pending_tx_info = pending_txs.get(&tx_hash).unwrap();
    let pending_tx = &pending_tx_info.pending_tx;
    // make sandwiches and simulate
    let victim_tx = VictimTx {
        tx_hash,
        from: pending_tx.tx.from,
        to: pending_tx.tx.to.unwrap_or_default(),
        data: pending_tx.tx.input.0.clone().into(),
        value: pending_tx.tx.value,
        gas_price: victim_gas_price,
        gas_limit: Some(pending_tx.tx.gas.as_u64()),
    };

    let swap_info = &pending_tx_info.touched_pairs;

    /*
    For now, we focus on the buys:
    1. Frontrun: Buy
    2. Victim: Buy
    3. Backrun: Sell
    */
    for info in swap_info {
        match info.direction {
            SwapDirection::Sell => continue,
            _ => {}
        }

        let main_currency = info.main_currency;
        let mc = MainCurrency::new(main_currency);
        let decimals = mc.decimals();

        let small_amount_in = if is_weth(main_currency) {
            U256::from(10).pow(U256::from(decimals - 2)) // 0.01 WETH
        } else {
            U256::from(10) * U256::from(10).pow(U256::from(decimals)) // 10 USDT, 10 USDC
        };
        let base_fee = new_block.next_base_fee;
        let max_fee = base_fee;

        let mut sandwich = Sandwich {
            amount_in: small_amount_in,
            swap_info: info.clone(),
            victim_tx: victim_tx.clone(),
            optimized_sandwich: None,
        };

        let batch_sandwich = BatchSandwich {
            sandwiches: vec![sandwich.clone()],
        };

        let simulated_sandwich = batch_sandwich
            .simulate(
                provider.clone(),
                None,
                new_block.block_number,
                base_fee,
                max_fee,
                None,
                None,
                None,
            )
            .await;
        if simulated_sandwich.is_err() {
            let e = simulated_sandwich.as_ref().err().unwrap();
            warn!("BatchSandwich.simulate error: {e:?}");
            continue;
        }
        let simulated_sandwich = simulated_sandwich.unwrap();
        // profit should be greater than 0 to simulate/optimize any further
        if simulated_sandwich.profit <= 0 {
            continue;
        }
        let ceiling_amount_in = if is_weth(main_currency) {
            U256::from(100) * U256::from(10).pow(U256::from(18)) // 100 ETH
        } else {
            U256::from(300000) * U256::from(10).pow(U256::from(decimals)) // 300000 USDT/USDC (both 6 decimals)
        };
        let optimized_sandwich = sandwich
            .optimize(
                provider.clone(),
                new_block.block_number,
                ceiling_amount_in,
                base_fee,
                max_fee,
                simulated_sandwich.front_access_list.clone(),
                simulated_sandwich.back_access_list.clone(),
            )
            .await;
        if optimized_sandwich.is_err() {
            let e = optimized_sandwich.as_ref().err().unwrap();
            warn!("Sandwich.optimize error: {e:?}");
            continue;
        }
        let optimized_sandwich = optimized_sandwich.unwrap();
        if optimized_sandwich.max_revenue > U256::zero() {
            // add optimized sandwiches to promising_sandwiches
            if !promising_sandwiches.contains_key(&tx_hash) {
                promising_sandwiches.insert(tx_hash, vec![sandwich.clone()]);
            } else {
                let sandwiches = promising_sandwiches.get_mut(&tx_hash).unwrap();
                sandwiches.push(sandwich.clone());
            }
        }
    }

    Ok(())
}
