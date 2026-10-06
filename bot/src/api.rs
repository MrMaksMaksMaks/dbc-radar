//! Публичный HTTP API для терминалов, ботов и лаунчпадов: те же вердикты, что в боте, в JSON.
//!
//!   GET /v1/health                    — состояние сервиса
//!   GET /v1/check/{address}           — токен, пул DBC, конфиг или пул Meteora DAMM v2 / DLMM
//!   GET /v1/latest?hours=2&limit=20   — последние запуски с вердиктами RED / RED-LINK / AMBER
//!   GET /v1/stats                     — запуски за 24 часа по вердиктам
//!
//! Адреса из базы отдаются всем. Без ключа также работает дешёвая проверка по одному аккаунту:
//! адрес пула DAMM v2 / DLMM определяется в токен, и если токен есть в базе — отдаётся его вердикт.
//! Поиск неизвестных пулов и токенов через RPC (включая историю транзакций) расходует лимиты
//! провайдеров, поэтому доступен только с ключом (заголовок `X-API-Key`).
//! Без ключа из ответа /v1/check убираются адреса создателя и получателей
//! (launch.creator, config.top_creator, config.fee_claimer, config.leftover_receiver).
//! Неверный ключ — ответ 401.
//! Лимит запросов — на IP в минуту. За Cloudflare берётся заголовок CF-Connecting-IP,
//! но только если соединение пришло с loopback (от локального cloudflared).

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Path, Query, State as AxState};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::onchain::{self, Lookup};
use crate::store::{self, now, State};

const NOT_INDEXED: &str = "address is not in the index; on-chain lookup requires an API key (X-API-Key)";

#[derive(Clone)]
struct Api {
    app: Arc<State>,
    keys: Arc<HashSet<String>>,
    limiter: Arc<Mutex<HashMap<String, (i64, u32)>>>,
}

pub async fn serve(app: Arc<State>) {
    let Some(bind) = app.cfg.api_bind.clone() else { return };
    let api = Api {
        keys: Arc::new(app.cfg.api_keys.iter().cloned().collect()),
        limiter: Arc::new(Mutex::new(HashMap::new())),
        app,
    };
    let router = Router::new()
        .route("/v1/health", get(health))
        .route("/v1/check/{address}", get(check))
        .route("/v1/latest", get(latest))
        .route("/v1/stats", get(stats))
        .with_state(api);
    let listener = match tokio::net::TcpListener::bind(&bind).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("API bind {bind}: {e}");
            return;
        }
    };
    tracing::info!(%bind, "API listening");
    if let Err(e) = axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()).await {
        tracing::error!("API stopped: {e}");
    }
}

fn err(status: StatusCode, code: &str, message: &str) -> Response {
    (status, Json(json!({"error": code, "message": message}))).into_response()
}

/// Проверка лимита и ключа. Ok(true) — валидный ключ, Ok(false) — без ключа;
/// Err — ответ 429 (лимит превышен) или 401 (неверный ключ).
async fn admit(api: &Api, headers: &HeaderMap, peer: SocketAddr) -> Result<bool, Response> {
    let key = headers.get("x-api-key").and_then(|v| v.to_str().ok());
    let key_ok = key.map(|k| api.keys.contains(k)).unwrap_or(false);
    // CF-Connecting-IP доверяем только от локального cloudflared, иначе заголовок можно подделать
    let client = if peer.ip().is_loopback() {
        headers
            .get("cf-connecting-ip")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
            .unwrap_or_else(|| peer.ip().to_string())
    } else {
        peer.ip().to_string()
    };
    let limit = if key_ok { api.app.cfg.api_rate_key } else { api.app.cfg.api_rate_public };
    let minute = now() / 60;
    {
        let mut map = api.limiter.lock().await;
        if map.len() > 50_000 {
            map.retain(|_, (m, _)| *m == minute);
        }
        let e = map.entry(client).or_insert((minute, 0));
        if e.0 != minute {
            *e = (minute, 0);
        }
        e.1 += 1;
        if e.1 > limit {
            return Err(err(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                &format!("limit is {limit} requests per minute"),
            ));
        }
    }
    // неверный ключ проверяется после лимита, чтобы перебор ключей тоже ограничивался
    if key.is_some() && !key_ok {
        return Err(err(StatusCode::UNAUTHORIZED, "invalid_api_key", "X-API-Key is not valid"));
    }
    Ok(key_ok)
}

/// Без ключа скрываем адреса создателя и получателей:
/// публичны вердикт, флаги и проценты, адреса операторов — только по ключу.
fn redact(v: &mut Value) {
    if let Some(c) = v.get_mut("config").and_then(Value::as_object_mut) {
        for k in ["top_creator", "fee_claimer", "leftover_receiver"] {
            c.remove(k);
        }
    }
    if let Some(l) = v.get_mut("launch").and_then(Value::as_object_mut) {
        l.remove("creator");
    }
}

