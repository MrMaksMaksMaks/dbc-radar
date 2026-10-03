//! Хранилище SQLite. u64-суммы пишем как INTEGER (влезают в i64 для реальных объёмов),
//! u128 (sqrt price) — как TEXT, чтобы не терять точность.

use crate::events::{CurveComplete, InitPool, Swap2};
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use std::sync::Mutex;

pub struct Db {
    conn: Mutex<Connection>,
}

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;

CREATE TABLE IF NOT EXISTS pools (
    pool             TEXT PRIMARY KEY,
    config           TEXT NOT NULL,
    creator          TEXT NOT NULL,
    base_mint        TEXT NOT NULL,
    pool_type        INTEGER NOT NULL,
    activation_point INTEGER NOT NULL,
    transfer_hook    INTEGER NOT NULL,
    created_sig      TEXT NOT NULL,
    created_slot     INTEGER NOT NULL,
    created_time     INTEGER,           -- unix, blockTime
    discovered_at    INTEGER NOT NULL,  -- unix, когда мы его увидели
    tracked          INTEGER NOT NULL,  -- 1 = собираем свопы
    last_sig         TEXT,              -- последняя обработанная подпись
    done             INTEGER NOT NULL DEFAULT 0,
    done_reason      TEXT
);
CREATE INDEX IF NOT EXISTS pools_active ON pools(tracked, done, created_time);
CREATE INDEX IF NOT EXISTS pools_config ON pools(config);

CREATE TABLE IF NOT EXISTS swaps (
    signature                 TEXT NOT NULL,
    event_index               INTEGER NOT NULL,
    pool                      TEXT NOT NULL,
    config                    TEXT NOT NULL,
    slot                      INTEGER NOT NULL,
    block_time                INTEGER,
    fee_payer                 TEXT NOT NULL,
    trade_direction           INTEGER NOT NULL,  -- 1 buy (quote->base), 0 sell
    has_referral              INTEGER NOT NULL,
    swap_mode                 INTEGER NOT NULL,
    amount_0                  INTEGER NOT NULL,
    amount_1                  INTEGER NOT NULL,
    included_fee_input_amount INTEGER NOT NULL,
    excluded_fee_input_amount INTEGER NOT NULL,
    amount_left               INTEGER NOT NULL,
    output_amount             INTEGER NOT NULL,
    next_sqrt_price           TEXT NOT NULL,
    trading_fee               INTEGER NOT NULL,
    protocol_fee              INTEGER NOT NULL,
    referral_fee              INTEGER NOT NULL,
    quote_reserve_amount      INTEGER NOT NULL,
    migration_threshold       INTEGER NOT NULL,
    event_timestamp           INTEGER NOT NULL,
    transfer_hook             INTEGER NOT NULL,
    PRIMARY KEY (signature, event_index)
);
CREATE INDEX IF NOT EXISTS swaps_pool_slot ON swaps(pool, slot);
CREATE INDEX IF NOT EXISTS swaps_pool_dir_payer ON swaps(pool, trade_direction, fee_payer);
CREATE INDEX IF NOT EXISTS pools_creator ON pools(creator);

CREATE TABLE IF NOT EXISTS curve_complete (
    pool          TEXT PRIMARY KEY,
    config        TEXT NOT NULL,
    signature     TEXT NOT NULL,
    slot          INTEGER NOT NULL,
    block_time    INTEGER,
    base_reserve  INTEGER NOT NULL,
    quote_reserve INTEGER NOT NULL
);

-- Откуда пришла каждая подпись: ws:0, ws:1, watch — для оценки полноты обнаружения.
CREATE TABLE IF NOT EXISTS discovery (
    signature TEXT NOT NULL,
    source    TEXT NOT NULL,
    seen_at   INTEGER NOT NULL,
    PRIMARY KEY (signature, source)
);

-- Сырые данные аккаунта PoolConfig: декодировать полностью позже (SDK/IDL),
-- quote_mint и fee_claimer вынимаем сразу.
CREATE TABLE IF NOT EXISTS configs (
    config      TEXT PRIMARY KEY,
    quote_mint  TEXT,
    fee_claimer TEXT,
    raw         BLOB,
    fetched_at  INTEGER NOT NULL
);
"#;

