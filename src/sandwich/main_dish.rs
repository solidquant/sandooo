use anyhow::Result;
use bounded_vec_deque::BoundedVecDeque;
use ethers::{
    providers::{Provider, Ws},
    types::{H160, H256, U256},
};
use log::{info, warn};
use std::{collections::HashMap, sync::Arc};

use crate::common::alert::Alert;
use crate::common::constants::Env;
use crate::common::streams::NewBlock;
use crate::common::utils::get_token_balance;
use crate::sandwich::simulation::{BatchSandwich, PendingTxInfo, Sandwich};

pub async fn main_dish(
    provider: &Arc<Provider<Ws>>,
    alert: &Alert,
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

            info!("----- Bundle ID: {} -----", bundle_id);
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

            let (bribe_amount, front_access_list, back_access_list) = match bot_batch_sandwich
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
                .await
            {
                Ok(simulated_sandwich) => {
                    if simulated_sandwich.revenue > 0 {
                        let bribe_amount = (U256::from(simulated_sandwich.revenue) * bribe_pct)
                            / U256::from(10000);
                        (
                            bribe_amount,
                            Some(simulated_sandwich.front_access_list),
                            Some(simulated_sandwich.back_access_list),
                        )
                    } else {
                        (U256::zero(), None, None)
                    }
                }
                Err(e) => {
                    warn!("bribe_amount simulated failed: {e:?}");
                    (U256::zero(), None, None)
                }
            };

            if bribe_amount.is_zero() {
                continue;
            }

            // final simulation
            let simulated_sandwich = bot_batch_sandwich
                .simulate(
                    provider.clone(),
                    owner,
                    new_block.block_number,
                    base_fee,
                    max_fee,
                    front_access_list,
                    back_access_list,
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
            // set limit as 30% above what we simulated
            let front_gas_limit = (simulated_sandwich.front_gas_used * 13) / 10;
            let back_gas_limit = (simulated_sandwich.back_gas_used * 13) / 10;

            let realistic_back_gas_limit = (simulated_sandwich.back_gas_used * 105) / 100;
            let max_priority_fee_per_gas = bribe_amount / U256::from(realistic_back_gas_limit);
            let max_fee_per_gas = base_fee + max_priority_fee_per_gas;

            info!("Sandwiches: {:?}", bot_batch_sandwich.sandwiches.len());
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
        }
    }

    Ok(())
}
