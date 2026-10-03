//! Данные для бота: база сборщика (только чтение) и кэш оценок конфигов.
//!
//! Кэш заполняется выводом `dbc-replay <db> risk --json`: так бот использует ту же оценку,
//! что и движок, без дублирования логики. Пересчёт — по таймеру и по запросу, когда
//! пользователь спрашивает о пуле с ещё неизвестным конфигом.

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde_json::Value;
use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, RwLock};

use crate::config::Config;

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Оценка одного конфига (одна запись из `risk --json`).
#[derive(Debug, Clone)]
pub struct ConfigRisk(pub Value);

impl ConfigRisk {
    pub fn s(&self, k: &str) -> String {
        self.0.get(k).and_then(Value::as_str).unwrap_or("").to_string()
    }
    pub fn u(&self, k: &str) -> u64 {
        self.0.get(k).and_then(Value::as_u64).unwrap_or(0)
    }
    pub fn f(&self, k: &str) -> Option<f64> {
        self.0.get(k).and_then(Value::as_f64)
    }
    pub fn verdict(&self) -> String {
        self.s("verdict")
    }
    pub fn flags(&self, k: &str) -> Vec<String> {
        self.0
            .get(k)
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|f| f.get("text").and_then(Value::as_str).map(str::to_owned)).collect())
            .unwrap_or_default()
    }
}

#[derive(Default)]
pub struct Cache {
    pub by_config: HashMap<String, ConfigRisk>,
    pub refreshed_at: Option<Instant>,
}

pub struct State {
    pub rpc: crate::onchain::Rpc,
    pub cfg: Config,
    pub cache: RwLock<Cache>,
    refresh_lock: Mutex<()>,
}

impl State {
    pub fn new(cfg: Config) -> Self {
        Self {
            rpc: crate::onchain::Rpc::new(&cfg.rpc_url, cfg.history_rpc_url.as_deref()),
            cfg,
            cache: RwLock::new(Cache::default()),
            refresh_lock: Mutex::new(()),
        }
    }

    pub fn db(&self) -> Result<Connection> {
        Connection::open_with_flags(&self.cfg.db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("open {}", self.cfg.db_path))
    }

    /// Пересчитать кэш. Если пересчёт уже идёт, дождаться его и не запускать второй.
    pub async fn refresh(&self) -> Result<()> {
        let requested = Instant::now();
        let _guard = self.refresh_lock.lock().await;
        if let Some(t) = self.cache.read().await.refreshed_at {
            if t >= requested {
                return Ok(()); // пока ждали, кэш уже обновили
            }
        }
        let started = Instant::now();
        let out = tokio::process::Command::new(&self.cfg.replay_bin)
            .args([self.cfg.db_path.as_str(), "risk", "--json"])
            .output()
            .await
            .with_context(|| format!("run {}", self.cfg.replay_bin))?;
        if !out.status.success() {
            bail!("dbc-replay failed: {}", String::from_utf8_lossy(&out.stderr));
        }
        let arr: Vec<Value> = serde_json::from_slice(&out.stdout).context("parse risk json")?;
        let mut map = HashMap::with_capacity(arr.len());
        for v in arr {
            if let Some(c) = v.get("config").and_then(Value::as_str) {
                map.insert(c.to_string(), ConfigRisk(v.clone()));
            }
        }
        let n = map.len();
        let mut cache = self.cache.write().await;
        cache.by_config = map;
        cache.refreshed_at = Some(Instant::now());
        tracing::info!(configs = n, secs = started.elapsed().as_secs(), "risk cache refreshed");
        Ok(())
    }

    pub async fn risk(&self, config: &str) -> Option<ConfigRisk> {
        self.cache.read().await.by_config.get(config).cloned()
    }

    pub async fn cache_age(&self) -> Option<Duration> {
        self.cache.read().await.refreshed_at.map(|t| t.elapsed())
    }
}

/// Пул из базы сборщика.
#[derive(Debug, Clone)]
pub struct PoolInfo {
    pub pool: String,
    pub config: String,
    pub creator: String,
    pub base_mint: String,
    pub created_time: Option<i64>,
    pub tracked: bool,
    pub graduated_time: Option<i64>,
    /// только для отслеживаемых пулов
    pub swaps: Option<PoolSwaps>,
}

#[derive(Debug, Clone)]
pub struct PoolSwaps {
    pub count: u64,
    pub traders: u64,
    pub creator_opening_buy_sol: f64,
    pub fanout_sellers: u64,
}

