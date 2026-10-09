//! config-dump: все поля конфига и пула DBC прямо из структур программы —
//! ровно то, что может прочитать любой бот из публичных аккаунтов.
//!
//!   dbc-replay dbc.sqlite config-dump <минт | пул | конфиг (адрес или начало)>
//!
//! Конфиг берётся из базы коллектора, а если его там нет — по RPC.
//! Состояние пула (резервы, накопленные комиссии, статус миграции) всегда читается по RPC:
//! HISTORY_RPC_URL или RPC_URL из окружения или из ./.env.

use crate::data::{CONFIG_WITH_TH_DISC, POOL_CONFIG_DISC};
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use dynamic_bonding_curve::state::{PoolConfig, PoolState};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

const DBC_PROGRAM: &str = "dbcij3LWUppWqq96dh6gJWwBifmcGfLSB5D4DuSMaqN";

fn rpc_url() -> Option<String> {
    for k in ["HISTORY_RPC_URL", "RPC_URL"] {
        if let Ok(v) = std::env::var(k) {
            if !v.trim().is_empty() {
                return Some(v.trim().to_string());
            }
        }
    }
    // запасной вариант: ./.env рядом с базой
    let text = std::fs::read_to_string(".env").ok()?;
    for k in ["HISTORY_RPC_URL", "RPC_URL"] {
        for line in text.lines() {
            if let Some(v) = line.strip_prefix(&format!("{k}=")) {
                let v = v.trim().trim_matches('"');
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

fn rpc(url: &str, method: &str, params: Value) -> Result<Value> {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    let v: Value = ureq::post(url).send_json(body)?.into_json()?;
    if let Some(e) = v.get("error") {
        bail!("{method}: {e}");
    }
    Ok(v["result"].clone())
}

fn account(url: &str, addr: &str) -> Result<Option<(Vec<u8>, String)>> {
    let r = rpc(url, "getAccountInfo", json!([addr, {"encoding": "base64"}]))?;
    let Some(v) = r.get("value").filter(|v| !v.is_null()) else { return Ok(None) };
    let data = v["data"][0].as_str().context("no data")?;
    let owner = v["owner"].as_str().unwrap_or_default().to_string();
    Ok(Some((base64::engine::general_purpose::STANDARD.decode(data)?, owner)))
}

/// Пул по минту через getProgramAccounts (если минта нет в базе).
fn pool_by_mint(url: &str, mint: &str) -> Result<Option<String>> {
    let off = 8 + std::mem::offset_of!(PoolState, base_mint);
    let size = 8 + std::mem::size_of::<PoolState>();
    let r = rpc(
        url,
        "getProgramAccounts",
        json!([DBC_PROGRAM, {"encoding": "base64", "dataSlice": {"offset": 0, "length": 0},
            "filters": [{"dataSize": size}, {"memcmp": {"offset": off, "bytes": mint}}]}]),
    )?;
    Ok(r.as_array().and_then(|a| a.first()).and_then(|x| x["pubkey"].as_str()).map(str::to_string))
}

fn parse_config(raw: &[u8]) -> Result<(PoolConfig, Option<String>)> {
    let size = std::mem::size_of::<PoolConfig>();
    if raw.len() < 8 + size {
        bail!("account too short for PoolConfig");
    }
    let disc: [u8; 8] = raw[..8].try_into().unwrap();
    let hook = if disc == POOL_CONFIG_DISC {
        None
    } else if disc == CONFIG_WITH_TH_DISC {
        raw.get(8 + size..8 + size + 32)
            .map(|b| anchor_lang::prelude::Pubkey::try_from(b).map(|p| p.to_string()).unwrap_or_default())
    } else {
        bail!("account is not a DBC config");
    };
    Ok((bytemuck::pod_read_unaligned::<PoolConfig>(&raw[8..8 + size]), hook))
}

pub fn run(conn: &Connection, target: &str) -> Result<()> {
    let like = format!("{target}%");
    // 1. база: минт → пул/конфиг, пул → конфиг, конфиг
    let mut pool: Option<String> = conn
        .query_row("SELECT pool FROM pools WHERE base_mint LIKE ?1 LIMIT 1", params![like], |r| r.get(0))
        .optional()?;
    if pool.is_none() {
        pool = conn
            .query_row("SELECT pool FROM pools WHERE pool LIKE ?1 LIMIT 1", params![like], |r| r.get(0))
            .optional()?;
    }
    let mut config: Option<String> = match &pool {
        Some(p) => conn.query_row("SELECT config FROM pools WHERE pool = ?1", params![p], |r| r.get(0)).optional()?,
        None => conn
            .query_row("SELECT config FROM configs WHERE config LIKE ?1 LIMIT 1", params![like], |r| r.get(0))
            .optional()?,
    };
    let url = rpc_url();

    // 2. не нашли в базе — полный адрес по RPC: конфиг, пул или минт
    if pool.is_none() && config.is_none() {
        let url = url.as_deref().ok_or_else(|| anyhow!("{target} not in db and no HISTORY_RPC_URL/RPC_URL for an on-chain lookup"))?;
        match account(url, target)? {
            Some((raw, owner)) if owner == DBC_PROGRAM && raw.len() >= 8 && (raw[..8] == POOL_CONFIG_DISC || raw[..8] == CONFIG_WITH_TH_DISC) => {
                config = Some(target.to_string())
            }
            Some((_, owner)) if owner == DBC_PROGRAM => pool = Some(target.to_string()),
            _ => pool = pool_by_mint(url, target)?,
        }
        if pool.is_none() && config.is_none() {
            bail!("{target}: no DBC pool or config found");
        }
    }

    // 3. пул по RPC (и конфиг из него, если ещё неизвестен)
    let mut pool_state: Option<PoolState> = None;
    if let (Some(p), Some(url)) = (&pool, url.as_deref()) {
        if let Some((raw, _)) = account(url, p)? {
            let size = std::mem::size_of::<PoolState>();
            if raw.len() >= 8 + size {
                let st: PoolState = bytemuck::pod_read_unaligned(&raw[8..8 + size]);
                if config.is_none() {
                    config = Some(st.config.to_string());
                }
                pool_state = Some(st);
            }
        }
    }
    let config = config.context("config unknown")?;

    // 4. конфиг: база, иначе RPC
    let (cfg, hook, source) = match crate::data::load_raw(conn, &config) {
        Some(raw) => {
            let (c, h) = parse_config(&raw)?;
            (c, h, "collector db (saved when the pool was created)")
        }
        None => {
            let url = url.as_deref().context("config not in db and no RPC url")?;
            let (raw, _) = account(url, &config)?.context("config account not found")?;
            let (c, h) = parse_config(&raw)?;
            (c, h, "RPC (live)")
        }
    };

    println!("== CONFIG {config}");
    println!("   source: {source}; transfer hook program: {}", hook.as_deref().unwrap_or("none"));
    println!("   fees: numerators are out of 1_000_000_000; base_fee mode 0 linear / 1 exponential / 2 rate limiter;");
    println!("   first_factor = periods, second_factor = period length (slots if activation_type 0, seconds if 1), third_factor = reduction");
    println!("{}", compact(&format!("{cfg:#?}")));

    match (&pool, pool_state) {
        (Some(p), Some(st)) => {
            println!("\n== POOL {p}  (live state from RPC; reserves and fees in base units, SOL = /1e9)");
            println!("{}", compact(&format!("{st:#?}")));
        }
        (Some(p), None) => println!("\n== POOL {p}: state not read (set HISTORY_RPC_URL or RPC_URL)"),
        (None, _) => println!("\n(config only: no pool given)"),
    }
    Ok(())
}

/// Сворачивает числовые массивы из `{:#?}` (паддинги, байты) в одну строку.
fn compact(s: &str) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let mut out = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        let l = lines[i];
        if l.trim_end().ends_with('[') {
            let mut j = i + 1;
            let mut nums = Vec::new();
            while j < lines.len() {
                let t = lines[j].trim().trim_end_matches(',');
                if t.parse::<i128>().is_ok() {
                    nums.push(t.to_string());
                    j += 1;
                } else {
                    break;
                }
            }
            if !nums.is_empty() && j < lines.len() && lines[j].trim().starts_with(']') {
                let close = lines[j].trim();
                out.push(format!("{}{}{}", l.trim_end(), nums.join(", "), close));
                i = j + 1;
                continue;
            }
        }
        out.push(l.to_string());
        i += 1;
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    #[test]
    fn compacts_numeric_arrays() {
        let s = "A {\n    padding: [\n        0,\n        1,\n    ],\n    x: 5,\n}";
        assert_eq!(super::compact(s), "A {\n    padding: [0, 1],\n    x: 5,\n}");
    }
}
