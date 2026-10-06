//! dbc-replay: проверочный и контрфактический повтор свопов одного пула.
//!
//! Проверка (симуляция против записанных EvtSwap2):
//!   dbc-replay ../dbc.sqlite 28VR
//!
//! Контрфакт (те же сделки при другом планировщике комиссий):
//!   dbc-replay ../dbc.sqlite 28VR --cf
//!   dbc-replay ../dbc.sqlite 28VR --cf-mode exp --cf-start 50 --cf-end 0.25 \
//!       --cf-periods 20 --cf-period-len 1 --early 10
//!
//! Флаги:
//!   --cf                 включить контрфакт со значениями по умолчанию
//!   --cf-mode exp|linear тип планировщика (по умолчанию exp)
//!   --cf-start <pct>     стартовая комиссия, % (по умолчанию 50)
//!   --cf-end <pct>       конечная комиссия, % (по умолчанию — минимальная комиссия исходного конфига)
//!   --cf-periods <n>     число периодов снижения (по умолчанию 20)
//!   --cf-period-len <n>  длина периода в слотах или секундах, как activation_type конфига (по умолчанию 1)
//!   --early <n>          "ранние" кошельки: первая сделка в пределах n слотов/секунд от активации (по умолчанию 10)
//!   --verbose            всегда печатать таблицу проверки

mod cf;
mod data;
mod engine;
mod risk;
mod cluster;
mod farm;

use anyhow::{anyhow, bail, Context, Result};
use dynamic_bonding_curve::{
    base_fee::get_base_fee_handler,
    params::swap::TradeDirection,
    state::{fee::FeeMode, PoolConfig, SwapResult2},
};
use std::collections::HashMap;

const WSOL: &str = "So11111111111111111111111111111111111111112";

fn short(s: &str) -> &str {
    &s[..s.len().min(8)]
}

struct Args {
    db: String,
    prefix: String,
    target: Option<String>,
    flags: HashMap<String, String>,
    switches: Vec<String>,
}

fn parse_args() -> Result<Args> {
    let usage = "usage: dbc-replay <dbc.sqlite> <pool prefix> [--cf ...]\n       dbc-replay <dbc.sqlite> risk [config prefix] [--json] [--limit N]\n       dbc-replay <dbc.sqlite> clusters [config prefix]\n       dbc-replay <dbc.sqlite> templates\n       dbc-replay <dbc.sqlite> impact [--since-hours N] [--sol-usd 122]";
    let mut flags = HashMap::new();
    let mut switches = Vec::new();
    let mut positional = Vec::new();
    let rest: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < rest.len() {
        let a = &rest[i];
        if !a.starts_with("--") {
            positional.push(a.clone());
            i += 1;
            continue;
        }
        let takes_value = !matches!(a.as_str(), "--cf" | "--verbose" | "--json");
        if takes_value {
            let v = rest.get(i + 1).with_context(|| format!("{a} needs a value"))?;
            flags.insert(a.clone(), v.clone());
            i += 2;
        } else {
            switches.push(a.clone());
            i += 1;
        }
    }
    if positional.len() < 2 || positional.len() > 3 {
        bail!("{usage}");
    }
    let mut it = positional.into_iter();
    Ok(Args {
        db: it.next().unwrap(),
        prefix: it.next().unwrap(),
        target: it.next(),
        flags,
        switches,
    })
}

impl Args {
    fn has(&self, s: &str) -> bool {
        self.switches.iter().any(|x| x == s)
    }
    fn wants_cf(&self) -> bool {
        self.has("--cf") || self.flags.keys().any(|k| k.starts_with("--cf"))
    }
    fn num<T: std::str::FromStr>(&self, key: &str, default: T) -> Result<T> {
        match self.flags.get(key) {
            Some(v) => v.parse().map_err(|_| anyhow!("bad value for {key}: {v}")),
            None => Ok(default),
        }
    }
}

fn min_fee_pct(c: &PoolConfig) -> Result<f64> {
    let bf = &c.pool_fees.base_fee;
    let h = get_base_fee_handler(bf.cliff_fee_numerator, bf.first_factor, bf.second_factor, bf.third_factor, bf.base_fee_mode)
        .map_err(|e| anyhow!("{e:?}"))?;
    let n = h.get_min_base_fee_numerator().map_err(|e| anyhow!("{e:?}"))?;
    Ok(n as f64 / 1e7)
}

fn describe_fee(c: &PoolConfig) -> String {
    let bf = &c.pool_fees.base_fee;
    let unit = if c.activation_type == 0 { "slot" } else { "s" };
    let start = bf.cliff_fee_numerator as f64 / 1e7;
    let dynamic = if c.pool_fees.dynamic_fee.is_dynamic_fee_enable() { " + dynamic" } else { "" };
    match bf.base_fee_mode {
        _ if bf.second_factor == 0 || bf.first_factor == 0 => format!("flat {start:.2}%{dynamic}"),
        0 | 1 => format!(
            "{} {:.2}% -> {:.2}% over {} periods x {} {unit}{dynamic}",
            if bf.base_fee_mode == 0 { "linear" } else { "exponential" },
            start,
            min_fee_pct(c).unwrap_or(f64::NAN),
            bf.first_factor,
            bf.second_factor
        ),
        _ => format!("rate limiter, cliff {start:.2}%{dynamic}"),
    }
}

