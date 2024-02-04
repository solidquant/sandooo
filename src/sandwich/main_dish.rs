use anyhow::Result;
use bounded_vec_deque::BoundedVecDeque;
use ethers::{
    providers::{Provider, Ws},
    types::{H160, H256, U256, U64},
};
use log::{info, warn};
use std::{collections::HashMap, sync::Arc};

use crate::common::alert::Alert;
use crate::common::constants::Env;
use crate::common::execution::{Executor, SandoBundle};
use crate::common::streams::NewBlock;
use crate::common::utils::get_token_balance;
use crate::sandwich::simulation::{BatchSandwich, PendingTxInfo, Sandwich};

pub async fn send_sando_bundle_request(
    executor: &Executor,
    sando_bundle: SandoBundle,
    block_number: U64,
    alert: &Alert,
) -> Result<()> {
    let bundle_request = executor
        .to_sando_bundle_request(sando_bundle, block_number, 1)
        .await?;
    // If you want to check the simulation results provided by Flashbots
    // run the following code, but this will take something like 0.1 ~ 0.3 seconds
    // executor.simulate_bundle(&bundle_request).await;
    let response = executor.broadcast_bundle(bundle_request).await?;
    info!("Bundle sent: {:?}", response);
    match alert
        .send(&format!("[{:?}] Bundle sent", block_number))
        .await
    {
        _ => {}
    }
    Ok(())
}

pub async fn main_dish(
    provider: &Arc<Provider<Ws>>,
    alert: &Alert,
    executor: &Executor,
    new_block: &NewBlock,
    owner: H160,
    bot_address: H160,
    main_currency: H160,
    bribe_pct: U256,
    promising_sandwiches: &HashMap<H256, Vec<Sandwich>>,
    simulated_bundle_ids: &mut BoundedVecDeque<String>,
    pending_txs: &HashMap<H256, PendingTxInfo>,
) -> Result<()> {
    let env = Env::new();

    // make some real sandwiches with the optimized sandwiches
    let bot_balance = if env.debug {
        U256::MAX
    } else {
        get_token_balance(provider.clone(), bot_address, main_currency)
            .await
            .unwrap_or_default()
    };

    let (owner, bot_address) = if env.debug {
        (None, None)
    } else {
        (Some(owner), Some(bot_address))
    };

    // send single sandwich bundles at a time
    // you can improve the code here to send multiple sandwiches at once
    for (promising_tx_hash, sandwiches) in promising_sandwiches {
        for sandwich in sandwiches {
            let optimized = sandwich.optimized_sandwich.as_ref().unwrap();
            let mut bot_sandwich = sandwich.clone();
            let amount_in = std::cmp::min(bot_balance, optimized.amount_in);
            bot_sandwich.amount_in = amount_in;
            let bot_batch_sandwich = BatchSandwich {
                sandwiches: vec![bot_sandwich],
            };
            let bundle_id = bot_batch_sandwich.bundle_id();

            if simulated_bundle_ids.contains(&bundle_id) {
                continue;
            }

            match alert.send(&bundle_id).await {
                Err(e) => warn!("Telegram error: {e:?}"),
                _ => {}
            };
            simulated_bundle_ids.push_back(bundle_id);

            for sandwich in &bot_batch_sandwich.sandwiches {
                sandwich.pretty_print();
            }

            // start by setting both base and max fee as base_fee
            let base_fee = new_block.next_base_fee;
            let max_fee = base_fee;

            let simulated_sandwich = bot_batch_sandwich
                .simulate(
                    provider.clone(),
                    owner,
                    new_block.block_number,
                    base_fee,
                    max_fee,
                    None,
                    None,
                    bot_address,
                )
                .await;
            if simulated_sandwich.is_err() {
                let e = simulated_sandwich.as_ref().err().unwrap();
                warn!("BatchSandwich.simulate error: {e:?}");
                continue;
            }
            let simulated_sandwich = simulated_sandwich.unwrap();
            if simulated_sandwich.revenue <= 0 {
                continue;
            }

            let bribe_amount =
                (U256::from(simulated_sandwich.revenue) * bribe_pct) / U256::from(10000);

            let realistic_back_gas_limit = (simulated_sandwich.back_gas_used * 105) / 100;
            let max_priority_fee_per_gas = bribe_amount / U256::from(realistic_back_gas_limit);
            let max_fee_per_gas = base_fee + max_priority_fee_per_gas;

            info!(
                "🥪🥪🥪 Sandwiches: {:?}",
                bot_batch_sandwich.sandwiches.len()
            );
            info!(
                "> Base fee: {:?} / Priority fee: {:?} / Max fee: {:?} / Bribe: {:?}",
                base_fee, max_priority_fee_per_gas, max_fee_per_gas, bribe_amount
            );
            info!(
                "> Revenue: {:?} / Profit: {:?} / Gas cost: {:?}",
                simulated_sandwich.revenue, simulated_sandwich.profit, simulated_sandwich.gas_cost
            );
            info!(
                "> Front gas: {:?} / Back gas: {:?}",
                simulated_sandwich.front_gas_used, simulated_sandwich.back_gas_used
            );

            let message = format!(
                "[{:?}] Front: {:?} / Back: {:?} / Bribe: {:?}",
                promising_tx_hash,
                simulated_sandwich.front_gas_used,
                simulated_sandwich.back_gas_used,
                bribe_amount,
            );
            match alert.send(&message).await {
                Err(e) => warn!("Telegram error: {e:?}"),
                _ => {}
            }

            // set limit as 30% above what we simulated
            let front_gas_limit = (simulated_sandwich.front_gas_used * 13) / 10;
            let back_gas_limit = (simulated_sandwich.back_gas_used * 13) / 10;

            let victim_tx_hashes = bot_batch_sandwich.victim_tx_hashes();
            let mut victim_txs = Vec::new();
            for tx_hash in victim_tx_hashes {
                if let Some(tx_info) = pending_txs.get(&tx_hash) {
                    let tx = tx_info.pending_tx.tx.clone();
                    victim_txs.push(tx);
                }
            }

            let sando_bundle = executor
                .create_sando_bundle(
                    victim_txs,
                    simulated_sandwich.front_calldata,
                    simulated_sandwich.back_calldata,
                    simulated_sandwich.front_access_list,
                    simulated_sandwich.back_access_list,
                    front_gas_limit,
                    back_gas_limit,
                    base_fee,
                    max_priority_fee_per_gas,
                    max_fee_per_gas,
                )
                .await;
            if sando_bundle.is_err() {
                let e = sando_bundle.as_ref().err().unwrap();
                warn!("Executor.create_sando_bundle error: {e:?}");
                continue;
            }
            let sando_bundle = sando_bundle.unwrap();
            match send_sando_bundle_request(&executor, sando_bundle, new_block.block_number, &alert)
                .await
            {
                Err(e) => warn!("send_sando_bundle_request error: {e:?}"),
                _ => {}
            }
        }
    }

    Ok(())
}
