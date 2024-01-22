pub static PROJECT_NAME: &str = "sandooo";

pub fn get_env(key: &str) -> String {
    std::env::var(key).unwrap_or(String::from(""))
}

#[derive(Debug, Clone)]
pub struct Env {
    pub https_url: String,
    pub wss_url: String,
    pub bot_address: String,
    pub private_key: String,
    pub identity_key: String,
    pub telegram_token: String,
    pub telegram_chat_id: String,
    pub use_alert: bool,
    pub debug: bool,
}

impl Env {
    pub fn new() -> Self {
        Env {
            https_url: get_env("HTTPS_URL"),
            wss_url: get_env("WSS_URL"),
            bot_address: get_env("BOT_ADDRESS"),
            private_key: get_env("PRIVATE_KEY"),
            identity_key: get_env("IDENTITY_KEY"),
            telegram_token: get_env("TELEGRAM_TOKEN"),
            telegram_chat_id: get_env("TELEGRAM_CHAT_ID"),
            use_alert: get_env("USE_ALERT").parse::<bool>().unwrap(),
            debug: get_env("DEBUG").parse::<bool>().unwrap(),
        }
    }
}

pub static COINBASE: &str = "0xDAFEA492D9c6733ae3d56b7Ed1ADB60692c98Bc5"; // Flashbots Builder

pub static WETH: &str = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";
pub static WETH_BALANCE_SLOT: i32 = 3;
pub static WETH_DECIMALS: u8 = 18;