fn print_config(c: &PoolConfig, quote_mint: &Option<String>) {
    println!("config:");
    println!("  quote mint           {}", quote_mint.as_deref().unwrap_or("?"));
    println!("  activation_type      {} (0 slot, 1 timestamp)", c.activation_type);
    println!("  collect_fee_mode     {} (0 quote, 1 output token)", c.collect_fee_mode);
    println!("  base fee             {}", describe_fee(c));
    println!("  first swap min fee   {}", c.is_first_swap_with_min_fee_enabled());
    println!("  migration threshold  {} ({:.3} if 9 decimals)", c.migration_quote_threshold, c.migration_quote_threshold as f64 / 1e9);
    println!("  token decimals       {}", c.token_decimal);
    println!("  migration option     {} (0 DAMM v1, 1 DAMM v2)", c.migration_option);
    println!(
        "  LP after migration   partner {}% + locked {}%, creator {}% + locked {}%",
        c.partner_liquidity_percentage,
        c.partner_permanent_locked_liquidity_percentage,
        c.creator_liquidity_percentage,
        c.creator_permanent_locked_liquidity_percentage
    );
    println!("  LP vesting partner   {:?}", c.partner_liquidity_vesting_info);
    println!("  LP vesting creator   {:?}", c.creator_liquidity_vesting_info);
    println!("  creator trading fee  {}% of trading fee", c.creator_trading_fee_percentage);
    println!(
        "  migration fee        {}% of threshold (creator share {}%)",
        c.migration_fee_percentage, c.creator_migration_fee_percentage
    );
}

