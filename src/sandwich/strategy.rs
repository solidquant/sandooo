use bounded_vec_deque::BoundedVecDeque;
use ethers::signers::{LocalWallet, Signer};
use ethers::{
    providers::{Middleware, Provider, Ws},
    types::{BlockNumber, H160, H256, U256, U64},
};
use log::{info, warn};
use std::{collections::HashMap, str::FromStr, sync::Arc};
use tokio::sync::broadcast::Sender;

use crate::common::alert::Alert;
use crate::common::constants::Env;
use crate::common::execution::Executor;
use crate::common::pools::{load_all_pools, Pool};
use crate::common::streams::{Event, NewBlock};
use crate::common::tokens::load_all_tokens;
use crate::common::utils::calculate_next_block_base_fee;
use crate::sandwich::appetizer::appetizer;
use crate::sandwich::main_dish::main_dish;
use crate::sandwich::simulation::{extract_swap_info, PendingTxInfo, Sandwich};

pub async fn run_sandwich_strategy(provider: Arc<Provider<Ws>>, event_sender: Sender<Event>) {
    let env = Env::new();

    let (pools, prev_pool_id) = load_all_pools(env.wss_url.clone(), 10000000, 50000)
        .await
        .unwrap();

    let block_number = provider.get_block_number().await.unwrap();
    let tokens_map = load_all_tokens(&provider, block_number, &pools, prev_pool_id)
        .await
        .unwrap();
    info!("Tokens map count: {:?}", tokens_map.len());

    // filter pools that don't have both token0 / token1 info
    let pools_vec: Vec<Pool> = pools
        .into_iter()
        .filter(|p| {
            let token0_exists = tokens_map.contains_key(&p.token0);
            let token1_exists = tokens_map.contains_key(&p.token1);
            token0_exists && token1_exists
        })
        .collect();
    info!("Filtered pools by tokens count: {:?}", pools_vec.len());

    let pools_map: HashMap<H160, Pool> = pools_vec
        .clone()
        .into_iter()
        .map(|p| (p.address, p))
        .collect();

    let block = provider
        .get_block(BlockNumber::Latest)
        .await
        .unwrap()
        .unwrap();
    let mut new_block = NewBlock {
        block_number: block.number.unwrap(),
        base_fee: block.base_fee_per_gas.unwrap(),
        next_base_fee: calculate_next_block_base_fee(
            block.gas_used,
            block.gas_limit,
            block.base_fee_per_gas.unwrap(),
        ),
    };

    let alert = Alert::new();
    let executor = Executor::new(provider.clone());

    let bot_address = H160::from_str(&env.bot_address).unwrap();
    let wallet = env
        .private_key
        .parse::<LocalWallet>()
        .unwrap()
        .with_chain_id(1 as u64);
    let owner = wallet.address();

    let mut event_receiver = event_sender.subscribe();

    let mut pending_txs: HashMap<H256, PendingTxInfo> = HashMap::new();
    let mut promising_sandwiches: HashMap<H256, Vec<Sandwich>> = HashMap::new();
    let mut simulated_bundle_ids = BoundedVecDeque::new(30);

    loop {
        match event_receiver.recv().await {
            Ok(event) => match event {
                Event::Block(block) => {
                    new_block = block;
                    info!("[Block #{:?}]", new_block.block_number);

                    // remove confirmed transactions
                    let block_with_txs = provider
                        .get_block_with_txs(new_block.block_number)
                        .await
                        .unwrap()
                        .unwrap();

                    let txs: Vec<H256> = block_with_txs
                        .transactions
                        .into_iter()
                        .map(|tx| tx.hash)
                        .collect();

                    for tx_hash in &txs {
                        if pending_txs.contains_key(tx_hash) {
                            // Remove any pending txs that have been confirmed
                            let removed = pending_txs.remove(tx_hash).unwrap();
                            promising_sandwiches.remove(tx_hash);
                            // info!(
                            //     "⚪️ V{:?} TX REMOVED: {:?} / Pending txs: {:?}",
                            //     removed.touched_pairs.get(0).unwrap().version,
                            //     tx_hash,
                            //     pending_txs.len()
                            // );
                        }
                    }

                    // remove pending txs older than 5 blocks
                    pending_txs.retain(|_, v| {
                        (new_block.block_number - v.pending_tx.added_block.unwrap()) < U64::from(3)
                    });
                    promising_sandwiches.retain(|h, _| pending_txs.contains_key(h));
                }
                Event::PendingTx(mut pending_tx) => {
                    let tx_hash = pending_tx.tx.hash;
                    let already_received = pending_txs.contains_key(&tx_hash);

                    let mut should_add = false;

                    if !already_received {
                        let tx_receipt = provider.get_transaction_receipt(tx_hash).await;
                        match tx_receipt {
                            Ok(receipt) => match receipt {
                                Some(_) => {
                                    // returning a receipt means that the tx is confirmed
                                    // should not be in pending_txs
                                    pending_txs.remove(&tx_hash);
                                }
                                None => {
                                    should_add = true;
                                }
                            },
                            _ => {}
                        }
                    }

                    let mut victim_gas_price = U256::zero();

                    match pending_tx.tx.transaction_type {
                        Some(tx_type) => {
                            if tx_type == U64::zero() {
                                victim_gas_price = pending_tx.tx.gas_price.unwrap_or_default();
                                should_add = victim_gas_price >= new_block.base_fee;
                            } else if tx_type == U64::from(2) {
                                victim_gas_price =
                                    pending_tx.tx.max_fee_per_gas.unwrap_or_default();
                                should_add = victim_gas_price >= new_block.base_fee;
                            }
                        }
                        _ => {}
                    }

                    let swap_info = if should_add {
                        match extract_swap_info(&provider, &new_block, &pending_tx, &pools_map)
                            .await
                        {
                            Ok(swap_info) => swap_info,
                            Err(e) => {
                                warn!("extract_swap_info error: {e:?}");
                                Vec::new()
                            }
                        }
                    } else {
                        Vec::new()
                    };

                    if swap_info.len() > 0 {
                        pending_tx.added_block = Some(new_block.block_number);
                        let pending_tx_info = PendingTxInfo {
                            pending_tx: pending_tx.clone(),
                            touched_pairs: swap_info.clone(),
                        };
                        pending_txs.insert(tx_hash, pending_tx_info.clone());
                        // info!(
                        //     "🔴 V{:?} TX ADDED: {:?} / Pending txs: {:?}",
                        //     pending_tx_info.touched_pairs.get(0).unwrap().version,
                        //     tx_hash,
                        //     pending_txs.len()
                        // );

                        match appetizer(
                            &provider,
                            &new_block,
                            tx_hash,
                            victim_gas_price,
                            &pending_txs,
                            &mut promising_sandwiches,
                        )
                        .await
                        {
                            Err(e) => warn!("appetizer error: {e:?}"),
                            _ => {}
                        }

                        if promising_sandwiches.len() > 0 {
                            match main_dish(
                                &provider,
                                &alert,
                                &executor,
                                &new_block,
                                owner,
                                bot_address,
                                U256::from(9900), // 99%
                                &promising_sandwiches,
                                &mut simulated_bundle_ids,
                                &pending_txs,
                            )
                            .await
                            {
                                Err(e) => warn!("main_dish error: {e:?}"),
                                _ => {}
                            }
                        }
                    }
                }
            },
            _ => {}
        }
    }
}
