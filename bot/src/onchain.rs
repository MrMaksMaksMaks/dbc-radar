//! Проверка адресов, которых нет в базе сборщика: бот сам читает данные через RPC.
//!
//!   * адрес пула DBC: читается аккаунт VirtualPool (или TransferHookPool) — из него
//!     берутся конфиг, создатель и токен;
//!   * адрес токена: самая ранняя транзакция токена — это создание пула (токен создаётся
//!     инструкцией инициализации пула), из неё декодируется событие EvtInitializePool.
//!
//! Найденный пул и его конфиг дописываются в базу (пул помечается как неотслеживаемый),
//! после чего движок может оценить конфиг как обычно.

use anyhow::{bail, Context, Result};
use base64::Engine;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::time::Duration;

use crate::store::now;

pub const DBC_PROGRAM_ID: &str = "dbcij3LWUppWqq96dh6gJWwBifmcGfLSB5D4DuSMaqN";
const TOKEN_PROGRAMS: &[&str] = &["TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA", "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb"];

/// Пулы Meteora, куда токены попадают после запуска: из них берём адреса токенов.
/// DAMM v2: Pool { pool_fees: PoolFeesStruct (160 байт), token_a_mint, token_b_mint, ... }.
/// DLMM: LbPair, token_x_mint / token_y_mint — смещения посчитаны по IDL DLMM.
const DAMM_V2_PROGRAM: &str = "cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG";
const DAMM_V2_POOL_DISC: [u8; 8] = [241, 154, 109, 4, 17, 177, 109, 188];
const DAMM_V2_MINTS: (usize, usize) = (8 + 160, 8 + 192);
const DLMM_PROGRAM: &str = "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo";
const DLMM_PAIR_DISC: [u8; 8] = [33, 11, 49, 98, 181, 101, 177, 13];
const DLMM_MINTS: (usize, usize) = (88, 120);
/// Типичные котируемые токены: в паре ищем «другой» токен.
const QUOTE_MINTS: &[&str] = &[
    "So11111111111111111111111111111111111111112",
    "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
    "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB",
];

/// Программы других лаунчпадов: если токен создан ими, это не DBC.
const OTHER_LAUNCHPADS: &[(&str, &str)] = &[
    ("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P", "pump.fun"),
    ("LanMV9sAd7wArD4vJFi2qDdfnVhFxYSUg6eADduJ3uj", "Raydium LaunchLab"),
    ("MoonCVVNZFSYkqNXP6bxHLPL6QQJiMagDL3qcqUQTrG", "Moonshot"),
];

const VIRTUAL_POOL_DISC: [u8; 8] = [213, 224, 5, 209, 98, 69, 119, 92];
const TRANSFER_HOOK_POOL_DISC: [u8; 8] = [237, 219, 184, 23, 42, 189, 169, 35];
const POOL_CONFIG_DISC: [u8; 8] = [26, 108, 14, 123, 116, 230, 129, 43];
const CONFIG_WITH_TH_DISC: [u8; 8] = [0x28, 0xDC, 0xC2, 0xFB, 0x29, 0xC7, 0x7B, 0xFD];
/// Anchor EVENT_IX_TAG (little-endian) и дискриминаторы EvtInitializePool (обычный и transfer-hook).
const EVENT_IX_TAG: [u8; 8] = [0xe4, 0x45, 0xa5, 0x2e, 0x51, 0xcb, 0x9a, 0x1d];
const EVT_INIT: [u8; 8] = [228, 50, 246, 85, 203, 66, 134, 37];
const EVT_INIT_TH: [u8; 8] = [213, 137, 164, 53, 193, 74, 15, 110];

/// Смещения полей в данных аккаунта пула: 8 байт дискриминатора + смещение в PoolState
/// (посчитаны offset_of! по коду программы DBC v0.2.1).
const OFF_CONFIG: usize = 8 + 64;
const OFF_CREATOR: usize = 8 + 96;
const OFF_BASE_MINT: usize = 8 + 128;
const OFF_ACTIVATION_POINT: usize = 8 + 288;
const OFF_POOL_TYPE: usize = 8 + 296;

