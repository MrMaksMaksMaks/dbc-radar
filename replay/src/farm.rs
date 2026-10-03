//! Кошельки «фермы» и синтетичность активности в пулах одного конфига.
//!
//! Признаки (номера — из расследования по оператору 2toDxUnv):
//!   4.  скорость миграции: время от создания пула до завершения кривой;
//!   7.  веерная раздача: кошельки, которые продают токены, ни разу не купив их в пуле;
//!   8.  повторяющийся набор участников: кошелёк торгует в большой доле пулов конфига
//!       и почти нигде больше (специфичность), — отличает ферму от общих ботов,
//!       которые торгуют все новые токены подряд;
//!   9.  доля связанного объёма: объём создателя, фермы и веерных продавцов к общему;
//!   10. постоянный «первый покупатель»: кошелёк покупает в первые секунды большинства пулов;
//!   11. фиксированная дев-покупка: одинаковая стартовая покупка создателя.
//! Всё остальное — «внешние» участники; их результат на кривой считается отдельно.

use anyhow::Result;
use rusqlite::{params, Connection};
use std::collections::{HashMap, HashSet};

/// Покупка в эти первые слоты после создания пула считается «ранней» (~10 с).
pub const EARLY_SLOTS: i64 = 25;
/// Доля пулов конфига, в которых должен торговать кошелёк фермы.
pub const RECUR_SHARE: f64 = 0.3;
/// Доля ранних покупок, чтобы считать кошелёк постоянным первым покупателем.
pub const FIRST_BUYER_SHARE: f64 = 0.5;
/// Доля пулов кошелька, приходящаяся на этот конфиг: ферма почти не торгует вне его.
pub const SPECIFICITY: f64 = 0.8;
/// Минимум отслеживаемых пулов для признаков повторяемости.
pub const MIN_POOLS: usize = 3;
/// Пул считается «торгуемым», если в нём не меньше стольких сделок не от создателя.
/// Пулы с мгновенной graduation (одна покупка создателя) в долю связанного объёма не входят.
pub const MIN_ACTIVE_SWAPS: usize = 3;

/// Во скольких отслеживаемых пулах всей базы торговал каждый кошелёк.
pub struct WalletIndex(HashMap<String, u32>);

pub fn wallet_index(conn: &Connection) -> Result<WalletIndex> {
    let mut st = conn.prepare("SELECT fee_payer, COUNT(DISTINCT pool) FROM swaps GROUP BY fee_payer")?;
    let m = st
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u32)))?
        .collect::<std::result::Result<HashMap<_, _>, _>>()?;
    Ok(WalletIndex(m))
}

#[derive(Debug, Clone, Default)]
pub struct FarmStats {
    pub pools: usize,
    pub graduated: usize,
    pub median_migration_secs: Option<f64>,
    pub farm_wallets: usize,
    pub first_buyers: usize,
    /// (сумма в SOL, доля пулов с этой суммой) — самая частая стартовая покупка создателя
    pub dev_buy_mode: Option<(f64, f64)>,
    pub median_fanout: Option<f64>,
    pub median_linked_share: Option<f64>,
    /// пулов с реальной торговлей (>= MIN_ACTIVE_SWAPS сделок не от создателя)
    pub active_pools: usize,
    pub external_wallets: usize,
    pub external_buy_lamports: u64,
    pub external_sell_lamports: u64,
    pub total_volume_lamports: u64,
    pub linked_volume_lamports: u64,
}

impl FarmStats {
    /// SOL, которые внешние участники оставили на кривой (покупки минус продажи).
    pub fn external_net_in_sol(&self) -> f64 {
        (self.external_buy_lamports as f64 - self.external_sell_lamports as f64) / 1e9
    }
}

struct Swap {
    wallet: String,
    buy: bool,
    input: u64,
    output: u64,
    slot: i64,
}

struct PoolData {
    config: String,
    creator: String,
    created_time: Option<i64>,
    created_slot: i64,
    graduated_time: Option<i64>,
    swaps: Vec<Swap>,
}

fn median(mut v: Vec<f64>) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = v.len();
    Some(if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 })
}

/// SOL-объём сделки: покупка — сколько SOL вошло, продажа — сколько вышло.
fn sol_volume(s: &Swap) -> u64 {
    if s.buy { s.input } else { s.output }
}

