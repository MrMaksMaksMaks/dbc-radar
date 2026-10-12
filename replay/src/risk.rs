//! Risk scoring of a DBC config.
//!
//! Two groups of signals:
//!   1. From the config account itself — known BEFORE the first buy:
//!      who gets the liquidity after migration and in what form, how many tokens
//!      go to the leftover receiver, the migration fee, token authorities.
//!   2. From the behaviour of this config's pools in the collector database:
//!      how many creators use it, the share of the creator's opening buy,
//!      wallets that sell tokens they did not buy in the pool.
//!
//! Points are a heuristic, not a verdict. Every flag is printed with an explanation,
//! so the reader can see what the score is made of.

use anyhow::Result;
use dynamic_bonding_curve::state::{LiquidityVestingInfo, PoolConfig};
use rusqlite::{params, Connection};

#[derive(Debug, Clone)]
pub struct LpSplit {
    /// % of liquidity that can be withdrawn right after migration
    pub creator_unlocked: u8,
    pub partner_unlocked: u8,
    /// % permanently locked
    pub locked: u8,
    /// % with vesting
    pub creator_vest: u8,
    pub partner_vest: u8,
    /// how many seconds after migration the vesting is fully unlocked (the max of the two)
    pub vest_full_release_secs: u64,
}

#[derive(Debug, Clone)]
pub struct ConfigFacts {
    pub lp: LpSplit,
    pub total_supply: u64,
    pub curve_supply: u64,
    pub leftover_to_receiver: u64,
    pub update_authority: u8,
    pub migration_fee_pct: u8,
    pub has_fee_scheduler: bool,
    pub threshold: u64,
    pub token_decimal: u8,
    /// transfer hook program (for ConfigWithTransferHook configs)
    pub transfer_hook: Option<String>,
    /// migration threshold ~0 with quote = SOL: there is no curve as a market, the token goes straight to DAMM v2
    pub instant_graduation: bool,
    /// base trading fee: (mode 0 linear / 1 exponential / 2 rate limiter,
    /// starting numerator out of 1e9, number of periods, period length, reduction per period)
    pub base_fee: (u8, u64, u16, u64, u64),
    /// 0 — periods in slots, 1 — in seconds
    pub activation_type: u8,
    /// creator's share of the trading fee after the protocol share (the rest goes to the partner)
    pub creator_trading_fee_pct: u8,
    pub fee_claimer: Option<String>,
    pub leftover_receiver: Option<String>,
    /// in how many configs of the database this address is the leftover receiver (>= PLATFORM_MIN_CONFIGS: a platform address)
    pub leftover_receiver_configs: usize,
}

/// An address receiving the leftover in this many configs is treated as a platform service address.
pub const PLATFORM_MIN_CONFIGS: usize = 10;
/// Migration threshold below 0.1 SOL — "instant graduation".
pub const INSTANT_THRESHOLD_LAMPORTS: u64 = 100_000_000;
const WSOL: &str = "So11111111111111111111111111111111111111112";

fn vest_release(v: &LiquidityVestingInfo) -> u64 {
    if v.is_initialized == 0 || v.vesting_percentage == 0 {
        return 0;
    }
    v.cliff_duration_from_migration_time as u64 + v.number_of_periods as u64 * v.frequency as u64
}

pub fn config_facts(c: &PoolConfig) -> ConfigFacts {
    let pv = c.partner_liquidity_vesting_info.vesting_percentage;
    let cv = c.creator_liquidity_vesting_info.vesting_percentage;
    // As in the program's get_liquidity_distribution: the creator's share is the remainder.
    let creator_unlocked = 100u8
        .saturating_sub(c.partner_liquidity_percentage)
        .saturating_sub(c.partner_permanent_locked_liquidity_percentage)
        .saturating_sub(pv)
        .saturating_sub(c.creator_permanent_locked_liquidity_percentage)
        .saturating_sub(cv);
    let lp = LpSplit {
        creator_unlocked,
        partner_unlocked: c.partner_liquidity_percentage,
        locked: c.partner_permanent_locked_liquidity_percentage + c.creator_permanent_locked_liquidity_percentage,
        creator_vest: cv,
        partner_vest: pv,
        vest_full_release_secs: vest_release(&c.partner_liquidity_vesting_info)
            .max(vest_release(&c.creator_liquidity_vesting_info)),
    };

    let lv = &c.locked_vesting_config;
    let locked_tokens = lv
        .cliff_unlock_amount
        .saturating_add(lv.amount_per_period.saturating_mul(lv.number_of_period));
    let used = c.swap_base_amount.saturating_add(c.migration_base_threshold).saturating_add(locked_tokens);

    // Fixed supply: after migration, the leftover beyond post_migration_token_supply is burned,
    // and what remains within the post supply goes to the leftover_receiver.
    // Non-fixed supply: the whole leftover is burned.
    let (total_supply, leftover_to_receiver) = if c.fixed_token_supply_flag == 1 {
        (c.post_migration_token_supply, c.post_migration_token_supply.saturating_sub(used))
    } else {
        (c.get_initial_base_supply().unwrap_or(used), 0)
    };

    let bf = &c.pool_fees.base_fee;
    ConfigFacts {
        lp,
        total_supply,
        curve_supply: c.swap_base_amount,
        leftover_to_receiver,
        update_authority: c.token_update_authority,
        migration_fee_pct: c.migration_fee_percentage,
        has_fee_scheduler: bf.base_fee_mode <= 1 && bf.first_factor > 0 && bf.second_factor > 0,
        base_fee: (bf.base_fee_mode, bf.cliff_fee_numerator, bf.first_factor, bf.second_factor, bf.third_factor),
        activation_type: c.activation_type,
        creator_trading_fee_pct: c.creator_trading_fee_percentage,
        threshold: c.migration_quote_threshold,
        token_decimal: c.token_decimal,
        transfer_hook: None,
        instant_graduation: c.quote_mint.to_string() == WSOL && c.migration_quote_threshold < INSTANT_THRESHOLD_LAMPORTS,
        fee_claimer: Some(c.fee_claimer.to_string()),
        leftover_receiver: Some(c.leftover_receiver.to_string()),
        leftover_receiver_configs: 1,
    }
}