async fn health(AxState(api): AxState<Api>) -> Response {
    let cache = api.app.cache.read().await;
    Json(json!({
        "ok": cache.refreshed_at.is_some(),
        "configs": cache.by_config.len(),
        "analysis_age_secs": cache.refreshed_at.map(|t| t.elapsed().as_secs()),
    }))
    .into_response()
}

fn pool_json(p: &store::PoolInfo) -> Value {
    json!({
        "pool": p.pool,
        "token": p.base_mint,
        "creator": p.creator,
        "config": p.config,
        "created_time": p.created_time,
        "graduated_time": p.graduated_time,
        "trades_sampled": p.tracked,
        "trades": p.swaps.as_ref().map(|s| json!({
            "count": s.count,
            "wallets": s.traders,
            "creator_opening_buy_sol": s.creator_opening_buy_sol,
            "fan_out_sellers": s.fanout_sellers,
        })),
    })
}

/// Вердикт конфига из кэша (при необходимости кэш пересчитывается).
async fn config_json(app: &State, config: &str) -> Option<Value> {
    if app.risk(config).await.is_none() {
        if let Err(e) = app.refresh().await {
            tracing::warn!("refresh failed: {e:#}");
        }
    }
    app.risk(config).await.map(|r| r.0)
}

/// Ответ по адресу, который уже есть в базе (пул, токен или конфиг).
async fn from_db(app: &State, addr: &str) -> Option<Value> {
    let conn = app.db().ok()?;
    if let Ok(Some(p)) = store::find_pool(&conn, addr) {
        let config = config_json(app, &p.config).await;
        return Some(json!({
            "kind": "launch",
            "verdict": config.as_ref().and_then(|c| c.get("verdict")).cloned(),
            "launch": pool_json(&p),
            "config": config,
        }));
    }
    if matches!(store::config_exists(&conn, addr), Ok(true)) {
        let config = config_json(app, addr).await?;
        return Some(json!({"kind": "config", "verdict": config.get("verdict").cloned(), "config": config}));
    }
    None
}

/// Токен (не пул Meteora): из базы, иначе через RPC.
async fn token_json(app: &State, mint: &str) -> Value {
    if let Some(v) = from_db(app, mint).await {
        return v;
    }
    match app.rpc.resolve(mint).await {
        Ok(Lookup::Found(f)) => found_json(app, &f).await,
        Ok(Lookup::OtherLaunchpad(lp, by_suffix)) => {
            json!({"kind": "token", "dbc": false, "launchpad": lp, "inferred_from_address": by_suffix})
        }
        Ok(Lookup::MintWithoutHistory) => {
            json!({"kind": "token", "dbc": null, "error": "launch_not_found", "message": "launch not found in available history; try the pool address"})
        }
        Ok(_) => json!({"kind": "unknown", "dbc": false}),
        Err(e) => json!({"kind": "unknown", "error": "rpc_error", "message": e.to_string()}),
    }
}

async fn found_json(app: &State, f: &onchain::FoundPool) -> Value {
    if let Err(e) = onchain::store_found(&app.cfg.db_path, &app.rpc, f).await {
        return json!({"kind": "unknown", "error": "store_error", "message": e.to_string()});
    }
    from_db(app, &f.pool).await.unwrap_or(json!({"kind": "unknown", "error": "not_found"}))
}

/// Проверка без ключа для адреса, которого нет в базе: только один getAccountInfo.
/// Пул DAMM v2 / DLMM → его токен → вердикт из базы (если токен там есть).
async fn public_lookup(api: &Api, addr: &str) -> Response {
    match api.app.rpc.resolve_cheap(addr).await {
        Ok(Some(Lookup::MeteoraPool { venue, mint })) => {
            let via = json!({"venue": venue, "pool": addr});
            if let Some(mut v) = from_db(&api.app, &mint).await {
                redact(&mut v);
                v["via"] = via;
                return Json(v).into_response();
            }
            if let Some(lp) = onchain::launchpad_by_suffix(&mint) {
                return Json(json!({
                    "kind": "token", "dbc": false, "launchpad": lp,
                    "inferred_from_address": true, "token": mint, "via": via,
                }))
                .into_response();
            }
            err(
                StatusCode::NOT_FOUND,
                "not_indexed",
                "the token of this pool is not in the index; on-chain lookup requires an API key (X-API-Key)",
            )
        }
        Ok(Some(Lookup::NotDbc)) => err(StatusCode::NOT_FOUND, "not_dbc", "not a Meteora DBC pool, token or config"),
        // токен: поиск его пула по истории — только с ключом; лаунчпад по окончанию адреса — бесплатно
        Ok(None) => match onchain::launchpad_by_suffix(addr) {
            Some(lp) => Json(json!({"kind": "token", "dbc": false, "launchpad": lp, "inferred_from_address": true}))
                .into_response(),
            None => err(StatusCode::NOT_FOUND, "not_indexed", NOT_INDEXED),
        },
        // пул DBC, которого нет в базе: запись в базу — только с ключом
        Ok(Some(_)) => err(StatusCode::NOT_FOUND, "not_indexed", NOT_INDEXED),
        Err(e) => err(StatusCode::BAD_GATEWAY, "rpc_error", &e.to_string()),
    }
}

