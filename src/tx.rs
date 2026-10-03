//! Разбор транзакции из getTransaction (encoding = "json"):
//! находим внутренние инструкции, адресованные программе DBC, и декодируем события.

use crate::events::{decode_event_ix, DbcEvent, DBC_PROGRAM_ID};
use anyhow::{Context, Result};
use serde_json::Value;

#[allow(dead_code)]
#[derive(Debug)]
pub struct ParsedTx {
    pub slot: u64,
    pub block_time: Option<i64>,
    /// Плательщик комиссии (первый подписант). Для анализа трейдеров это
    /// приближение: при оплате через релейер реальный владелец может отличаться.
    pub fee_payer: String,
    pub failed: bool,
    /// (порядковый номер события DBC в транзакции, событие)
    pub events: Vec<(u32, DbcEvent)>,
}

fn str_array(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

pub fn parse_tx(tx: &Value) -> Result<ParsedTx> {
    let slot = tx.get("slot").and_then(Value::as_u64).context("no slot")?;
    let block_time = tx.get("blockTime").and_then(Value::as_i64);
    let meta = tx.get("meta").context("no meta")?;
    let failed = !meta.get("err").map(Value::is_null).unwrap_or(true);

    // Порядок ключей для v0-транзакций: статические, затем загруженные writable, затем readonly.
    let mut keys = str_array(tx.pointer("/transaction/message/accountKeys"));
    keys.extend(str_array(meta.pointer("/loadedAddresses/writable")));
    keys.extend(str_array(meta.pointer("/loadedAddresses/readonly")));
    let fee_payer = keys.first().cloned().unwrap_or_default();

    let mut events = Vec::new();
    if !failed {
        let mut n: u32 = 0;
        if let Some(groups) = meta.get("innerInstructions").and_then(Value::as_array) {
            for group in groups {
                let Some(ixs) = group.get("instructions").and_then(Value::as_array) else {
                    continue;
                };
                for ix in ixs {
                    let Some(pidx) = ix.get("programIdIndex").and_then(Value::as_u64) else {
                        continue;
                    };
                    if keys.get(pidx as usize).map(String::as_str) != Some(DBC_PROGRAM_ID) {
                        continue;
                    }
                    let Some(data58) = ix.get("data").and_then(Value::as_str) else {
                        continue;
                    };
                    let Ok(data) = bs58::decode(data58).into_vec() else {
                        continue;
                    };
                    match decode_event_ix(&data) {
                        Ok(Some(ev)) => {
                            events.push((n, ev));
                            n += 1;
                        }
                        Ok(None) => {}
                        Err(e) => tracing::warn!("event decode error: {e:#}"),
                    }
                }
            }
        }
    }

    Ok(ParsedTx { slot, block_time, fee_payer, failed, events })
}