#[derive(Debug, Clone, Default)]
pub struct Behavior {
    pub pools: u64,
    pub creators: u64,
    pub top_creator: Option<String>,
    pub top_creator_pools: u64,
    pub tracked: u64,
    pub graduated: u64,
    /// median of the creator's opening buy as % of the migration threshold (over tracked pools)
    pub median_prebuy_pct: Option<f64>,
    /// median number of wallets that sold tokens they did not buy in the pool
    pub median_outside_wallets: Option<f64>,
    /// median SOL received by such wallets
    pub median_outside_sol: Option<f64>,
    /// farm wallets and how synthetic the activity is (signals 4, 7–11)
    pub farm: Option<crate::farm::FarmStats>,
}

fn median(mut v: Vec<f64>) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = v.len();
    Some(if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 })
}

pub fn behavior(conn: &Connection, config: &str, threshold: u64) -> Result<Behavior> {
    let mut b = Behavior::default();
    let (pools, creators): (i64, i64) = conn.query_row(
        "SELECT COUNT(*), COUNT(DISTINCT creator) FROM pools WHERE config = ?1",
        params![config],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    b.pools = pools as u64;
    b.creators = creators as u64;
    if let Ok((c, n)) = conn.query_row(
        "SELECT creator, COUNT(*) n FROM pools WHERE config = ?1 GROUP BY creator ORDER BY n DESC LIMIT 1",
        params![config],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
    ) {
        b.top_creator = Some(c);
        b.top_creator_pools = n as u64;
    }

    let mut st = conn.prepare(
        "SELECT p.pool,
           (SELECT COALESCE(SUM(s.included_fee_input_amount), 0) FROM swaps s
             WHERE s.pool = p.pool AND s.trade_direction = 1
               AND (s.fee_payer = p.creator OR s.signature = p.created_sig)
               AND s.slot <= p.created_slot + 2) AS prebuy,
           EXISTS(SELECT 1 FROM curve_complete c WHERE c.pool = p.pool) AS grad,
           (SELECT COUNT(*) FROM swaps s WHERE s.pool = p.pool) AS n_swaps
         FROM pools p WHERE p.config = ?1 AND p.tracked = 1",
    )?;
    let rows: Vec<(String, i64, bool, i64)> = st
        .query_map(params![config], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<std::result::Result<_, _>>()?;

    let mut prebuy = Vec::new();
    let mut ow = Vec::new();
    let mut osol = Vec::new();
    let mut outside = conn.prepare(
        "SELECT COUNT(DISTINCT s.fee_payer), COALESCE(SUM(s.output_amount), 0), COALESCE(SUM(s.included_fee_input_amount), 0)
         FROM swaps s JOIN pools p ON p.pool = s.pool
         WHERE s.pool = ?1 AND s.trade_direction = 0 AND s.fee_payer <> p.creator
           AND s.fee_payer NOT IN (SELECT fee_payer FROM swaps b WHERE b.pool = ?1 AND b.trade_direction = 1)",
    )?;
    // how many tokens the creator bought: it cannot distribute more than that on the curve
    let mut creator_bought = conn.prepare(
        "SELECT COALESCE(SUM(s.output_amount), 0) FROM swaps s JOIN pools p ON p.pool = s.pool
         WHERE s.pool = ?1 AND s.trade_direction = 1 AND s.fee_payer = p.creator",
    )?;
    for (pool, pb, grad, n_swaps) in rows {
        if n_swaps == 0 {
            continue; // the pool has not traded yet or the data is not collected
        }
        b.tracked += 1;
        if grad {
            b.graduated += 1;
        }
        if threshold > 0 {
            prebuy.push(100.0 * pb as f64 / threshold as f64);
        }
        let (w, sol, tokens): (i64, i64, i64) = outside.query_row(params![pool], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        let bought: i64 = creator_bought.query_row(params![pool], |r| r.get(0))?;
        // sellers without a buy sold more than the creator bought: the distribution is not from the operator
        // (e.g. a sniper distributed its purchase to its own wallets) — we do not count it as a signal
        if (tokens as i128) * 100 > (bought as i128) * 105 {
            ow.push(0.0);
            osol.push(0.0);
        } else {
            ow.push(w as f64);
            osol.push(sol as f64 / 1e9);
        }
    }
    b.median_prebuy_pct = median(prebuy);
    b.median_outside_wallets = median(ow);
    b.median_outside_sol = median(osol);
    Ok(b)
}

#[derive(Debug, Clone)]
pub struct Flag {
    pub points: u32,
    pub text: String,
}

/// Result for a config: two axes and a verdict.
#[derive(Debug, Clone)]
pub struct Report {
    /// What the config ALLOWS (config account only), 0..100
    pub capability: u32,
    pub capability_flags: Vec<Flag>,
    /// What was ACTUALLY observed in this config's pools, 0..100; None — no tracked pools
    pub evidence: Option<u32>,
    pub evidence_flags: Vec<Flag>,
    pub verdict: Verdict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verdict {
    /// The activity in the pools is created by the creator itself and wallets linked to it
    Synthetic,
    /// The config itself is clean by observations, but linked by shared addresses to RED configs
    RedLinked,
    /// The config allows withdrawing the liquidity and dumping a large supply leftover
    RugCapable,
    /// The creator goes through the whole curve itself: there are no buyers on the curve
    SelfGraduation,
    Standard,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Synthetic => "RED",
            Verdict::RedLinked => "RED-LINK",
            Verdict::RugCapable => "AMBER",
            Verdict::SelfGraduation => "SELF-GRAD",
            Verdict::Standard => "GREEN",
        }
    }
    pub fn describe(self) -> &'static str {
        match self {
            Verdict::Synthetic => "synthetic launches: trading is dominated by the creator and wallets linked to it",
            Verdict::RedLinked => "shares operator addresses with configs where synthetic launches were observed",
            Verdict::RugCapable => "risky config or inconclusive suspicious trading: the creator or launchpad can take most of the buyers' SOL or liquidity during trading or at/after migration",
            Verdict::SelfGraduation => "instant / self-funded graduation: no real bonding-curve market; risk moves to the post-migration DAMM v2 pool",
            Verdict::Standard => "no major red flags in config or observed behaviour",
        }
    }
}

fn human_secs(s: u64) -> String {
    if s >= 86_400 {
        format!("{:.1} days", s as f64 / 86_400.0)
    } else if s >= 3_600 {
        format!("{:.1} h", s as f64 / 3_600.0)
    } else {
        format!("{s} s")
    }
}

/// Weights calibrated on 167 real configs (October 2, 2026):
///   * unlocked LP on its own is normal (the Meteora Invent template: partner 50% + creator 40% unlocked);
///   * 100% permanently locked LP is the most common setup (100 of 167 configs), no selling "from outside" observed in it;
///   * the combination "creator unlocked >= 50% + leftover >= 20% of supply" — 51 configs, 78% of observed pools;
///     selling of tokens "from outside" is seen in 13 of 32 such configs with data and in none of the other 27.
/// "Sniper fee" parameters, if the config qualifies.
pub struct FeeTrap {
    pub start_pct: f64,
    pub secs: u64,
    pub share_pct: f64,
    pub receiver: &'static str,
}

/// Verdict thresholds. The defaults are the values the engine was validated with on the data;
/// any of them can be overridden with an environment variable (or a line in ./.env), e.g.
/// RISK_FEE_TRAP_MIN_START_PCT=60. An invalid value — a warning to stderr and the default value.
/// `dbc-replay <db> thresholds` prints the effective values.
#[derive(Debug, Clone)]
pub struct Thresholds {
    pub red_min_evidence: u32,
    pub amber_min_capability: u32,
    pub strong_linked_share: f64,
    pub strong_fanout_wallets: f64,
    pub fast_migration_secs: f64,
    pub fee_trap_min_start_pct: f64,
    pub fee_trap_min_secs: u64,
    pub fee_trap_min_share_pct: f64,
    pub fee_trap_points: u32,
    pub template_points: u32,
    pub platform_min_configs: usize,
    pub platform_min_creators: u64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Thresholds {
            red_min_evidence: 50,
            amber_min_capability: 50,
            strong_linked_share: 0.8,
            strong_fanout_wallets: 10.0,
            fast_migration_secs: 300.0,
            fee_trap_min_start_pct: 70.0,
            fee_trap_min_secs: 90,
            fee_trap_min_share_pct: 80.0,
            fee_trap_points: 50,
            template_points: 0,
            platform_min_configs: 5,
            platform_min_creators: 10,
        }
    }
}

