//! dbc-collector: собирает пулы и свопы Meteora DBC в SQLite.
//!
//! Задачи:
//!   - discovery::run_ws     — websocket logsSubscribe, ловит создание пулов;
//!   - discovery::run_worker — забирает транзакции создания и пишет пулы;
//!   - poller::run           — собирает свопы отслеживаемых пулов;
//!   - status::run           — отмечает graduation по аккаунтам пулов (все пулы, не только отслеживаемые).

mod app;
mod db;
mod discovery;
mod events;
mod poller;
mod rpc;
mod status;
mod trace;
mod tx;

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::mpsc;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| default.to_string())
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let rpc_url = env_or("RPC_URL", "https://solana-rpc.publicnode.com");
    let ws_urls: Vec<String> = env_or("WS_URLS", &env_or("WS_URL", "wss://solana-rpc.publicnode.com"))
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    let ws_stall_secs: u64 = env_or("WS_STALL_SECS", "20").parse().context("WS_STALL_SECS")?;
    let watch: HashSet<String> = env_or("WATCH_ADDRESSES", "")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    let rps: f64 = env_or("RPC_RPS", "5").parse().context("RPC_RPS")?;
    let db_path = env_or("DB_PATH", "dbc.sqlite");
    let sample_every: u64 = env_or("SAMPLE_EVERY", "4").parse().context("SAMPLE_EVERY")?;
    let track_hours: i64 = env_or("TRACK_HOURS", "24").parse().context("TRACK_HOURS")?;
    let poll_interval_secs: u64 =
        env_or("POLL_INTERVAL_SECS", "20").parse().context("POLL_INTERVAL_SECS")?;
    // проверка graduation по аккаунтам: как часто и за сколько часов назад
    let grad_check_secs: u64 = env_or("GRAD_CHECK_SECS", "120").parse().context("GRAD_CHECK_SECS")?;
    let grad_check_hours: i64 = env_or("GRAD_CHECK_HOURS", "24").parse().context("GRAD_CHECK_HOURS")?;
    // отдельный узел для этой проверки (у PublicNode предел ~6 адресов на getMultipleAccounts)
    let status_rpc_url = env_or("STATUS_RPC_URL", &rpc_url);
    let status_rps: f64 = env_or("STATUS_RPC_RPS", "2").parse().context("STATUS_RPC_RPS")?;
    let config_allowlist: HashSet<String> = env_or("CONFIG_ALLOWLIST", "")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();

    tracing::info!(
        %rpc_url, ws_sources = ws_urls.len(), watch = watch.len(), rps, %db_path, sample_every, track_hours,
        allowlist = config_allowlist.len(), "starting dbc-collector"
    );

    // Режим проверки: `dbc-collector decode <signature>` — разобрать одну транзакцию
    // и напечатать найденные события DBC, ничего не записывая в базу.
    let args: Vec<String> = std::env::args().collect();
    // Режим доказательств: `dbc-collector trace <pool prefix> [--csv export.csv]`
    if args.get(1).map(String::as_str) == Some("trace") {
        let prefix = args.get(2).context("usage: dbc-collector trace <pool prefix> [--csv solscan_export.csv]")?;
        let csv = args.iter().position(|a| a == "--csv").and_then(|i| args.get(i + 1)).map(String::as_str);
        let rpc = rpc::Rpc::new(rpc_url, rps);
        return trace::run(&rpc, &db_path, prefix, csv).await;
    }
    if args.get(1).map(String::as_str) == Some("decode") {
        let sig = args.get(2).context("usage: dbc-collector decode <signature>")?;
        let rpc = rpc::Rpc::new(rpc_url, rps);
        let raw = rpc.get_transaction(sig).await?.context("transaction not found")?;
        let parsed = tx::parse_tx(&raw)?;
        println!("slot {} time {:?} fee_payer {}", parsed.slot, parsed.block_time, parsed.fee_payer);
        for (i, ev) in &parsed.events {
            println!("#{i}: {ev:#?}");
        }
        if parsed.events.is_empty() {
            println!("no DBC events found");
        }
        return Ok(());
    }

    // Разовый проход: `dbc-collector grad-sweep <hours>` — отметить graduation по аккаунтам
    // пулов, созданных за последние <hours> часов (например, для старых данных).
    if args.get(1).map(String::as_str) == Some("grad-sweep") {
        let hours: i64 = args.get(2).context("usage: dbc-collector grad-sweep <hours>")?.parse().context("hours")?;
        let rpc = rpc::Rpc::new(status_rpc_url, status_rps);
        let db = db::Db::open(&db_path)?;
        let (checked, found) = status::sweep(&rpc, &db, hours * 3600).await?;
        println!("checked {checked} pools without graduation, found {found} graduated");
        return Ok(());
    }

    tracing::info!(%status_rpc_url, status_rps, "graduation check from pool accounts");
    let app = Arc::new(app::App {
        rpc: rpc::Rpc::new(rpc_url, rps),
        status_rpc: rpc::Rpc::new(status_rpc_url, status_rps),
        db: db::Db::open(&db_path)?,
        ws_urls,
        ws_stall_secs,
        watch,
        sample_every,
        config_allowlist,
        track_secs: track_hours * 3600,
        poll_interval_secs,
    });

    let (tx, rx) = mpsc::channel::<discovery::Found>(50_000);
    for (i, url) in app.ws_urls.iter().enumerate() {
        tokio::spawn(discovery::run_ws(app.clone(), tx.clone(), url.clone(), format!("ws:{i}")));
    }
    tokio::spawn(discovery::run_watch(app.clone(), tx.clone()));
    drop(tx);
    tokio::spawn(discovery::run_worker(app.clone(), rx));
    tokio::spawn(poller::run(app.clone()));
    tokio::spawn(status::run(app.clone(), grad_check_secs, grad_check_hours * 3600));

    tokio::signal::ctrl_c().await?;
    tracing::info!("shutting down");
    Ok(())
}
