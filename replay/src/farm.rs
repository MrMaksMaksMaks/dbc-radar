//! "Farm" wallets and how synthetic the activity is in the pools of one config.
//!
//! Signals (numbers from the investigation of one bundle-launch operator):
//!   4.  migration speed: time from pool creation to curve completion;
//!   7.  fan-out: wallets that sell tokens without ever buying them in the pool;
//!   8.  recurring set of participants: a wallet trades in a large share of the config's pools
//!       and almost nowhere else (specificity) — this separates a farm from common bots
//!       that trade every new token. Specificity is computed both by pool count
//!       and by SOL volume: dust in unrelated pools does not dilute a farm wallet;
//!   9.  linked volume share: volume of the creator, the farm and the fan-out sellers relative to the total;
//!   10. permanent "first buyer": a wallet buys in the first seconds of most pools;
//!   11. fixed opening buy: the same amount for the creator's first buy
//!       or — if the operator starts from a farm wallet — for the first non-creator buy
//!       (common bots that trade every new token are skipped here).
//! Everyone else is an "external" participant; their result on the curve is computed separately.

use anyhow::Result;
use rusqlite::{params, Connection};
use std::collections::{HashMap, HashSet};

/// Master wallet: paid or received SOL for trades of at least this many different signers in the pool...
const MASTER_MIN_SIGNERS: usize = 3;
/// ...and for at least this many trades.
const MASTER_MIN_SWAPS: usize = 5;
/// A buy within these first slots after pool creation counts as "early" (~10 s).
pub const EARLY_SLOTS: i64 = 25;
/// Share of the config's pools a farm wallet must trade in.
pub const RECUR_SHARE: f64 = 0.3;
/// Share of early buys needed to count a wallet as a permanent first buyer.
pub const FIRST_BUYER_SHARE: f64 = 0.5;
/// Share of the wallet's pools that belong to this config: a farm barely trades outside it.
pub const SPECIFICITY: f64 = 0.8;
/// Minimum tracked pools for the recurrence signals.
pub const MIN_POOLS: usize = 3;
/// A pool counts as "traded" if it has at least this many non-creator trades.
/// Pools with instant graduation (a single creator buy) are not included in the linked volume share.
pub const MIN_ACTIVE_SWAPS: usize = 3;

/// For each wallet across the whole database: (how many tracked pools it traded in, its volume
/// in quote token units: a buy — how much went in, a sell — how much came out).
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
    /// farm wallets that are specific by volume only: they trade small amounts in unrelated pools too
    /// (possible camouflage)
    pub volume_only_wallets: usize,
    pub first_buyers: usize,
    /// (amount in SOL, share of pools with this amount) — the creator's most frequent opening buy
    pub dev_buy_mode: Option<(f64, f64)>,
    /// the same for the first non-creator buy (excluding common bots)
    pub first_buy_mode: Option<(f64, f64)>,
    pub median_fanout: Option<f64>,
    pub median_linked_share: Option<f64>,
    /// pools with real trading (>= MIN_ACTIVE_SWAPS non-creator trades)
    pub active_pools: usize,
    pub external_wallets: usize,
    pub external_buy_lamports: u64,
    pub external_sell_lamports: u64,
    pub total_volume_lamports: u64,
    pub linked_volume_lamports: u64,
    /// master wallets: paid or received SOL for trades of at least MASTER_MIN_SIGNERS other signers
    pub master_wallets: usize,
    /// median over pools: share of volume paid for by someone other than the signer (a master via proxies)
    pub median_proxied_share: Option<f64>,
    /// over active pools: SOL left on the curve by external wallets (buys − sells;
    /// positive — externals put in more than they took out). Research only (--research).
    pub external_net_per_pool: Vec<f64>,
    /// pools where sellers without a buy sold more tokens than the creator bought
    /// (the distribution is not from the operator — not counted as linked)
    pub unattributed_fanout_pools: usize,
    /// over active pools: (SOL that the linked wallets — creator, fan-out, farm, masters —
    /// took off the curve beyond what they put in; whether the pool graduated; creation time). Positive — the operator took other people's
    /// money on the curve. Research only (--research).
    pub operator_net_per_pool: Vec<(f64, bool, i64)>,
}

impl FarmStats {
    /// SOL that external participants left on the curve (buys minus sells).
    pub fn external_net_in_sol(&self) -> f64 {
        (self.external_buy_lamports as f64 - self.external_sell_lamports as f64) / 1e9
    }
}