/// (variable, value, default, what it sets)
pub type ThresholdRow = (&'static str, String, String, &'static str);

impl Thresholds {
    pub fn from_env() -> Self {
        let file = std::fs::read_to_string(".env").unwrap_or_default();
        let get = |k: &str| -> Option<String> {
            std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty()).or_else(|| {
                file.lines()
                    .find_map(|l| l.trim().strip_prefix(k).and_then(|r| r.strip_prefix('=')))
                    .map(|v| v.trim().trim_matches('"').to_string())
                    .filter(|v| !v.is_empty())
            })
        };
        fn parse<T: std::str::FromStr + Copy>(v: Option<String>, k: &str, d: T) -> T {
            match v {
                None => d,
                Some(v) => v.parse().unwrap_or_else(|_| {
                    eprintln!("warning: {k}={v} is not a valid number, using the default");
                    d
                }),
            }
        }
        let d = Self::default();
        let mut t = Self::default();
        macro_rules! set {
            ($field:ident, $key:literal) => {
                t.$field = parse(get($key), $key, d.$field);
            };
        }
        set!(red_min_evidence, "RISK_RED_MIN_EVIDENCE");
        set!(amber_min_capability, "RISK_AMBER_MIN_CAPABILITY");
        set!(strong_linked_share, "RISK_STRONG_LINKED_SHARE");
        set!(strong_fanout_wallets, "RISK_STRONG_FANOUT_WALLETS");
        set!(fast_migration_secs, "RISK_FAST_MIGRATION_SECS");
        set!(fee_trap_min_start_pct, "RISK_FEE_TRAP_MIN_START_PCT");
        set!(fee_trap_min_secs, "RISK_FEE_TRAP_MIN_SECS");
        set!(fee_trap_min_share_pct, "RISK_FEE_TRAP_MIN_SHARE_PCT");
        set!(fee_trap_points, "RISK_FEE_TRAP_POINTS");
        set!(template_points, "RISK_TEMPLATE_POINTS");
        set!(platform_min_configs, "RISK_PLATFORM_GUARD_MIN_CONFIGS");
        set!(platform_min_creators, "RISK_PLATFORM_GUARD_MIN_CREATORS");
        t
    }

    pub fn rows(&self) -> Vec<ThresholdRow> {
        let d = Self::default();
        vec![
            ("RISK_RED_MIN_EVIDENCE", self.red_min_evidence.to_string(), d.red_min_evidence.to_string(),
             "evidence points needed for RED (plus a strong trading signal)"),
            ("RISK_AMBER_MIN_CAPABILITY", self.amber_min_capability.to_string(), d.amber_min_capability.to_string(),
             "capability points needed for AMBER"),
            ("RISK_STRONG_LINKED_SHARE", self.strong_linked_share.to_string(), d.strong_linked_share.to_string(),
             "share of volume from creator-linked wallets that counts as a strong trading signal"),
            ("RISK_STRONG_FANOUT_WALLETS", self.strong_fanout_wallets.to_string(), d.strong_fanout_wallets.to_string(),
             "median wallets per pool selling tokens they never bought: strong fan-out signal"),
            ("RISK_FAST_MIGRATION_SECS", self.fast_migration_secs.to_string(), d.fast_migration_secs.to_string(),
             "curve filled faster than this (median, s) is flagged"),
            ("RISK_FEE_TRAP_MIN_START_PCT", self.fee_trap_min_start_pct.to_string(), d.fee_trap_min_start_pct.to_string(),
             "sniper fee: minimum starting trading fee, %"),
            ("RISK_FEE_TRAP_MIN_SECS", self.fee_trap_min_secs.to_string(), d.fee_trap_min_secs.to_string(),
             "sniper fee: minimum fee decay duration, s"),
            ("RISK_FEE_TRAP_MIN_SHARE_PCT", self.fee_trap_min_share_pct.to_string(), d.fee_trap_min_share_pct.to_string(),
             "sniper fee: minimum share of the fee taken by one address, %"),
            ("RISK_FEE_TRAP_POINTS", self.fee_trap_points.to_string(), d.fee_trap_points.to_string(),
             "capability points for the sniper fee (>= AMBER threshold makes it AMBER on its own)"),
            ("RISK_TEMPLATE_POINTS", self.template_points.to_string(), d.template_points.to_string(),
             "evidence points for sharing a parameter template with RED configs (0 = note only)"),
            ("RISK_PLATFORM_GUARD_MIN_CONFIGS", self.platform_min_configs.to_string(), d.platform_min_configs.to_string(),
             "platform guard: launchpad with at least this many configs, under half of them RED"),
            ("RISK_PLATFORM_GUARD_MIN_CREATORS", self.platform_min_creators.to_string(), d.platform_min_creators.to_string(),
             "platform guard: config with at least this many creators needs direct evidence for RED"),
        ]
    }
}

