//! Тексты сообщений в MarkdownV2. Любой текст, кроме разметки, проходит через markdown::esc.

use crate::markdown::{bold, code, esc, italic, link};
use crate::store::{now, ConfigRisk, PoolInfo};
use std::collections::HashMap;

pub fn emoji(verdict: &str) -> &'static str {
    match verdict {
        "RED" | "RED-LINK" => "🔴",
        "AMBER" => "🟠",
        "SELF-GRAD" => "⚪",
        "GREEN" => "🟢",
        _ => "❔",
    }
}

pub fn title(verdict: &str) -> &'static str {
    match verdict {
        "RED" => "Synthetic launches",
        "RED-LINK" => "Linked to a synthetic-launch operator",
        "AMBER" => "Risky config",
        "SELF-GRAD" => "Instant graduation",
        "GREEN" => "No major red flags",
        _ => "Unknown",
    }
}

fn ago(t: Option<i64>) -> String {
    let Some(t) = t else { return "time unknown".into() };
    let d = (now() - t).max(0);
    match d {
        0..=59 => format!("{d}s ago"),
        60..=3599 => format!("{}m ago", d / 60),
        3600..=86_399 => format!("{}h ago", d / 3600),
        _ => format!("{}d ago", d / 86_400),
    }
}

/// «1 trade», «5 trades».
fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn short(a: &str) -> String {
    if a.len() > 12 { format!("{}…{}", &a[..4], &a[a.len() - 4..]) } else { a.to_string() }
}

fn bullets(lines: &[String], max: usize) -> String {
    let mut out = String::new();
    for l in lines.iter().take(max) {
        out.push_str(&format!("• {}\n", esc(l)));
    }
    if lines.len() > max {
        out.push_str(&format!("{}\n", italic(&format!("+{} more", lines.len() - max))));
    }
    out
}

/// Тревожные признаки (с баллами, от весомых) и информационные строки (без баллов).
fn risks_and_notes(r: &ConfigRisk) -> (Vec<String>, Vec<String>) {
    let mut all = r.scored_flags("capability_flags");
    all.extend(r.scored_flags("evidence_flags"));
    let mut risks: Vec<(i64, String)> = all.iter().filter(|(p, _)| *p > 0).cloned().collect();
    risks.sort_by(|a, b| b.0.cmp(&a.0));
    let notes = all.into_iter().filter(|(p, _)| *p <= 0).map(|(_, t)| t).collect();
    (risks.into_iter().map(|(_, t)| t).collect(), notes)
}

/// Положительные факты из конфига и наблюдений — то, что говорит в пользу запуска.
fn positives(r: &ConfigRisk) -> Vec<String> {
    let mut out = Vec::new();
    let locked = r.u("locked_lp_pct");
    let unlocked = r.u("creator_unlocked_lp_pct") + r.u("partner_unlocked_lp_pct");
    if locked >= 50 {
        out.push(format!("{locked}% of post-migration liquidity is permanently locked"));
    }
    if unlocked == 0 {
        out.push("no liquidity can be withdrawn right after migration".to_string());
    }
    if let Some(left) = r.f("leftover_to_receiver_pct") {
        if left < 5.0 {
            out.push(format!("leftover supply to the receiver is small ({left:.1}%)"));
        }
    }
    let tracked = r.u("tracked");
    if tracked >= 3 && r.scored_flags("evidence_flags").iter().all(|(p, _)| *p <= 0) {
        out.push(format!("no creator-linked trading observed in {tracked} tracked pools of this config"));
    }
    out
}

/// Блоки «почему / мелкие сигналы», «в пользу», «заметки» — сначала риски, потом положительное.
fn reasons(r: &ConfigRisk, max_risks: usize) -> String {
    let v = r.verdict();
    let (risks, notes) = risks_and_notes(r);
    let good = positives(r);
    let mut s = String::new();
    if !risks.is_empty() {
        let title = if v == "GREEN" { "Minor signals (below warning level)" } else { "Why" };
        s.push_str(&format!("{}\n{}\n", bold(title), bullets(&risks, max_risks)));
    } else if v != "GREEN" {
        s.push_str(&format!("{}\n• {}\n\n", bold("Why"), esc(&r.s("verdict_text"))));
    }
    if !good.is_empty() {
        s.push_str(&format!("{}\n{}\n", bold("In its favour"), bullets(&good, 4)));
    }
    if !notes.is_empty() {
        s.push_str(&format!("{}\n{}\n", bold("Notes"), bullets(&notes, 2)));
    }
    s
}

