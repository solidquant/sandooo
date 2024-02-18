use anyhow::Result;
use ethers::prelude::*;
use ethers::providers::{Middleware, Provider};
use ethers::signers::{LocalWallet, Signer};
use ethers::types::transaction::{eip2718::TypedTransaction, eip2930::AccessList};
use ethers_flashbots::*;
use serde::Deserialize;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use url::Url;

use crate::common::abi::Abi;
use crate::common::constants::Env;

#[derive(Debug, Clone)]
pub struct SandoBundle {
    pub frontrun_tx: TypedTransaction,
    pub victim_txs: Vec<Transaction>,
    pub backrun_tx: TypedTransaction,
}

#[derive(Debug, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct SendBundleResponse {
    pub bundle_hash: BundleHash,
}

pub async fn send_bundle(
    builder: String,
    relay_url: Url,
    identity: LocalWallet,
    bundle: BundleRequest,
) -> Result<(String, Option<SendBundleResponse>)> {
    let relay = Relay::new(relay_url, Some(identity.clone()));
    let result: Option<SendBundleResponse> = relay.request("eth_sendBundle", [bundle]).await?;
    Ok((builder, result))
}

pub struct Executor {
    pub provider: Arc<Provider<Ws>>,
    pub abi: Abi,
    pub owner: LocalWallet,
    pub identity: LocalWallet,
    pub bot_address: H160,
    pub builder_urls: HashMap<String, Url>,
    pub client: SignerMiddleware<FlashbotsMiddleware<Arc<Provider<Ws>>, LocalWallet>, LocalWallet>,
}

impl Executor {
    pub fn new(provider: Arc<Provider<Ws>>) -> Self {
        let env = Env::new();
        let abi = Abi::new();
        let bot_address = H160::from_str(&env.bot_address).unwrap();

        let owner = env
            .private_key
            .parse::<LocalWallet>()
            .unwrap()
            .with_chain_id(1 as u64);

        let identity = env
            .identity_key
            .parse::<LocalWallet>()
            .unwrap()
            .with_chain_id(1 as u64);

        let relay_url = Url::parse("https://relay.flashbots.net").unwrap();

        let client = SignerMiddleware::new(
            FlashbotsMiddleware::new(provider.clone(), relay_url.clone(), identity.clone()),
            owner.clone(),
        );

        // The endpoints here will gracefully fail if it doesn't work
        let mut builder_urls = HashMap::new();
        builder_urls.insert(
            "flashbots".to_string(),
            Url::parse("https://relay.flashbots.net").unwrap(),
        );
        builder_urls.insert(
            "beaverbuild".to_string(),
            Url::parse("https://rpc.beaverbuild.org").unwrap(),
        );
        builder_urls.insert(
            "rsync".to_string(),
            Url::parse("https://rsync-builder.xyz").unwrap(),
        );
        builder_urls.insert(
            "titanbuilder".to_string(),
            Url::parse("https://rpc.titanbuilder.xyz").unwrap(),
        );
        builder_urls.insert(
            "builder0x69".to_string(),
            Url::parse("https://builder0x69.io").unwrap(),
        );
        builder_urls.insert("f1b".to_string(), Url::parse("https://rpc.f1b.io").unwrap());
        builder_urls.insert(
            "lokibuilder".to_string(),
            Url::parse("https://rpc.lokibuilder.xyz").unwrap(),
        );
        builder_urls.insert(
            "eden".to_string(),
            Url::parse("https://api.edennetwork.io/v1/rpc").unwrap(),
        );
        builder_urls.insert(
            "penguinbuild".to_string(),
            Url::parse("https://rpc.penguinbuild.org").unwrap(),
        );
        builder_urls.insert(
            "gambit".to_string(),
            Url::parse("https://builder.gmbit.co/rpc").unwrap(),
        );
        builder_urls.insert(
            "idcmev".to_string(),
            Url::parse("https://rpc.idcmev.xyz").unwrap(),
        );

        Self {
            provider,
            abi,
            owner,
            identity,
            bot_address,
            builder_urls,
            client,
        }
    }

    pub async fn _common_fields(&self) -> Result<(H160, U256, U64)> {
        let nonce = self
            .provider
            .get_transaction_count(self.owner.address(), Some(BlockNumber::Latest.into()))
            .await?;
        Ok((self.owner.address(), U256::from(nonce), U64::from(1)))
    }

    pub async fn transfer_in_tx(&self, amount_in: U256) -> Result<TypedTransaction> {
        let tx = {
            let mut inner: TypedTransaction =
                TransactionRequest::pay(self.bot_address, amount_in).into();
            self.client
                .fill_transaction(&mut inner, None)
                .await
                .unwrap();
            inner
        };
        Ok(tx)
    }

    pub async fn transfer_out_tx(
        &self,
        token: H160,
        amount: U256,
        max_priority_fee_per_gas: U256,
        max_fee_per_gas: U256,
    ) -> Result<TypedTransaction> {
        let common = self._common_fields().await?;
        let calldata = self.abi.sando_bot.encode("recoverToken", (token, amount))?;
        let to = NameOrAddress::Address(self.bot_address);
        Ok(TypedTransaction::Eip1559(Eip1559TransactionRequest {
            to: Some(to),
            from: Some(common.0),
            data: Some(calldata),
            value: Some(U256::zero()),
            chain_id: Some(common.2),
            max_priority_fee_per_gas: Some(max_priority_fee_per_gas),
            max_fee_per_gas: Some(max_fee_per_gas),
            gas: Some(U256::from(600000)),
            nonce: Some(common.1),
            access_list: AccessList::default(),
        }))
    }