static THRESHOLDS: std::sync::OnceLock<Thresholds> = std::sync::OnceLock::new();

/// Effective thresholds (read once per run).
pub fn thresholds() -> &'static Thresholds {
    THRESHOLDS.get_or_init(Thresholds::from_env)
}

/// Platform config guard: a launchpad config (the partner has many configs, synthetic trading in fewer than
/// half) used by many creators gets RED only with direct evidence —
/// mass fan-out or trading through proxy wallets. Otherwise AMBER with an explanation:
/// the signals stay visible, but the verdict does not read as an accusation of the platform.
/// Returns true if the verdict was downgraded.
pub fn platform_guard(rep: &mut Report, b: &Behavior, partner_configs: usize, partner_synthetic: usize) -> bool {
    let th = thresholds();
    if rep.verdict != Verdict::Synthetic
        || partner_configs < th.platform_min_configs
        || partner_synthetic * 2 >= partner_configs
        || b.creators < th.platform_min_creators
    {
        return false;
    }
    let fanout = b.median_outside_wallets.unwrap_or(0.0) >= th.strong_fanout_wallets;
    let proxies = b.farm.as_ref().is_some_and(|f| f.master_wallets >= 1);
    if fanout || proxies {
        return false;
    }
    rep.evidence_flags.push(Flag {
        points: 0,
        text: format!(
            "config of a launchpad with {partner_configs} configs (synthetic trading in {partner_synthetic}) used by {} creators: \
             here RED needs direct evidence (mass fan-out or trading through proxy wallets), so the signals above are a warning",
            b.creators
        ),
    });
    rep.verdict = Verdict::RugCapable;
    true
}

pub fn fee_trap(f: &ConfigFacts) -> Option<FeeTrap> {
    let (mode, cliff, periods, period_len, _) = f.base_fee;
    if mode > 1 || periods == 0 || period_len == 0 {
        return None; // rate limiter or flat fee
    }
    let start_pct = cliff as f64 / 1e7;
    // activation_type 0: periods in slots (~0.4 s)
    let raw = periods as u64 * period_len;
    let secs = if f.activation_type == 0 { raw * 2 / 5 } else { raw };
    let cr = f.creator_trading_fee_pct as f64;
    let (receiver, share_pct) = if cr >= 50.0 { ("the creator", cr) } else { ("the launchpad (fee claimer)", 100.0 - cr) };
    let th = thresholds();
    (start_pct >= th.fee_trap_min_start_pct && secs >= th.fee_trap_min_secs && share_pct >= th.fee_trap_min_share_pct)
        .then_some(FeeTrap { start_pct, secs, share_pct, receiver })
}