const DISCLAIMER: &str = "Heuristic on-chain analysis of public data. Not financial advice.";

pub fn config_report(r: &ConfigRisk) -> String {
    let v = r.verdict();
    let mut s = format!("{} {}\n{}\n\n", emoji(&v), bold(&format!("{v} — {}", title(&v))), esc(&r.s("verdict_text")));
    s.push_str(&format!("Config {}\n\n", code(&r.s("config"))));

    s.push_str(&format!("{}\n", bold("What the config allows (known before the first buy)")));
    let cap = r.flags("capability_flags");
    if cap.is_empty() {
        s.push_str(&format!("• {}\n", esc("no risky settings found")));
    } else {
        s.push_str(&bullets(&cap, 4));
    }
    s.push_str(&format!(
        "• {}\n\n",
        esc(&format!(
            "post-migration liquidity: creator {}% unlocked, partner {}% unlocked, {}% locked, {}% vested",
            r.u("creator_unlocked_lp_pct"),
            r.u("partner_unlocked_lp_pct"),
            r.u("locked_lp_pct"),
            r.u("vested_lp_pct")
        ))
    ));

    let tracked = r.u("tracked");
    s.push_str(&format!(
        "{}\n",
        bold(&format!("What we observed ({} pools, {} tracked, {} graduated)", r.u("pools"), tracked, r.u("graduated")))
    ));
    let ev = r.flags("evidence_flags");
    if !ev.is_empty() {
        s.push_str(&bullets(&ev, 5));
    } else if tracked == 0 {
        s.push_str(&format!("• {}\n", esc("no tracked pools yet")));
    } else {
        s.push_str(&format!("• {}\n", esc("nothing unusual in trading")));
    }
    s.push('\n');
    let good = positives(r);
    if !good.is_empty() {
        s.push_str(&format!("{}\n{}\n", bold("In its favour"), bullets(&good, 4)));
    }
    s.push_str(&italic(DISCLAIMER));
    s
}

pub fn pool_card(p: &PoolInfo, r: Option<&ConfigRisk>) -> String {
    let (v, t) = r.map(|r| (r.verdict(), title(&r.verdict()))).unwrap_or(("?".into(), "config not analysed yet"));
    let mut s = format!("{} {}\n\n", emoji(&v), bold(&format!("{v} — {t}")));
    s.push_str(&format!("Token {}\n", code(&p.base_mint)));
    s.push_str(&format!("Pool {}\n", code(&p.pool)));
    // адрес создателя сокращён: полностью он виден в пуле на Solscan, но бот не выдаёт готовый список операторов
    s.push_str(&format!("Creator {}\n", code(&short(&p.creator))));
    let instant = r.map(|r| r.0.get("instant_graduation").and_then(|v| v.as_bool()).unwrap_or(false)).unwrap_or(false);
    let age = p.created_time.map(|t| now() - t).unwrap_or(0);
    // данные пула ещё не собраны: отслеживается, но есть только покупка создателя
    let collecting = p.tracked && p.swaps.as_ref().map(|sw| sw.count <= 1).unwrap_or(true) && age > 300 && p.graduated_time.is_none();
    let grad = match p.graduated_time {
        Some(g) => format!("graduated {} after launch", dur(g - p.created_time.unwrap_or(g))),
        None if instant => "graduates instantly (migration threshold ≈ 0)".into(),
        None if !p.tracked => "graduation status not sampled".into(),
        None if collecting => "trades are still being collected".into(),
        // нет записи о graduation — это не значит, что токен ещё на кривой
        None => "graduation not recorded".into(),
    };
    s.push_str(&format!("{}\n\n", esc(&format!("Created {} · {}", ago(p.created_time), grad))));

    match &p.swaps {
        Some(_) if collecting => {
            s.push_str(&format!("{}\n\n", italic("Trades of this pool are still being collected; the verdict comes from its config and operator.")));
        }
        Some(sw) => {
            s.push_str(&format!("{}\n", bold("This pool")));
            s.push_str(&format!(
                "• {}\n• {}\n• {}\n\n",
                esc(&format!("{} by {}", plural(sw.count, "trade", "trades"), plural(sw.traders, "wallet", "wallets"))),
                esc(&format!("creator's opening buy: {:.3} SOL", sw.creator_opening_buy_sol)),
                esc(&format!("{} sold tokens they never bought here", plural(sw.fanout_sellers, "wallet", "wallets")))
            ));
        }
        None => {
            s.push_str(&format!("{}\n\n", italic("Trades of this pool are not sampled; the verdict comes from its config and operator.")));
        }
    }

    if let Some(r) = r {
        s.push_str(&reasons(r, 5));
    }
    s.push_str(&italic(DISCLAIMER));
    s
}

