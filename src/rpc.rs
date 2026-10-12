//! Thin JSON-RPC client on top of reqwest with rate limiting and retries.
//! Deliberately without solana-client: fewer dependencies and a faster build.

use anyhow::{anyhow, bail, Result};
use base64::Engine;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::{interval, Interval, MissedTickBehavior};

pub struct Rpc {
    http: reqwest::Client,
    url: String,
    limiter: Mutex<Interval>,
}

#[allow(dead_code)] // slot/block_time will be useful for debugging and backfill
#[derive(Debug, Clone)]
pub struct SigInfo {
    pub signature: String,
    pub slot: u64,
    pub block_time: Option<i64>,
    pub failed: bool,
}

impl Rpc {
    pub fn new(url: String, rps: f64) -> Self {
        let mut iv = interval(Duration::from_secs_f64(1.0 / rps.max(0.1)));
        iv.set_missed_tick_behavior(MissedTickBehavior::Delay);
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("http client"),
            url,
            limiter: Mutex::new(iv),
        }
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let mut last_err = anyhow!("no attempts");
        for attempt in 0..6u32 {
            self.limiter.lock().await.tick().await;
            match self.http.post(&self.url).json(&body).send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.as_u16() == 429 || status.is_server_error() {
                        last_err = anyhow!("{method}: HTTP {status}");
                    } else {
                        let v: Value = resp.json().await?;
                        if let Some(err) = v.get("error") {
                            // -32005/-32007 and similar provider errors are usually temporary,
                            // other errors are returned upward right away.
                            let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
                            if matches!(code, -32005 | -32007 | -32004 | -32603) {
                                last_err = anyhow!("{method}: rpc error {err}");
                            } else {
                                bail!("{method}: rpc error {err}");
                            }
                        } else {
                            return Ok(v.get("result").cloned().unwrap_or(Value::Null));
                        }
                    }
                }
                Err(e) => last_err = anyhow!("{method}: {e}"),
            }
            let backoff = Duration::from_millis(500 * 2u64.pow(attempt));
            tracing::debug!(?backoff, "retrying {method}: {last_err}");
            tokio::time::sleep(backoff).await;
        }
        Err(last_err)
    }

    /// Signatures for an address, newest first (as the RPC returns them).
    pub async fn get_signatures(
        &self,
        address: &str,
        until: Option<&str>,
        before: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SigInfo>> {
        let mut cfg = json!({"limit": limit, "commitment": "confirmed"});
        if let Some(u) = until {
            cfg["until"] = json!(u);
        }
        if let Some(b) = before {
            cfg["before"] = json!(b);
        }
        let res = self.call("getSignaturesForAddress", json!([address, cfg])).await?;
        let arr = res.as_array().cloned().unwrap_or_default();
        Ok(arr
            .into_iter()
            .filter_map(|s| {
                Some(SigInfo {
                    signature: s.get("signature")?.as_str()?.to_string(),
                    slot: s.get("slot")?.as_u64()?,
                    block_time: s.get("blockTime").and_then(Value::as_i64),
                    failed: !s.get("err").map(Value::is_null).unwrap_or(true),
                })
            })
            .collect())
    }

    /// Transaction in json encoding. Ok(None) if the RPC does not serve it yet.
    pub async fn get_transaction(&self, signature: &str) -> Result<Option<Value>> {
        let res = self
            .call(
                "getTransaction",
                json!([signature, {
                    "encoding": "json",
                    "commitment": "confirmed",
                    "maxSupportedTransactionVersion": 1
                }]),
            )
            .await?;
        Ok(if res.is_null() { None } else { Some(res) })
    }

    /// Accounts in a batch (up to 100 addresses): for each, (owner, data) or None if the account does not exist.
    /// `slice` = (offset, length) — read only part of the data (smaller response; public nodes
    /// reject requests that are too large).
    pub async fn get_multiple_accounts(
        &self,
        keys: &[String],
        slice: Option<(usize, usize)>,
    ) -> Result<Vec<Option<(String, Vec<u8>)>>> {
        let mut cfg = json!({"encoding": "base64", "commitment": "confirmed"});
        if let Some((offset, length)) = slice {
            cfg["dataSlice"] = json!({"offset": offset, "length": length});
        }
        let res = self.call("getMultipleAccounts", json!([keys, cfg])).await?;
        let arr = res.get("value").and_then(Value::as_array).cloned().unwrap_or_default();
        if arr.len() != keys.len() {
            bail!("getMultipleAccounts: {} accounts for {} keys", arr.len(), keys.len());
        }
        arr.into_iter()
            .map(|v| {
                if v.is_null() {
                    return Ok(None);
                }
                let owner = v.get("owner").and_then(Value::as_str).unwrap_or("").to_string();
                let data = v.pointer("/data/0").and_then(Value::as_str).unwrap_or("");
                Ok(Some((owner, base64::engine::general_purpose::STANDARD.decode(data)?)))
            })
            .collect()
    }

    /// Raw account data (base64 -> bytes).
    pub async fn get_account_data(&self, pubkey: &str) -> Result<Option<Vec<u8>>> {
        let res = self
            .call(
                "getAccountInfo",
                json!([pubkey, {"encoding": "base64", "commitment": "confirmed"}]),
            )
            .await?;
        let Some(data) = res.pointer("/value/data/0").and_then(Value::as_str) else {
            return Ok(None);
        };
        Ok(Some(base64::engine::general_purpose::STANDARD.decode(data)?))
    }
}
