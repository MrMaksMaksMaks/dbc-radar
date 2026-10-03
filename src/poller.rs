//! Сбор свопов: по кругу опрашиваем подписи каждого отслеживаемого пула,
//! начиная с последней обработанной, и разбираем новые транзакции от старых к новым.

use crate::app::App;
use crate::rpc::SigInfo;
use crate::tx::parse_tx;
use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;

const PAGE: usize = 1000;
const WARN_BACKLOG: usize = 5000;

pub async fn run(app: Arc<App>) {
    loop {
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
            tracing::info!(pools, active, swaps, completed, "round done");
        }
        tokio::time::sleep(Duration::from_secs(app.poll_interval_secs)).await;
    }
}

async fn poll_pool(app: &App, pool: &str, last_sig: Option<&str>) -> Result<()> {
    // Собираем все подписи новее last_sig (RPC отдаёт новые первыми).
    let new_sigs = match collect_new(app, pool, last_sig).await {
        Ok(v) => v,
        // Узел RPC не знает подпись `until` (код -32020: другой бэкенд за балансировщиком,
        // отставание индекса или транзакция выпала из форка). Тогда идём без `until`
        // и обрезаем список сами, на нашей стороне.
        Err(e) if last_sig.is_some() && format!("{e:#}").contains("-32020") => {
            tracing::debug!(%pool, "until-signature unknown to RPC, falling back: {e:#}");
            collect_without_until(app, pool, last_sig.unwrap()).await?
        }
        Err(e) => return Err(e),
    };
    if new_sigs.len() > WARN_BACKLOG {
        tracing::warn!(%pool, n = new_sigs.len(), "large backlog for pool");
    }

    // От старых к новым; last_sig двигаем только после успешной обработки,
    // чтобы при сбое следующий проход продолжил с того же места.
    for s in new_sigs.iter().rev() {
        if !s.failed {
            match app.rpc.get_transaction(&s.signature).await? {
                Some(raw) => {
                    let parsed = parse_tx(&raw)?;
                    app.ingest(&s.signature, &parsed).await?;
                }
                None => {
                    tracing::debug!(%pool, sig = %s.signature, "tx not yet available, retry next round");
                    return Ok(());
                }
            }
        }
        app.db.set_last_sig(pool, &s.signature)?;
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

/// Страницы подписей без `until`, пока не встретится `last_sig` (не включая его).
/// Если подпись так и не нашлась за MAX_FALLBACK_PAGES страниц, возвращаем всё собранное:
/// повторные сделки отсекаются INSERT OR IGNORE, а last_sig сдвинется на свежую подпись,
/// которую RPC уже знает.
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
