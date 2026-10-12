//! Swap collection: in a loop we poll the signatures of every tracked pool,
//! starting from the last processed one, and parse new transactions from oldest to newest.

use crate::app::App;
use futures_util::StreamExt;
use crate::rpc::SigInfo;
use crate::tx::parse_tx;
use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;

const PAGE: usize = 1000;
const WARN_BACKLOG: usize = 5000;

/// How many transactions of one pool to process per round: a heavy pool must not
/// hold up the others; the unprocessed rest continues in the next round.
const MAX_TX_PER_POOL_ROUND: usize = 150;
/// How many transactions to request concurrently.
const FETCH_CONCURRENCY: usize = 8;
/// After this many recorded swaps the pool is no longer polled.
const MAX_SWAPS_PER_POOL: i64 = 500;
/// A transaction younger than this age (s) is awaited on the main node: it may not be indexed yet.
/// Older — the main node most likely will not serve it anymore (short history): take it from the backup RPC
/// or skip it, so the pool does not get stuck until the end of the tracking window.
const FRESH_TX_SECS: i64 = 120;

pub async fn run(app: Arc<App>) {
    loop {
        let started = std::time::Instant::now();
        if let Ok(n) = app.db.expire_pools(app.track_secs) {
            if n > 0 {
                tracing::info!("stopped tracking {n} pools (window expired)");
            }
        }
        match app.db.active_pools() {
            Ok(pools) => {
                for (pool, last_sig) in pools {
                    if let Err(e) = poll_pool(&app, &pool, last_sig.as_deref()).await {
                        tracing::warn!(%pool, "poll failed: {e:#}");
                    }
                }
            }
            Err(e) => tracing::error!("active_pools: {e:#}"),
        }
        if let Ok((pools, active, swaps, completed)) = app.db.stats() {
            tracing::info!(pools, active, swaps, completed, secs = started.elapsed().as_secs(), "round done");
        }
        tokio::time::sleep(Duration::from_secs(app.poll_interval_secs)).await;
    }
}

async fn poll_pool(app: &App, pool: &str, last_sig: Option<&str>) -> Result<()> {
    // Collect all signatures newer than last_sig (RPC returns the newest first).
    let new_sigs = match collect_new(app, pool, last_sig).await {
        Ok(v) => v,
        // The RPC node does not know the `until` signature (code -32020: a different backend behind the load balancer,
        // index lag, or the transaction dropped out of a fork). Then we go without `until`
        // and trim the list ourselves, on our side.
        Err(e) if last_sig.is_some() && format!("{e:#}").contains("-32020") => {
            tracing::debug!(%pool, "until-signature unknown to RPC, falling back: {e:#}");
            collect_without_until(app, pool, last_sig.unwrap()).await?
        }
        Err(e) => return Err(e),
    };
    if new_sigs.len() > WARN_BACKLOG {
        tracing::warn!(%pool, n = new_sigs.len(), "large backlog for pool");
    }

    // Transactions are fetched in parallel (up to FETCH_CONCURRENCY requests at once; the overall
    // RPC_RPS limit is enforced by the limiter in Rpc), but written strictly from oldest to newest:
    // `buffered` preserves the order. last_sig only moves after successful processing,
    // so after a failure the next round continues from the same place.
    let batch: Vec<&SigInfo> = new_sigs.iter().rev().take(MAX_TX_PER_POOL_ROUND).collect();
    let futs: Vec<_> = batch.iter().map(|s| fetch_tx(&app.rpc, s.signature.clone(), s.failed)).collect();
    let mut fetched = futures_util::stream::iter(futs).buffered(FETCH_CONCURRENCY);

    let mut i = 0;
    while let Some(res) = fetched.next().await {
        let s = batch[i];
        i += 1;
        match res? {
            // failed transaction: no events, just move the cursor
            None => {}
            Some(Some(raw)) => {
                let parsed = parse_tx(&raw)?;
                app.ingest(&s.signature, &parsed).await?;
            }
            Some(None) => {
                let age = s.block_time.map(|t| now_secs() - t).unwrap_or(0);
                if age < FRESH_TX_SECS {
                    tracing::debug!(%pool, sig = %s.signature, "tx not yet available, retry next round");
                    return Ok(());
                }
                match fetch_backup(app, &s.signature).await {
                    Some(raw) => {
                        let parsed = parse_tx(&raw)?;
                        app.ingest(&s.signature, &parsed).await?;
                    }
                    None => {
                        tracing::warn!(%pool, sig = %s.signature, age, "tx unavailable on main and backup RPC, skipped");
                    }
                }
            }
        }
        app.db.set_last_sig(pool, &s.signature)?;
    }

    // Our signals need the curve stage; pools that trade for hours without graduation
    // must not eat up the RPC limit.
    if app.db.swap_count(pool)? >= MAX_SWAPS_PER_POOL {
        app.db.mark_done(pool, "swap_cap")?;
        tracing::debug!(%pool, "stopped tracking: swap cap reached");
    }
    Ok(())
}

async fn collect_new(app: &App, pool: &str, last_sig: Option<&str>) -> Result<Vec<SigInfo>> {
    let mut out: Vec<SigInfo> = Vec::new();
    let mut before: Option<String> = None;
    loop {
        let page = app.rpc.get_signatures(pool, last_sig, before.as_deref(), PAGE).await?;
        let full = page.len() == PAGE;
        before = page.last().map(|s| s.signature.clone());
        out.extend(page);
        if !full {
            break;
        }
    }
    Ok(out)
}

/// Pages of signatures without `until`, until `last_sig` is met (not including it).
/// If the signature is not found within MAX_FALLBACK_PAGES pages, return everything collected:
/// repeated trades are dropped by INSERT OR IGNORE, and last_sig moves to a fresh signature
/// that the RPC already knows.
async fn collect_without_until(app: &App, pool: &str, last_sig: &str) -> Result<Vec<SigInfo>> {
    const MAX_FALLBACK_PAGES: usize = 5;
    let mut out: Vec<SigInfo> = Vec::new();
    let mut before: Option<String> = None;
    for _ in 0..MAX_FALLBACK_PAGES {
        let page = app.rpc.get_signatures(pool, None, before.as_deref(), PAGE).await?;
        let full = page.len() == PAGE;
        before = page.last().map(|s| s.signature.clone());
        for s in page {
            if s.signature == last_sig {
                return Ok(out);
            }
            out.push(s);
        }
        if !full {
            break;
        }
    }
    Ok(out)
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Transaction from the backup RPC within the hourly budget; None — no backup RPC, the budget is spent
/// or the backup node did not serve it either.
async fn fetch_backup(app: &App, sig: &str) -> Option<serde_json::Value> {
    let rpc = app.backup_rpc.as_ref()?;
    if !app.take_backup_budget() {
        tracing::debug!(sig, "backup RPC hourly budget exhausted");
        return None;
    }
    match rpc.get_transaction(sig).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(sig, "backup RPC getTransaction failed: {e:#}");
            None
        }
    }
}

/// None — the transaction failed (no events); Some(None) — the RPC does not serve it yet.
async fn fetch_tx(rpc: &crate::rpc::Rpc, sig: String, failed: bool) -> Result<Option<Option<serde_json::Value>>> {
    if failed {
        return Ok(None);
    }
    rpc.get_transaction(&sig).await.map(Some)
}