    pub async fn to_typed_transaction(
        &self,
        calldata: Bytes,
        access_list: AccessList,
        gas_limit: u64,
        nonce: U256,
        max_priority_fee_per_gas: U256,
        max_fee_per_gas: U256,
    ) -> Result<TypedTransaction> {
        let common = self._common_fields().await?;
        let to = NameOrAddress::Address(self.bot_address);
        Ok(TypedTransaction::Eip1559(Eip1559TransactionRequest {
            to: Some(to.clone()),
            from: Some(common.0),
            data: Some(calldata),
            value: Some(U256::zero()),
            chain_id: Some(common.2),
            max_priority_fee_per_gas: Some(max_priority_fee_per_gas),
            max_fee_per_gas: Some(max_fee_per_gas),
            gas: Some(U256::from(gas_limit)),
            nonce: Some(nonce),
            access_list,
        }))
    }

    pub async fn create_sando_bundle(
        &self,
        victim_txs: Vec<Transaction>,
        front_calldata: Bytes,
        back_calldata: Bytes,
        front_access_list: AccessList,
        back_access_list: AccessList,
        front_gas_limit: u64,
        back_gas_limit: u64,
        base_fee: U256,
        max_priority_fee_per_gas: U256,
        max_fee_per_gas: U256,
    ) -> Result<SandoBundle> {
        let common = self._common_fields().await?;
        let to = NameOrAddress::Address(self.bot_address);
        let front_nonce = common.1;
        let back_nonce = front_nonce + U256::from(1); // should increase nonce by 1
        let frontrun_tx = TypedTransaction::Eip1559(Eip1559TransactionRequest {
            to: Some(to.clone()),
            from: Some(common.0),
            data: Some(front_calldata),
            value: Some(U256::zero()),
            chain_id: Some(common.2),
            max_priority_fee_per_gas: Some(U256::zero()),
            max_fee_per_gas: Some(base_fee),
            gas: Some(U256::from(front_gas_limit)),
            nonce: Some(front_nonce),
            access_list: front_access_list,
        });
        let backrun_tx = TypedTransaction::Eip1559(Eip1559TransactionRequest {
            to: Some(to),
            from: Some(common.0),
            data: Some(back_calldata),
            value: Some(U256::zero()),
            chain_id: Some(common.2),
            max_priority_fee_per_gas: Some(max_priority_fee_per_gas),
            max_fee_per_gas: Some(max_fee_per_gas),
            gas: Some(U256::from(back_gas_limit)),
            nonce: Some(back_nonce),
            access_list: back_access_list,
        });
        Ok(SandoBundle {
            frontrun_tx,
            victim_txs,
            backrun_tx,
        })
    }

    pub async fn to_bundle_request(
        &self,
        tx: TypedTransaction,
        block_number: U64,
        retries: usize,
    ) -> Result<BundleRequest> {
        let signature = self.client.signer().sign_transaction(&tx).await?;
        let bundle = BundleRequest::new()
            .push_transaction(tx.rlp_signed(&signature))
            .set_block(block_number + U64::from(retries))
            .set_simulation_block(block_number)
            .set_simulation_timestamp(0);
        Ok(bundle)
    }

    pub async fn to_sando_bundle_request(
        &self,
        sando_bundle: SandoBundle,
        block_number: U64,
        retries: usize,
    ) -> Result<BundleRequest> {
        let frontrun_signature = self
            .client
            .signer()
            .sign_transaction(&sando_bundle.frontrun_tx)
            .await?;
        let signed_frontrun_tx = sando_bundle.frontrun_tx.rlp_signed(&frontrun_signature);

        let backrun_signature = self
            .client
            .signer()
            .sign_transaction(&sando_bundle.backrun_tx)
            .await?;
        let signed_backrun_tx = sando_bundle.backrun_tx.rlp_signed(&backrun_signature);

        let mut bundle = BundleRequest::new()
            .set_block(block_number + U64::from(retries))
            .set_simulation_block(block_number)
            .set_simulation_timestamp(0);

        bundle = bundle.push_transaction(signed_frontrun_tx);
        for victim_tx in &sando_bundle.victim_txs {
            let signed_victim_tx = victim_tx.rlp();
            bundle = bundle.push_transaction(signed_victim_tx);
        }
        bundle = bundle.push_transaction(signed_backrun_tx);

        Ok(bundle)
    }

    pub async fn simulate_bundle(&self, bundle: &BundleRequest) {
        match self.client.inner().simulate_bundle(bundle).await {
            Ok(simulated) => {
                println!("{:?}", simulated);
            }
            Err(e) => {
                println!("Flashbots bundle simulation error: {e:?}");
            }
        }
    }

    pub async fn broadcast_bundle(
        &self,
        bundle: BundleRequest,
    ) -> Result<HashMap<String, SendBundleResponse>> {
        let mut requests = Vec::new();
        for (builder, url) in &self.builder_urls {
            requests.push(tokio::task::spawn(send_bundle(
                builder.clone(),
                url.clone(),
                self.identity.clone(),
                bundle.clone(),
            )));
        }
        let results = futures::future::join_all(requests).await;
        let mut response_map = HashMap::new();
        for result in results {
            match result {
                Ok(response) => match response {
                    Ok(bundle_response) => {
                        let builder = bundle_response.0;
                        let send_bundle_response = bundle_response.1.unwrap_or_default();
                        response_map.insert(builder.clone(), send_bundle_response);
                    }
                    Err(_) => {}
                },
                Err(_) => {}
            }
        }

        Ok(response_map)
    }
}
