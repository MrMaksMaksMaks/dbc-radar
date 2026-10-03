//! `dbc-collector trace <pool prefix> [--csv solscan_export.csv]`
//!
//! Собирает цепочку доказательств по одному пулу в Markdown со ссылками на Solscan:
//!   1. запуск и стартовая покупка создателя (из базы);
//!   2. откуда у «продавцов извне» взялись токены: для каждого такого кошелька
//!      ищется последняя транзакция перед его первой продажей, в которой его баланс
//!      токена пула вырос, и определяется, с какого адреса токены ушли (RPC);
//!   3. продажи этих кошельков обратно в пул (из базы);
//!   4. graduation (из базы);
//!   5. выход создателя после миграции: вывод остатка, снятие ликвидности, продажа
//!      (из CSV-выгрузки Solscan «DeFi Activities» кошелька создателя, если передана).

use crate::events::DBC_PROGRAM_ID;
use crate::rpc::Rpc;
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OpenFlags};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};

/// Сколько подписей кошелька просматривать назад от его первой продажи.
const LOOKBACK: usize = 100;

fn tx_link(sig: &str) -> String {
    format!("[{}…](https://solscan.io/tx/{})", &sig[..sig.len().min(10)], sig)
}
fn addr_link(a: &str) -> String {
    format!("[{}…](https://solscan.io/account/{})", &a[..a.len().min(8)], a)
}

struct PoolRow {
    pool: String,
    config: String,
    creator: String,
    base_mint: String,
    created_sig: String,
    created_time: Option<i64>,
}

struct Sell {
    sig: String,
    time: Option<i64>,
    tokens: u64,
    sol: u64,
}

/// Изменение баланса токена `mint` по владельцам в одной транзакции (post − pre).
fn token_deltas(tx: &Value, mint: &str) -> HashMap<String, i128> {
    let mut out: HashMap<String, i128> = HashMap::new();
    let mut add = |arr: Option<&Value>, sign: i128| {
        if let Some(items) = arr.and_then(Value::as_array) {
            for b in items {
                if b.get("mint").and_then(Value::as_str) != Some(mint) {
                    continue;
                }
                let owner = b.get("owner").and_then(Value::as_str).unwrap_or("?").to_string();
                let amt: i128 = b
                    .pointer("/uiTokenAmount/amount")
                    .and_then(Value::as_str)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                *out.entry(owner).or_default() += sign * amt;
            }
        }
    };
    add(tx.pointer("/meta/postTokenBalances"), 1);
    add(tx.pointer("/meta/preTokenBalances"), -1);
    out.retain(|_, v| *v != 0);
    out
}

fn touches_program(tx: &Value, program: &str) -> bool {
    let mut keys: Vec<&str> = Vec::new();
    for p in ["/transaction/message/accountKeys", "/meta/loadedAddresses/writable", "/meta/loadedAddresses/readonly"] {
        if let Some(a) = tx.pointer(p).and_then(Value::as_array) {
            keys.extend(a.iter().filter_map(Value::as_str));
        }
    }
    keys.contains(&program)
}

struct Inflow {
    sig: String,
    time: Option<i64>,
    amount: i128,
    sources: Vec<String>,
}

/// Последний входящий перевод токена `mint` на кошелёк перед подписью `before`.
async fn find_inflow(rpc: &Rpc, wallet: &str, mint: &str, before: &str) -> Result<Option<Inflow>> {
    let sigs = rpc.get_signatures(wallet, None, Some(before), LOOKBACK).await?;
    for s in sigs {
        if s.failed {
            continue;
        }
        let Some(tx) = rpc.get_transaction(&s.signature).await? else { continue };
        if touches_program(&tx, DBC_PROGRAM_ID) {
            continue; // это своп в DBC, не перевод
        }
        let d = token_deltas(&tx, mint);
        let got = d.get(wallet).copied().unwrap_or(0);
        if got > 0 {
            let mut sources: Vec<String> = d.iter().filter(|(_, v)| **v < 0).map(|(k, _)| k.clone()).collect();
            sources.sort();
            return Ok(Some(Inflow { sig: s.signature, time: s.block_time, amount: got, sources }));
        }
    }
    Ok(None)
}

