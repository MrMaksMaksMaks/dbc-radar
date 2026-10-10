//! Общее состояние, правило выборки и запись событий транзакции в базу.

use crate::db::Db;
use crate::events::{DbcEvent, InitPool};
use crate::rpc::Rpc;
use crate::tx::ParsedTx;
use anyhow::Result;
use std::collections::HashSet;

pub struct App {
    pub rpc: Rpc,
    /// RPC для проверки graduation по аккаунтам (getMultipleAccounts): отдельный узел и лимит,
    /// чтобы не отнимать запросы у загрузки свопов
    pub status_rpc: Rpc,
    pub db: Db,
    /// websocket-адреса для обнаружения пулов (работают параллельно, дубликаты отсекаются)
    pub ws_urls: Vec<String>,
    /// если от websocket нет сообщений столько секунд — переподключаемся
    pub ws_stall_secs: u64,
    /// адреса (создатели, получатели комиссий), чьи транзакции опрашиваются напрямую:
    /// полное покрытие их запусков, их пулы отслеживаются всегда
    pub watch: HashSet<String>,
    pub sample_every: u64,
    pub config_allowlist: HashSet<String>,
    pub track_secs: i64,
    pub poll_interval_secs: u64,
    /// запасной RPC для транзакций, которые основной узел уже не отдаёт (обычно HISTORY_RPC_URL)
    pub backup_rpc: Option<Rpc>,
    /// сколько транзакций в час можно взять из запасного RPC (бережём кредиты)
    pub backup_max_per_hour: u32,
    /// (номер часа, сколько взято в этом часу)
    pub backup_used: std::sync::Mutex<(i64, u32)>,
}

impl App {
    /// Резервирует одну транзакцию из часового бюджета запасного RPC.
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

    /// Отслеживаем пул, если его конфиг в allowlist или он попал в выборку.
    /// Выборка детерминированная: первые 8 байт адреса пула (адреса равномерно
    /// распределены), поэтому после перезапуска решения не меняются.
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

    /// Записывает в базу всё полезное из разобранной транзакции.
    pub async fn ingest(&self, sig: &str, p: &ParsedTx) -> Result<()> {
        // 1. Новые пулы (обычно приходят из обнаружения, но могут встретиться где угодно).
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
        // 2. Свопы и завершение кривой — только для отслеживаемых пулов.
        //    Свопы из транзакции создания пула (первая покупка создателя) тоже попадают сюда.
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

    /// Один раз сохраняет сырые данные конфига (для будущего бэктеста).
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