struct Swap {
    wallet: String,
    /// whose SOL is in the trade (sol_owner from the collector); for old records — the signer
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

/// Rounds an amount to three significant digits: "the same amount" both for 8.06 SOL and for 0.00123 SOL.
fn sig3(l: u64) -> u64 {
    if l < 1000 {
        return l;
    }
    let digits = (l as f64).log10().floor() as u32;
    let unit = 10u64.pow(digits - 2);
    (l + unit / 2) / unit * unit
}

/// The most frequent amount (to three significant digits) and its share: (SOL, share).
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

/// An amount in SOL for display: small amounts with enough digits, not "0.00".
pub fn fmt_sol(sol: f64) -> String {
    if sol >= 0.1 {
        format!("{sol:.2}")
    } else if sol >= 0.001 {
        format!("{sol:.4}")
    } else {
        format!("{sol:.6}")
    }
}

/// SOL volume of a trade: a buy — how much SOL went in, a sell — how much came out.
fn sol_volume(s: &Swap) -> u64 {
    if s.buy { s.input } else { s.output }
}

/// One config: the farm is detected and the metrics are computed on that same config.
pub fn analyze_config(conn: &Connection, config: &str, idx: &WalletIndex, metrics_since: i64) -> Result<FarmStats> {
    let c = [config.to_string()];
    analyze_scope(conn, &c, &c, idx, metrics_since)
}

/// `scope` — configs whose pools are used to detect the farm (the operator's whole cluster:
/// if the operator runs one farm across several configs, specificity is computed against the cluster).
/// `metrics` — configs whose pools are used to compute the activity metrics.
/// `metrics_since`: pools created earlier take part in farm detection, but not in the metrics.
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

/// Pools and trades of the scope (cluster), loaded once: the metrics of every config
/// in the cluster are computed from them without re-reading the database.
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