fn dur(secs: i64) -> String {
    let s = secs.max(0);
    if s < 120 { format!("{s}s") } else if s < 7200 { format!("{}m", s / 60) } else { format!("{}h", s / 3600) }
}

/// Оператор: кластер конфигов из кэша (все записи с тем же номером кластера).
pub fn operator(r: &ConfigRisk, all: &HashMap<String, ConfigRisk>) -> String {
    let cluster = r.u("cluster");
    let size = r.u("cluster_size");
    // кластер без синтетики — просто связанные конфиги, а не «оператор»
    let any_red = size > 1
        && all.values().any(|x| x.u("cluster") == cluster && matches!(x.verdict().as_str(), "RED" | "RED-LINK"));
    let heading = if size <= 1 || any_red { "Operator" } else { "Linked configs" };
    let mut s = format!("{}\n\n", bold(heading));
    // адреса оператора сокращены (как в API без ключа): бот не выдаёт готовый список операторов
    s.push_str(&format!("Top creator {}\n", code(&short(&r.s("top_creator")))));
    s.push_str(&format!("Fee claimer {}\n", code(&short(&r.s("fee_claimer")))));
    s.push_str(&format!(
        "Leftover receiver {} {}\n\n",
        code(&short(&r.s("leftover_receiver"))),
        esc(&format!("(in {})", plural(r.u("leftover_receiver_configs"), "config", "configs")))
    ));
    if size <= 1 {
        s.push_str(&esc(&format!(
            "Single config, {} pools by {} creator(s). No other configs share its creator, fee or leftover receiver, or wallets.",
            r.u("pools"),
            r.u("creators")
        )));
        s.push('\n');
    } else {
        let mut members: Vec<&ConfigRisk> = all.values().filter(|x| x.u("cluster") == cluster).collect();
        members.sort_by(|a, b| b.u("pools").cmp(&a.u("pools")));
        s.push_str(&format!(
            "{}\n",
            esc(&format!(
                "Linked to {} configs by shared creators, fee/leftover receivers or wallets{}:",
                members.len(),
                if any_red { "" } else { " (no synthetic launches observed among them)" }
            ))
        ));
        for m in members.iter().take(10) {
            s.push_str(&format!(
                "{} {} {}\n",
                emoji(&m.verdict()),
                code(&short(&m.s("config"))),
                esc(&format!("{} · {} pools", m.verdict(), m.u("pools")))
            ));
        }
        if members.len() > 10 {
            s.push_str(&italic(&format!("+{} more", members.len() - 10)));
            s.push('\n');
        }
    }
    s
}

pub fn recent(config: &str, rows: &[(String, String, Option<i64>, bool)]) -> String {
    let mut s = format!("{}\n{}\n\n", bold("Recent launches"), code(config));
    if rows.is_empty() {
        s.push_str(&esc("No pools recorded yet."));
        return s;
    }
    for (pool, mint, t, grad) in rows {
        s.push_str(&format!(
            "• {} {}\n",
            link(&short(mint), &format!("https://solscan.io/token/{mint}")),
            esc(&format!("{} · {} · pool {}", ago(*t), if *grad { "graduated" } else { "no graduation seen" }, short(pool)))
        ));
    }
    s
}