pub fn score(f: &ConfigFacts, b: &Behavior) -> Report {
    let th = thresholds();
    let lp = &f.lp;
    let leftover_pct = if f.total_supply > 0 { 100.0 * f.leftover_to_receiver as f64 / f.total_supply as f64 } else { 0.0 };

    // --- Config capabilities ---
    let mut cap = Vec::new();
    let add = |v: &mut Vec<Flag>, points: u32, text: String| v.push(Flag { points, text });
    let creator_heavy = lp.creator_unlocked >= 50;
    let big_leftover = leftover_pct >= 20.0;
    let platform_custody = f.leftover_receiver_configs >= PLATFORM_MIN_CONFIGS;
    if creator_heavy && big_leftover && !platform_custody {
        add(&mut cap, 55, format!(
            "creator gets {}% of post-migration liquidity unlocked AND {:.0}% of supply goes to the leftover receiver: \
             liquidity can be pulled and leftover dumped right after migration",
            lp.creator_unlocked, leftover_pct
        ));
    } else {
        if creator_heavy {
            add(&mut cap, 20, format!("creator gets {}% of post-migration liquidity unlocked", lp.creator_unlocked));
        }
        if big_leftover && platform_custody {
            add(&mut cap, 5, format!(
                "{leftover_pct:.0}% of supply goes to platform address {} (leftover receiver in {} configs): \
                 safety depends on the platform's rules for these tokens",
                f.leftover_receiver.as_deref().unwrap_or("?"),
                f.leftover_receiver_configs
            ));
        } else if big_leftover {
            add(&mut cap, 20, format!("{leftover_pct:.0}% of supply goes to the leftover receiver after migration"));
        }
    }
    if f.instant_graduation {
        add(&mut cap, 0, format!(
            "migration threshold is ~0 ({:.4} SOL): no bonding-curve market, the token moves to DAMM v2 at once; \
             the liquidity and leftover risks above apply to buyers in that pool",
            f.threshold as f64 / 1e9
        ));
    }
    if lp.partner_unlocked >= 40 {
        add(&mut cap, 10, format!("launchpad (partner) gets {}% of post-migration liquidity unlocked", lp.partner_unlocked));
    }
    let vest = lp.creator_vest + lp.partner_vest;
    if vest > 0 && vest + lp.locked < 50 && lp.vest_full_release_secs <= 7 * 86_400 {
        add(&mut cap, 5, format!("vested {}% of liquidity fully unlocks {} after migration", vest, human_secs(lp.vest_full_release_secs)));
    }
    // The migration fee takes this share of the SOL collected on the curve: only the remainder goes
    // to the post-migration pool (PoolConfig::get_migration_quote_amount). At 50%+ buyers lose
    // most of the liquidity at the moment of graduation — this alone is a risky config.
    let fee = f.migration_fee_pct;
    if fee >= 10 {
        let points = match fee {
            50.. => 50,
            25..=49 => 25,
            _ => 10,
        };
        add(
            &mut cap,
            points,
            format!(
                "migration fee takes {fee}% of the SOL raised on the curve: only {}% goes into the post-migration pool",
                100 - fee as u32
            ),
        );
    }
    if let Some(h) = &f.transfer_hook {
        add(&mut cap, 20, format!(
            "token has a transfer hook ({h}): custom code runs on every transfer and could restrict selling; \
             legitimate for compliance/royalties, so check what the program does"
        ));
    }
    if matches!(f.update_authority, 3 | 4) {
        add(&mut cap, 40, "a creator or partner keeps MINT authority over the token".into());
    }
    // "Sniper fee": a starting fee of 70%+ decays for longer than 90 s, and 80%+ of the fee
    // (after the protocol share) goes to one address. Early buyers hand most of their SOL
    // to this address as a fee. This is a config capability (visible in the parameters), not evidence.
    if let Some(t) = fee_trap(f) {
        add(&mut cap, th.fee_trap_points, format!(
            "sniper fee: trading fee starts at {:.0}% and decays over {}; {:.0}% of the fee (after the protocol share) goes to {}: \
             buyers in the first minutes pay most of their SOL as fee to one address",
            t.start_pct, human_secs(t.secs), t.share_pct, t.receiver
        ));
    }
    let capability = cap.iter().map(|f| f.points).sum::<u32>().min(100);

    // --- Observed behaviour ---
    let mut ev = Vec::new();
    let self_grad = f.instant_graduation
        || matches!((b.median_prebuy_pct, b.median_outside_wallets), (Some(p), Some(w)) if p >= 95.0 && w < 1.0);
    if let Some(w) = b.median_outside_wallets {
        if w >= th.strong_fanout_wallets {
            add(&mut ev, 30, format!(
                "fan-out: median {:.0} wallets per pool sold tokens they never bought in the pool ({:.2} SOL per pool)",
                w,
                b.median_outside_sol.unwrap_or(0.0)
            ));
        } else if w >= 3.0 {
            add(&mut ev, 10, format!("fan-out: median {w:.0} wallets per pool sold tokens they never bought in the pool"));
        }
    }
    if let Some(fs) = &b.farm {
        if let Some(sh) = fs.median_linked_share {
            let only_creator = fs.farm_wallets < 3 && fs.master_wallets == 0;
            if sh >= th.strong_linked_share && only_creator {
                // almost all volume is the creator's own trades: others barely trade
                // (an abandoned launch or selling to externals), this is not farm-produced volume
                add(&mut ev, 20, format!(
                    "{:.0}% of trading volume (median per pool) comes from the creator itself: few other traders",
                    sh * 100.0
                ));
            } else if sh >= th.strong_linked_share {
                let masters = if fs.master_wallets > 0 {
                    format!(" and {} wallet(s) trading through proxy signers", fs.master_wallets)
                } else {
                    String::new()
                };
                add(&mut ev, 40, format!(
                    "{:.0}% of trading volume (median per pool) comes from the creator, {} recurring linked wallets{}",
                    sh * 100.0,
                    fs.farm_wallets,
                    masters
                ));
            } else if sh >= 0.5 {
                add(&mut ev, 20, format!("{:.0}% of trading volume (median per pool) comes from the creator and linked wallets", sh * 100.0));
            }
        }
        if let Some(px) = fs.median_proxied_share.filter(|x| *x >= 0.3) {
            add(
                &mut ev,
                0,
                format!(
                    "{:.0}% of curve volume (median per pool) is paid by a wallet other than the one signing the trade: one wallet trades through several proxy wallets",
                    px * 100.0
                ),
            );
        }
        if fs.volume_only_wallets >= 5 {
            add(
                &mut ev,
                0,
                format!(
                    "{} linked wallets also trade small amounts in unrelated pools (possible decoy activity); they are still counted as linked by volume",
                    fs.volume_only_wallets
                ),
            );
        }
        if fs.farm_wallets >= 5 && fs.median_linked_share.unwrap_or(0.0) < 0.8 {
            add(&mut ev, 10, format!("{} wallets trade in most of this config's pools and almost nowhere else", fs.farm_wallets));
        }
        if fs.first_buyers > 0 {
            add(&mut ev, 15, format!("{} wallet(s) buy in the first seconds of most pools of this config", fs.first_buyers));
        }
        // scripted start: the same first buy by the creator or, if the operator starts
        // from a farm wallet, the same first non-creator buy; points are given once
        if let Some((sol, share)) = fs.dev_buy_mode.filter(|(_, s)| *s >= 0.8) {
            add(
                &mut ev,
                15,
                format!("creator's opening buy is the same {} SOL in {:.0}% of pools", crate::farm::fmt_sol(sol), share * 100.0),
            );
        } else if let Some((sol, share)) = fs.first_buy_mode.filter(|(_, s)| *s >= 0.8) {
            add(
                &mut ev,
                15,
                format!(
                    "the first buy after launch (not by the creator) is the same {} SOL in {:.0}% of pools",
                    crate::farm::fmt_sol(sol),
                    share * 100.0
                ),
            );
        }
        if let Some(m) = fs.median_migration_secs {
            if m < th.fast_migration_secs && !f.instant_graduation {
                add(
                    &mut ev,
                    10,
                    format!("the curve fills a median {m:.0} s after launch: no time for an open market, early buyers are bots or a bundle"),
                );
            }
        }
    }
    if b.creators == 1 && b.pools >= 5 {
        add(&mut ev, 20, format!("config used by a single creator ({} launches): launchpad and creator are likely the same party", b.pools));
    }
    if let Some(p) = b.median_prebuy_pct {
        if !self_grad && p >= 50.0 {
            add(&mut ev, 10, format!("creator's opening buy is {p:.0}% of the migration threshold (median)"));
        }
    }
    let evidence = if b.tracked > 0 { Some(ev.iter().map(|f| f.points).sum::<u32>().min(100)) } else { None };

    // RED needs strong trading evidence: at least 80% of the volume from linked wallets
    // or mass fan-out (10+ sellers without buys per pool). A sum of weak signals (about
    // half of the volume linked, a single creator, the same first buy) does not give RED:
    // the verdict is then set by the config, and the signals remain visible in the report.
    // 80% linked volume counts only if more than just the creator is linked:
    // 3+ farm wallets or a master wallet with proxies (otherwise it is the creator's own volume).
    let strong_trading = b.median_outside_wallets.unwrap_or(0.0) >= th.strong_fanout_wallets
        || b.farm.as_ref().is_some_and(|fs| {
            fs.median_linked_share.unwrap_or(0.0) >= th.strong_linked_share && (fs.farm_wallets >= 3 || fs.master_wallets >= 1)
        });
    let verdict = if evidence.unwrap_or(0) >= th.red_min_evidence && strong_trading {
        Verdict::Synthetic
    } else if self_grad {
        Verdict::SelfGraduation
    } else if capability >= th.amber_min_capability {
        Verdict::RugCapable
    } else {
        Verdict::Standard
    };

    Report { capability, capability_flags: cap, evidence, evidence_flags: ev, verdict }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain_facts() -> ConfigFacts {
        ConfigFacts {
            lp: LpSplit { creator_unlocked: 0, partner_unlocked: 0, locked: 100, creator_vest: 0, partner_vest: 0, vest_full_release_secs: 0 },
            total_supply: 1_000_000_000,
            curve_supply: 800_000_000,
            leftover_to_receiver: 0,
            update_authority: 1,
            migration_fee_pct: 0,
            has_fee_scheduler: true,
            threshold: 85_000_000_000,
            token_decimal: 6,
            transfer_hook: None,
            instant_graduation: false,
            base_fee: (0, 0, 0, 0, 0),
            activation_type: 1,
            creator_trading_fee_pct: 0,
            fee_claimer: None,
            leftover_receiver: None,
            leftover_receiver_configs: 1,
        }
    }

    #[test]
    fn platform_guard_needs_direct_evidence() {
        let red = || Report {
            capability: 0,
            capability_flags: vec![],
            evidence: Some(55),
            evidence_flags: vec![],
            verdict: Verdict::Synthetic,
        };
        let farm = |masters| Some(crate::farm::FarmStats { master_wallets: masters, ..Default::default() });
        // launchpad config (7 configs, synthetic trading in 2), 30 creators, no fan-out and no proxies → AMBER
        let b = Behavior { pools: 62, creators: 30, median_outside_wallets: Some(1.0), farm: farm(0), ..Default::default() };
        let mut r = red();
        assert!(platform_guard(&mut r, &b, 7, 2));
        assert_eq!(r.verdict, Verdict::RugCapable);
        assert!(r.evidence_flags.iter().any(|f| f.points == 0 && f.text.contains("launchpad with 7 configs")));
        // mass fan-out is direct evidence, RED stays
        let mut r = red();
        assert!(!platform_guard(&mut r, &Behavior { median_outside_wallets: Some(27.0), ..b.clone() }, 7, 2));
        assert_eq!(r.verdict, Verdict::Synthetic);
        // trading through proxy wallets — too
        let mut r = red();
        assert!(!platform_guard(&mut r, &Behavior { farm: farm(1), ..b.clone() }, 7, 2));
        // few creators: the config effectively belongs to one operator
        let mut r = red();
        assert!(!platform_guard(&mut r, &Behavior { creators: 7, ..b.clone() }, 7, 2));
        // the partner has one config (bundle template) or synthetic trading in half of its configs: not a platform
        let mut r = red();
        assert!(!platform_guard(&mut r, &b, 1, 1));
        let mut r = red();
        assert!(!platform_guard(&mut r, &b, 6, 3));
        // not RED — leave it alone
        let mut r = Report { verdict: Verdict::Standard, ..red() };
        assert!(!platform_guard(&mut r, &b, 7, 2));
        assert_eq!(r.verdict, Verdict::Standard);
    }

    #[test]
    fn thresholds_default_and_list() {
        let t = Thresholds::default();
        assert_eq!((t.red_min_evidence, t.template_points, t.fee_trap_points), (50, 0, 50));
        assert!(t.rows().iter().all(|(k, v, d, _)| k.starts_with("RISK_") && v == d));
    }

    #[test]
    fn sniper_fee_is_amber_not_red() {
        let b = Behavior { pools: 20, creators: 15, ..Default::default() };
        // 90% -> decay over 120 periods of 1 s, 100% of the fee to the creator: a trap
        let mut f = plain_facts();
        f.base_fee = (0, 900_000_000, 120, 1, 7_400_000);
        f.creator_trading_fee_pct = 100;
        let r = score(&f, &b);
        assert_eq!(r.verdict, Verdict::RugCapable, "{:?}", r.capability_flags);
        assert!(r.capability_flags.iter().any(|x| x.text.starts_with("sniper fee") && x.text.contains("the creator")));
        // the same, but the fee is split 50/50: there is no single receiver
        f.creator_trading_fee_pct = 50;
        assert_eq!(score(&f, &b).verdict, Verdict::Standard);
        // a platform's regular anti-sniper fee: 50% for 60 s
        f.creator_trading_fee_pct = 0;
        f.base_fee = (1, 500_000_000, 60, 1, 1_000);
        assert_eq!(score(&f, &b).verdict, Verdict::Standard);
        // periods in slots: 300 slots ≈ 120 s, 80% to the partner — a trap
        f.base_fee = (0, 800_000_000, 30, 10, 1_000);
        f.activation_type = 0;
        f.creator_trading_fee_pct = 20;
        let t = fee_trap(&f).expect("trap");
        assert_eq!((t.secs, t.receiver), (120, "the launchpad (fee claimer)"));
    }

    /// A bundle-template config (89% of liquidity to the creator unlocked, 90% leftover) must get CRITICAL,
    /// and the same config with locked liquidity and no leftover — LOW.
    /// DBC_FIXTURE_CONFIG=/path/to/fee_in_quote_config.bin ; RISK_DB=/tmp/risk.sqlite — keep the test database.
    #[test]
    fn scores_rug_style_config_as_critical() {
        let Ok(path) = std::env::var("DBC_FIXTURE_CONFIG") else {
            eprintln!("DBC_FIXTURE_CONFIG not set, skipping");
            return;
        };
        let raw = std::fs::read(path).unwrap();
        let size = std::mem::size_of::<PoolConfig>();
        let mut c: PoolConfig = bytemuck::pod_read_unaligned(&raw[8..8 + size]);

        c.partner_liquidity_percentage = 0;
        c.partner_permanent_locked_liquidity_percentage = 0;
        c.creator_permanent_locked_liquidity_percentage = 0;
        c.creator_liquidity_percentage = 89;
        c.partner_liquidity_vesting_info.vesting_percentage = 0;
        c.creator_liquidity_vesting_info.is_initialized = 1;
        c.creator_liquidity_vesting_info.vesting_percentage = 11;
        c.creator_liquidity_vesting_info.cliff_duration_from_migration_time = 86_400;
        c.creator_liquidity_vesting_info.number_of_periods = 1;
        c.creator_liquidity_vesting_info.frequency = 1;
        c.fixed_token_supply_flag = 1;
        c.pre_migration_token_supply = 1_000_000_000_000_000;
        c.post_migration_token_supply = 1_000_000_000_000_000;
        c.swap_base_amount = 80_000_000_000_000;
        c.migration_base_threshold = 20_000_000_000_000;
        c.locked_vesting_config = Default::default();
        c.migration_quote_threshold = 11_510_000_000;
        c.token_update_authority = 1;

        let f = config_facts(&c);
        assert_eq!(f.lp.creator_unlocked, 89);
        assert_eq!(f.leftover_to_receiver, 900_000_000_000_000);

        // behaviour: 6 pools by one creator, opening buy 8.057 SOL, 20 satellite sellers
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE pools(pool, config, creator, base_mint, pool_type, activation_point, transfer_hook,
               created_sig, created_slot, created_time, discovered_at, tracked, last_sig, done, done_reason);
             CREATE TABLE swaps(signature, event_index, pool, config, slot, block_time, fee_payer, trade_direction,
               has_referral, swap_mode, amount_0, amount_1, included_fee_input_amount, excluded_fee_input_amount,
               amount_left, output_amount, next_sqrt_price, trading_fee, protocol_fee, referral_fee,
               quote_reserve_amount, migration_threshold, event_timestamp, transfer_hook);
             CREATE TABLE curve_complete(pool, config, signature, slot, block_time, base_reserve, quote_reserve);",
        )
        .unwrap();
        for p in 0..6 {
            let pool = format!("pool{p}");
            conn.execute(
                "INSERT INTO pools VALUES(?1,'cfg','creatorX','mint',0,100,0,?2,100,0,0,1,NULL,0,NULL)",
                params![pool, format!("create{p}")],
            )
            .unwrap();
            let ins = |sig: &str, payer: &str, dir: i64, input: i64, out: i64, slot: i64| {
                conn.execute(
                    "INSERT INTO swaps VALUES(?1,0,?2,'cfg',?3,0,?4,?5,0,0,0,0,?6,?6,0,?7,'0',0,0,0,0,0,0,0)",
                    params![sig, pool, slot, payer, dir, input, out],
                )
                .unwrap();
            };
            ins(&format!("create{p}"), "creatorX", 1, 8_057_000_000, 61_000_000_000_000, 100);
            for w in 0..20 {
                ins(&format!("s{p}-{w}"), &format!("sat{w}"), 0, 547_646_000_000, 150_000_000, 150);
            }
            ins(&format!("b{p}"), "retail", 1, 300_000_000, 700_000_000_000, 140);
            conn.execute("INSERT INTO curve_complete VALUES(?1,'cfg','x',200,0,0,0)", params![pool]).unwrap();
        }
        let b = behavior(&conn, "cfg", c.migration_quote_threshold).unwrap();
        assert_eq!((b.pools, b.creators, b.tracked, b.graduated), (6, 1, 6, 6));
        assert!((b.median_prebuy_pct.unwrap() - 70.0).abs() < 0.1);
        assert_eq!(b.median_outside_wallets, Some(20.0));

        let r = score(&f, &b);
        for fl in r.capability_flags.iter().chain(&r.evidence_flags) {
            eprintln!("+{} {}", fl.points, fl.text);
        }
        assert_eq!(r.verdict, Verdict::Synthetic);
        assert_eq!(r.capability, 60);

        // Weak signals (the same first buy, a single creator) without strong trading
        // evidence do not give RED, even if they add up to 50+ points.
        let weak = Behavior {
            pools: 7,
            creators: 1,
            tracked: 3,
            median_outside_wallets: Some(0.0),
            farm: Some(crate::farm::FarmStats {
                median_linked_share: Some(0.55),
                dev_buy_mode: Some((3.0, 1.0)),
                first_buyers: 1,
                ..Default::default()
            }),
            ..Default::default()
        };
        let rw = score(&f, &weak);
        // almost all volume is the creator's trades, no farms or masters: not RED
        let creator_only = Behavior {
            pools: 8,
            creators: 1,
            tracked: 1,
            median_outside_wallets: Some(0.0),
            farm: Some(crate::farm::FarmStats { median_linked_share: Some(0.99), ..Default::default() }),
            ..Default::default()
        };
        let rc = score(&f, &creator_only);
        assert_ne!(rc.verdict, Verdict::Synthetic, "creator's own trades in a pool nobody else trades are not synthetic volume");
        assert!(rc.evidence_flags.iter().any(|x| x.text.contains("creator itself")));
        assert!(rw.evidence.unwrap() >= 50, "weak signals add up: {:?}", rw.evidence);
        assert_eq!(rw.verdict, Verdict::RugCapable, "weak signals do not make RED; the risky config gives AMBER");

        // The same config without observations — capability only.
        assert_eq!(score(&f, &Behavior { pools: 3, creators: 3, ..Default::default() }).verdict, Verdict::RugCapable);

        // Default Meteora Invent template: partner 50% + creator 40% unlocked, 10% locked, leftover 0.
        let mut invent = c;
        invent.partner_liquidity_percentage = 50;
        invent.creator_liquidity_percentage = 40;
        invent.partner_permanent_locked_liquidity_percentage = 5;
        invent.creator_permanent_locked_liquidity_percentage = 5;
        invent.creator_liquidity_vesting_info.vesting_percentage = 0;
        invent.post_migration_token_supply = 100_000_000_000_000;
        let inv = score(&config_facts(&invent), &Behavior { pools: 40, creators: 35, ..Default::default() });
        assert_eq!(inv.verdict, Verdict::Standard, "{:?}", inv.capability_flags);

        // Honest setup: all liquidity permanently locked, no leftover, a shared config.
        let mut honest = c;
        honest.creator_liquidity_percentage = 0;
        honest.creator_permanent_locked_liquidity_percentage = 50;
        honest.partner_permanent_locked_liquidity_percentage = 50;
        honest.creator_liquidity_vesting_info.vesting_percentage = 0;
        honest.post_migration_token_supply = 100_000_000_000_000;
        let hf = config_facts(&honest);
        assert_eq!(hf.lp.creator_unlocked, 0);
        let h = score(&hf, &Behavior { pools: 40, creators: 35, ..Default::default() });
        assert_eq!((h.verdict, h.capability), (Verdict::Standard, 0));

        // Instant graduation is recognized from the config, without observations.
        let mut inst = c;
        inst.quote_mint = WSOL.parse().unwrap();
        inst.migration_quote_threshold = 1_000;
        let fi = config_facts(&inst);
        assert!(fi.instant_graduation);
        assert_eq!(score(&fi, &Behavior::default()).verdict, Verdict::SelfGraduation);

        // Leftover at a platform address (shared by many configs): a soft flag instead of +55.
        let mut plat = config_facts(&honest);
        plat.leftover_to_receiver = plat.total_supply * 85 / 100;
        plat.leftover_receiver_configs = 38;
        let pr = score(&plat, &Behavior { pools: 38, creators: 1, ..Default::default() });
        assert_eq!((pr.verdict, pr.capability), (Verdict::Standard, 5), "{:?}", pr.capability_flags);
    }
}

/// Base fee description for the report: starting rate, how and over what time it decays, final rate.
pub fn describe_base_fee(f: &ConfigFacts) -> String {
    let (mode, cliff, n, freq, red) = f.base_fee;
    let pct = |num: f64| num / 1e7; // numerator out of 1e9 -> percent
    if !f.has_fee_scheduler {
        return format!("flat {:.2}%", pct(cliff as f64));
    }
    let end = match mode {
        0 => (cliff as f64 - n as f64 * red as f64).max(0.0),
        _ => cliff as f64 * (1.0 - red as f64 / 10_000.0).powi(n as i32),
    };
    let unit = if f.activation_type == 1 { "s" } else { "slots" };
    format!(
        "{} fee scheduler: starts at {:.2}%, falls {} over {} periods of {} {} ({} {} in total), ends at {:.2}%",
        if mode == 0 { "linear" } else { "exponential" },
        pct(cliff as f64),
        if mode == 0 { format!("by {:.3} pp per period", pct(red as f64)) } else { format!("by {:.2}% per period", red as f64 / 100.0) },
        n,
        freq,
        unit,
        n as u64 * freq,
        unit,
        pct(end)
    )
}