/// Доля комиссии в сделке, %.
fn fee_pct(c: &PoolConfig, s: &engine::RecordedSwap, r: &SwapResult2) -> f64 {
    let dir = if s.trade_direction == 1 { TradeDirection::QuoteToBase } else { TradeDirection::BaseToQuote };
    let fees = (r.trading_fee + r.protocol_fee + r.referral_fee) as f64;
    let on_input = FeeMode::get_fee_mode(c.collect_fee_mode, dir, s.has_referral)
        .map(|m| m.fees_on_input)
        .unwrap_or(true);
    let gross = if on_input { r.included_fee_input_amount as f64 } else { r.output_amount as f64 + fees };
    if gross > 0.0 { 100.0 * fees / gross } else { 0.0 }
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let conn = data::open(&args.db)?;
    match args.prefix.as_str() {
        "risk" => return run_risk(&conn, &args),
        "clusters" => return run_clusters(&conn, &args),
        "templates" => return run_templates(&conn, &args),
        "impact" | "damage" => return run_damage(&conn, &args),
        _ => {}
    }
    let init = data::find_pool(&conn, &args.prefix)?;
    let config = data::load_config(&conn, &init.config)?;
    let quote_mint = data::load_quote_mint(&conn, &init.config);
    let swaps = data::load_swaps(&conn, &init.pool)?;

    println!("pool {}  config {}  swaps {}", init.pool, init.config, swaps.len());
    print_config(&config, &quote_mint);
    println!();

    // 1. Проверка движка на исходном конфиге (заодно восстанавливает порядок сделок).
    let outcomes = engine::validate(&config, &init, swaps)?;
    let matched = outcomes.iter().filter(|o| o.matched()).count();
    if !args.wants_cf() || args.has("--verbose") || matched != outcomes.len() {
        println!("{:>4}  {:>10}  {:<4}  {:<8}  {:<8}  result", "#", "slot", "dir", "sig", "wallet");
        for (n, o) in outcomes.iter().enumerate() {
            let s = &o.swap;
            let dir = if s.trade_direction == 1 { "buy" } else { "sell" };
            let status = if o.matched() {
                "OK".to_string()
            } else {
                match &o.sim {
                    Err(e) => format!("SIM ERROR: {e}"),
                    Ok(_) if o.diffs.is_empty() => "MISMATCH: quote_reserve".to_string(),
                    Ok(_) => format!("MISMATCH: {}", o.diffs.join("; ")),
                }
            };
            println!("{:>4}  {:>10}  {:<4}  {:<8}  {:<8}  {}", n, s.slot, dir, short(&s.signature), short(&s.fee_payer), status);
        }
        println!();
    }
    println!("validation: matched {matched}/{} swaps exactly", outcomes.len());
    if !args.wants_cf() {
        return Ok(());
    }
    if matched != outcomes.len() {
        println!("WARNING: engine does not reproduce every trade; counterfactual numbers are less reliable");
    }
    if quote_mint.as_deref() != Some(WSOL) {
        println!("NOTE: quote is not SOL; amounts below assume 9 quote decimals");
    }

    // 2. Контрфактический конфиг.
    let schedule = cf::FeeSchedule {
        mode: match args.flags.get("--cf-mode").map(String::as_str).unwrap_or("exp") {
            "exp" | "exponential" => 1,
            "linear" | "lin" => 0,
            m => bail!("--cf-mode must be exp or linear, got {m}"),
        },
        start_pct: args.num("--cf-start", 50.0)?,
        end_pct: args.num("--cf-end", min_fee_pct(&config)?.max(0.25))?,
        periods: args.num("--cf-periods", 20u16)?,
        period_len: args.num("--cf-period-len", 1u64)?,
    };
    let early: u64 = args.num("--early", 10)?;
    let cf_config = cf::with_schedule(&config, &schedule)?;

    let ordered: Vec<engine::RecordedSwap> = outcomes.iter().map(|o| o.swap.clone()).collect();
    let start_pool = engine::initial_pool(&config, &init)?;
    let real = cf::run(&config, start_pool, &ordered, &init.created_sig)?;
    let alt = cf::run(&cf_config, start_pool, &ordered, &init.created_sig)?;

    // Базовый прогон по модели должен совпадать с реальностью.
    let baseline_ok = real.trades.iter().zip(&ordered).all(|(t, s)| matches!(t, cf::TradeStatus::Done(r) if *r == s.result));
    println!(
        "baseline (original config, same trade model) reproduces recorded trades: {}",
        if baseline_ok { "yes" } else { "NO — some wallets traded tokens from outside this pool" }
    );
    println!();
    println!("original fee:        {}", describe_fee(&config));
    println!("counterfactual fee:  {}", schedule.describe(config.activation_type));
    println!();

    let dec_base = 10f64.powi(config.token_decimal as i32);
    let unit = if config.activation_type == 0 { "slot" } else { "s" };
    let ap = init.activation_point;
    let point = |s: &engine::RecordedSwap| if config.activation_type == 0 { s.slot } else { s.event_timestamp };

    println!(
        "{:>3}  {:>7}  {:<4}  {:<8}  {:>7}  {:>7}  {:>14}  {:>14}",
        "#", format!("+{unit}"), "dir", "wallet", "fee%", "cf fee%", "real out", "cf out"
    );
    for (i, s) in ordered.iter().enumerate() {
        let is_buy = s.trade_direction == 1;
        let fmt_out = |r: &SwapResult2| {
            if is_buy {
                format!("{:.0} tok", r.output_amount as f64 / dec_base)
            } else {
                format!("{:.4} SOL", r.output_amount as f64 / 1e9)
            }
        };
        let (cf_fee, cf_out) = match &alt.trades[i] {
            cf::TradeStatus::Done(r) => (format!("{:.2}", fee_pct(&cf_config, s, r)), fmt_out(r)),
            cf::TradeStatus::Skipped(why) => ("-".into(), format!("skipped: {why}")),
            cf::TradeStatus::Error(e) => ("-".into(), format!("error: {}", &e[..e.len().min(40)])),
        };
        println!(
            "{:>3}  {:>7}  {:<4}  {:<8}  {:>7.2}  {:>7}  {:>14}  {:>14}",
            i,
            format!("+{}", point(s).saturating_sub(ap)),
            if is_buy { "buy" } else { "sell" },
            short(&s.fee_payer),
            fee_pct(&config, s, &s.result),
            cf_fee,
            fmt_out(&s.result),
            cf_out
        );
    }
    println!();

    // Кошельки.
    println!(
        "{:<8}  {:<5}  {:>12}  {:>12}  {:>14}  {:>14}",
        "wallet", "early", "real PnL SOL", "cf PnL SOL", "real tok left", "cf tok left"
    );
    let mut wallets: Vec<(&String, &cf::WalletStats)> = real.wallets.iter().collect();
    wallets.sort_by_key(|(_, w)| w.first_point);
    let is_early = |w: &cf::WalletStats| w.first_point.map(|p| p.saturating_sub(ap) <= early).unwrap_or(false);
    let (mut er, mut ec, mut orl, mut oc, mut n_early) = (0i128, 0i128, 0i128, 0i128, 0);
    let (mut ot_real, mut ot_cf) = (0i128, 0i128);
    for (addr, w) in &wallets {
        let a = alt.wallets.get(*addr).cloned().unwrap_or_default();
        let e = is_early(w);
        if e {
            er += w.realized();
            ec += a.realized();
            n_early += 1;
        } else {
            orl += w.realized();
            oc += a.realized();
            ot_real += w.base;
            ot_cf += a.base;
        }
        println!(
            "{:<8}  {:<5}  {:>12.4}  {:>12.4}  {:>14.0}  {:>14.0}{}",
            short(addr),
            if e { "yes" } else { "" },
            w.realized() as f64 / 1e9,
            a.realized() as f64 / 1e9,
            w.base as f64 / dec_base,
            a.base as f64 / dec_base,
            if w.external || a.external { "  (tokens from outside)" } else { "" }
        );
    }
    println!();

    let grad = |r: &cf::RunResult| match r.completed_at {
        Some(k) => format!("yes, at trade #{k}"),
        None => format!(
            "no ({:.3} of {:.3} SOL collected)",
            r.final_pool.quote_reserve as f64 / 1e9,
            config.migration_quote_threshold as f64 / 1e9
        ),
    };
    let skipped = |r: &cf::RunResult| r.trades.iter().filter(|t| !matches!(t, cf::TradeStatus::Done(_))).count();

    println!("summary                                   real          counterfactual");
    println!("  early wallets ({n_early}, first trade <= +{early} {unit})");
    println!("    realized PnL, SOL               {:>12.4}  {:>12.4}", er as f64 / 1e9, ec as f64 / 1e9);
    println!("  other wallets realized PnL, SOL   {:>12.4}  {:>12.4}", orl as f64 / 1e9, oc as f64 / 1e9);
    println!(
        "  other wallets tokens held         {:>12.0}  {:>12.0}  ({:+.1}%)",
        ot_real as f64 / dec_base,
        ot_cf as f64 / dec_base,
        if ot_real != 0 { 100.0 * (ot_cf - ot_real) as f64 / ot_real as f64 } else { 0.0 }
    );
    println!("  trading fees (partner+creator), SOL eq {:>7.4}  {:>12.4}", real.trading_fee_equiv / 1e9, alt.trading_fee_equiv / 1e9);
    println!("  protocol fees, SOL eq             {:>12.4}  {:>12.4}", real.protocol_fee_equiv / 1e9, alt.protocol_fee_equiv / 1e9);
    println!("  graduation                        {}  |  {}", grad(&real), grad(&alt));
    println!("  trades skipped / failed           {:>12}  {:>12}", skipped(&real), skipped(&alt));
    Ok(())
}

// ---------------------------------------------------------------- risk mode

struct RiskRow {
    config: String,
    quote_mint: Option<String>,
    facts: risk::ConfigFacts,
    beh: risk::Behavior,
    rep: risk::Report,
    /// номер кластера операторов и его размер
    cluster: usize,
    cluster_size: usize,
    template: String,
}

struct Analysis {
    rows: Vec<RiskRow>,
    infos: Vec<cluster::ConfigInfo>,
    clusters: Vec<cluster::Cluster>,
    skipped: usize,
}