async fn check(
    AxState(api): AxState<Api>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(address): Path<String>,
) -> Response {
    let key_ok = match admit(&api, &headers, peer).await {
        Ok(k) => k,
        Err(r) => return r,
    };
    let Some(addr) = store::extract_address(&address) else {
        return err(StatusCode::BAD_REQUEST, "bad_address", "expected a Solana address");
    };
    if let Some(mut v) = from_db(&api.app, &addr).await {
        if !key_ok {
            redact(&mut v);
        }
        return Json(v).into_response();
    }
    if !key_ok {
        return public_lookup(&api, &addr).await;
    }
    // дальше только запросы с валидным ключом: полный поиск, адреса не скрываются
    let v = match api.app.rpc.resolve(&addr).await {
        Ok(Lookup::Found(f)) => found_json(&api.app, &f).await,
        Ok(Lookup::MeteoraPool { venue, mint }) => {
            let mut v = token_json(&api.app, &mint).await;
            v["via"] = json!({"venue": venue, "pool": addr});
            v
        }
        Ok(Lookup::OtherLaunchpad(lp, by_suffix)) => {
            json!({"kind": "token", "dbc": false, "launchpad": lp, "inferred_from_address": by_suffix})
        }
        Ok(Lookup::MintWithoutHistory) => {
            return err(
                StatusCode::NOT_FOUND,
                "launch_not_found",
                "launch not found in available history; try the pool address",
            )
        }
        Ok(Lookup::NotDbc) => return err(StatusCode::NOT_FOUND, "not_dbc", "not a Meteora DBC pool, token or config"),
        Err(e) => return err(StatusCode::BAD_GATEWAY, "rpc_error", &e.to_string()),
    };
    Json(v).into_response()
}

async fn latest(
    AxState(api): AxState<Api>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Err(r) = admit(&api, &headers, peer).await {
        return r;
    }
    let hours: i64 = q.get("hours").and_then(|h| h.parse().ok()).unwrap_or(2).clamp(1, 24);
    let limit: usize = q.get("limit").and_then(|l| l.parse().ok()).unwrap_or(20).clamp(1, 100);
    let rows = match api.app.db().and_then(|c| store::latest_pools(&c, hours * 3600, 5000)) {
        Ok(r) => r,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, "db_error", &e.to_string()),
    };
    let cache = api.app.cache.read().await;
    let items: Vec<Value> = rows
        .into_iter()
        .filter_map(|(pool, config, token, t)| {
            let v = cache.by_config.get(&config)?.verdict();
            matches!(v.as_str(), "RED" | "RED-LINK" | "AMBER")
                .then(|| json!({"pool": pool, "token": token, "config": config, "verdict": v, "created_time": t}))
        })
        .take(limit)
        .collect();
    Json(json!({"hours": hours, "count": items.len(), "launches": items})).into_response()
}

async fn stats(AxState(api): AxState<Api>, ConnectInfo(peer): ConnectInfo<SocketAddr>, headers: HeaderMap) -> Response {
    if let Err(r) = admit(&api, &headers, peer).await {
        return r;
    }
    let last24 = api.app.db().and_then(|c| store::pools_since(&c, now() - 86_400)).unwrap_or_default();
    let cache = api.app.cache.read().await;
    let mut day: HashMap<String, u64> = HashMap::new();
    for (c, n) in &last24 {
        let v = cache.by_config.get(c).map(|r| r.verdict()).unwrap_or_else(|| "UNANALYSED".into());
        *day.entry(v).or_default() += n;
    }
    let total: u64 = day.values().sum();
    Json(json!({
        "launches_24h": total,
        "by_verdict_24h": day,
        "configs_analysed": cache.by_config.len(),
        "analysis_age_secs": cache.refreshed_at.map(|t| t.elapsed().as_secs()),
    }))
    .into_response()
}
