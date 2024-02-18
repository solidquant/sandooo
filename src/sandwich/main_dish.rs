use anyhow::Result;
use bounded_vec_deque::BoundedVecDeque;
use ethers::{
    providers::{Provider, Ws},
    types::{H160, H256, U256, U64},
};
use log::{info, warn};
use std::str::FromStr;
use std::{collections::HashMap, sync::Arc};

use crate::common::alert::Alert;
use crate::common::constants::*;
use crate::common::execution::{Executor, SandoBundle};
use crate::common::streams::NewBlock;
use crate::common::utils::get_token_balance;
use crate::sandwich::simulation::{BatchSandwich, PendingTxInfo, Sandwich};

pub async fn get_token_balances(
    provider: &Arc<Provider<Ws>>,
    owner: H160,
    tokens: &Vec<H160>,
) -> HashMap<H160, U256> {
    let mut token_balances = HashMap::new();
    for token in tokens {
        let balance = get_token_balance(provider.clone(), owner, *token)
            .await
            .unwrap_or_default();
        token_balances.insert(*token, balance);
    }
    token_balances
}

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

#[derive(Debug, Clone)]
pub struct Ingredients {
    pub tx_hash: H256,
    pub pair: H160,
    pub main_currency: H160,
    pub amount_in: U256,
    pub max_revenue: U256,
    pub score: f64,
    pub sandwich: Sandwich,
}

pub async fn main_dish(
    provider: &Arc<Provider<Ws>>,
    alert: &Alert,
    executor: &Executor,
    new_block: &NewBlock,
    owner: H160,
    bot_address: H160,
    bribe_pct: U256,
    promising_sandwiches: &HashMap<H256, Vec<Sandwich>>,
    simulated_bundle_ids: &mut BoundedVecDeque<String>,
    pending_txs: &HashMap<H256, PendingTxInfo>,
) -> Result<()> {
    let env = Env::new();

    let weth = H160::from_str(WETH).unwrap();
    let usdt = H160::from_str(USDT).unwrap();
    let usdc = H160::from_str(USDC).unwrap();

    let bot_balances = if env.debug {
        // assume you have infinite funds when debugging
        let mut bot_balances = HashMap::new();
        bot_balances.insert(weth, U256::MAX);
        bot_balances.insert(usdt, U256::MAX);
        bot_balances.insert(usdc, U256::MAX);
        bot_balances
    } else {
        let bot_balances =
            get_token_balances(&provider, bot_address, &vec![weth, usdt, usdc]).await;
        bot_balances
    };

    let mut plate = Vec::new();
    for (promising_tx_hash, sandwiches) in promising_sandwiches {
        for sandwich in sandwiches {
            let optimized_sandwich = sandwich.optimized_sandwich.as_ref().unwrap();
            let amount_in = optimized_sandwich.amount_in;
            let max_revenue = optimized_sandwich.max_revenue;
            let score = (max_revenue.as_u128() as f64) / (amount_in.as_u128() as f64);
            let clean_sandwich = Sandwich {
                amount_in,
                swap_info: sandwich.swap_info.clone(),
                victim_tx: sandwich.victim_tx.clone(),
                optimized_sandwich: None,
            };
            let ingredients = Ingredients {
                tx_hash: *promising_tx_hash,
                pair: sandwich.swap_info.target_pair,
                main_currency: sandwich.swap_info.main_currency,
                amount_in,
                max_revenue,
                score,
                sandwich: clean_sandwich,
            };
            plate.push(ingredients);
        }
    }

    /*
    [Multi-sandwich algorithm] Sorting by score.

    Score is calculated as: revenue / amount_in
    Sorting by score in descending order will place the sandwich opportunities with higer scores at the top
    This is good because we want to invest in opportunities that have the greatest return over cost ratios

    * Note:
    Score for WETH pairs and USDT/USDC pairs will be different in scale.
    USDT/USDC pairs will always have bigger scores, because amount_in is represented as stable amounts (decimals = 6)
    and max_revenue is represented as WETH amount (decimals = 18)
    However, this is good, because we can pick up stable sandwiches first (where there's less competition)

    After we've go through all stable pair sandwiches, we next pick up WETH pairs by score order
    */
    plate.sort_by(|x, y| y.score.partial_cmp(&x.score).unwrap());

    /*
    Say you have: [sando1, sando2, sando3] on your plate
    We then want to send bundles as such:
    - <sando1>
    - <sando1, sando2>
    - <sando1, sando2, sando3>
    3 bundles in total. This way you can optimize your profits.
    However, if you have infinite funds, you can always group all of the sandwich opportunities.
    */
    for i in 0..plate.len() {
        let mut balances = bot_balances.clone();
        let mut sandwiches = Vec::new();

        for j in 0..(i + 1) {
            let ingredient = &plate[j];
            let main_currency = ingredient.main_currency;
            let balance = *balances.get(&main_currency).unwrap();
            let optimized = ingredient.amount_in;
            let amount_in = std::cmp::min(balance, optimized);

            let mut final_sandwich = ingredient.sandwich.clone();
            final_sandwich.amount_in = amount_in;

            let new_balance = balance - amount_in;
            balances.insert(main_currency, new_balance);

            sandwiches.push(final_sandwich);
        }

        let final_batch_sandwich = BatchSandwich { sandwiches };

        let bundle_id = final_batch_sandwich.bundle_id();

        if simulated_bundle_ids.contains(&bundle_id) {
            continue;
        }

        simulated_bundle_ids.push_back(bundle_id.clone());

        let base_fee = new_block.next_base_fee;
        let max_fee = base_fee;

        let (owner, bot_address) = if env.debug {
            (None, None)
        } else {
            (Some(owner), Some(bot_address))
        };

        // set bribe amount as 1 initially, just so we can add the bribe operation gas usage
        // we'll figure out the priority fee and the bribe amount after this simulation
        let (bribe_amount, front_access_list, back_access_list) = match final_batch_sandwich
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
                    let bribe_amount =
                        (U256::from(simulated_sandwich.revenue) * bribe_pct) / U256::from(10000);
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
        let simulated_sandwich = final_batch_sandwich
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

        info!(
            "🥪🥪🥪 Sandwiches: {:?} ({})",
            final_batch_sandwich.sandwiches.len(),
            bundle_id
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
            bundle_id,
            simulated_sandwich.front_gas_used,
            simulated_sandwich.back_gas_used,
            bribe_amount,
        );
        match alert.send(&message).await {
            Err(e) => warn!("Telegram error: {e:?}"),
            _ => {}
        }

        let victim_tx_hashes = final_batch_sandwich.victim_tx_hashes();
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

    Ok(())
}