#[derive(Debug, Clone, PartialEq)]
pub struct FoundPool {
    pub pool: String,
    pub config: String,
    pub creator: String,
    pub base_mint: String,
    pub pool_type: u8,
    pub activation_point: u64,
    pub created_sig: String,
    pub created_slot: u64,
    pub created_time: Option<i64>,
}

fn pk(b: &[u8]) -> String {
    bs58::encode(b).into_string()
}

fn u64_at(d: &[u8], off: usize) -> Option<u64> {
    d.get(off..off + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap()))
}

/// Разбор аккаунта пула DBC.
pub fn parse_pool_account(pool: &str, data: &[u8]) -> Option<FoundPool> {
    let disc: [u8; 8] = data.get(..8)?.try_into().ok()?;
    if disc != VIRTUAL_POOL_DISC && disc != TRANSFER_HOOK_POOL_DISC {
        return None;
    }
    Some(FoundPool {
        pool: pool.to_string(),
        config: pk(data.get(OFF_CONFIG..OFF_CONFIG + 32)?),
        creator: pk(data.get(OFF_CREATOR..OFF_CREATOR + 32)?),
        base_mint: pk(data.get(OFF_BASE_MINT..OFF_BASE_MINT + 32)?),
        pool_type: *data.get(OFF_POOL_TYPE)?,
        activation_point: u64_at(data, OFF_ACTIVATION_POINT)?,
        created_sig: String::new(),
        created_slot: 0,
        created_time: None,
    })
}

/// Разбор события EvtInitializePool из данных внутренней инструкции.
pub fn parse_init_event(data: &[u8]) -> Option<(String, String, String, String, u8, u64)> {
    if data.len() < 16 + 32 * 4 + 1 + 8 || data[..8] != EVENT_IX_TAG {
        return None;
    }
    let disc: [u8; 8] = data[8..16].try_into().ok()?;
    if disc != EVT_INIT && disc != EVT_INIT_TH {
        return None;
    }
    let p = &data[16..];
    Some((pk(&p[0..32]), pk(&p[32..64]), pk(&p[64..96]), pk(&p[96..128]), p[128], u64_at(p, 129)?))
}

