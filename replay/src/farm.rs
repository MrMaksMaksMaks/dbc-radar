//! Кошельки «фермы» и синтетичность активности в пулах одного конфига.
//!
//! Признаки (номера — из расследования по оператору 2toDxUnv):
//!   4.  скорость миграции: время от создания пула до завершения кривой;
//!   7.  веерная раздача: кошельки, которые продают токены, ни разу не купив их в пуле;
//!   8.  повторяющийся набор участников: кошелёк торгует в большой доле пулов конфига
//!       и почти нигде больше (специфичность), — отличает ферму от общих ботов,
//!       которые торгуют все новые токены подряд. Специфичность считается и по числу пулов,
//!       и по SOL-объёму: пыль в сторонних пулах не размывает кошелёк фермы;
//!   9.  доля связанного объёма: объём создателя, фермы и веерных продавцов к общему;
//!   10. постоянный «первый покупатель»: кошелёк покупает в первые секунды большинства пулов;
//!   11. фиксированная стартовая покупка: одинаковая сумма первой покупки создателя
//!       или — если оператор начинает с кошелька фермы — первой покупки не от создателя
//!       (общие боты, торгующие все новые токены, при этом пропускаются).
//! Всё остальное — «внешние» участники; их результат на кривой считается отдельно.

use anyhow::Result;
use rusqlite::{params, Connection};
use std::collections::{HashMap, HashSet};

/// Мастер-кошелёк: платил или получал SOL за сделки не менее стольких разных подписантов в пуле...
const MASTER_MIN_SIGNERS: usize = 3;
/// ...и не менее стольких сделок.
const MASTER_MIN_SWAPS: usize = 5;
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

/// Для каждого кошелька по всей базе: (в скольких отслеживаемых пулах торговал, его объём
/// в единицах котируемого токена: покупка — сколько вошло, продажа — сколько вышло).
pub struct WalletIndex(HashMap<String, (u32, u64)>);

pub fn wallet_index(conn: &Connection) -> Result<WalletIndex> {
    let mut st = conn.prepare(
        "SELECT fee_payer, COUNT(DISTINCT pool),
                SUM(CASE WHEN trade_direction = 1 THEN included_fee_input_amount ELSE output_amount END)
         FROM swaps GROUP BY fee_payer",
    )?;
    let m = st
        .query_map([], |r| {
            Ok((r.get::<_, String>(0)?, (r.get::<_, i64>(1)? as u32, r.get::<_, Option<i64>>(2)?.unwrap_or(0).max(0) as u64)))
        })?
        .collect::<std::result::Result<HashMap<_, _>, _>>()?;
    Ok(WalletIndex(m))
}

#[derive(Debug, Clone, Default)]
pub struct FarmStats {
    pub pools: usize,
    pub graduated: usize,
    pub median_migration_secs: Option<f64>,
    pub farm_wallets: usize,
    /// кошельки фермы, специфичные только по объёму: торгуют мелочью и в сторонних пулах
    /// (возможная маскировка)
    pub volume_only_wallets: usize,
    pub first_buyers: usize,
    /// (сумма в SOL, доля пулов с этой суммой) — самая частая стартовая покупка создателя
    pub dev_buy_mode: Option<(f64, f64)>,
    /// то же для первой покупки не от создателя (без общих ботов)
    pub first_buy_mode: Option<(f64, f64)>,
    pub median_fanout: Option<f64>,
    pub median_linked_share: Option<f64>,
    /// пулов с реальной торговлей (>= MIN_ACTIVE_SWAPS сделок не от создателя)
    pub active_pools: usize,
    pub external_wallets: usize,
    pub external_buy_lamports: u64,
    pub external_sell_lamports: u64,
    pub total_volume_lamports: u64,
    pub linked_volume_lamports: u64,
    /// мастер-кошельки: платили или получали SOL за сделки не менее MASTER_MIN_SIGNERS чужих подписантов
    pub master_wallets: usize,
    /// медиана по пулам: доля объёма, за которую платил не подписант (мастер через прокси)
    pub median_proxied_share: Option<f64>,
}

impl FarmStats {
    /// SOL, которые внешние участники оставили на кривой (покупки минус продажи).
    pub fn external_net_in_sol(&self) -> f64 {
        (self.external_buy_lamports as f64 - self.external_sell_lamports as f64) / 1e9
    }
}