fn json_escape(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

fn flags_json(v: &[risk::Flag]) -> String {
    let items: Vec<String> = v
        .iter()
        .map(|f| format!("{{\"points\":{},\"text\":{}}}", f.points, json_escape(&f.text)))
        .collect();
    format!("[{}]", items.join(","))
}

fn leftover_pct(f: &risk::ConfigFacts) -> f64 {
    if f.total_supply > 0 { 100.0 * f.leftover_to_receiver as f64 / f.total_supply as f64 } else { 0.0 }
}

/// Оценка всех конфигов базы + кластеры операторов + перенос риска по связям.
/// Кластеры строятся по ВСЕЙ базе, даже если запрошен один конфиг: иначе связи не видны.
fn analyze(conn: &rusqlite::Connection) -> Result<Analysis> {
    let mut st = conn.prepare("SELECT config, COUNT(*) n FROM pools GROUP BY config ORDER BY n DESC")?;
    let configs: Vec<String> = st
        .query_map([], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;

    // Проход 1: читаем конфиги и адреса.
    let mut loaded = Vec::new();
    let mut infos = Vec::new();
    let mut skipped = 0;
    for cfg in configs {
        let (config, hook) = match data::load_config_ext(conn, &cfg) {
            Ok(c) => c,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        let raw = data::load_raw(conn, &cfg).unwrap_or_default();
        infos.push(cluster::load_info(conn, &cfg, &raw, hook.is_some())?);
        loaded.push((cfg, config, hook));
    }
    let mut leftover_count: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for i in &infos {
        if let Some(l) = &i.leftover_receiver {
            *leftover_count.entry(l.clone()).or_default() += 1;
        }
    }

    // Проход 2: оценка с учётом того, какие адреса — общие адреса платформ.
    let widx = farm::wallet_index(conn)?;
    let mut rows = Vec::new();
    for ((cfg, config, hook), info) in loaded.into_iter().zip(&infos) {
        let mut facts = risk::config_facts(&config);
        facts.transfer_hook = hook;
        facts.leftover_receiver_configs = facts
            .leftover_receiver
            .as_ref()
            .and_then(|l| leftover_count.get(l).copied())
            .unwrap_or(1);
        let mut beh = risk::behavior(conn, &cfg, config.migration_quote_threshold)?;
        let fs = farm::analyze_config(conn, &cfg, &widx, 0)?;
        if fs.pools > 0 {
            beh.farm = Some(fs);
        }
        let rep = risk::score(&facts, &beh);
        rows.push(RiskRow {
            quote_mint: data::load_quote_mint(conn, &cfg),
            template: info.template.clone(),
            config: cfg,
            facts,
            beh,
            rep,
            cluster: 0,
            cluster_size: 1,
        });
    }

    let clusters = cluster::clusters(&infos);
    for c in &clusters {
        for &m in &c.members {
            rows[m].cluster = c.id;
            rows[m].cluster_size = c.members.len();
        }
    }

    // Ферма на уровне оператора: если оператор держит одну ферму на нескольких конфигах,
    // специфичность кошельков считается к кластеру. Только для кластеров операторов
    // (есть конфиг, позволяющий забрать ликвидность и сбросить остаток) и не для платформ
    // (общий получатель остатка в 10+ конфигах), иначе постоянные трейдеры площадки
    // ошибочно попали бы в «ферму».
    for c in &clusters {
        if !operator_scope(c, &rows) {
            continue;
        }
        let scope: Vec<String> = c.members.iter().map(|&m| rows[m].config.clone()).collect();
        for &m in &c.members {
            let fs = farm::analyze_scope(conn, &scope, &[rows[m].config.clone()], &widx, 0)?;
            let r = &mut rows[m];
            r.beh.farm = if fs.pools > 0 { Some(fs) } else { None };
            r.rep = risk::score(&r.facts, &r.beh);
        }
    }

    // Общий адрес получателя остатка считается «адресом платформы» только у чистых кластеров:
    // если в кластере есть конфиг с наблюдаемыми инсайдерскими продажами, это адрес оператора.
    for c in &clusters {
        let has_red = c.members.iter().any(|&m| rows[m].rep.verdict == risk::Verdict::Synthetic);
        if !has_red {
            continue;
        }
        for &m in &c.members {
            let r = &mut rows[m];
            if r.facts.leftover_receiver_configs >= risk::PLATFORM_MIN_CONFIGS {
                r.facts.leftover_receiver_configs = 1;
                r.rep = risk::score(&r.facts, &r.beh);
            }
        }
    }

    // Перенос риска.
    let red: Vec<bool> = rows.iter().map(|r| r.rep.verdict == risk::Verdict::Synthetic).collect();
    let mut red_templates: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (i, r) in rows.iter().enumerate() {
        if red[i] {
            *red_templates.entry(r.template.clone()).or_default() += 1;
        }
    }
    for c in &clusters {
        let red_in_cluster = c.members.iter().filter(|&&m| red[m]).count();
        let kinds: Vec<&str> = c.links.keys().map(|k| k.label()).collect();
        for &m in &c.members {
            if red[m] {
                continue;
            }
            let r = &mut rows[m];
            if red_in_cluster > 0 {
                r.rep.evidence_flags.push(risk::Flag {
                    points: 50,
                    text: format!(
                        "linked by shared {} to {} config(s) with observed insider selling (operator cluster #{})",
                        kinds.join(", "),
                        red_in_cluster,
                        c.id
                    ),
                });
                r.rep.verdict = risk::Verdict::RedLinked;
            } else if let Some(n) = red_templates.get(&r.template) {
                r.rep.evidence_flags.push(risk::Flag {
                    points: 20,
                    text: format!("identical parameter template to {n} config(s) with observed insider selling"),
                });
            } else {
                continue;
            }
            r.rep.evidence = Some(r.rep.evidence_flags.iter().map(|f| f.points).sum::<u32>().min(100));
        }
    }

    Ok(Analysis { rows, infos, clusters, skipped })
}

/// Кластер оператора, а не платформы: несколько конфигов, хотя бы один позволяет забрать
/// ликвидность и сбросить остаток, и ни у одного остаток не хранит адрес платформы.
fn operator_scope(c: &cluster::Cluster, rows: &[RiskRow]) -> bool {
    c.members.len() > 1
        && c.members.iter().any(|&m| rows[m].rep.capability >= 50)
        && c.members.iter().all(|&m| rows[m].facts.leftover_receiver_configs < risk::PLATFORM_MIN_CONFIGS)
}

fn sort_rows(rows: &mut [&RiskRow]) {
    rows.sort_by(|a, b| {
        a.rep.verdict
            .cmp(&b.rep.verdict)
            .then(b.rep.evidence.unwrap_or(0).cmp(&a.rep.evidence.unwrap_or(0)))
            .then(b.rep.capability.cmp(&a.rep.capability))
            .then(b.beh.pools.cmp(&a.beh.pools))
    });
}

fn template_summary(r: &RiskRow) -> String {
    let f = &r.facts;
    format!(
        "LP creator {}% / partner {}% unlocked, {}% locked, {}% vested; leftover {:.0}%; threshold {:.2}; {}{}",
        f.lp.creator_unlocked,
        f.lp.partner_unlocked,
        f.lp.locked,
        f.lp.creator_vest + f.lp.partner_vest,
        leftover_pct(f),
        f.threshold as f64 / 1e9,
        if f.has_fee_scheduler { "fee scheduler" } else { "flat fee" },
        if f.transfer_hook.is_some() { "; transfer hook" } else { "" }
    )
}

fn run_risk(conn: &rusqlite::Connection, args: &Args) -> Result<()> {
    let a = analyze(conn)?;
    let prefix = args.target.clone().unwrap_or_default();
    let mut rows: Vec<&RiskRow> = a.rows.iter().filter(|r| r.config.starts_with(&prefix)).collect();
    if rows.is_empty() {
        bail!("no readable configs match");
    }
    sort_rows(&mut rows);

    if args.has("--json") {
        let opt = |v: Option<f64>| v.map(|x| format!("{x:.4}")).unwrap_or_else(|| "null".into());
        let items: Vec<String> = rows
            .iter()
            .map(|r| {
                format!(
                    "{{\"config\":{},\"quote_mint\":{},\"verdict\":\"{}\",\"verdict_text\":{},\"capability\":{},\"evidence\":{},\"cluster\":{},\"cluster_size\":{},\"template\":\"{}\",\"pools\":{},\"creators\":{},\"top_creator\":{},\"tracked\":{},\"graduated\":{},\"creator_unlocked_lp_pct\":{},\"partner_unlocked_lp_pct\":{},\"locked_lp_pct\":{},\"vested_lp_pct\":{},\"vest_full_release_secs\":{},\"leftover_to_receiver_pct\":{:.2},\"transfer_hook\":{},\"instant_graduation\":{},\"fee_claimer\":{},\"leftover_receiver\":{},\"leftover_receiver_configs\":{},\"median_prebuy_pct\":{},\"median_outside_wallets\":{},\"median_outside_sol\":{},\"capability_flags\":{},\"evidence_flags\":{}}}",
                    json_escape(&r.config),
                    r.quote_mint.as_deref().map(json_escape).unwrap_or_else(|| "null".into()),
                    r.rep.verdict.label(),
                    json_escape(r.rep.verdict.describe()),
                    r.rep.capability,
                    r.rep.evidence.map(|e| e.to_string()).unwrap_or_else(|| "null".into()),
                    r.cluster,
                    r.cluster_size,
                    r.template,
                    r.beh.pools,
                    r.beh.creators,
                    r.beh.top_creator.as_deref().map(json_escape).unwrap_or_else(|| "null".into()),
                    r.beh.tracked,
                    r.beh.graduated,
                    r.facts.lp.creator_unlocked,
                    r.facts.lp.partner_unlocked,
                    r.facts.lp.locked,
                    r.facts.lp.creator_vest + r.facts.lp.partner_vest,
                    r.facts.lp.vest_full_release_secs,
                    leftover_pct(&r.facts),
                    r.facts.transfer_hook.as_deref().map(json_escape).unwrap_or_else(|| "null".into()),
                    r.facts.instant_graduation,
                    r.facts.fee_claimer.as_deref().map(json_escape).unwrap_or_else(|| "null".into()),
                    r.facts.leftover_receiver.as_deref().map(json_escape).unwrap_or_else(|| "null".into()),
                    r.facts.leftover_receiver_configs,
                    opt(r.beh.median_prebuy_pct),
                    opt(r.beh.median_outside_wallets),
                    opt(r.beh.median_outside_sol),
                    flags_json(&r.rep.capability_flags),
                    flags_json(&r.rep.evidence_flags),
                )
            })
            .collect();
        println!("[{}]", items.join(",\n"));
        return Ok(());
    }

    if rows.len() == 1 {
        let r = rows[0];
        let f = &r.facts;
        let b = &r.beh;
        let dec = 10f64.powi(f.token_decimal as i32);
        println!("config      {}", r.config);
        println!("quote mint  {}", r.quote_mint.as_deref().unwrap_or("?"));
        println!("VERDICT     {}: {}", r.rep.verdict.label(), r.rep.verdict.describe());
        println!(
            "scores      capability {}/100, evidence {}",
            r.rep.capability,
            r.rep.evidence.map(|e| format!("{e}/100")).unwrap_or_else(|| "n/a (no tracked pools)".into())
        );
        println!(
            "operator    cluster #{} ({} config{}), template {}",
            r.cluster,
            r.cluster_size,
            if r.cluster_size == 1 { "" } else { "s" },
            r.template
        );
        println!();
        println!("config account (known before the first buy):");
        println!(
            "  post-migration LP    creator unlocked {}%, partner unlocked {}%, locked {}%, vested {}%",
            f.lp.creator_unlocked, f.lp.partner_unlocked, f.lp.locked, f.lp.creator_vest + f.lp.partner_vest
        );
        if f.lp.creator_vest + f.lp.partner_vest > 0 {
            println!("  vesting fully unlocks {:.1} h after migration", f.lp.vest_full_release_secs as f64 / 3600.0);
        }
        println!(
            "  token supply         {:.0}; sold on curve {:.0}; to leftover receiver {:.0} ({:.0}%)",
            f.total_supply as f64 / dec,
            f.curve_supply as f64 / dec,
            f.leftover_to_receiver as f64 / dec,
            leftover_pct(f)
        );
        println!(
            "  migration threshold  {:.4} (9 decimals){}",
            f.threshold as f64 / 1e9,
            if f.instant_graduation { "  <- instant graduation" } else { "" }
        );
        println!("  fee claimer          {}", f.fee_claimer.as_deref().unwrap_or("?"));
        println!(
            "  leftover receiver    {} (in {} config{})",
            f.leftover_receiver.as_deref().unwrap_or("?"),
            f.leftover_receiver_configs,
            if f.leftover_receiver_configs == 1 { "" } else { "s" }
        );
        println!("  migration fee        {}%", f.migration_fee_pct);
        println!("  transfer hook        {}", f.transfer_hook.as_deref().unwrap_or("none"));
        println!("  anti-sniper fee      {}", if f.has_fee_scheduler { "fee scheduler on" } else { "none (flat fee)" });
        println!("  token authority      {}", match f.update_authority {
            0 => "creator can update metadata",
            1 => "immutable",
            2 => "partner can update metadata",
            3 => "creator: update + MINT",
            4 => "partner: update + MINT",
            _ => "?",
        });
        for fl in &r.rep.capability_flags {
            println!("  +{:<3} {}", fl.points, fl.text);
        }
        println!();
        println!("observed pools and links:");
        println!("  pools {}  creators {}  tracked {}  graduated {}", b.pools, b.creators, b.tracked, b.graduated);
        if let Some(c) = &b.top_creator {
            println!("  top creator          {} ({} pools)", c, b.top_creator_pools);
        }
        if let Some(p) = b.median_prebuy_pct {
            println!("  creator opening buy  {:.0}% of threshold (median)", p);
        }
        if let Some(w) = b.median_outside_wallets {
            println!("  outside-token sellers {:.0} wallets / {:.3} SOL per pool (median)", w, b.median_outside_sol.unwrap_or(0.0));
        }
        if let Some(fs) = &b.farm {
            println!(
                "  linked activity      {} recurring wallets ({} early first-buyers); linked volume {} of total (median per pool)",
                fs.farm_wallets,
                fs.first_buyers,
                fs.median_linked_share.map(|x| format!("{:.0}%", x * 100.0)).unwrap_or("-".into())
            );
            println!(
                "  external wallets     {} wallets; net SOL left on the curve {:.3}",
                fs.external_wallets,
                fs.external_net_in_sol()
            );
            if let Some(m) = fs.median_migration_secs {
                println!("  launch -> migration  {:.0} s (median)", m);
            }
            if let Some((sol, sh)) = fs.dev_buy_mode {
                println!("  opening buy          {} SOL in {:.0}% of pools", farm::fmt_sol(sol), sh * 100.0);
            }
            if let Some((sol, sh)) = fs.first_buy_mode {
                println!("  first non-creator buy {} SOL in {:.0}% of pools", farm::fmt_sol(sol), sh * 100.0);
            }
        }
        for fl in &r.rep.evidence_flags {
            println!("  +{:<3} {}", fl.points, fl.text);
        }
        if r.cluster_size > 1 {
            println!("  see: dbc-replay <db> clusters {}", short(&r.config));
        }
        return Ok(());
    }

    let limit: usize = args.num("--limit", 40)?;
    println!(
        "{:<8}  {:<9}  {:>4}  {:>5}  {:>7}  {:>5}  {:>4}  {:>6}  {:>8}  {:>7}  {:>7}",
        "config", "verdict", "cap", "evid", "cluster", "pools", "crtr", "crLP%", "leftov%", "prebuy%", "outside"
    );
    for r in rows.iter().take(limit) {
        println!(
            "{:<8}  {:<9}  {:>4}  {:>5}  {:>7}  {:>5}  {:>4}  {:>6}  {:>8.0}  {:>7}  {:>7}",
            short(&r.config),
            r.rep.verdict.label(),
            r.rep.capability,
            r.rep.evidence.map(|e| e.to_string()).unwrap_or_else(|| "-".into()),
            if r.cluster_size > 1 { format!("#{}/{}", r.cluster, r.cluster_size) } else { "-".into() },
            r.beh.pools,
            r.beh.creators,
            r.facts.lp.creator_unlocked,
            leftover_pct(&r.facts),
            r.beh.median_prebuy_pct.map(|p| format!("{p:.0}")).unwrap_or_else(|| "-".into()),
            r.beh.median_outside_wallets.map(|w| format!("{w:.0}")).unwrap_or_else(|| "-".into()),
        );
    }
    println!();
    let count = |v: risk::Verdict| rows.iter().filter(|r| r.rep.verdict == v).count();
    let pools = |v: risk::Verdict| rows.iter().filter(|r| r.rep.verdict == v).map(|r| r.beh.pools).sum::<u64>();
    println!(
        "summary ({} configs{}):",
        rows.len(),
        if a.skipped > 0 { format!(", {} unreadable skipped", a.skipped) } else { String::new() }
    );
    for v in [
        risk::Verdict::Synthetic,
        risk::Verdict::RedLinked,
        risk::Verdict::RugCapable,
        risk::Verdict::SelfGraduation,
        risk::Verdict::Standard,
    ] {
        println!("  {:<9} {:>4} configs {:>5} pools  {}", v.label(), count(v), pools(v), v.describe());
    }
    println!("cap = what the config allows, evid = what was observed; cluster = #id/size of the operator cluster");
    println!("details: dbc-replay <db> risk <config prefix> | operators: dbc-replay <db> clusters | templates: dbc-replay <db> templates");
    Ok(())
}

fn run_clusters(conn: &rusqlite::Connection, args: &Args) -> Result<()> {
    let a = analyze(conn)?;
    let verdict_counts = |c: &cluster::Cluster| {
        let mut m: std::collections::BTreeMap<risk::Verdict, usize> = std::collections::BTreeMap::new();
        for &i in &c.members {
            *m.entry(a.rows[i].rep.verdict).or_default() += 1;
        }
        m
    };

    if let Some(prefix) = &args.target {
        let idx = a
            .rows
            .iter()
            .position(|r| r.config.starts_with(prefix.as_str()))
            .context("no readable config matches")?;
        let c = a.clusters.iter().find(|c| c.members.contains(&idx)).unwrap();
        let pools: u64 = c.members.iter().map(|&i| a.rows[i].beh.pools).sum();
        let templates: std::collections::HashSet<&str> = c.members.iter().map(|&i| a.rows[i].template.as_str()).collect();
        println!("operator cluster #{}: {} configs, {} pools, {} parameter template(s)", c.id, c.members.len(), pools, templates.len());
        let links: Vec<String> = c.links.iter().map(|(k, n)| format!("{} x{}", k.label(), n)).collect();
        println!("links: {}", if links.is_empty() { "none (single config)".into() } else { links.join(", ") });
        println!();
        println!("{:<44}  {:<9}  {:>5}  {:>4}  {:<16}", "config", "verdict", "pools", "crtr", "template");
        let mut members: Vec<usize> = c.members.clone();
        members.sort_by(|&x, &y| a.rows[x].rep.verdict.cmp(&a.rows[y].rep.verdict).then(a.rows[y].beh.pools.cmp(&a.rows[x].beh.pools)));
        for i in members {
            let r = &a.rows[i];
            println!("{:<44}  {:<9}  {:>5}  {:>4}  {:<16}", r.config, r.rep.verdict.label(), r.beh.pools, r.beh.creators, r.template);
        }
        if !c.shared.is_empty() {
            println!();
            println!("shared addresses (role, address, configs):");
            for s in c.shared.iter().take(30) {
                println!("  {:<18} {:<44} {}", s.kind.label(), s.address, s.configs);
            }
            if c.shared.len() > 30 {
                println!("  ... {} more", c.shared.len() - 30);
            }
        }
        return Ok(());
    }

    let mut multi: Vec<&cluster::Cluster> = a.clusters.iter().filter(|c| c.members.len() > 1).collect();
    multi.sort_by(|x, y| {
        let wx = x.members.iter().map(|&i| a.rows[i].rep.verdict).min();
        let wy = y.members.iter().map(|&i| a.rows[i].rep.verdict).min();
        wx.cmp(&wy).then(y.members.len().cmp(&x.members.len()))
    });
    println!(
        "{:>4}  {:>7}  {:>5}  {:>5}  {:<26}  {:<40}  {}",
        "#", "configs", "pools", "tmpl", "verdicts", "links", "top creator"
    );
    for c in &multi {
        let pools: u64 = c.members.iter().map(|&i| a.rows[i].beh.pools).sum();
        let templates: std::collections::HashSet<&str> = c.members.iter().map(|&i| a.rows[i].template.as_str()).collect();
        let vc: Vec<String> = verdict_counts(c).iter().map(|(v, n)| format!("{} {}", v.label(), n)).collect();
        let links: Vec<String> = c.links.iter().map(|(k, n)| format!("{} x{}", k.label(), n)).collect();
        let top = c
            .members
            .iter()
            .max_by_key(|&&i| a.rows[i].beh.top_creator_pools)
            .and_then(|&i| a.rows[i].beh.top_creator.clone())
            .unwrap_or_default();
        println!(
            "{:>4}  {:>7}  {:>5}  {:>5}  {:<26}  {:<40}  {}",
            c.id,
            c.members.len(),
            pools,
            templates.len(),
            vc.join(", "),
            links.join(", "),
            short(&top)
        );
    }
    println!();
    println!(
        "{} operator clusters with 2+ configs (out of {} configs); details: dbc-replay <db> clusters <config prefix>",
        multi.len(),
        a.rows.len()
    );
    let _ = &a.infos;
    Ok(())
}

fn run_templates(conn: &rusqlite::Connection, _args: &Args) -> Result<()> {
    let a = analyze(conn)?;
    let mut by_tpl: std::collections::BTreeMap<&str, Vec<usize>> = std::collections::BTreeMap::new();
    for (i, r) in a.rows.iter().enumerate() {
        by_tpl.entry(r.template.as_str()).or_default().push(i);
    }
    let mut list: Vec<(&str, Vec<usize>)> = by_tpl.into_iter().filter(|(_, v)| v.len() > 1).collect();
    list.sort_by(|x, y| y.1.len().cmp(&x.1.len()));
    println!("{:<16}  {:>7}  {:>5}  {:>8}  {:<28}  parameters", "template", "configs", "pools", "clusters", "verdicts");
    for (t, idx) in &list {
        let pools: u64 = idx.iter().map(|&i| a.rows[i].beh.pools).sum();
        let clusters: std::collections::HashSet<usize> = idx.iter().map(|&i| a.rows[i].cluster).collect();
        let mut vc: std::collections::BTreeMap<risk::Verdict, usize> = std::collections::BTreeMap::new();
        for &i in idx {
            *vc.entry(a.rows[i].rep.verdict).or_default() += 1;
        }
        let vcs: Vec<String> = vc.iter().map(|(v, n)| format!("{} {}", v.label(), n)).collect();
        println!(
            "{:<16}  {:>7}  {:>5}  {:>8}  {:<28}  {}",
            t,
            idx.len(),
            pools,
            clusters.len(),
            vcs.join(", "),
            template_summary(&a.rows[idx[0]])
        );
    }
    println!();
    println!("templates shared by 2+ configs; 'clusters' = how many distinct operator clusters use the template");
    Ok(())
}

// ---------------------------------------------------------------- impact

/// `impact` (алиас `damage`): синтетичность активности и результат внешних участников
/// по операторам с вердиктом RED / RED-LINK.
fn run_damage(conn: &rusqlite::Connection, args: &Args) -> Result<()> {
    let a = analyze(conn)?;
    let since_hours: f64 = args.num("--since-hours", 0.0)?;
    let sol_usd: f64 = args.num("--sol-usd", 0.0)?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64;
    let cutoff = if since_hours > 0.0 { now - (since_hours * 3600.0) as i64 } else { 0 };
    let widx = farm::wallet_index(conn)?;
    let usd = |sol: f64| if sol_usd > 0.0 { format!(" (~${:.0})", sol * sol_usd) } else { String::new() };

    struct G {
        label: String,
        verdict: risk::Verdict,
        creator: String,
        pools: usize,
        grad: usize,
        mig: Vec<f64>,
        total: u64,
        linked: u64,
        ext_wallets: usize,
        ext_buy: u64,
        ext_sell: u64,
        active: usize,
    }
    let mut groups: Vec<G> = Vec::new();
    for c in &a.clusters {
        let worst = c.members.iter().map(|&i| a.rows[i].rep.verdict).min().unwrap();
        if !matches!(worst, risk::Verdict::Synthetic | risk::Verdict::RedLinked) {
            continue;
        }
        let mut g = G {
            label: if c.members.len() > 1 { format!("cluster #{} ({} cfg)", c.id, c.members.len()) } else { short(&a.rows[c.members[0]].config).to_string() },
            verdict: worst,
            creator: String::new(),
            pools: 0,
            grad: 0,
            mig: Vec::new(),
            total: 0,
            linked: 0,
            ext_wallets: 0,
            ext_buy: 0,
            ext_sell: 0,
            active: 0,
        };
        let mut best = 0u64;
        let scope: Vec<String> = if operator_scope(c, &a.rows) {
            c.members.iter().map(|&m| a.rows[m].config.clone()).collect()
        } else {
            Vec::new()
        };
        for &i in &c.members {
            let r = &a.rows[i];
            if r.beh.top_creator_pools > best {
                best = r.beh.top_creator_pools;
                g.creator = r.beh.top_creator.clone().unwrap_or_default();
            }
            let fs = if scope.is_empty() {
                farm::analyze_config(conn, &r.config, &widx, cutoff)?
            } else {
                farm::analyze_scope(conn, &scope, &[r.config.clone()], &widx, cutoff)?
            };
            g.pools += fs.pools;
            g.active += fs.active_pools;
            g.grad += fs.graduated;
            if let Some(m) = fs.median_migration_secs {
                g.mig.push(m);
            }
            g.total += fs.total_volume_lamports;
            g.linked += fs.linked_volume_lamports;
            g.ext_wallets += fs.external_wallets;
            g.ext_buy += fs.external_buy_lamports;
            g.ext_sell += fs.external_sell_lamports;
        }
        if g.total > 0 {
            groups.push(g);
        }
    }
    groups.sort_by(|x, y| y.total.cmp(&x.total));

    let window = if since_hours > 0.0 { format!("last {since_hours} h") } else { "all collected data".into() };
    println!("Activity in RED / RED-LINK operators ({window}), tracked pools only");
    println!("linked = creator + recurring config-specific wallets + fan-out sellers; external = everyone else");
    println!();
    println!(
        "{:<20}  {:<9}  {:<8}  {:>5}  {:>5}  {:>7}  {:>11}  {:>7}  {:>8}  {:>12}",
        "operator", "verdict", "creator", "pools", "grad", "migr s", "volume SOL", "linked", "ext wal", "ext net SOL"
    );
    let (mut tv, mut tl, mut tw, mut tb, mut ts, mut tp) = (0u64, 0u64, 0usize, 0u64, 0u64, 0usize);
    for g in &groups {
        let mut m = g.mig.clone();
        m.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let med = if m.is_empty() { "-".to_string() } else { format!("{:.0}", m[m.len() / 2]) };
        let net = (g.ext_buy as f64 - g.ext_sell as f64) / 1e9;
        println!(
            "{:<20}  {:<9}  {:<8}  {:>5}  {:>5}  {:>7}  {:>11.1}  {:>7}  {:>8}  {:>12.3}",
            g.label,
            g.verdict.label(),
            short(&g.creator),
            g.pools,
            g.grad,
            med,
            g.total as f64 / 1e9,
            if g.active >= farm::MIN_POOLS { format!("{:.0}%", 100.0 * g.linked as f64 / g.total as f64) } else { "n/a".into() },
            g.ext_wallets,
            net
        );
        tv += g.total;
        tl += g.linked;
        tw += g.ext_wallets;
        tb += g.ext_buy;
        ts += g.ext_sell;
        tp += g.pools;
    }
    println!();
    if tv > 0 {
        let vol = tv as f64 / 1e9;
        let net = (tb as f64 - ts as f64) / 1e9;
        println!("TOTAL ({} tracked pools)", tp);
        println!("  trading volume on the curve:      {:>10.1} SOL{}", vol, usd(vol));
        println!("  of it creator-linked:             {:>9.1}%", 100.0 * tl as f64 / tv as f64);
        println!("  external wallets (pool-wallet pairs): {}", tw);
        println!("  external buys / sells:            {:>10.2} / {:.2} SOL", tb as f64 / 1e9, ts as f64 / 1e9);
        println!("  external net SOL left on curves:  {:>10.2} SOL{}  (upper bound of external losses on the curve)", net, usd(net.max(0.0)));
    }
    println!();
    println!("notes: external = not the creator, not a fan-out seller, not a wallet that trades in >=30% of the config's pools");
    println!("       with >=80% of its activity there (across the operator cluster); generic bots count as external.");
    println!("       linked % is n/a when fewer than 3 pools had real trading (>=3 swaps not by the creator).");
    Ok(())
}