fn fmt_tok(raw: i128, dec: u32) -> String {
    let v = raw as f64 / 10f64.powi(dec as i32);
    if v.abs() >= 1000.0 { format!("{:.0}", v) } else { format!("{:.2}", v) }
}

fn rel(t: Option<i64>, t0: Option<i64>) -> String {
    match (t, t0) {
        (Some(a), Some(b)) => format!("+{}s", a - b),
        _ => "?".into(),
    }
}

/// Строки CSV-выгрузки Solscan «DeFi Activities», относящиеся к токену.
fn csv_exit_rows(path: &str, mint: &str) -> Result<Vec<Vec<String>>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {path}"))?;
    let mut rows = Vec::new();
    for line in text.lines().skip(1) {
        let cols: Vec<String> = line.split(',').map(|s| s.trim_matches('"').to_string()).collect();
        if cols.len() >= 13 && (cols[5] == mint || cols[8] == mint) {
            rows.push(cols);
        }
    }
    Ok(rows)
}

pub async fn run(rpc: &Rpc, db_path: &str, prefix: &str, csv: Option<&str>) -> Result<()> {
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut st = conn.prepare(
        "SELECT pool, config, creator, base_mint, created_sig, created_time FROM pools WHERE pool LIKE ?1 || '%'",
    )?;
    let pools: Vec<PoolRow> = st
        .query_map(params![prefix], |r| {
            Ok(PoolRow {
                pool: r.get(0)?,
                config: r.get(1)?,
                creator: r.get(2)?,
                base_mint: r.get(3)?,
                created_sig: r.get(4)?,
                created_time: r.get(5)?,
            })
        })?
        .collect::<std::result::Result<_, _>>()?;
    let p = match pools.len() {
        1 => &pools[0],
        0 => bail!("no pool starting with {prefix}"),
        n => bail!("{n} pools start with {prefix}, use a longer prefix"),
    };
    let dec: u32 = 6; // токены DBC в наших данных — 6 знаков; уточняется ниже по конфигу, если есть
    let t0 = p.created_time;

    println!("# Evidence trace: pool {}\n", p.pool);
    println!("- token: {}", addr_link(&p.base_mint));
    println!("- config: {}", addr_link(&p.config));
    println!("- creator: {}", addr_link(&p.creator));
    println!("- pool created: {} ({})\n", tx_link(&p.created_sig), t0.map(|t| t.to_string()).unwrap_or("?".into()));

    // 1. Покупки создателя
    println!("## 1. Creator's opening buy\n");
    let mut st = conn.prepare(
        "SELECT signature, block_time, included_fee_input_amount, output_amount FROM swaps
         WHERE pool = ?1 AND fee_payer = ?2 AND trade_direction = 1 ORDER BY slot, event_index",
    )?;
    let buys: Vec<(String, Option<i64>, i64, i64)> = st
        .query_map(params![p.pool, p.creator], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<std::result::Result<_, _>>()?;
    let threshold: i64 = conn
        .query_row("SELECT MAX(migration_threshold) FROM swaps WHERE pool = ?1", params![p.pool], |r| r.get::<_, Option<i64>>(0))?
        .unwrap_or(0);
    if buys.is_empty() {
        println!("No buys by the creator address were recorded in this pool.\n");
    }
    for (sig, t, sol, tok) in &buys {
        println!(
            "- {} {}: paid {:.4} SOL, received {} tokens ({:.0}% of the migration threshold)",
            tx_link(sig),
            rel(*t, t0),
            *sol as f64 / 1e9,
            fmt_tok(*tok as i128, dec),
            if threshold > 0 { 100.0 * *sol as f64 / threshold as f64 } else { 0.0 }
        );
    }
    println!();

    // Продавцы «извне» и их продажи
    let mut st = conn.prepare(
        "WITH buyers AS (SELECT DISTINCT fee_payer FROM swaps WHERE pool = ?1 AND trade_direction = 1)
         SELECT fee_payer, signature, block_time, included_fee_input_amount, output_amount FROM swaps
         WHERE pool = ?1 AND trade_direction = 0 AND fee_payer <> ?2
           AND fee_payer NOT IN (SELECT fee_payer FROM buyers)
         ORDER BY slot, event_index",
    )?;
    let mut sells: BTreeMap<String, Vec<Sell>> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    for row in st.query_map(params![p.pool, p.creator], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<i64>>(2)?, r.get::<_, i64>(3)?, r.get::<_, i64>(4)?))
    })? {
        let (w, sig, t, tok, sol) = row?;
        if !sells.contains_key(&w) {
            order.push(w.clone());
        }
        sells.entry(w).or_default().push(Sell { sig, time: t, tokens: tok as u64, sol: sol as u64 });
    }

    // 2. Откуда токены
    println!("## 2. Where the sellers' tokens came from\n");
    println!("{} wallets sold tokens in this pool without buying any here.\n", order.len());
    println!("| wallet | received | from | transfer tx | time |");
    println!("|---|---|---|---|---|");
    let (mut from_creator, mut from_other, mut unknown) = (0usize, 0usize, 0usize);
    let mut source_count: HashMap<String, usize> = HashMap::new();
    for w in &order {
        let first = &sells[w][0];
        match find_inflow(rpc, w, &p.base_mint, &first.sig).await {
            Ok(Some(inf)) => {
                let src = if inf.sources.is_empty() { "?".to_string() } else { inf.sources.join(", ") };
                if inf.sources.iter().any(|s| s == &p.creator) {
                    from_creator += 1;
                } else {
                    from_other += 1;
                }
                for s in &inf.sources {
                    *source_count.entry(s.clone()).or_default() += 1;
                }
                let src_md = src.split(", ").map(addr_link).collect::<Vec<_>>().join(", ");
                println!("| {} | {} | {} | {} | {} |", addr_link(w), fmt_tok(inf.amount, dec), src_md, tx_link(&inf.sig), rel(inf.time, t0));
            }
            Ok(None) => {
                unknown += 1;
                println!("| {} | ? | not found in last {} txs before first sell | - | - |", addr_link(w), LOOKBACK);
            }
            Err(e) => {
                unknown += 1;
                println!("| {} | ? | rpc error: {} | - | - |", addr_link(w), e);
            }
        }
    }
    println!();
    println!(
        "Summary: {} wallet(s) received tokens directly from the creator, {} from other addresses, {} not traced.",
        from_creator, from_other, unknown
    );
    let mut srcs: Vec<(String, usize)> = source_count.into_iter().collect();
    srcs.sort_by(|a, b| b.1.cmp(&a.1));
    if !srcs.is_empty() {
        println!("Most common sources:");
        for (s, n) in srcs.iter().take(5) {
            println!("- {} — {} wallet(s){}", addr_link(s), n, if s == &p.creator { " (creator)" } else { "" });
        }
    }
    println!();

    // 3. Продажи обратно в пул
    println!("## 3. Selling back into the pool\n");
    println!("| wallet | sells | tokens sold | SOL received | first sell |");
    println!("|---|---|---|---|---|");
    let (mut tot_tok, mut tot_sol) = (0u128, 0u128);
    for w in &order {
        let v = &sells[w];
        let tok: u128 = v.iter().map(|s| s.tokens as u128).sum();
        let sol: u128 = v.iter().map(|s| s.sol as u128).sum();
        tot_tok += tok;
        tot_sol += sol;
        println!(
            "| {} | {} | {} | {:.4} | {} {} |",
            addr_link(w),
            v.len(),
            fmt_tok(tok as i128, dec),
            sol as f64 / 1e9,
            tx_link(&v[0].sig),
            rel(v[0].time, t0)
        );
    }
    println!();
    println!("Total: {} tokens sold for {:.4} SOL.\n", fmt_tok(tot_tok as i128, dec), tot_sol as f64 / 1e9);

    // 4. Graduation
    println!("## 4. Graduation\n");
    match conn.query_row(
        "SELECT signature, block_time, quote_reserve FROM curve_complete WHERE pool = ?1",
        params![p.pool],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?, r.get::<_, i64>(2)?)),
    ) {
        Ok((sig, t, q)) => println!("- {} {}: curve complete with {:.4} SOL in the pool\n", tx_link(&sig), rel(t, t0), q as f64 / 1e9),
        Err(_) => println!("- not recorded (pool not graduated while tracked)\n"),
    }

    // 5. Выход
    println!("## 5. Exit after migration\n");
    match csv {
        None => println!("Pass `--csv <Solscan DeFi Activities export of the creator>` to include this step.\n"),
        Some(path) => {
            let rows = csv_exit_rows(path, &p.base_mint)?;
            if rows.is_empty() {
                println!("No rows for this token in the CSV (export window may not cover this launch).\n");
            }
            // только действия выхода: вывод остатка из DBC, операции в DAMM v2; по времени, затем по смыслу
            let rank = |c: &Vec<String>| -> u8 {
                match (c[3].as_str(), c[12].contains("dbcij"), c[12].contains("cpamd")) {
                    ("ACTIVITY_TOKEN_REMOVE_LIQ", true, _) => 0,
                    ("ACTIVITY_TOKEN_REMOVE_LIQ", _, true) => 1,
                    ("ACTIVITY_TOKEN_SWAP", _, true) => 2,
                    _ => 9,
                }
            };
            let mut rows: Vec<Vec<String>> = rows.into_iter().filter(|c| rank(c) < 9).collect();
            rows.sort_by_key(|c| (c[1].parse::<i64>().unwrap_or(0), rank(c)));
            for c in rows.iter() {
                let program = if c[12].contains("cpamd") { "DAMM v2" } else if c[12].contains("dbcij") { "DBC, leftover withdrawal" } else { "other" };
                let (t1, a1, t2, a2) = (&c[5], &c[6], &c[8], &c[9]);
                let side = |t: &str, a: &str, d: &str| -> String {
                    let dec: i32 = d.parse().unwrap_or(0);
                    let v: f64 = a.parse::<f64>().unwrap_or(0.0) / 10f64.powi(dec);
                    if t == "So11111111111111111111111111111111111111112" { format!("{v:.4} SOL") } else if t.is_empty() { String::new() } else { format!("{v:.0} tokens") }
                };
                let t: Option<i64> = c[1].parse().ok();
                println!(
                    "- {} {} {} by {} ({}): {} {}",
                    tx_link(&c[0]),
                    rel(t, t0),
                    c[3].trim_start_matches("ACTIVITY_"),
                    addr_link(&c[4]),
                    program,
                    side(t1, a1, &c[7]),
                    side(t2, a2, &c[10])
                );
            }
            println!();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn deltas_detect_transfer() {
        let tx = json!({
            "transaction": {"message": {"accountKeys": ["creator", "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"]}},
            "meta": {
                "preTokenBalances": [
                    {"accountIndex": 1, "mint": "M", "owner": "creator", "uiTokenAmount": {"amount": "1000"}},
                    {"accountIndex": 2, "mint": "OTHER", "owner": "creator", "uiTokenAmount": {"amount": "5"}}
                ],
                "postTokenBalances": [
                    {"accountIndex": 1, "mint": "M", "owner": "creator", "uiTokenAmount": {"amount": "400"}},
                    {"accountIndex": 3, "mint": "M", "owner": "sat1", "uiTokenAmount": {"amount": "600"}},
                    {"accountIndex": 2, "mint": "OTHER", "owner": "creator", "uiTokenAmount": {"amount": "5"}}
                ]
            }
        });
        let d = token_deltas(&tx, "M");
        assert_eq!(d.get("creator"), Some(&-600));
        assert_eq!(d.get("sat1"), Some(&600));
        assert_eq!(d.len(), 2);
        assert!(!touches_program(&tx, DBC_PROGRAM_ID));
    }
}
