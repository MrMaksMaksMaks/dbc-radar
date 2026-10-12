use anyhow::{Context, Result};
use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    pub telegram_token: String,
    pub bot_username: String,
    /// collector database (the bot reads it and appends pools it found on request over RPC)
    pub db_path: String,
    /// HTTP RPC for checking addresses that are not in the database
    pub rpc_url: String,
    /// node with full transaction history (e.g. Helius) — only for finding a pool by token
    /// address; empty — RPC_URL is used
    pub history_rpc_url: Option<String>,
    /// dbc-replay binary: the scores of all configs come from it (`risk --json`)
    pub replay_bin: String,
    /// how often to re-score configs, seconds
    pub refresh_secs: u64,
    /// channel or chat for alerts (numeric id or @username); empty — no alerts
    pub alert_chat: Option<String>,
    /// how often to post a summary to the channel, seconds
    pub digest_secs: u64,
    /// file with the list of configs that have already been alerted
    pub alerted_file: String,
    /// HTTP API address (e.g. 127.0.0.1:8080); empty — API disabled
    pub api_bind: Option<String>,
    /// API keys: a key allows looking up unknown addresses over RPC and a higher limit
    pub api_keys: Vec<String>,
    /// requests per minute per IP without a key / with a key
    pub api_rate_public: u32,
    pub api_rate_key: u32,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let get = |k: &str, d: &str| env::var(k).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| d.to_string());
        Ok(Self {
            telegram_token: env::var("TELEGRAM_BOT_TOKEN").context("TELEGRAM_BOT_TOKEN must be set")?,
            bot_username: get("TELEGRAM_BOT_USERNAME", "DBC_Radar_bot"),
            db_path: get("DB_PATH", "dbc.sqlite"),
            rpc_url: get("RPC_URL", "https://solana-rpc.publicnode.com"),
            history_rpc_url: env::var("HISTORY_RPC_URL").ok().filter(|v| !v.trim().is_empty()),
            replay_bin: get("REPLAY_BIN", "replay/target/release/dbc-replay"),
            refresh_secs: get("REFRESH_SECS", "300").parse().context("REFRESH_SECS")?,
            alert_chat: env::var("ALERT_CHAT").ok().filter(|v| !v.trim().is_empty()),
            digest_secs: get("DIGEST_SECS", "21600").parse().context("DIGEST_SECS")?,
            alerted_file: get("ALERTED_FILE", "bot_alerted.txt"),
            api_bind: env::var("API_BIND").ok().filter(|v| !v.trim().is_empty()),
            api_keys: get("API_KEYS", "").split(',').map(str::trim).filter(|k| !k.is_empty()).map(str::to_owned).collect(),
            api_rate_public: get("API_RATE_PUBLIC", "30").parse().context("API_RATE_PUBLIC")?,
            api_rate_key: get("API_RATE_KEY", "300").parse().context("API_RATE_KEY")?,
        })
    }
}