/// Найти пул по адресу пула или по адресу токена.
pub fn find_pool(conn: &Connection, addr: &str) -> Result<Option<PoolInfo>> {
    let row = conn
        .query_row(
            "SELECT pool, config, creator, base_mint, created_time, created_slot, tracked,
                    (SELECT block_time FROM curve_complete c WHERE c.pool = p.pool)
             FROM pools p WHERE pool = ?1 OR base_mint = ?1
             ORDER BY created_time DESC LIMIT 1",
            params![addr],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, bool>(6)?,
                    r.get::<_, Option<i64>>(7)?,
                ))
            },
        )
        .optional()?;
    let Some((pool, config, creator, base_mint, created_time, created_slot, tracked, graduated_time)) = row else {
        return Ok(None);
    };
    let swaps = if tracked {
        let (count, traders): (i64, i64) = conn.query_row(
            "SELECT COUNT(*), COUNT(DISTINCT fee_payer) FROM swaps WHERE pool = ?1",
            params![pool],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let opening: i64 = conn.query_row(
            "SELECT COALESCE(SUM(included_fee_input_amount), 0) FROM swaps
             WHERE pool = ?1 AND fee_payer = ?2 AND trade_direction = 1 AND slot <= ?3",
            params![pool, creator, created_slot + 2],
            |r| r.get(0),
        )?;
        let fanout: i64 = conn.query_row(
            "SELECT COUNT(DISTINCT fee_payer) FROM swaps s
             WHERE s.pool = ?1 AND s.trade_direction = 0 AND s.fee_payer <> ?2
               AND s.fee_payer NOT IN (SELECT fee_payer FROM swaps b WHERE b.pool = ?1 AND b.trade_direction = 1)",
            params![pool, creator],
            |r| r.get(0),
        )?;
        Some(PoolSwaps {
            count: count as u64,
            traders: traders as u64,
            creator_opening_buy_sol: opening as f64 / 1e9,
            fanout_sellers: fanout as u64,
        })
    } else {
        None
    };
    Ok(Some(PoolInfo { pool, config, creator, base_mint, created_time, tracked, graduated_time, swaps }))
}

pub fn config_exists(conn: &Connection, addr: &str) -> Result<bool> {
    Ok(conn
        .query_row("SELECT 1 FROM configs WHERE config = ?1", params![addr], |_| Ok(()))
        .optional()?
        .is_some())
}

/// Последние пулы конфига: (pool, base_mint, created_time, graduated).
pub fn recent_pools(conn: &Connection, config: &str, n: usize) -> Result<Vec<(String, String, Option<i64>, bool)>> {
    let mut st = conn.prepare(
        "SELECT pool, base_mint, created_time, EXISTS(SELECT 1 FROM curve_complete c WHERE c.pool = p.pool)
         FROM pools p WHERE config = ?1 ORDER BY created_time DESC LIMIT ?2",
    )?;
    let v = st
        .query_map(params![config, n as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(v)
}

/// Пулы, созданные после `since`: config -> количество.
pub fn pools_since(conn: &Connection, since: i64) -> Result<HashMap<String, u64>> {
    let mut st = conn.prepare("SELECT config, COUNT(*) FROM pools WHERE created_time >= ?1 GROUP BY config")?;
    let m = st
        .query_map(params![since], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(m)
}

/// Последние пулы (до `limit`) за последние `secs` секунд: (pool, config, base_mint, created_time).
pub fn latest_pools(conn: &Connection, secs: i64, limit: usize) -> Result<Vec<(String, String, String, Option<i64>)>> {
    let mut st = conn.prepare(
        "SELECT pool, config, base_mint, created_time FROM pools
         WHERE created_time >= ?1 ORDER BY created_time DESC LIMIT ?2",
    )?;
    let v = st
        .query_map(params![now() - secs, limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(v)
}

/// Похоже ли на адрес Solana (base58, 32–44 символа). Возвращает первый такой фрагмент сообщения,
/// чтобы принимать и ссылки Solscan / DexScreener / Jupiter.
pub fn extract_address(text: &str) -> Option<String> {
    const B58: &str = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    text.split(|c: char| !B58.contains(c))
        .find(|w| (32..=44).contains(&w.len()))
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_addresses_from_text_and_links() {
        let a = "2toDxUnvDSdWz9QV2N7PTGvs9UaEoCdbznLux88mH4cY";
        assert_eq!(extract_address(a).as_deref(), Some(a));
        assert_eq!(extract_address(&format!("https://solscan.io/token/{a}?cluster=x")).as_deref(), Some(a));
        assert_eq!(extract_address("/check hello"), None);
    }
}