    // the sol_owner column exists in collector databases newer than this change
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

/// Farm and metrics over an already loaded scope; `metrics` — configs to compute the metrics for.
pub fn analyze_loaded(data: &ScopeData, metrics: &[String], idx: &WalletIndex, metrics_since: i64) -> FarmStats {
    let pools = &data.0;
    let metric_set: HashSet<&str> = metrics.iter().map(String::as_str).collect();

    let n = pools.len();
    let mut out = FarmStats::default();
    if n == 0 {
        return out;
    }

    // --- Farm detection (over all tracked pools of the config) ---
    let mut in_pools: HashMap<&str, u32> = HashMap::new();
    // wallet's SOL volume in the pools of the scope (for volume specificity)
    let mut vol_in: HashMap<&str, u64> = HashMap::new();
    let mut early_in: HashMap<&str, u32> = HashMap::new();
    // the same, but only over the pools of the config being scored (metrics)
    let mut in_metric: HashMap<&str, u32> = HashMap::new();
    let mut early_metric: HashMap<&str, u32> = HashMap::new();
    let mut fanout_per_pool: Vec<HashSet<&str>> = Vec::with_capacity(n);
    let mut dev_buys: Vec<u64> = Vec::new();
    // first non-creator buys (up to 5 per pool): wallet and amount, in order
    let mut opening: Vec<Vec<(&str, u64)>> = Vec::with_capacity(n);
    let mut unattributed_fanout = 0usize;
    for p in pools.iter() {
        let buyers: HashSet<&str> = p.swaps.iter().filter(|s| s.buy).map(|s| s.wallet.as_str()).collect();
        let mut seen: HashSet<&str> = HashSet::new();
        let mut early: HashSet<&str> = HashSet::new();
        let mut fanout: HashSet<&str> = HashSet::new();
        // tokens bought by the creator and tokens sold by the fan-out: on the curve tokens can
        // only be obtained by buying, so the creator could not distribute more than it bought
        let (mut creator_tokens, mut fanout_tokens) = (0u128, 0u128);
        let mut dev = 0u64;
        let mut first: Vec<(&str, u64)> = Vec::new();
        for s in &p.swaps {
            let w = s.wallet.as_str();
            if w == p.creator {
                if s.buy && s.slot <= p.created_slot + 2 {
                    dev += s.input;
                }
                if s.buy {
                    creator_tokens += s.output as u128;
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
                fanout_tokens += s.input as u128;
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
        // the fan-out sold more than the creator bought: the source is another buyer (e.g.
        // a sniper that distributed tokens to its own wallets), not the operator
        if fanout_tokens * 100 > creator_tokens * 105 {
            if !fanout.is_empty() && metric_set.contains(p.config.as_str()) {
                unattributed_fanout += 1;
            }
            fanout.clear();
        }
        fanout_per_pool.push(fanout);
        opening.push(first);
    }

    // Specificity: (by pool count, by volume). A common bot is spread over many pools both by
    // pools and by volume; a farm wallet that trades dust in unrelated pools as camouflage
    // loses specificity by pools, but not by volume.
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
    // Recurrence — over the whole scope (operator cluster: the farm is spread over its configs)
    // OR over the config's own pools: in a large cluster one config's farm is a small share
    // of all the cluster's pools, but almost all pools of its own config. Specificity — over the scope.
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

    // fixed opening buy by the creator
    out.dev_buy_mode = amount_mode(&dev_buys);
    // fixed first non-creator buy: the first buy in the pool by a wallet
    // that is not a common bot (common bots with a fixed amount exist in every config)
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

    // --- Activity metrics (only pools within the window) ---
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
        // master wallets of the pool: they pay or receive SOL for trades of several other signers
        // (a single relayer signing for many is the opposite case and does not get here)
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
        let (mut ext_buy, mut ext_sell) = (0u64, 0u64);
        let (mut op_buy, mut op_sell) = (0u64, 0u64);
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
                if s.buy {
                    op_buy += s.input;
                } else {
                    op_sell += s.output;
                }
            } else {
                external.insert(w);
                if s.buy {
                    out.external_buy_lamports += s.input;
                    ext_buy += s.input;
                } else {
                    out.external_sell_lamports += s.output;
                    ext_sell += s.output;
                }
            }
        }
        out.total_volume_lamports += total;
        out.linked_volume_lamports += linked;
        if total > 0 && non_creator >= MIN_ACTIVE_SWAPS {
            out.active_pools += 1;
            shares.push(linked as f64 / total as f64);
            proxied_shares.push(proxied as f64 / total as f64);
            out.external_net_per_pool.push((ext_buy as f64 - ext_sell as f64) / 1e9);
            out.operator_net_per_pool.push((
                (op_sell as f64 - op_buy as f64) / 1e9,
                p.graduated_time.is_some(),
                p.created_time.unwrap_or(0),
            ));
        }
    }
    out.master_wallets = all_masters.len();
    out.median_proxied_share = if proxied_shares.is_empty() { None } else { median(proxied_shares) };
    out.external_wallets = external.len();
    out.median_migration_secs = median(mig);
    out.median_linked_share = if shares.len() >= MIN_POOLS.min(out.pools.max(1)) { median(shares) } else { None };
    out.median_fanout = median(fanouts);
    out.unattributed_fanout_pools = unattributed_fanout;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 4 pools: a creator with the same buy, farm F1..F3 in every pool (and nowhere else),
    /// a common bot BOT also trades in 6 other pools, one external buyer per pool.
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
            sw(&p, "F1", true, 1_000_000_000, 1, 101); // permanent first buyer
            sw(&p, "F2", true, 2_000_000_000, 1, 140);
            sw(&p, "F3", false, 1, 3_000_000_000, 150); // fan-out seller (never bought)
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
        assert_eq!(f.farm_wallets, 3, "F1, F2, F3 are the farm; BOT is a common bot");
        assert_eq!(f.first_buyers, 1);
        assert_eq!(f.median_migration_secs, Some(70.0));
        let (sol, share) = f.dev_buy_mode.unwrap();
        assert!((sol - 8.06).abs() < 0.011 && share == 1.0);
        // first non-creator buy — F1 for 1 SOL (BOT is skipped as a common bot)
        assert_eq!(f.first_buy_mode, Some((1.0, 1.0)));
        assert_eq!(f.external_wallets, 5, "4 externals + the common bot");
        assert!((f.external_net_in_sol() - (4.0 * 0.2 + 4.0 * 0.01)).abs() < 1e-9);
        assert!(f.median_linked_share.unwrap() > 0.9);
    }