/// Один конфиг: ферма определяется и метрики считаются по нему же.
pub fn analyze_config(conn: &Connection, config: &str, idx: &WalletIndex, metrics_since: i64) -> Result<FarmStats> {
    let c = [config.to_string()];
    analyze_scope(conn, &c, &c, idx, metrics_since)
}

/// `scope` — конфиги, по пулам которых определяется ферма (весь кластер оператора:
/// если оператор держит одну ферму на нескольких конфигах, специфичность считается к кластеру).
/// `metrics` — конфиги, по пулам которых считаются метрики активности.
/// `metrics_since`: пулы, созданные раньше, участвуют в определении фермы, но не в метриках.
pub fn analyze_scope(
    conn: &Connection,
    scope: &[String],
    metrics: &[String],
    idx: &WalletIndex,
    metrics_since: i64,
) -> Result<FarmStats> {
    let mut st = conn.prepare_cached(
        "SELECT p.pool, p.creator, p.created_time, p.created_slot,
                (SELECT block_time FROM curve_complete c WHERE c.pool = p.pool), p.config
         FROM pools p WHERE p.config = ?1 AND p.tracked = 1",
    )?;
    let mut heads: Vec<(String, String, Option<i64>, i64, Option<i64>, String)> = Vec::new();
    for cfg in scope {
        let rows: Vec<_> = st
            .query_map(params![cfg], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?
            .collect::<std::result::Result<_, _>>()?;
        heads.extend(rows);
    }
    let metric_set: HashSet<&str> = metrics.iter().map(String::as_str).collect();

    let mut sw = conn.prepare_cached(
        "SELECT fee_payer, trade_direction, included_fee_input_amount, output_amount, slot
         FROM swaps WHERE pool = ?1 ORDER BY slot, event_index",
    )?;
    let mut pools: Vec<PoolData> = Vec::new();
    for (pool, creator, created_time, created_slot, graduated_time, config) in heads {
        let swaps: Vec<Swap> = sw
            .query_map(params![pool], |r| {
                Ok(Swap {
                    wallet: r.get(0)?,
                    buy: r.get::<_, i64>(1)? == 1,
                    input: r.get::<_, i64>(2)? as u64,
                    output: r.get::<_, i64>(3)? as u64,
                    slot: r.get(4)?,
                })
            })?
            .collect::<std::result::Result<_, _>>()?;
        if !swaps.is_empty() {
            pools.push(PoolData { config, creator, created_time, created_slot, graduated_time, swaps });
        }
    }

    let n = pools.len();
    let mut out = FarmStats::default();
    if n == 0 {
        return Ok(out);
    }

    // --- Определение фермы (по всем отслеживаемым пулам конфига) ---
    let mut in_pools: HashMap<&str, u32> = HashMap::new();
    let mut early_in: HashMap<&str, u32> = HashMap::new();
    let mut fanout_per_pool: Vec<HashSet<&str>> = Vec::with_capacity(n);
    let mut dev_buys: Vec<u64> = Vec::new();
    for p in &pools {
        let buyers: HashSet<&str> = p.swaps.iter().filter(|s| s.buy).map(|s| s.wallet.as_str()).collect();
        let mut seen: HashSet<&str> = HashSet::new();
        let mut early: HashSet<&str> = HashSet::new();
        let mut fanout: HashSet<&str> = HashSet::new();
        let mut dev = 0u64;
        for s in &p.swaps {
            let w = s.wallet.as_str();
            if w == p.creator {
                if s.buy && s.slot <= p.created_slot + 2 {
                    dev += s.input;
                }
                continue;
            }
            seen.insert(w);
            if s.buy && s.slot <= p.created_slot + EARLY_SLOTS {
                early.insert(w);
            }
            if !s.buy && !buyers.contains(w) {
                fanout.insert(w);
            }
        }
        for w in seen {
            *in_pools.entry(w).or_default() += 1;
        }
        for w in early {
            *early_in.entry(w).or_default() += 1;
        }
        if dev > 0 && metric_set.contains(p.config.as_str()) {
            dev_buys.push(dev);
        }
        fanout_per_pool.push(fanout);
    }

    let specific = |w: &str, k: u32| -> bool {
        let global = idx.0.get(w).copied().unwrap_or(k).max(k);
        k as f64 / global as f64 >= SPECIFICITY
    };
    let mut farm: HashSet<&str> = HashSet::new();
    if n >= MIN_POOLS {
        let need = ((RECUR_SHARE * n as f64).ceil() as u32).max(MIN_POOLS as u32);
        for (w, k) in &in_pools {
            if *k >= need && specific(w, *k) {
                farm.insert(w);
            }
        }
        let need_first = ((FIRST_BUYER_SHARE * n as f64).ceil() as u32).max(MIN_POOLS as u32);
        for (w, k) in &early_in {
            if *k >= need_first && specific(w, in_pools[w]) {
                out.first_buyers += 1;
                farm.insert(w);
            }
        }
    }
    out.farm_wallets = farm.len();

    // фиксированная дев-покупка: самая частая сумма с точностью 0,01 SOL
    if dev_buys.len() >= MIN_POOLS {
        let mut buckets: HashMap<u64, usize> = HashMap::new();
        for d in &dev_buys {
            *buckets.entry((d + 5_000_000) / 10_000_000).or_default() += 1;
        }
        let (b, c) = buckets.into_iter().max_by_key(|(_, c)| *c).unwrap();
        out.dev_buy_mode = Some((b as f64 * 0.01, c as f64 / dev_buys.len() as f64));
    }

    // --- Метрики активности (только пулы в окне) ---
    let mut mig = Vec::new();
    let mut shares = Vec::new();
    let mut fanouts = Vec::new();
    let mut external: HashSet<&str> = HashSet::new();
    for (p, fanout) in pools.iter().zip(&fanout_per_pool) {
        if !metric_set.contains(p.config.as_str()) || p.created_time.unwrap_or(i64::MAX) < metrics_since {
            continue;
        }
        out.pools += 1;
        if let (Some(g), Some(c)) = (p.graduated_time, p.created_time) {
            out.graduated += 1;
            mig.push((g - c).max(0) as f64);
        }
        fanouts.push(fanout.len() as f64);
        let (mut total, mut linked) = (0u64, 0u64);
        let non_creator = p.swaps.iter().filter(|s| s.wallet != p.creator).count();
        for s in &p.swaps {
            let v = sol_volume(s);
            total += v;
            let w = s.wallet.as_str();
            if w == p.creator || fanout.contains(w) || farm.contains(w) {
                linked += v;
            } else {
                external.insert(w);
                if s.buy {
                    out.external_buy_lamports += s.input;
                } else {
                    out.external_sell_lamports += s.output;
                }
            }
        }
        out.total_volume_lamports += total;
        out.linked_volume_lamports += linked;
        if total > 0 && non_creator >= MIN_ACTIVE_SWAPS {
            out.active_pools += 1;
            shares.push(linked as f64 / total as f64);
        }
    }
    out.external_wallets = external.len();
    out.median_migration_secs = median(mig);
    out.median_linked_share = if shares.len() >= MIN_POOLS.min(out.pools.max(1)) { median(shares) } else { None };
    out.median_fanout = median(fanouts);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 4 пула: создатель с одинаковой покупкой, ферма F1..F3 в каждом пуле (и нигде больше),
    /// общий бот BOT торгует ещё в 6 чужих пулах, внешний покупатель — по одному на пул.
    #[test]
    fn separates_farm_from_generic_bots_and_externals() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE pools(pool, config, creator, created_time, created_slot, tracked);
             CREATE TABLE swaps(pool, fee_payer, trade_direction, included_fee_input_amount, output_amount, slot, event_index);
             CREATE TABLE curve_complete(pool, block_time);",
        )
        .unwrap();
        let sw = |pool: &str, w: &str, buy: bool, i: i64, o: i64, slot: i64| {
            conn.execute(
                "INSERT INTO swaps VALUES(?1,?2,?3,?4,?5,?6,0)",
                params![pool, w, if buy { 1 } else { 0 }, i, o, slot],
            )
            .unwrap();
        };
        for k in 0..4 {
            let p = format!("P{k}");
            conn.execute("INSERT INTO pools VALUES(?1,'CFG','HUB',1000,100,1)", params![p]).unwrap();
            conn.execute("INSERT INTO curve_complete VALUES(?1, 1070)", params![p]).unwrap();
            sw(&p, "HUB", true, 8_057_000_000, 1, 100);
            sw(&p, "F1", true, 1_000_000_000, 1, 101); // постоянный первый покупатель
            sw(&p, "F2", true, 2_000_000_000, 1, 140);
            sw(&p, "F3", false, 1, 3_000_000_000, 150); // веерный продавец (не покупал)
            sw(&p, "BOT", true, 10_000_000, 1, 101);
            sw(&p, &format!("EXT{k}"), true, 200_000_000, 1, 160);
        }
        for k in 0..6 {
            conn.execute("INSERT INTO pools VALUES(?1,'OTHER','X',1000,100,1)", params![format!("Q{k}")]).unwrap();
            sw(&format!("Q{k}"), "BOT", true, 10_000_000, 1, 101);
        }
        let idx = wallet_index(&conn).unwrap();
        let f = analyze_config(&conn, "CFG", &idx, 0).unwrap();
        assert_eq!(f.pools, 4);
        assert_eq!(f.farm_wallets, 3, "F1, F2, F3 — ферма; BOT — общий бот");
        assert_eq!(f.first_buyers, 1);
        assert_eq!(f.median_migration_secs, Some(70.0));
        let (sol, share) = f.dev_buy_mode.unwrap();
        assert!((sol - 8.06).abs() < 0.011 && share == 1.0);
        assert_eq!(f.external_wallets, 5, "4 внешних + общий бот");
        assert!((f.external_net_in_sol() - (4.0 * 0.2 + 4.0 * 0.01)).abs() < 1e-9);
        assert!(f.median_linked_share.unwrap() > 0.9);
    }

    /// Ферма оператора разнесена по двум конфигам: по одному конфигу кошельки неспецифичны
    /// (половина их активности в другом конфиге), по кластеру — специфичны.
    /// Пулы мгновенной graduation (только покупка создателя) в долю связанного объёма не входят.
    #[test]
    fn cluster_scope_and_inactive_pools() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE pools(pool, config, creator, created_time, created_slot, tracked);
             CREATE TABLE swaps(pool, fee_payer, trade_direction, included_fee_input_amount, output_amount, slot, event_index);
             CREATE TABLE curve_complete(pool, block_time);",
        )
        .unwrap();
        let sw = |pool: &str, w: &str, buy: bool, i: i64, o: i64| {
            conn.execute(
                "INSERT INTO swaps VALUES(?1,?2,?3,?4,?5,150,0)",
                params![pool, w, if buy { 1 } else { 0 }, i, o],
            )
            .unwrap();
        };
        for (cfg, k) in [("A", 0), ("A", 1), ("A", 2), ("B", 3), ("B", 4), ("B", 5)] {
            let p = format!("P{k}");
            conn.execute("INSERT INTO pools VALUES(?1,?2,'HUB',1000,100,1)", params![p, cfg]).unwrap();
            for w in ["F1", "F2", "F3"] {
                sw(&p, w, true, 1_000_000_000, 1);
            }
        }
        // конфиг C: мгновенная graduation, только покупка создателя
        for k in 0..3 {
            let p = format!("I{k}");
            conn.execute("INSERT INTO pools VALUES(?1,'C','HUB2',1000,100,1)", params![p]).unwrap();
            conn.execute("INSERT INTO swaps VALUES(?1,'HUB2',1,5000,1,100,0)", params![p]).unwrap();
        }
        let idx = wallet_index(&conn).unwrap();
        let alone = analyze_config(&conn, "A", &idx, 0).unwrap();
        assert_eq!(alone.farm_wallets, 0, "per config: only 3 of 6 pools are in A");
        let scope = vec!["A".to_string(), "B".to_string()];
        let joint = analyze_scope(&conn, &scope, &["A".to_string()], &idx, 0).unwrap();
        assert_eq!(joint.farm_wallets, 3);
        assert_eq!(joint.pools, 3, "metrics only over config A");
        let inst = analyze_config(&conn, "C", &idx, 0).unwrap();
        assert_eq!(inst.active_pools, 0);
        assert!(inst.median_linked_share.is_none());
    }
}
