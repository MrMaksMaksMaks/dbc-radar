//! Discovery of new pools: a logsSubscribe subscription to the DBC program.
//! In the logs we look for an InitializeVirtualPool* instruction, then a separate worker
//! fetches the transaction over HTTP and decodes EvtInitializePool.
//!
//! Reliability:
//!   * several websocket sources in parallel (WS_URLS), duplicates are dropped;
//!   * watchdog timer: if a source is silent for longer than WS_STALL_SECS, reconnect
//!     (the DBC program is constantly active, silence means a stuck stream);
//!   * for addresses in WATCH_ADDRESSES — direct polling of their signatures over HTTP: full
//!     coverage of specific operators' launches regardless of websocket;
//!   * every signature is written to the discovery table with its source, to measure completeness.

use crate::app::App;
use crate::tx::parse_tx;
use anyhow::{bail, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::events::DBC_PROGRAM_ID;

const INIT_LOG_MARKER: &str = "Instruction: InitializeVirtualPool";

pub type Found = (String, String); // (signature, source)

pub async fn run_ws(app: Arc<App>, tx: mpsc::Sender<Found>, url: String, label: String) {
    loop {
        if let Err(e) = ws_session(&app, &tx, &url, &label).await {
            tracing::warn!(source = %label, "websocket session ended: {e:#}");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

async fn ws_session(app: &App, tx: &mpsc::Sender<Found>, url: &str, label: &str) -> Result<()> {
    let (mut ws, _) = connect_async(url).await?;
    let sub = json!({
        "jsonrpc": "2.0", "id": 1, "method": "logsSubscribe",
        "params": [{"mentions": [DBC_PROGRAM_ID]}, {"commitment": "confirmed"}]
    });
    ws.send(Message::Text(sub.to_string())).await?;
    tracing::info!(source = %label, "subscribed to DBC logs");

    let stall = Duration::from_secs(app.ws_stall_secs.max(5));
    let mut ping = tokio::time::interval(Duration::from_secs(20));
    let mut last_msg = tokio::time::Instant::now();
    loop {
        tokio::select! {
            _ = ping.tick() => {
                if last_msg.elapsed() > stall {
                    bail!("no notifications for {}s, reconnecting", stall.as_secs());
                }
                ws.send(Message::Ping(Vec::new())).await?;
            }
            msg = ws.next() => {
                let Some(msg) = msg else { bail!("stream closed") };
                match msg? {
                    Message::Text(t) => {
                        last_msg = tokio::time::Instant::now();
                        handle_notification(&t, tx, label);
                    }
                    Message::Ping(p) => ws.send(Message::Pong(p)).await?,
                    Message::Close(f) => bail!("closed by server: {f:?}"),
                    _ => {}
                }
            }
        }
    }
}

/// Direct polling of signatures of watched addresses (creators, fee receivers).
pub async fn run_watch(app: Arc<App>, tx: mpsc::Sender<Found>) {
    if app.watch.is_empty() {
        return;
    }
    let mut cursor: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    loop {
        for addr in &app.watch {
            let until = cursor.get(addr).cloned();
            let limit = if until.is_some() { 1000 } else { 200 };
            match app.rpc.get_signatures(addr, until.as_deref(), None, limit).await {
                Ok(sigs) => {
                    if let Some(newest) = sigs.first() {
                        cursor.insert(addr.clone(), newest.signature.clone());
                    }
                    for s in sigs.iter().rev().filter(|s| !s.failed) {
                        if tx.send((s.signature.clone(), "watch".into())).await.is_err() {
                            return;
                        }
                    }
                }
                Err(e) => {
                    if format!("{e:#}").contains("not found") {
                        cursor.remove(addr); // RPC forgot the cursor signature: start over
                    }
                    tracing::warn!(%addr, "watch poll failed: {e:#}");
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(app.poll_interval_secs.max(5))).await;
    }
}

fn handle_notification(text: &str, tx: &mpsc::Sender<Found>, label: &str) {
    let Ok(v) = serde_json::from_str::<Value>(text) else { return };
    let Some(val) = v.pointer("/params/result/value") else { return };
    if !val.get("err").map(Value::is_null).unwrap_or(true) {
        return;
    }
    let is_init = val
        .get("logs")
        .and_then(Value::as_array)
        .map(|logs| logs.iter().any(|l| l.as_str().is_some_and(|s| s.contains(INIT_LOG_MARKER))))
        .unwrap_or(false);
    if !is_init {
        return;
    }
    if let Some(sig) = val.get("signature").and_then(Value::as_str) {
        if tx.try_send((sig.to_string(), label.to_string())).is_err() {
            tracing::warn!("discovery queue full, dropping {sig}");
        }
    }
}

/// Fetches the found transactions (from all sources) and writes them to the database.
pub async fn run_worker(app: Arc<App>, mut rx: mpsc::Receiver<Found>) {
    const RECENT: usize = 200_000;
    let mut seen: HashSet<String> = HashSet::new();
    let mut order: VecDeque<String> = VecDeque::new();
    while let Some((sig, source)) = rx.recv().await {
        if let Err(e) = app.db.record_discovery(&sig, &source) {
            tracing::warn!("record discovery: {e:#}");
        }
        if !seen.insert(sig.clone()) {
            continue; // already processed from another source
        }
        order.push_back(sig.clone());
        if order.len() > RECENT {
            if let Some(old) = order.pop_front() {
                seen.remove(&old);
            }
        }
        if let Err(e) = process_creation(&app, &sig).await {
            tracing::warn!(%source, "tx {sig}: {e:#}");
        }
    }
}

async fn process_creation(app: &App, sig: &str) -> Result<()> {
    // Right after the notification the RPC may not serve the transaction yet.
    for attempt in 0..6u64 {
        if let Some(raw) = app.rpc.get_transaction(sig).await? {
            let parsed = parse_tx(&raw)?;
            return app.ingest(sig, &parsed).await;
        }
        tokio::time::sleep(Duration::from_millis(800 * (attempt + 1))).await;
    }
    bail!("transaction not available")
}