struct Swap {
    wallet: String,
    /// чьи SOL в сделке (sol_owner из коллектора); для старых записей — подписант
    payer: String,
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

/// Округление суммы до трёх значащих цифр: «одинаковая сумма» и для 8,06 SOL, и для 0,00123 SOL.
fn sig3(l: u64) -> u64 {
    if l < 1000 {
        return l;
    }
    let digits = (l as f64).log10().floor() as u32;
    let unit = 10u64.pow(digits - 2);
    (l + unit / 2) / unit * unit
}

/// Самая частая сумма (с точностью до трёх значащих цифр) и её доля: (SOL, доля).
fn amount_mode(v: &[u64]) -> Option<(f64, f64)> {
    if v.len() < MIN_POOLS {
        return None;
    }
    let mut buckets: HashMap<u64, usize> = HashMap::new();
    for a in v {
        *buckets.entry(sig3(*a)).or_default() += 1;
    }
    let (b, c) = buckets.into_iter().max_by_key(|(_, c)| *c)?;
    Some((b as f64 / 1e9, c as f64 / v.len() as f64))
}

/// Сумма в SOL для текста: мелкие суммы с нужным числом знаков, а не «0.00».
pub fn fmt_sol(sol: f64) -> String {
    if sol >= 0.1 {
        format!("{sol:.2}")
    } else if sol >= 0.001 {
        format!("{sol:.4}")
    } else {
        format!("{sol:.6}")
    }
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
    let data = load_scope(conn, scope)?;
    Ok(analyze_loaded(&data, metrics, idx, metrics_since))
}

/// Пулы и сделки области (кластера), загруженные один раз: по ним считаются метрики
/// каждого конфига кластера без повторного чтения базы.
pub struct ScopeData(Vec<PoolData>);

pub fn load_scope(conn: &Connection, scope: &[String]) -> Result<ScopeData> {
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

    // колонка sol_owner есть в базах коллектора новее этого изменения
    let has_owner = conn.prepare("SELECT sol_owner FROM swaps LIMIT 0").is_ok();
    let mut sw = conn.prepare_cached(if has_owner {
        "SELECT fee_payer, trade_direction, included_fee_input_amount, output_amount, slot, COALESCE(sol_owner, fee_payer)
         FROM swaps WHERE pool = ?1 ORDER BY slot, event_index"
    } else {
        "SELECT fee_payer, trade_direction, included_fee_input_amount, output_amount, slot, fee_payer
         FROM swaps WHERE pool = ?1 ORDER BY slot, event_index"
    })?;
    let mut pools: Vec<PoolData> = Vec::new();
    for (pool, creator, created_time, created_slot, graduated_time, config) in heads {
        let swaps: Vec<Swap> = sw
            .query_map(params![pool], |r| {
                Ok(Swap {
                    wallet: r.get(0)?,
                    payer: r.get(5)?,
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
    Ok(ScopeData(pools))
}

/// Ферма и метрики по уже загруженной области; `metrics` — конфиги, для которых считаются метрики.
pub fn analyze_loaded(data: &ScopeData, metrics: &[String], idx: &WalletIndex, metrics_since: i64) -> FarmStats {
    let pools = &data.0;
    let metric_set: HashSet<&str> = metrics.iter().map(String::as_str).collect();

    let n = pools.len();
    let mut out = FarmStats::default();
    if n == 0 {
        return out;
    }

    // --- Определение фермы (по всем отслеживаемым пулам конфига) ---
    let mut in_pools: HashMap<&str, u32> = HashMap::new();
    // SOL-объём кошелька в пулах области (для специфичности по объёму)
    let mut vol_in: HashMap<&str, u64> = HashMap::new();
    let mut early_in: HashMap<&str, u32> = HashMap::new();
    // то же, но только по пулам оцениваемого конфига (metrics)
    let mut in_metric: HashMap<&str, u32> = HashMap::new();
    let mut early_metric: HashMap<&str, u32> = HashMap::new();
    let mut fanout_per_pool: Vec<HashSet<&str>> = Vec::with_capacity(n);
    let mut dev_buys: Vec<u64> = Vec::new();
    // первые покупки не от создателя (до 5 на пул): кошелёк и сумма, по порядку
    let mut opening: Vec<Vec<(&str, u64)>> = Vec::with_capacity(n);
    for p in pools.iter() {
        let buyers: HashSet<&str> = p.swaps.iter().filter(|s| s.buy).map(|s| s.wallet.as_str()).collect();
        let mut seen: HashSet<&str> = HashSet::new();
        let mut early: HashSet<&str> = HashSet::new();
        let mut fanout: HashSet<&str> = HashSet::new();
        let mut dev = 0u64;
        let mut first: Vec<(&str, u64)> = Vec::new();
        for s in &p.swaps {
            let w = s.wallet.as_str();
            if w == p.creator {
                if s.buy && s.slot <= p.created_slot + 2 {
                    dev += s.input;
                }
                continue;
            }
            if s.buy && first.len() < 5 {
                first.push((w, s.input));
            }
            seen.insert(w);
            *vol_in.entry(w).or_default() += sol_volume(s);
            if s.buy && s.slot <= p.created_slot + EARLY_SLOTS {
                early.insert(w);
            }
            if !s.buy && !buyers.contains(w) {
                fanout.insert(w);
            }
        }
        let is_metric = metric_set.contains(p.config.as_str());
        for w in seen {
            *in_pools.entry(w).or_default() += 1;
            if is_metric {
                *in_metric.entry(w).or_default() += 1;
            }
        }
        for w in early {
            *early_in.entry(w).or_default() += 1;
            if is_metric {
                *early_metric.entry(w).or_default() += 1;
            }
        }
        if dev > 0 && metric_set.contains(p.config.as_str()) {
            dev_buys.push(dev);
        }
        fanout_per_pool.push(fanout);
        opening.push(first);
    }

    // Специфичность: (по числу пулов, по объёму). Общий бот размазан по множеству пулов и по
    // пулам, и по объёму; кошелёк фермы, который ради маскировки торгует пылью в сторонних
    // пулах, теряет специфичность по пулам, но не по объёму.
    let spec = |w: &str, k: u32| -> (bool, bool) {
        let (gp, gv) = idx.0.get(w).copied().unwrap_or((k, 0));
        let by_pools = k as f64 / gp.max(k) as f64 >= SPECIFICITY;
        let v = vol_in.get(w).copied().unwrap_or(0);
        let by_volume = v > 0 && v as f64 / gv.max(v) as f64 >= SPECIFICITY;
        (by_pools, by_volume)
    };
    let specific = |w: &str, k: u32| -> bool {
        let (p, v) = spec(w, k);
        p || v
    };
    // Повторяемость — по всей области (кластер оператора: ферма разнесена по его конфигам)
    // ИЛИ по пулам самого конфига: в большом кластере ферма одного конфига — малая доля
    // всех пулов кластера, но почти все пулы своего конфига. Специфичность — по области.
    let n_metric = pools.iter().filter(|p| metric_set.contains(p.config.as_str())).count();
    let recurs = |k_scope: u32, k_metric: u32, share: f64| -> bool {
        let need = |total: usize| ((share * total as f64).ceil() as u32).max(MIN_POOLS as u32);
        (n >= MIN_POOLS && k_scope >= need(n)) || (n_metric >= MIN_POOLS && k_metric >= need(n_metric))
    };
    let mut farm: HashSet<&str> = HashSet::new();
    for (w, k) in &in_pools {
        if recurs(*k, in_metric.get(w).copied().unwrap_or(0), RECUR_SHARE) && specific(w, *k) {
            farm.insert(w);
        }
    }
    for (w, k) in &early_in {
        if recurs(*k, early_metric.get(w).copied().unwrap_or(0), FIRST_BUYER_SHARE) && specific(w, in_pools[w]) {
            out.first_buyers += 1;
            farm.insert(w);
        }
    }
    out.farm_wallets = farm.len();
    out.volume_only_wallets = farm
        .iter()
        .filter(|w| {
            let (p, v) = spec(w, in_pools.get(*w).copied().unwrap_or(0));
            !p && v
        })
        .count();

    // фиксированная стартовая покупка создателя
    out.dev_buy_mode = amount_mode(&dev_buys);
    // фиксированная первая покупка не от создателя: первая покупка в пуле от кошелька,
    // который не является общим ботом (общие боты с фиксированной суммой есть в любом конфиге)
    let mut first_buys: Vec<u64> = Vec::new();
    for (p, first) in pools.iter().zip(&opening) {
        if !metric_set.contains(p.config.as_str()) {
            continue;
        }
        if let Some((_, amount)) = first.iter().find(|(w, _)| specific(w, in_pools.get(w).copied().unwrap_or(1))) {
            first_buys.push(*amount);
        }
    }
    out.first_buy_mode = amount_mode(&first_buys);

    // --- Метрики активности (только пулы в окне) ---
    let mut mig = Vec::new();
    let mut shares = Vec::new();
    let mut fanouts = Vec::new();
    let mut proxied_shares = Vec::new();
    let mut all_masters: HashSet<&str> = HashSet::new();
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
        // мастер-кошельки пула: платят или получают SOL за сделки нескольких чужих подписантов
        // (один релейер, подписывающий за многих, — обратный случай и сюда не попадает)
        let mut signers_of: HashMap<&str, (HashSet<&str>, usize)> = HashMap::new();
        for s in &p.swaps {
            if s.payer != s.wallet {
                let e = signers_of.entry(s.payer.as_str()).or_default();
                e.0.insert(s.wallet.as_str());
                e.1 += 1;
            }
        }
        let masters: HashSet<&str> = signers_of
            .iter()
            .filter(|(_, (signers, n))| signers.len() >= MASTER_MIN_SIGNERS && *n >= MASTER_MIN_SWAPS)
            .map(|(m, _)| *m)
            .collect();
        all_masters.extend(masters.iter().copied());
        let (mut total, mut linked, mut proxied) = (0u64, 0u64, 0u64);
        let non_creator = p.swaps.iter().filter(|s| s.wallet != p.creator).count();
        for s in &p.swaps {
            let v = sol_volume(s);
            total += v;
            let w = s.wallet.as_str();
            let payer = s.payer.as_str();
            if masters.contains(payer) {
                proxied += v;
            }
            if w == p.creator || fanout.contains(w) || farm.contains(w)
                || payer == p.creator || masters.contains(payer) || farm.contains(payer)
            {
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
            proxied_shares.push(proxied as f64 / total as f64);
        }
    }
    out.master_wallets = all_masters.len();
    out.median_proxied_share = if proxied_shares.is_empty() { None } else { median(proxied_shares) };
    out.external_wallets = external.len();
    out.median_migration_secs = median(mig);
    out.median_linked_share = if shares.len() >= MIN_POOLS.min(out.pools.max(1)) { median(shares) } else { None };
    out.median_fanout = median(fanouts);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 4 пула: создатель с одинаковой покупкой, ферма F1..F3 в каждом пуле (и нигде больше),
    /// общий бот BOT торгует ещё в 6 чужих пулах, внешний покупатель — по одному на пул.
    #[test]
    fn rounds_amounts_and_formats_small_sums() {
        assert_eq!(sig3(8_057_000_000), 8_060_000_000);
        assert_eq!(sig3(1_234_567), 1_230_000);
        assert_eq!(sig3(999), 999);
        assert_eq!(amount_mode(&[1_234_000, 1_231_000, 1_229_000, 5_000_000]), Some((0.00123, 0.75)));
        assert_eq!(amount_mode(&[1, 2]), None);
        assert_eq!(fmt_sol(0.00123), "0.0012");
        assert_eq!(fmt_sol(8.06), "8.06");
        assert_eq!(fmt_sol(0.000012), "0.000012");
    }

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
        // первая покупка не от создателя — F1 на 1 SOL (BOT пропускается как общий бот)
        assert_eq!(f.first_buy_mode, Some((1.0, 1.0)));
        assert_eq!(f.external_wallets, 5, "4 внешних + общий бот");
        assert!((f.external_net_in_sol() - (4.0 * 0.2 + 4.0 * 0.01)).abs() < 1e-9);
        assert!(f.median_linked_share.unwrap() > 0.9);
    }

    /// Ферма оператора разнесена по двум конфигам: по одному конфигу кошельки неспецифичны
    /// (половина их активности в другом конфиге), по кластеру — специфичны.
    /// Пулы мгновенной graduation (только покупка создателя) в долю связанного объёма не входят.
    #[test]
    fn master_behind_fresh_proxies_is_linked_but_relayer_is_not() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE pools(pool, config, creator, created_time, created_slot, tracked);
             CREATE TABLE swaps(pool, fee_payer, trade_direction, included_fee_input_amount, output_amount, slot, event_index, sol_owner);
             CREATE TABLE curve_complete(pool, block_time);",
        )
        .unwrap();
        let sw = |pool: &str, signer: &str, payer: &str, i: i64| {
            conn.execute("INSERT INTO swaps VALUES(?1,?2,1,?3,1,140,0,?4)", params![pool, signer, i, payer]).unwrap();
        };
        // конфиг M: в каждом из 3 пулов мастер MASTER платит за 5 сделок 4 свежих прокси
        for k in 0..3 {
            let p = format!("M{k}");
            conn.execute("INSERT INTO pools VALUES(?1,'M','CM',1000,100,1)", params![p]).unwrap();
            for j in 0..5 {
                sw(&p, &format!("PROXY{k}_{}", j % 4), "MASTER", 1_000_000_000);
            }
            sw(&p, &format!("EXT{k}"), &format!("EXT{k}"), 10_000_000);
        }
        // конфиг R: релейер RELAY подписывает за 5 разных пользователей, каждый платит сам
        for k in 0..3 {
            let p = format!("R{k}");
            conn.execute("INSERT INTO pools VALUES(?1,'R','CR',1000,100,1)", params![p]).unwrap();
            for j in 0..5 {
                sw(&p, "RELAY", &format!("USER{k}_{j}"), 1_000_000_000);
            }
        }
        let idx = wallet_index(&conn).unwrap();
        let m = analyze_config(&conn, "M", &idx, 0).unwrap();
        assert_eq!(m.master_wallets, 1);
        assert!(m.median_linked_share.unwrap() > 0.99, "master volume is linked: {:?}", m.median_linked_share);
        assert!(m.median_proxied_share.unwrap() > 0.99);
        let r = analyze_config(&conn, "R", &idx, 0).unwrap();
        assert_eq!(r.master_wallets, 0, "a relayer signing for many users is not a master");
    }

    #[test]
    fn decoy_dust_does_not_hide_a_farm_wallet() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE pools(pool, config, creator, created_time, created_slot, tracked);
             CREATE TABLE swaps(pool, fee_payer, trade_direction, included_fee_input_amount, output_amount, slot, event_index);
             CREATE TABLE curve_complete(pool, block_time);",
        )
        .unwrap();
        let sw = |pool: &str, w: &str, i: i64| {
            conn.execute("INSERT INTO swaps VALUES(?1,?2,1,?3,1,140,0)", params![pool, w, i]).unwrap();
        };
        // конфиг A: 4 пула; FARM покупает по 1 SOL, общий BOT — по 0,01 SOL
        for k in 0..4 {
            let p = format!("A{k}");
            conn.execute("INSERT INTO pools VALUES(?1,'A','HUB',1000,100,1)", params![p]).unwrap();
            sw(&p, "FARM", 1_000_000_000);
            sw(&p, "BOT", 10_000_000);
        }
        // 10 сторонних пулов: FARM торгует пылью для маскировки, BOT — обычными суммами
        for k in 0..10 {
            let p = format!("Z{k}");
            conn.execute("INSERT INTO pools VALUES(?1,'Z','OTHER',1000,100,1)", params![p]).unwrap();
            sw(&p, "FARM", 100_000);
            sw(&p, "BOT", 10_000_000);
        }
        let idx = wallet_index(&conn).unwrap();
        let f = analyze_config(&conn, "A", &idx, 0).unwrap();
        // FARM: по пулам 4/14 — не специфичен, по объёму 4 SOL из 4,001 — специфичен
        assert_eq!(f.farm_wallets, 1, "FARM — ферма, BOT — общий бот");
        assert_eq!(f.volume_only_wallets, 1);
    }

    #[test]
    fn farm_of_one_config_inside_a_large_cluster() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE pools(pool, config, creator, created_time, created_slot, tracked);
             CREATE TABLE swaps(pool, fee_payer, trade_direction, included_fee_input_amount, output_amount, slot, event_index);
             CREATE TABLE curve_complete(pool, block_time);",
        )
        .unwrap();
        let sw = |pool: &str, w: &str, i: i64, slot: i64| {
            conn.execute("INSERT INTO swaps VALUES(?1,?2,1,?3,1,?4,0)", params![pool, w, i, slot]).unwrap();
        };
        // конфиг A: 3 пула, FIRST покупает первым на 8,2125 SOL, F2 торгует в каждом
        for k in 0..3 {
            let p = format!("A{k}");
            conn.execute("INSERT INTO pools VALUES(?1,'A','HUB',1000,100,1)", params![p]).unwrap();
            sw(&p, "FIRST", 8_212_500_000, 101);
            sw(&p, "F2", 1_000_000_000, 140);
        }
        // конфиг B того же кластера: 10 пулов с разовыми внешними покупателями
        for k in 0..10 {
            let p = format!("B{k}");
            conn.execute("INSERT INTO pools VALUES(?1,'B','HUB',1000,100,1)", params![p]).unwrap();
            sw(&p, &format!("EXT{k}"), 100_000_000, 140);
        }
        let idx = wallet_index(&conn).unwrap();
        let scope = vec!["A".to_string(), "B".to_string()];
        // в кластере 13 пулов: по доле кластера (30% = 4 пула) FIRST и F2 не проходят, по доле A — проходят
        let f = analyze_scope(&conn, &scope, &["A".to_string()], &idx, 0).unwrap();
        assert_eq!(f.farm_wallets, 2);
        assert_eq!(f.first_buyers, 1);
        assert_eq!(f.first_buy_mode, Some((8.21, 1.0)));
        // для конфига B ферма не появляется
        let b = analyze_scope(&conn, &scope, &["B".to_string()], &idx, 0).unwrap();
        assert_eq!(b.farm_wallets, 0);
    }

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
