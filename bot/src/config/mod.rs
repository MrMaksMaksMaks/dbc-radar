use anyhow::{Context, Result};
use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    pub telegram_token: String,
    pub bot_username: String,
    /// база сборщика (бот читает её и дописывает пулы, найденные по запросу через RPC)
    pub db_path: String,
    /// HTTP RPC для проверки адресов, которых нет в базе
    pub rpc_url: String,
    /// узел с полной историей транзакций (например, Helius) — только для поиска пула по адресу
    /// токена; пусто — используется RPC_URL
    pub history_rpc_url: Option<String>,
    /// бинарник dbc-replay: из него берётся оценка всех конфигов (`risk --json`)
    pub replay_bin: String,
    /// как часто пересчитывать оценку конфигов, секунды
    pub refresh_secs: u64,
    /// канал или чат для оповещений (числовой id или @username); пусто — без оповещений
    pub alert_chat: Option<String>,
    /// как часто публиковать сводку в канал, секунды
    pub digest_secs: u64,
    /// файл со списком конфигов, о которых уже было оповещение
    pub alerted_file: String,
    /// адрес HTTP API (например 127.0.0.1:8080); пусто — API выключен
    pub api_bind: Option<String>,
    /// ключи API: с ключом разрешён поиск неизвестных адресов через RPC и выше лимит
    pub api_keys: Vec<String>,
    /// запросов в минуту с одного IP без ключа / с ключом
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
