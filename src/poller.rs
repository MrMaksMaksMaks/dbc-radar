//! Сбор свопов: по кругу опрашиваем подписи каждого отслеживаемого пула,
//! начиная с последней обработанной, и разбираем новые транзакции от старых к новым.

use crate::app::App;
use futures_util::StreamExt;
use crate::rpc::SigInfo;
use crate::tx::parse_tx;
use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;

const PAGE: usize = 1000;
const WARN_BACKLOG: usize = 5000;

/// Сколько транзакций одного пула обрабатывать за круг: тяжёлый пул не должен
/// задерживать остальные, недообработанное продолжится на следующем круге.
const MAX_TX_PER_POOL_ROUND: usize = 150;
/// Сколько транзакций запрашивать одновременно.
const FETCH_CONCURRENCY: usize = 8;
/// После стольких записанных свопов пул больше не опрашивается.
const MAX_SWAPS_PER_POOL: i64 = 500;

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

    // Транзакции забираем параллельно (до FETCH_CONCURRENCY запросов сразу; общий лимит
    // RPC_RPS соблюдается ограничителем в Rpc), а записываем строго от старых к новым:
    // `buffered` сохраняет порядок. last_sig двигаем только после успешной обработки,
    // поэтому при сбое следующий круг продолжит с того же места.
    let batch: Vec<&SigInfo> = new_sigs.iter().rev().take(MAX_TX_PER_POOL_ROUND).collect();
    let futs: Vec<_> = batch.iter().map(|s| fetch_tx(&app.rpc, s.signature.clone(), s.failed)).collect();
    let mut fetched = futures_util::stream::iter(futs).buffered(FETCH_CONCURRENCY);

    let mut i = 0;
    while let Some(res) = fetched.next().await {
        let s = batch[i];
        i += 1;
        match res? {
            // неуспешная транзакция: событий нет, просто сдвигаем курсор
            None => {}
            Some(Some(raw)) => {
                let parsed = parse_tx(&raw)?;
                app.ingest(&s.signature, &parsed).await?;
            }
            Some(None) => {
                tracing::debug!(%pool, sig = %s.signature, "tx not yet available, retry next round");
                return Ok(());
            }
        }
        app.db.set_last_sig(pool, &s.signature)?;
    }

    // Для наших признаков нужен этап кривой; пулы, которые торгуются часами без graduation,
    // не должны съедать лимит RPC.
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

/// None — транзакция неуспешна (событий нет); Some(None) — RPC её ещё не отдаёт.
async fn fetch_tx(rpc: &crate::rpc::Rpc, sig: String, failed: bool) -> Result<Option<Option<serde_json::Value>>> {
    if failed {
        return Ok(None);
    }
    rpc.get_transaction(&sig).await.map(Some)
}
