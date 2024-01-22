use anyhow::Result;
use ethers::types::{H256, U64};
use teloxide::prelude::*;
use teloxide::types::ChatId;

use crate::common::constants::Env;

pub struct Alert {
    pub bot: Option<Bot>,
    pub chat_id: Option<ChatId>,
}

impl Alert {
    pub fn new() -> Self {
        let env = Env::new();
        if env.use_alert {
            let bot = Bot::from_env();
            let chat_id = ChatId(env.telegram_chat_id.parse::<i64>().unwrap());
            Self {
                bot: Some(bot),
                chat_id: Some(chat_id),
            }
        } else {
            Self {
                bot: None,
                chat_id: None,
            }
        }
    }

    pub async fn send(&self, message: &str) -> Result<()> {
        match &self.bot {
            Some(bot) => {
                bot.send_message(self.chat_id.unwrap(), message).await?;
            }
            _ => {}
        }
        Ok(())
    }

    pub async fn send_bundle_sent(
        &self,
        block_number: U64,
        tx_hash: H256,
        gambit_hash: H256,
    ) -> Result<()> {
        let eigenphi_url = format!("https://eigenphi.io/mev/eigentx/{:?}", tx_hash);
        let gambit_url = format!("https://gmbit-co.vercel.app/auction?txHash={:?}", tx_hash);
        let mut message = format!("[Block #{:?}] Bundle sent: {:?}", block_number, tx_hash);
        message = format!("{}\n-Eigenphi: {}", message, eigenphi_url);
        message = format!("{}\n-Gambit: {}", message, gambit_url);
        message = format!("{}\n-Gambit bundle hash: {:?}", message, gambit_hash);
        self.send(&message).await?;
        Ok(())
    }
}
