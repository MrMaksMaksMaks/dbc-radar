//! Разбор транзакции из getTransaction (encoding = "json"):
//! находим внутренние инструкции, адресованные программе DBC, и декодируем события.

use crate::events::{decode_event_ix, DbcEvent, DBC_PROGRAM_ID};
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;

const WSOL: &str = "So11111111111111111111111111111111111111112";
/// Владельцы хранилищ пулов (pool authority PDA программ DBC и DAMM v2) и сами программы:
/// это пул, а не участник сделки.
const NOT_PARTICIPANTS: [&str; 4] = [
    "FhVo3mqL8PW5pH5U2CN4XE33DokiyZnUwuGpH2hmHLuM",
    "HLnpSz9h2S4hiLQ43rnSD9XkcUThA7B8hQMKmDaiTLcC",
    DBC_PROGRAM_ID,
    "cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG",
];

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
    /// Изменение SOL по владельцам (лампорты всех аккаунтов; токен-аккаунты, включая
    /// обёрнутый SOL, относятся к их владельцу). Хранилища пулов исключены.
    pub sol_by_owner: HashMap<String, i64>,
    /// Изменение токенов (кроме обёрнутого SOL) по владельцам, в единицах токена.
    pub tok_by_owner: HashMap<String, i128>,
}

impl ParsedTx {
    /// Фактические стороны сделки: (чьи SOL — плательщик при покупке, получатель при
    /// продаже; кто получил или отдал токены). Подписант транзакции может быть другим
    /// (прокси, релейер). Если транзакция содержит несколько свопов, стороны общие для всех.
    pub fn trade_owners(&self, buy: bool) -> (Option<String>, Option<String>) {
        let sol = self
            .sol_by_owner
            .iter()
            .filter(|(_, d)| if buy { **d < 0 } else { **d > 0 })
            .max_by_key(|(_, d)| d.unsigned_abs())
            .map(|(o, _)| o.clone());
        let tok = self
            .tok_by_owner
            .iter()
            .filter(|(_, d)| if buy { **d > 0 } else { **d < 0 })
            .max_by_key(|(_, d)| d.unsigned_abs())
            .map(|(o, _)| o.clone());
        (sol, tok)
    }
}

/// Изменения балансов по владельцам из meta транзакции.
fn owner_deltas(meta: &Value, keys: &[String]) -> (HashMap<String, i64>, HashMap<String, i128>) {
    let mut owner_of: HashMap<u64, String> = HashMap::new();
    let mut tok: HashMap<String, i128> = HashMap::new();
    for (field, sign) in [("preTokenBalances", -1i128), ("postTokenBalances", 1i128)] {
        for e in meta.get(field).and_then(Value::as_array).into_iter().flatten() {
            let (Some(idx), Some(owner)) = (e.get("accountIndex").and_then(Value::as_u64), e.get("owner").and_then(Value::as_str))
            else {
                continue;
            };
            owner_of.insert(idx, owner.to_string());
            if e.get("mint").and_then(Value::as_str) == Some(WSOL) {
                continue; // обёрнутый SOL учитывается в лампортах аккаунта
            }
            let amount: i128 = e.pointer("/uiTokenAmount/amount").and_then(Value::as_str).and_then(|a| a.parse().ok()).unwrap_or(0);
            *tok.entry(owner.to_string()).or_default() += sign * amount;
        }
    }
    let pre = meta.get("preBalances").and_then(Value::as_array);
    let post = meta.get("postBalances").and_then(Value::as_array);
    let mut sol: HashMap<String, i64> = HashMap::new();
    if let (Some(pre), Some(post)) = (pre, post) {
        for (i, k) in keys.iter().enumerate() {
            let (Some(a), Some(b)) = (pre.get(i).and_then(Value::as_i64), post.get(i).and_then(Value::as_i64)) else {
                continue;
            };
            let owner = owner_of.get(&(i as u64)).cloned().unwrap_or_else(|| k.clone());
            *sol.entry(owner).or_default() += b - a;
        }
    }
    for x in NOT_PARTICIPANTS {
        sol.remove(x);
        tok.remove(x);
    }
    sol.retain(|_, d| *d != 0);
    tok.retain(|_, d| *d != 0);
    (sol, tok)
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

    let (sol_by_owner, tok_by_owner) = if failed { Default::default() } else { owner_deltas(meta, &keys) };
    Ok(ParsedTx { slot, block_time, fee_payer, failed, events, sol_by_owner, tok_by_owner })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Прокси P подписывает покупку, SOL платит мастер M из обёрнутого SOL, токены получает P.
    #[test]
    fn finds_payer_behind_proxy() {
        let tx = json!({
            "slot": 1, "blockTime": 2,
            "transaction": {"message": {"accountKeys": ["P", "M", "Mwsol", "Ptok", "VAULT"]}},
            "meta": {
                "err": null,
                "preBalances":  [10_000_000, 5_000_000_000u64, 1_002_039_280u64, 2_039_280, 100],
                "postBalances": [9_995_000,  5_000_000_000u64, 2_039_280,        2_039_280, 1_000_000_100u64],
                "preTokenBalances": [
                    {"accountIndex": 2, "owner": "M", "mint": WSOL, "uiTokenAmount": {"amount": "1000000000"}},
                    {"accountIndex": 3, "owner": "P", "mint": "MINT", "uiTokenAmount": {"amount": "0"}},
                    {"accountIndex": 4, "owner": "FhVo3mqL8PW5pH5U2CN4XE33DokiyZnUwuGpH2hmHLuM", "mint": WSOL, "uiTokenAmount": {"amount": "0"}}
                ],
                "postTokenBalances": [
                    {"accountIndex": 2, "owner": "M", "mint": WSOL, "uiTokenAmount": {"amount": "0"}},
                    {"accountIndex": 3, "owner": "P", "mint": "MINT", "uiTokenAmount": {"amount": "777"}},
                    {"accountIndex": 4, "owner": "FhVo3mqL8PW5pH5U2CN4XE33DokiyZnUwuGpH2hmHLuM", "mint": WSOL, "uiTokenAmount": {"amount": "1000000000"}}
                ],
                "innerInstructions": []
            }
        });
        let p = parse_tx(&tx).unwrap();
        assert_eq!(p.fee_payer, "P");
        assert_eq!(p.trade_owners(true), (Some("M".to_string()), Some("P".to_string())));
        assert!(!p.sol_by_owner.contains_key("FhVo3mqL8PW5pH5U2CN4XE33DokiyZnUwuGpH2hmHLuM"), "pool vault excluded");
    }
}
