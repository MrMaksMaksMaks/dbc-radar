use std::sync::Arc;

use anyhow::Result;
use dbc_radar_bot::{alerts, config::Config, handlers, store::State};
use teloxide::prelude::*;

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let cfg = Config::from_env()?;
    tracing::info!(db = %cfg.db_path, replay = %cfg.replay_bin, alerts = cfg.alert_chat.is_some(), "starting DBC Radar bot");
    let bot = Bot::new(cfg.telegram_token.clone());
    let state = Arc::new(State::new(cfg));

    // первый пересчёт до приёма сообщений, чтобы бот сразу отвечал вердиктами
    if let Err(e) = state.refresh().await {
        tracing::warn!("initial refresh failed: {e:#}");
    }
    tokio::spawn(alerts::run_refresher(bot.clone(), state.clone()));
    tokio::spawn(alerts::run_digest(bot.clone(), state.clone()));

    let handler = dptree::entry()
        .branch(Update::filter_message().endpoint(handlers::handle_message))
        .branch(Update::filter_callback_query().endpoint(handlers::handle_callback));

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![state])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
    Ok(())
}