/// Результат поиска адреса через RPC.
#[derive(Debug)]
pub enum Lookup {
    Found(FoundPool),
    /// адрес — пул Meteora DAMM v2 или DLMM; проверять нужно токен этого пула
    MeteoraPool { venue: &'static str, mint: String },
    /// токен запущен на другом лаунчпаде; bool — вывод сделан только по окончанию адреса
    OtherLaunchpad(&'static str, bool),
    /// это токен, но создание его пула не найдено в доступной истории узла
    MintWithoutHistory,
    NotDbc,
}

pub struct Rpc {
    http: reqwest::Client,
    url: String,
    history_url: String,
}

impl Rpc {
    pub fn new(url: &str, history_url: Option<&str>) -> Self {
        Self {
            http: reqwest::Client::builder().timeout(Duration::from_secs(20)).build().expect("http client"),
            url: url.to_string(),
            history_url: history_url.unwrap_or(url).to_string(),
        }
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.call_at(&self.url, method, params).await
    }

    /// Запросы, которым нужна полная история (подписи и транзакции прошлых дней).
    async fn call_history(&self, method: &str, params: Value) -> Result<Value> {
        self.call_at(&self.history_url, method, params).await
    }

    async fn call_at(&self, url: &str, method: &str, params: Value) -> Result<Value> {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let mut last = anyhow::anyhow!("no attempts");
        for attempt in 0..3u64 {
            match self.http.post(url).json(&body).send().await {
                Ok(r) if r.status().is_success() => {
                    let v: Value = r.json().await?;
                    if let Some(e) = v.get("error") {
                        bail!("{method}: {e}");
                    }
                    return Ok(v.get("result").cloned().unwrap_or(Value::Null));
                }
                Ok(r) => last = anyhow::anyhow!("{method}: HTTP {}", r.status()),
                Err(e) => last = anyhow::anyhow!("{method}: {e}"),
            }
            tokio::time::sleep(Duration::from_millis(500 * (attempt + 1))).await;
        }
        Err(last)
    }

    /// (владелец, данные) аккаунта или None, если аккаунта нет.
    pub async fn account(&self, addr: &str) -> Result<Option<(String, Vec<u8>)>> {
        let r = self.call("getAccountInfo", json!([addr, {"encoding": "base64", "commitment": "confirmed"}])).await?;
        let Some(v) = r.get("value").filter(|v| !v.is_null()) else { return Ok(None) };
        let owner = v.get("owner").and_then(Value::as_str).unwrap_or("").to_string();
        let data = v.pointer("/data/0").and_then(Value::as_str).unwrap_or("");
        Ok(Some((owner, base64::engine::general_purpose::STANDARD.decode(data)?)))
    }

    /// Пул по адресу токена: самая ранняя транзакция токена — создание пула.
    async fn pool_by_mint(&self, mint: &str) -> Result<MintOrigin> {
        let mut before: Option<String> = None;
        let mut oldest: Option<(String, u64, Option<i64>)> = None;
        for _ in 0..5 {
            let mut cfg = json!({"limit": 1000, "commitment": "confirmed"});
            if let Some(b) = &before {
                cfg["before"] = json!(b);
            }
            let page = self.call_history("getSignaturesForAddress", json!([mint, cfg])).await?;
            let arr = page.as_array().cloned().unwrap_or_default();
            if let Some(last) = arr.last() {
                let sig = last.get("signature").and_then(Value::as_str).unwrap_or("").to_string();
                let slot = last.get("slot").and_then(Value::as_u64).unwrap_or(0);
                let t = last.get("blockTime").and_then(Value::as_i64);
                before = Some(sig.clone());
                oldest = Some((sig, slot, t));
            }
            if arr.len() < 1000 {
                break;
            }
        }
        let Some((sig, slot, t)) = oldest else { return Ok(MintOrigin::Unknown) };
        let tx = self
            .call_history("getTransaction", json!([sig, {"encoding": "json", "commitment": "confirmed", "maxSupportedTransactionVersion": 1}]))
            .await?;
        if tx.is_null() {
            return Ok(MintOrigin::Unknown);
        }
        let str_arr = |p: &str| -> Vec<String> {
            tx.pointer(p).and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect()).unwrap_or_default()
        };
        let mut keys = str_arr("/transaction/message/accountKeys");
        keys.extend(str_arr("/meta/loadedAddresses/writable"));
        keys.extend(str_arr("/meta/loadedAddresses/readonly"));
        for (program, name) in OTHER_LAUNCHPADS {
            if keys.iter().any(|k| k == program) {
                return Ok(MintOrigin::Launchpad(name));
            }
        }
        for group in tx.pointer("/meta/innerInstructions").and_then(Value::as_array).cloned().unwrap_or_default() {
            for ix in group.get("instructions").and_then(Value::as_array).cloned().unwrap_or_default() {
                let pid = ix.get("programIdIndex").and_then(Value::as_u64).unwrap_or(u64::MAX) as usize;
                if keys.get(pid).map(String::as_str) != Some(DBC_PROGRAM_ID) {
                    continue;
                }
                let Some(data) = ix.get("data").and_then(Value::as_str).and_then(|d| bs58::decode(d).into_vec().ok()) else { continue };
                if let Some((pool, config, creator, base_mint, pool_type, activation_point)) = parse_init_event(&data) {
                    if base_mint == mint {
                        return Ok(MintOrigin::Dbc(FoundPool {
                            pool,
                            config,
                            creator,
                            base_mint,
                            pool_type,
                            activation_point,
                            created_sig: sig,
                            created_slot: slot,
                            created_time: t,
                        }));
                    }
                }
            }
        }
        Ok(MintOrigin::Unknown)
    }

    /// Найти пул DBC по адресу пула или токена (или пула Meteora, где торгуется токен).
    pub async fn resolve(&self, addr: &str) -> Result<Lookup> {
        let Some((owner, data)) = self.account(addr).await? else { return Ok(Lookup::NotDbc) };
        if owner == DBC_PROGRAM_ID {
            return Ok(parse_pool_account(addr, &data).map(Lookup::Found).unwrap_or(Lookup::NotDbc));
        }
        if owner == DAMM_V2_PROGRAM {
            return Ok(pair_mint(&data, DAMM_V2_POOL_DISC, DAMM_V2_MINTS)
                .map(|mint| Lookup::MeteoraPool { venue: "DAMM v2", mint })
                .unwrap_or(Lookup::NotDbc));
        }
        if owner == DLMM_PROGRAM {
            return Ok(pair_mint(&data, DLMM_PAIR_DISC, DLMM_MINTS)
                .map(|mint| Lookup::MeteoraPool { venue: "DLMM", mint })
                .unwrap_or(Lookup::NotDbc));
        }
        if TOKEN_PROGRAMS.contains(&owner.as_str()) {
            return Ok(match self.pool_by_mint(addr).await? {
                MintOrigin::Dbc(p) => Lookup::Found(p),
                MintOrigin::Launchpad(name) => Lookup::OtherLaunchpad(name, false),
                MintOrigin::Unknown => match launchpad_by_suffix(addr) {
                    Some(name) => Lookup::OtherLaunchpad(name, true),
                    None => Lookup::MintWithoutHistory,
                },
            });
        }
        Ok(Lookup::NotDbc)
    }
}

enum MintOrigin {
    Dbc(FoundPool),
    Launchpad(&'static str),
    Unknown,
}

/// Токен пары, который не является котируемым (SOL, USDC, USDT).
pub fn pair_mint(data: &[u8], disc: [u8; 8], (a, b): (usize, usize)) -> Option<String> {
    if data.get(..8)? != disc {
        return None;
    }
    let ma = pk(data.get(a..a + 32)?);
    let mb = pk(data.get(b..b + 32)?);
    if QUOTE_MINTS.contains(&ma.as_str()) { Some(mb) } else { Some(ma) }
}

/// Лаунчпады с «фирменным» окончанием адресов токенов.
pub fn launchpad_by_suffix(mint: &str) -> Option<&'static str> {
    if mint.ends_with("pump") {
        Some("pump.fun")
    } else if mint.ends_with("bonk") {
        Some("letsbonk.fun (Raydium LaunchLab)")
    } else {
        None
    }
}

/// Дописать найденный пул и (если его ещё нет) сырые данные конфига в базу сборщика.
pub async fn store_found(db_path: &str, rpc: &Rpc, p: &FoundPool) -> Result<()> {
    let config_raw = {
        let conn = Connection::open(db_path)?;
        let known: bool = conn
            .query_row("SELECT EXISTS(SELECT 1 FROM configs WHERE config = ?1 AND raw IS NOT NULL)", params![p.config], |r| r.get(0))?;
        if known { None } else { Some(()) }
    };
    let raw = if config_raw.is_some() {
        let (owner, data) = rpc.account(&p.config).await?.context("config account not found")?;
        let disc: [u8; 8] = data.get(..8).and_then(|d| d.try_into().ok()).unwrap_or_default();
        if owner != DBC_PROGRAM_ID || (disc != POOL_CONFIG_DISC && disc != CONFIG_WITH_TH_DISC) {
            bail!("not a DBC config account");
        }
        Some(data)
    } else {
        None
    };

    let db_path = db_path.to_string();
    let p = p.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let conn = Connection::open(&db_path)?;
        conn.busy_timeout(Duration::from_secs(10))?;
        if let Some(raw) = raw {
            let key = |a: usize| raw.get(a..a + 32).map(pk);
            conn.execute(
                "INSERT OR IGNORE INTO configs (config, quote_mint, fee_claimer, raw, fetched_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![p.config, key(8), key(40), raw, now()],
            )?;
        }
        // пул найден по запросу: не отслеживается, сборщик его не опрашивает
        conn.execute(
            "INSERT OR IGNORE INTO pools
             (pool, config, creator, base_mint, pool_type, activation_point, transfer_hook,
              created_sig, created_slot, created_time, discovered_at, tracked, last_sig, done)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7, ?8, ?9, ?10, 0, NULL, 1)",
            params![
                p.pool, p.config, p.creator, p.base_mint, p.pool_type, p.activation_point as i64,
                p.created_sig, p.created_slot as i64, p.created_time, now()
            ],
        )?;
        if !p.created_sig.is_empty() {
            let _ = conn.execute(
                "INSERT OR IGNORE INTO discovery (signature, source, seen_at) VALUES (?1, 'bot', ?2)",
                params![p.created_sig, now()],
            );
        }
        Ok(())
    })
    .await??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pool_account_fields() {
        let mut d = vec![0u8; 8 + 416];
        d[..8].copy_from_slice(&VIRTUAL_POOL_DISC);
        d[OFF_CONFIG..OFF_CONFIG + 32].copy_from_slice(&[1u8; 32]);
        d[OFF_CREATOR..OFF_CREATOR + 32].copy_from_slice(&[2u8; 32]);
        d[OFF_BASE_MINT..OFF_BASE_MINT + 32].copy_from_slice(&[3u8; 32]);
        d[OFF_ACTIVATION_POINT..OFF_ACTIVATION_POINT + 8].copy_from_slice(&777u64.to_le_bytes());
        d[OFF_POOL_TYPE] = 1;
        let p = parse_pool_account("P", &d).unwrap();
        assert_eq!(p.config, pk(&[1u8; 32]));
        assert_eq!(p.creator, pk(&[2u8; 32]));
        assert_eq!(p.base_mint, pk(&[3u8; 32]));
        assert_eq!((p.pool_type, p.activation_point), (1, 777));
        d[0] = 0;
        assert!(parse_pool_account("P", &d).is_none());
    }

    #[test]
    fn reads_pair_mints_and_suffixes() {
        let mut d = vec![0u8; 8 + 1104];
        d[..8].copy_from_slice(&DAMM_V2_POOL_DISC);
        d[DAMM_V2_MINTS.0..DAMM_V2_MINTS.0 + 32].copy_from_slice(&[7u8; 32]);
        let wsol = bs58::decode(QUOTE_MINTS[0]).into_vec().unwrap();
        d[DAMM_V2_MINTS.1..DAMM_V2_MINTS.1 + 32].copy_from_slice(&wsol);
        assert_eq!(pair_mint(&d, DAMM_V2_POOL_DISC, DAMM_V2_MINTS), Some(pk(&[7u8; 32])));
        // токен на месте b, SOL на месте a
        d[DAMM_V2_MINTS.0..DAMM_V2_MINTS.0 + 32].copy_from_slice(&wsol);
        d[DAMM_V2_MINTS.1..DAMM_V2_MINTS.1 + 32].copy_from_slice(&[9u8; 32]);
        assert_eq!(pair_mint(&d, DAMM_V2_POOL_DISC, DAMM_V2_MINTS), Some(pk(&[9u8; 32])));
        assert_eq!(launchpad_by_suffix("7VertkgF9KLhxxJXHX6uaWuoYZTP9LdGj2bWmVXVpump"), Some("pump.fun"));
        assert_eq!(launchpad_by_suffix("599xQ7SKdQ2zvzVgci7tYWzhP9qBnxBxJHrFGpTu2jFf"), None);
    }

    #[test]
    fn parses_init_event() {
        let mut d = Vec::new();
        d.extend_from_slice(&EVENT_IX_TAG);
        d.extend_from_slice(&EVT_INIT);
        for b in 1..=4u8 {
            d.extend_from_slice(&[b; 32]);
        }
        d.push(0);
        d.extend_from_slice(&42u64.to_le_bytes());
        let (pool, config, creator, mint, t, ap) = parse_init_event(&d).unwrap();
        assert_eq!((pool, config, creator, mint), (pk(&[1; 32]), pk(&[2; 32]), pk(&[3; 32]), pk(&[4; 32])));
        assert_eq!((t, ap), (0, 42));
    }
}
