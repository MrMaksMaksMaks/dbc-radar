//! Статус graduation по аккаунтам пулов.
//!
//! Событие EvtCurveComplete приходит только из транзакций, которые успел загрузить poller,
//! а он может не дойти до конца кривой (лимиты RPC, транзакции, которые узел не отдаёт).
//! Аккаунт пула DBC хранит время завершения кривой (finish_curve_timestamp), поэтому
//! graduation любого пула — отслеживаемого или нет — проверяется одним getMultipleAccounts
//! на 100 пулов. Найденное записывается в curve_complete с подписью "account"; пул при этом
//! не закрывается: poller продолжает догружать его свопы.

use crate::db::{now, Db};
use crate::rpc::Rpc;
use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;

use crate::app::App;

const DBC_PROGRAM_ID: &str = "dbcij3LWUppWqq96dh6gJWwBifmcGfLSB5D4DuSMaqN";
const VIRTUAL_POOL_DISC: [u8; 8] = [213, 224, 5, 209, 98, 69, 119, 92];
const TRANSFER_HOOK_POOL_DISC: [u8; 8] = [237, 219, 184, 23, 42, 189, 169, 35];
/// Смещения в данных аккаунта: 8 байт дискриминатора + смещение поля в PoolState
/// (программа DBC, ревизия f552f20; finish_curve_timestamp проверен на mainnet).
const OFF_BASE_RESERVE: usize = 8 + 224;
const OFF_QUOTE_RESERVE: usize = 8 + 232;
const OFF_FINISH_CURVE_TS: usize = 8 + 336;
/// Предел getMultipleAccounts.
const BATCH: usize = 100;

pub async fn run(app: Arc<App>, every_secs: u64, window_secs: i64) {
    loop {
        match sweep(&app.rpc, &app.db, window_secs).await {
            Ok((checked, found)) if found > 0 => {
                tracing::info!(checked, found, "graduations found from pool accounts")
            }
            Ok((checked, _)) => tracing::debug!(checked, "graduation sweep: nothing new"),
            Err(e) => tracing::warn!("graduation sweep: {e:#}"),
        }
        tokio::time::sleep(Duration::from_secs(every_secs)).await;
    }
}

/// Один проход: пулы без записи о graduation, созданные за последние `window_secs`.
/// Возвращает (проверено пулов, найдено новых graduation).
pub async fn sweep(rpc: &Rpc, db: &Db, window_secs: i64) -> Result<(usize, usize)> {
    let pools = db.pools_without_graduation(now() - window_secs)?;
    let mut found = 0;
    for chunk in pools.chunks(BATCH) {
        let keys: Vec<String> = chunk.iter().map(|(p, _)| p.clone()).collect();
        let accounts = rpc.get_multiple_accounts(&keys).await?;
        for ((pool, config), acc) in chunk.iter().zip(accounts) {
            let Some((owner, data)) = acc else { continue };
            if let Some((ts, base, quote)) = parse_finished(&owner, &data) {
                if db.insert_curve_from_account(pool, config, ts, base, quote)? {
                    found += 1;
                }
            }
        }
    }
    Ok((pools.len(), found))
}

fn u64_at(d: &[u8], off: usize) -> Option<u64> {
    d.get(off..off + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap()))
}

/// (время завершения кривой, base_reserve, quote_reserve), если кривая пула завершена.
pub fn parse_finished(owner: &str, d: &[u8]) -> Option<(i64, u64, u64)> {
    if owner != DBC_PROGRAM_ID {
        return None;
    }
    let disc: [u8; 8] = d.get(..8)?.try_into().ok()?;
    if disc != VIRTUAL_POOL_DISC && disc != TRANSFER_HOOK_POOL_DISC {
        return None;
    }
    let ts = u64_at(d, OFF_FINISH_CURVE_TS)?;
    if ts == 0 {
        return None;
    }
    Some((ts as i64, u64_at(d, OFF_BASE_RESERVE)?, u64_at(d, OFF_QUOTE_RESERVE)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_finished_curve() {
        let mut d = vec![0u8; 424];
        d[..8].copy_from_slice(&VIRTUAL_POOL_DISC);
        assert_eq!(parse_finished(DBC_PROGRAM_ID, &d), None); // кривая не завершена
        d[OFF_FINISH_CURVE_TS..OFF_FINISH_CURVE_TS + 8].copy_from_slice(&1791261146u64.to_le_bytes());
        d[OFF_QUOTE_RESERVE..OFF_QUOTE_RESERVE + 8].copy_from_slice(&10_950_000_000u64.to_le_bytes());
        d[OFF_BASE_RESERVE..OFF_BASE_RESERVE + 8].copy_from_slice(&7u64.to_le_bytes());
        assert_eq!(parse_finished(DBC_PROGRAM_ID, &d), Some((1791261146, 7, 10_950_000_000)));
        assert_eq!(parse_finished("other", &d), None);
        d[0] = 0;
        assert_eq!(parse_finished(DBC_PROGRAM_ID, &d), None);
    }
}