fn i(v: u64) -> i64 {
    // Реальные суммы далеко от 2^63; насыщение лучше молчаливого переполнения.
    i64::try_from(v).unwrap_or(i64::MAX)
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Db {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// Возвращает true, если пул новый.
    pub fn insert_pool(
        &self,
        p: &InitPool,
        sig: &str,
        slot: u64,
        block_time: Option<i64>,
        tracked: bool,
    ) -> Result<bool> {
        let n = self.conn.lock().unwrap().execute(
            "INSERT OR IGNORE INTO pools
             (pool, config, creator, base_mint, pool_type, activation_point, transfer_hook,
              created_sig, created_slot, created_time, discovered_at, tracked, last_sig)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?8)",
            params![
                p.pool, p.config, p.creator, p.base_mint, p.pool_type, i(p.activation_point),
                p.transfer_hook, sig, i(slot), block_time, now(), tracked
            ],
        )?;
        Ok(n > 0)
    }

    pub fn insert_swap(
        &self,
        sig: &str,
        event_index: u32,
        slot: u64,
        block_time: Option<i64>,
        fee_payer: &str,
        s: &Swap2,
    ) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT OR IGNORE INTO swaps VALUES
             (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24)",
            params![
                sig, event_index, s.pool, s.config, i(slot), block_time, fee_payer,
                s.trade_direction, s.has_referral, s.swap_mode, i(s.amount_0), i(s.amount_1),
                i(s.included_fee_input_amount), i(s.excluded_fee_input_amount), i(s.amount_left),
                i(s.output_amount), s.next_sqrt_price.to_string(), i(s.trading_fee),
                i(s.protocol_fee), i(s.referral_fee), i(s.quote_reserve_amount),
                i(s.migration_threshold), i(s.current_timestamp), s.transfer_hook
            ],
        )?;
        Ok(())
    }

    pub fn insert_curve_complete(
        &self,
        c: &CurveComplete,
        sig: &str,
        slot: u64,
        block_time: Option<i64>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO curve_complete VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![c.pool, c.config, sig, i(slot), block_time, i(c.base_reserve), i(c.quote_reserve)],
        )?;
        conn.execute(
            "UPDATE pools SET done = 1, done_reason = 'curve_complete' WHERE pool = ?1",
            params![c.pool],
        )?;
        Ok(())
    }

    pub fn record_discovery(&self, sig: &str, source: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT OR IGNORE INTO discovery VALUES (?1, ?2, ?3)",
            params![sig, source, now()],
        )?;
        Ok(())
    }

    pub fn set_last_sig(&self, pool: &str, sig: &str) -> Result<()> {
        self.conn
            .lock()
            .unwrap()
            .execute("UPDATE pools SET last_sig = ?2 WHERE pool = ?1", params![pool, sig])?;
        Ok(())
    }

    /// Закрывает отслеживание пулов старше окна. Возвращает число закрытых.
    pub fn expire_pools(&self, track_secs: i64) -> Result<usize> {
        Ok(self.conn.lock().unwrap().execute(
            "UPDATE pools SET done = 1, done_reason = 'window_expired'
             WHERE tracked = 1 AND done = 0
               AND COALESCE(created_time, discovered_at) < ?1",
            params![now() - track_secs],
        )?)
    }

    /// Активные пулы: (pool, last_sig).
    pub fn active_pools(&self) -> Result<Vec<(String, Option<String>)>> {
        let conn = self.conn.lock().unwrap();
        let mut st = conn.prepare(
            "SELECT pool, last_sig FROM pools WHERE tracked = 1 AND done = 0
             ORDER BY created_slot",
        )?;
        let rows = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn is_tracked(&self, pool: &str) -> Result<bool> {
        Ok(self
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT tracked FROM pools WHERE pool = ?1", params![pool], |r| {
                r.get::<_, bool>(0)
            })
            .optional()?
            .unwrap_or(false))
    }

    pub fn config_known(&self, config: &str) -> Result<bool> {
        Ok(self
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT 1 FROM configs WHERE config = ?1", params![config], |_| Ok(()))
            .optional()?
            .is_some())
    }

    pub fn insert_config(&self, config: &str, raw: Option<&[u8]>) -> Result<()> {
        // PoolConfig (bytemuck, repr C): [0..8] дискриминатор, [8..40] quote_mint, [40..72] fee_claimer
        let key = |a: usize| {
            raw.filter(|d| d.len() >= a + 32)
                .map(|d| bs58::encode(&d[a..a + 32]).into_string())
        };
        self.conn.lock().unwrap().execute(
            "INSERT OR REPLACE INTO configs VALUES (?1,?2,?3,?4,?5)",
            params![config, key(8), key(40), raw, now()],
        )?;
        Ok(())
    }

    pub fn stats(&self) -> Result<(i64, i64, i64, i64)> {
        let conn = self.conn.lock().unwrap();
        let q = |sql: &str| conn.query_row(sql, [], |r| r.get::<_, i64>(0));
        Ok((
            q("SELECT COUNT(*) FROM pools")?,
            q("SELECT COUNT(*) FROM pools WHERE tracked = 1 AND done = 0")?,
            q("SELECT COUNT(*) FROM swaps")?,
            q("SELECT COUNT(*) FROM curve_complete")?,
        ))
    }
}