pub fn stats(all: &HashMap<String, ConfigRisk>, last24: &HashMap<String, u64>, cache_age_secs: Option<u64>) -> String {
    let order = ["RED", "RED-LINK", "AMBER", "SELF-GRAD", "GREEN"];
    let mut cfgs: HashMap<String, (u64, u64)> = HashMap::new();
    for r in all.values() {
        let e = cfgs.entry(r.verdict()).or_default();
        e.0 += 1;
        e.1 += r.u("pools");
    }
    let mut day: HashMap<String, u64> = HashMap::new();
    let mut unknown = 0u64;
    for (c, n) in last24 {
        match all.get(c) {
            Some(r) => *day.entry(r.verdict()).or_default() += n,
            None => unknown += n,
        }
    }
    let total_day: u64 = day.values().sum::<u64>() + unknown;
    let mut s = format!("{}\n\n", bold("DBC Radar — what we see"));
    s.push_str(&format!("{}\n", bold(&format!("Launches in the last 24h: {total_day}"))));
    for v in order {
        let n = day.get(v).copied().unwrap_or(0);
        if n > 0 {
            let pct = if total_day > 0 { 100.0 * n as f64 / total_day as f64 } else { 0.0 };
            s.push_str(&format!("{} {}\n", emoji(v), esc(&format!("{} — {n} ({pct:.0}%)", title(v)))));
        }
    }
    if unknown > 0 {
        s.push_str(&format!("❔ {}\n", esc(&format!("New configs, not analysed yet — {unknown}"))));
    }
    s.push_str(&format!("\n{}\n", bold("All observed configs")));
    for v in order {
        if let Some((c, p)) = cfgs.get(v) {
            s.push_str(&format!("{} {}\n", emoji(v), esc(&format!("{v}: {c} configs, {p} pools"))));
        }
    }
    if let Some(a) = cache_age_secs {
        s.push_str(&format!("\n{}", italic(&format!("Analysis updated {} min ago.", a / 60))));
    }
    s
}

pub fn start() -> String {
    format!(
        "{}\n\n{}\n\n{}\n{}\n\n{}",
        bold("DBC Radar"),
        esc("Risk and origin check for Meteora DBC launches — before the first buy."),
        esc("Send a token, pool or config address (or a Solscan / DexScreener / Jupiter link)."),
        esc("Commands: /check <address>, /latest, /stats, /how"),
        italic(DISCLAIMER)
    )
}

pub fn how() -> String {
    let lines = [
        "Every DBC launch follows the rules of a config account: who gets the liquidity after graduation, how much is locked, how much supply goes to a leftover receiver.",
        "DBC Radar reads these rules from chain, so risky configs are visible before anyone buys.",
        "It also watches trading in the pools: if most volume comes from the creator and wallets that reappear across the config's launches, the launches are synthetic.",
        "New configs are linked to known operators through shared creators, fee and leftover receivers and wallets.",
    ];
    let mut s = format!("{}\n\n", bold("How it works"));
    for l in lines {
        s.push_str(&format!("• {}\n", esc(l)));
    }
    s.push_str(&format!(
        "\n🔴 {}\n🟠 {}\n⚪ {}\n🟢 {}",
        esc("RED / RED-LINK — synthetic launches or linked to their operator"),
        esc("AMBER — risky config: the creator or launchpad can take most of the buyers' SOL or liquidity at or after migration"),
        esc("SELF-GRAD — instant graduation, no market on the curve"),
        esc("GREEN — no major red flags; minor signals are listed in the report")
    ));
    s
}

/// Последние рискованные запуски: (pool, base_mint, created_time, verdict).
pub fn latest(rows: &[(String, String, Option<i64>, String)]) -> String {
    let mut s = format!("{}\n", bold("Latest risky launches"));
    if rows.is_empty() {
        s.push_str(&esc("No RED or AMBER launches in the last 2 hours."));
        return s;
    }
    s.push_str(&format!("{}\n\n", esc("Tap a launch to see why it was flagged.")));
    for (_, mint, t, v) in rows {
        s.push_str(&format!("{} {} {}\n", emoji(v), code(&short(mint)), esc(&format!("{} · {}", title(v), ago(*t)))));
    }
    s
}

pub fn short_addr(a: &str) -> String {
    short(a)
}
