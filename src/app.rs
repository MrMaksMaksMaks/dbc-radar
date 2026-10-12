//! Shared state, the sampling rule, and writing transaction events to the database.

use crate::db::Db;
use crate::events::{DbcEvent, InitPool};
use crate::rpc::Rpc;
use crate::tx::ParsedTx;
use anyhow::Result;
use std::collections::HashSet;

pub struct App {
    pub rpc: Rpc,
    /// RPC for checking graduation via accounts (getMultipleAccounts): a separate node and limit,
    /// so it does not take requests away from loading swaps
    pub status_rpc: Rpc,
    pub db: Db,
    /// websocket URLs for pool discovery (they run in parallel, duplicates are dropped)
    pub ws_urls: Vec<String>,
    /// if there are no websocket messages for this many seconds, reconnect
    pub ws_stall_secs: u64,
    /// addresses (creators, fee receivers) whose transactions are polled directly:
    /// full coverage of their launches, their pools are always tracked
    pub watch: HashSet<String>,
    pub sample_every: u64,
    pub config_allowlist: HashSet<String>,
    pub track_secs: i64,
    pub poll_interval_secs: u64,
    /// backup RPC for transactions the main node no longer serves (usually HISTORY_RPC_URL)
    pub backup_rpc: Option<Rpc>,
    /// how many transactions per hour may be taken from the backup RPC (to save credits)
    pub backup_max_per_hour: u32,
    /// (hour number, how many were taken in this hour)
    pub backup_used: std::sync::Mutex<(i64, u32)>,
}

impl App {
    /// Reserves one transaction from the backup RPC's hourly budget.
    pub fn take_backup_budget(&self) -> bool {
        let hour = chrono_hour();
        let mut g = self.backup_used.lock().unwrap();
        if g.0 != hour {
            *g = (hour, 0);
        }
        if g.1 >= self.backup_max_per_hour {
            return false;
        }
        g.1 += 1;
        true
    }

    /// We track a pool if its config is in the allowlist or it falls into the sample.
    /// The sample is deterministic: the first 8 bytes of the pool address (addresses are uniformly
    /// distributed), so decisions do not change after a restart.
    pub fn should_track(&self, p: &InitPool) -> bool {
        if self.config_allowlist.contains(&p.config) || self.watch.contains(&p.creator) {
            return true;
        }
        if self.sample_every <= 1 {
            return true;
        }
        let bytes = bs58::decode(&p.pool).into_vec().unwrap_or_default();
        if bytes.len() < 8 {
            return false;
        }
        let h = u64::from_le_bytes(bytes[..8].try_into().unwrap());
        h % self.sample_every == 0
    }

    /// Writes everything useful from a parsed transaction to the database.
    pub async fn ingest(&self, sig: &str, p: &ParsedTx) -> Result<()> {
        // 1. New pools (usually come from discovery, but can show up anywhere).
        for (_, ev) in &p.events {
            if let DbcEvent::InitializePool(ip) = ev {
                let tracked = self.should_track(ip);
                if self.db.insert_pool(ip, sig, p.slot, p.block_time, tracked)? {
                    if tracked {
                        tracing::info!(pool = %ip.pool, config = %ip.config, "new tracked pool");
                    } else {
                        tracing::debug!(pool = %ip.pool, config = %ip.config, "new pool (not sampled)");
                    }
                    self.ensure_config(&ip.config).await;
                }
            }
        }
        // 2. Swaps and curve completion — only for tracked pools.
        //    Swaps from the pool creation transaction (the creator's first buy) also end up here.
        for (idx, ev) in &p.events {
            match ev {
                DbcEvent::Swap2(s) if self.db.is_tracked(&s.pool)? => {
                    let owners = p.trade_owners(s.trade_direction == 1);
                    self.db
                        .insert_swap(sig, *idx, p.slot, p.block_time, &p.fee_payer, owners, s)?;
                }
                DbcEvent::CurveComplete(c) if self.db.is_tracked(&c.pool)? => {
                    tracing::info!(pool = %c.pool, "curve complete");
                    self.db.insert_curve_complete(c, sig, p.slot, p.block_time)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Saves the raw config data once (for future backtesting).
    async fn ensure_config(&self, config: &str) {
        match self.db.config_known(config) {
            Ok(true) => return,
            Ok(false) => {}
            Err(e) => {
                tracing::warn!("config lookup: {e:#}");
                return;
            }
        }
        match self.rpc.get_account_data(config).await {
            Ok(raw) => {
                if let Err(e) = self.db.insert_config(config, raw.as_deref()) {
                    tracing::warn!("store config {config}: {e:#}");
                }
            }
            Err(e) => tracing::warn!("fetch config {config}: {e:#}"),
        }
    }
}

fn chrono_hour() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64 / 3600)
        .unwrap_or(0)
}
