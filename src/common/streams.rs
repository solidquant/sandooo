use ethers::{
    providers::{Middleware, Provider, Ws},
    types::*,
};
use std::sync::Arc;
use tokio::sync::broadcast::Sender;
use tokio_stream::StreamExt;

use crate::common::utils::calculate_next_block_base_fee;

#[derive(Default, Debug, Clone)]
pub struct NewBlock {
    pub block_number: U64,
    pub base_fee: U256,
    pub next_base_fee: U256,
}

#[derive(Debug, Clone)]
pub struct NewPendingTx {
    pub added_block: Option<U64>,
    pub tx: Transaction,
}

impl Default for NewPendingTx {
    fn default() -> Self {
        Self {
            added_block: None,
            tx: Transaction::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum Event {
    Block(NewBlock),
    PendingTx(NewPendingTx),
}

pub async fn stream_new_blocks(provider: Arc<Provider<Ws>>, event_sender: Sender<Event>) {
    let stream = provider.subscribe_blocks().await.unwrap();
    let mut stream = stream.filter_map(|block| match block.number {
        Some(number) => Some(NewBlock {
            block_number: number,
            base_fee: block.base_fee_per_gas.unwrap_or_default(),
            next_base_fee: U256::from(calculate_next_block_base_fee(
                block.gas_used,
                block.gas_limit,
                block.base_fee_per_gas.unwrap_or_default(),
            )),
        }),
        None => None,
    });

    while let Some(block) = stream.next().await {
        match event_sender.send(Event::Block(block)) {
            Ok(_) => {}
            Err(_) => {}
        }
    }
}

pub async fn stream_pending_transactions(provider: Arc<Provider<Ws>>, event_sender: Sender<Event>) {
    let stream = provider.subscribe_pending_txs().await.unwrap();
    let mut stream = stream.transactions_unordered(256).fuse();

    while let Some(result) = stream.next().await {
        match result {
            Ok(tx) => match event_sender.send(Event::PendingTx(NewPendingTx {
                added_block: None,
                tx,
            })) {
                Ok(_) => {}
                Err(_) => {}
            },
            Err(_) => {}
        };
    }
}
