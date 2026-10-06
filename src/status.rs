//! Статус graduation по аккаунтам пулов.
//!
//! Событие EvtCurveComplete приходит только из транзакций, которые успел загрузить poller,
//! а он может не дойти до конца кривой (лимиты RPC, транзакции, которые узел не отдаёт).
//! Аккаунт пула DBC хранит время завершения кривой (finish_curve_timestamp), поэтому
//! graduation любого пула — отслеживаемого или нет — проверяется одним getMultipleAccounts
//! на 100 пулов (читается только кусок данных с резервами и временем завершения).
//! Найденное записывается в curve_complete с подписью "account"; пул при этом
//! не закрывается: poller продолжает догружать его свопы.

use crate::db::{now, Db};
use crate::rpc::Rpc;
use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;

use crate::app::App;

const DBC_PROGRAM_ID: &str = "dbcij3LWUppWqq96dh6gJWwBifmcGfLSB5D4DuSMaqN";
/// Смещения в данных аккаунта: 8 байт дискриминатора + смещение поля в PoolState
/// (программа DBC, ревизия f552f20; finish_curve_timestamp проверен на mainnet).
const OFF_BASE_RESERVE: usize = 8 + 224;
const OFF_FINISH_CURVE_TS: usize = 8 + 336;
/// Читаем кусок данных от base_reserve до конца finish_curve_timestamp (120 байт).
const SLICE: (usize, usize) = (OFF_BASE_RESERVE, OFF_FINISH_CURVE_TS + 8 - OFF_BASE_RESERVE);
/// Смещения внутри куска.
const S_BASE: usize = 0;
const S_QUOTE: usize = 8;
const S_FINISH: usize = OFF_FINISH_CURVE_TS - OFF_BASE_RESERVE;
/// Предел getMultipleAccounts; если узел отклоняет запрос, пачка уменьшается вдвое.
const BATCH: usize = 100;

pub async fn run(app: Arc<App>, every_secs: u64, window_secs: i64) {
    loop {
        match sweep(&app.status_rpc, &app.db, window_secs).await {
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
    let mut batch = BATCH;
    let mut pos = 0;
    while pos < pools.len() {
        let chunk = &pools[pos..(pos + batch).min(pools.len())];
        let keys: Vec<String> = chunk.iter().map(|(p, _)| p.clone()).collect();
        let accounts = match rpc.get_multiple_accounts(&keys, Some(SLICE)).await {
            Ok(a) => a,
            // узел отклонил запрос (например, "Request blocked"): уменьшаем пачку и повторяем
            Err(e) if batch > 1 && format!("{e:#}").contains("-32602") => {
                batch /= 2;
                tracing::info!(batch, "getMultipleAccounts rejected, retrying with a smaller batch");
                continue;
            }
            Err(e) => return Err(e),
        };
        for ((pool, config), acc) in chunk.iter().zip(accounts) {
            let Some((owner, data)) = acc else { continue };
            if let Some((ts, base, quote)) = parse_finished(&owner, &data) {
                if db.insert_curve_from_account(pool, config, ts, base, quote)? {
                    found += 1;
                }
            }
        }
        pos += chunk.len();
    }
    Ok((pools.len(), found))
}

fn u64_at(d: &[u8], off: usize) -> Option<u64> {
    d.get(off..off + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap()))
}

/// (время завершения кривой, base_reserve, quote_reserve), если кривая пула завершена.
/// `d` — кусок данных аккаунта SLICE; адреса берутся из базы пулов, владелец проверяется.
pub fn parse_finished(owner: &str, d: &[u8]) -> Option<(i64, u64, u64)> {
    if owner != DBC_PROGRAM_ID {
        return None;
    }
    let ts = u64_at(d, S_FINISH)?;
    if ts == 0 {
        return None;
    }
    Some((ts as i64, u64_at(d, S_BASE)?, u64_at(d, S_QUOTE)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_finished_curve() {
        assert_eq!(SLICE, (232, 120));
        let mut d = vec![0u8; SLICE.1];
        assert_eq!(parse_finished(DBC_PROGRAM_ID, &d), None); // кривая не завершена
        // значение из mainnet: пул Ai3r…, base64 "2nnEagAAAAA="
        d[S_FINISH..S_FINISH + 8].copy_from_slice(&[0xda, 0x79, 0xc4, 0x6a, 0, 0, 0, 0]);
        d[S_QUOTE..S_QUOTE + 8].copy_from_slice(&10_950_000_000u64.to_le_bytes());
        d[S_BASE..S_BASE + 8].copy_from_slice(&7u64.to_le_bytes());
        assert_eq!(parse_finished(DBC_PROGRAM_ID, &d), Some((1791261146, 7, 10_950_000_000)));
        assert_eq!(parse_finished("other", &d), None);
        assert_eq!(parse_finished(DBC_PROGRAM_ID, &d[..50]), None); // обрезанные данные
    }
}
