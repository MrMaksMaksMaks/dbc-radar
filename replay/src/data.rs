//! Чтение данных сборщика (dbc.sqlite).

use crate::engine::{PoolInit, RecordedSwap};
use anyhow::{bail, Context, Result};
use dynamic_bonding_curve::state::{PoolConfig, SwapResult2};
use rusqlite::{params, Connection};

/// Дискриминатор аккаунта PoolConfig (из IDL).
pub const POOL_CONFIG_DISC: [u8; 8] = [26, 108, 14, 123, 116, 230, 129, 43];
/// Дискриминатор ConfigWithTransferHook: { config: PoolConfig, transfer_hook_program: Pubkey, padding }.
pub const CONFIG_WITH_TH_DISC: [u8; 8] = [0x28, 0xDC, 0xC2, 0xFB, 0x29, 0xC7, 0x7B, 0xFD];

pub fn open(path: &str) -> Result<Connection> {
    Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open {path}"))
}

/// Находит пул по началу адреса (например "28VR").
pub fn find_pool(conn: &Connection, prefix: &str) -> Result<PoolInit> {
    let mut st = conn.prepare(
        "SELECT pool, config, creator, base_mint, pool_type, activation_point, created_sig
         FROM pools WHERE pool LIKE ?1 || '%'",
    )?;
    let rows: Vec<PoolInit> = st
        .query_map(params![prefix], |r| {
            Ok(PoolInit {
                pool: r.get(0)?,
                config: r.get(1)?,
                creator: r.get(2)?,
                base_mint: r.get(3)?,
                pool_type: r.get::<_, i64>(4)? as u8,
                activation_point: r.get::<_, i64>(5)? as u64,
                created_sig: r.get(6)?,
            })
        })?
        .collect::<std::result::Result<_, _>>()?;
    match rows.len() {
        0 => bail!("no pool starting with {prefix}"),
        1 => Ok(rows.into_iter().next().unwrap()),
        n => bail!("{n} pools start with {prefix}, use a longer prefix"),
    }
}

pub fn load_config(conn: &Connection, config: &str) -> Result<PoolConfig> {
    Ok(load_config_ext(conn, config)?.0)
}

/// Конфиг и, для конфигов с transfer hook, адрес программы-хука.
/// ConfigWithTransferHook начинается с той же структуры PoolConfig, поэтому
/// все поля читаются одинаково.
pub fn load_config_ext(conn: &Connection, config: &str) -> Result<(PoolConfig, Option<String>)> {
    let raw: Option<Vec<u8>> = conn
        .query_row("SELECT raw FROM configs WHERE config = ?1", params![config], |r| r.get(0))
        .with_context(|| format!("config {config} not in db"))?;
    let raw = raw.context("config raw data is empty")?;
    let size = std::mem::size_of::<PoolConfig>();
    if raw.len() < 8 + size {
        bail!("config data too short: {} < {}", raw.len(), 8 + size);
    }
    let disc: [u8; 8] = raw[..8].try_into().unwrap();
    let hook = if disc == POOL_CONFIG_DISC {
        None
    } else if disc == CONFIG_WITH_TH_DISC {
        let h = raw.get(8 + size..8 + size + 32).context("transfer hook program missing")?;
        Some(bs58_encode(h))
    } else {
        bail!("account is not a DBC config");
    };
    Ok((bytemuck::pod_read_unaligned::<PoolConfig>(&raw[8..8 + size]), hook))
}

pub fn load_raw(conn: &Connection, config: &str) -> Option<Vec<u8>> {
    conn.query_row("SELECT raw FROM configs WHERE config = ?1", params![config], |r| r.get(0))
        .ok()
        .flatten()
}

fn bs58_encode(b: &[u8]) -> String {
    anchor_lang::prelude::Pubkey::try_from(b).map(|p| p.to_string()).unwrap_or_default()
}

pub fn load_swaps(conn: &Connection, pool: &str) -> Result<Vec<RecordedSwap>> {
    let mut st = conn.prepare(
        "SELECT signature, event_index, slot, event_timestamp, fee_payer, trade_direction,
                has_referral, swap_mode, amount_0, included_fee_input_amount,
                excluded_fee_input_amount, amount_left, output_amount, next_sqrt_price,
                trading_fee, protocol_fee, referral_fee, quote_reserve_amount
         FROM swaps WHERE pool = ?1",
    )?;
    let u = |v: i64| v as u64;
    let rows = st
        .query_map(params![pool], |r| {
            let sqrt: String = r.get(13)?;
            Ok(RecordedSwap {
                signature: r.get(0)?,
                event_index: r.get::<_, i64>(1)? as u32,
                slot: u(r.get(2)?),
                event_timestamp: u(r.get(3)?),
                fee_payer: r.get(4)?,
                trade_direction: r.get::<_, i64>(5)? as u8,
                has_referral: r.get(6)?,
                swap_mode: r.get::<_, i64>(7)? as u8,
                amount_0: u(r.get(8)?),
                result: SwapResult2 {
                    included_fee_input_amount: u(r.get(9)?),
                    excluded_fee_input_amount: u(r.get(10)?),
                    amount_left: u(r.get(11)?),
                    output_amount: u(r.get(12)?),
                    next_sqrt_price: sqrt.parse().unwrap_or(0),
                    trading_fee: u(r.get(14)?),
                    protocol_fee: u(r.get(15)?),
                    referral_fee: u(r.get(16)?),
                },
                quote_reserve_amount: u(r.get(17)?),
            })
        })?
        .collect::<std::result::Result<_, _>>()?;
    Ok(rows)
}

pub fn load_quote_mint(conn: &Connection, config: &str) -> Option<String> {
    conn.query_row("SELECT quote_mint FROM configs WHERE config = ?1", params![config], |r| r.get(0))
        .ok()
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dynamic_bonding_curve::state::PoolConfig;

    #[test]
    fn reads_both_config_kinds() {
        let c = PoolConfig::default();
        let body = bytemuck::bytes_of(&c).to_vec();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE configs(config, quote_mint, fee_claimer, raw, fetched_at);").unwrap();
        let mut plain = POOL_CONFIG_DISC.to_vec();
        plain.extend_from_slice(&body);
        let mut hooked = CONFIG_WITH_TH_DISC.to_vec();
        hooked.extend_from_slice(&body);
        hooked.extend_from_slice(&[7u8; 32]);
        hooked.extend_from_slice(&[0u8; 48]);
        conn.execute("INSERT INTO configs VALUES('a',NULL,NULL,?1,0)", params![plain]).unwrap();
        conn.execute("INSERT INTO configs VALUES('b',NULL,NULL,?1,0)", params![hooked]).unwrap();
        assert!(load_config_ext(&conn, "a").unwrap().1.is_none());
        let hook = load_config_ext(&conn, "b").unwrap().1.unwrap();
        assert_eq!(hook, anchor_lang::prelude::Pubkey::new_from_array([7u8; 32]).to_string());
        // размер как в программе: 8 + PoolConfig + 32 + 48 = 8 + 1120
        assert_eq!(hooked.len(), 8 + 1120);
    }
}