    /// The operator's farm is spread over two configs: per config the wallets are not specific
    /// (half of their activity is in the other config), per cluster they are.
    /// Instant-graduation pools (creator buy only) are not included in the linked volume share.
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
        // config M: in each of 3 pools the master MASTER pays for 5 trades of 4 fresh proxies
        for k in 0..3 {
            let p = format!("M{k}");
            conn.execute("INSERT INTO pools VALUES(?1,'M','CM',1000,100,1)", params![p]).unwrap();
            for j in 0..5 {
                sw(&p, &format!("PROXY{k}_{}", j % 4), "MASTER", 1_000_000_000);
            }
            sw(&p, &format!("EXT{k}"), &format!("EXT{k}"), 10_000_000);
        }
        // config R: relayer RELAY signs for 5 different users, each pays for themselves
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
        // external wallet EXTk bought 0.01 SOL in each of the 3 pools and sold nothing
        assert_eq!(m.external_net_per_pool.len(), 3);
        assert!(m.external_net_per_pool.iter().all(|x| (x - 0.01).abs() < 1e-9));
        // the master bought 5 SOL in each pool and sold nothing: the operator put money in, not took it out
        assert!(m.operator_net_per_pool.iter().all(|(x, grad, _)| (x + 5.0).abs() < 1e-9 && !grad));
        let r = analyze_config(&conn, "R", &idx, 0).unwrap();
        assert_eq!(r.master_wallets, 0, "a relayer signing for many users is not a master");
    }

    #[test]
    fn fanout_by_a_sniper_is_not_the_operators() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE pools(pool, config, creator, created_time, created_slot, tracked);
             CREATE TABLE swaps(pool, fee_payer, trade_direction, included_fee_input_amount, output_amount, slot, event_index);
             CREATE TABLE curve_complete(pool, block_time);",
        )
        .unwrap();
        let sw = |pool: &str, w: &str, buy: bool, i: i64, o: i64| {
            conn.execute("INSERT INTO swaps VALUES(?1,?2,?3,?4,?5,140,0)", params![pool, w, if buy { 1 } else { 0 }, i, o]).unwrap();
        };
        for k in 0..3 {
            let p = format!("S{k}");
            conn.execute("INSERT INTO pools VALUES(?1,'SN','DEV',1000,100,1)", params![p]).unwrap();
            sw(&p, "DEV", true, 300_000_000, 10_000); // the creator bought 10,000 tokens
            sw(&p, &format!("SNIPER{k}"), true, 15_000_000_000, 300_000); // the sniper bought 300,000
            for j in 0..5 {
                // the sniper's wallets sell 50,000 tokens each that they never bought in the pool
                sw(&p, &format!("SUB{k}_{j}"), false, 50_000, 1_000_000_000);
            }
            sw(&p, &format!("EXT{k}"), true, 100_000_000, 2_000);
        }
        let idx = wallet_index(&conn).unwrap();
        let f = analyze_config(&conn, "SN", &idx, 0).unwrap();
        assert_eq!(f.unattributed_fanout_pools, 3);
        assert_eq!(f.median_fanout, Some(0.0), "a sniper's own fan-out is not linked to the operator");
        assert!(f.median_linked_share.unwrap_or(0.0) < 0.1);
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
        // config A: 4 pools; FARM buys 1 SOL each, the common BOT 0.01 SOL each
        for k in 0..4 {
            let p = format!("A{k}");
            conn.execute("INSERT INTO pools VALUES(?1,'A','HUB',1000,100,1)", params![p]).unwrap();
            sw(&p, "FARM", 1_000_000_000);
            sw(&p, "BOT", 10_000_000);
        }
        // 10 unrelated pools: FARM trades dust as camouflage, BOT trades normal amounts
        for k in 0..10 {
            let p = format!("Z{k}");
            conn.execute("INSERT INTO pools VALUES(?1,'Z','OTHER',1000,100,1)", params![p]).unwrap();
            sw(&p, "FARM", 100_000);
            sw(&p, "BOT", 10_000_000);
        }
        let idx = wallet_index(&conn).unwrap();
        let f = analyze_config(&conn, "A", &idx, 0).unwrap();
        // FARM: by pools 4/14 — not specific, by volume 4 SOL out of 4.001 — specific
        assert_eq!(f.farm_wallets, 1, "FARM is the farm, BOT is a common bot");
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
        // config A: 3 pools, FIRST buys first for 8.2125 SOL, F2 trades in every one
        for k in 0..3 {
            let p = format!("A{k}");
            conn.execute("INSERT INTO pools VALUES(?1,'A','HUB',1000,100,1)", params![p]).unwrap();
            sw(&p, "FIRST", 8_212_500_000, 101);
            sw(&p, "F2", 1_000_000_000, 140);
        }
        // config B of the same cluster: 10 pools with one-off external buyers
        for k in 0..10 {
            let p = format!("B{k}");
            conn.execute("INSERT INTO pools VALUES(?1,'B','HUB',1000,100,1)", params![p]).unwrap();
            sw(&p, &format!("EXT{k}"), 100_000_000, 140);
        }
        let idx = wallet_index(&conn).unwrap();
        let scope = vec!["A".to_string(), "B".to_string()];
        // the cluster has 13 pools: by cluster share (30% = 4 pools) FIRST and F2 do not pass, by A's share they do
        let f = analyze_scope(&conn, &scope, &["A".to_string()], &idx, 0).unwrap();
        assert_eq!(f.farm_wallets, 2);
        assert_eq!(f.first_buyers, 1);
        assert_eq!(f.first_buy_mode, Some((8.21, 1.0)));
        // no farm appears for config B
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
        // config C: instant graduation, creator buy only
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
