//! Обработчики: сообщения (команды через match) и нажатия инлайн-кнопок.

use std::sync::Arc;

use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardMarkup, LinkPreviewOptions, MessageId, ParseMode};
use teloxide::RequestError;

use crate::keyboards as kb;
use crate::markdown::esc;
use crate::render;
use crate::store::{self, now, State};

type HandlerResult = ResponseResult<()>;

fn no_preview() -> LinkPreviewOptions {
    LinkPreviewOptions { is_disabled: true, url: None, prefer_small_media: false, prefer_large_media: false, show_above_text: false }
}

/// Отправить сообщение в MarkdownV2. Если Telegram отклонит разметку, отправить без неё,
/// чтобы пользователь всё равно получил ответ.
pub async fn send(bot: &Bot, chat: ChatId, text: String, markup: Option<InlineKeyboardMarkup>) -> ResponseResult<Message> {
    let mut req = bot.send_message(chat, text.clone()).parse_mode(ParseMode::MarkdownV2).link_preview_options(no_preview());
    if let Some(m) = markup.clone() {
        req = req.reply_markup(m);
    }
    match req.await {
        Ok(m) => Ok(m),
        Err(RequestError::Api(e)) => {
            tracing::warn!("markdown rejected, sending plain: {e}");
            let mut plain = bot.send_message(chat, text.replace('\\', ""));
            if let Some(m) = markup {
                plain = plain.reply_markup(m);
            }
            plain.await
        }
        Err(e) => Err(e),
    }
}

/// Отредактировать сообщение. «Message is not modified» (повторное нажатие) — не ошибка.
pub async fn edit(bot: &Bot, chat: ChatId, id: MessageId, text: String, markup: Option<InlineKeyboardMarkup>) -> HandlerResult {
    let mut req = bot.edit_message_text(chat, id, text).parse_mode(ParseMode::MarkdownV2).link_preview_options(no_preview());
    if let Some(m) = markup {
        req = req.reply_markup(m);
    }
    match req.await {
        Ok(_) => Ok(()),
        Err(RequestError::Api(e)) if e.to_string().contains("not modified") => Ok(()),
        Err(e) => Err(e),
    }
}

/// Ответ на команду или адрес: (текст, клавиатура).
type View = (String, Option<InlineKeyboardMarkup>);

fn err_view(e: impl std::fmt::Display) -> View {
    tracing::warn!("view error: {e}");
    (esc("Something went wrong while reading the data. Please try again in a minute."), None)
}

/// Карточка пула по адресу пула или токена. None — не найдено.
async fn pool_view(state: &State, addr: &str) -> Option<View> {
    let conn = match state.db() {
        Ok(c) => c,
        Err(e) => return Some(err_view(e)),
    };
    match store::find_pool(&conn, addr) {
        Ok(Some(p)) => {
            let r = state.risk(&p.config).await;
            let text = render::pool_card(&p, r.as_ref());
            Some((text, Some(kb::pool(&p.pool))))
        }
        Ok(None) => None,
        Err(e) => Some(err_view(e)),
    }
}

async fn config_view(state: &State, config: &str, back: Option<String>) -> Option<View> {
    let r = state.risk(config).await?;
    Some((render::config_report(&r), Some(kb::config(config, back))))
}

fn not_found(addr: &str) -> View {
    (
        format!(
            "{}\n\n{}",
            esc(&format!("{addr} is not a Meteora DBC pool, token or config.")),
            esc("Send the address of a token launched on Meteora's Dynamic Bonding Curve, its pool, or its config.")
        ),
        Some(kb::back_home()),
    )
}

/// Есть ли адрес в базе сборщика (как пул, токен или конфиг).
fn in_db(state: &State, addr: &str) -> bool {
    let Ok(conn) = state.db() else { return false };
    matches!(store::find_pool(&conn, addr), Ok(Some(_))) || matches!(store::config_exists(&conn, addr), Ok(true))
}

/// Адреса нет в базе: ищем пул через RPC, дописываем в базу и оцениваем.
async fn check_onchain(bot: &Bot, chat: ChatId, state: &State, addr: &str) -> HandlerResult {
    let wait = send(bot, chat, esc("🔎 Not in our index yet — looking it up on-chain…"), None).await?;
    let view = onchain_view(state, addr).await;
    edit(bot, chat, wait.id, view.0, view.1).await
}

async fn onchain_view(state: &State, addr: &str) -> View {
    use crate::onchain::Lookup;
    match state.rpc.resolve(addr).await {
        Ok(Lookup::Found(found)) => found_view(state, &found, addr).await,
        // пул Meteora DAMM v2 / DLMM: проверяем его токен
        Ok(Lookup::MeteoraPool { venue, mint }) => {
            let note = format!("Meteora {venue} pool → token {mint}");
            let inner = if in_db(state, &mint) {
                resolve(state, &mint).await
            } else {
                match state.rpc.resolve(&mint).await {
                    Ok(Lookup::Found(found)) => found_view(state, &found, &mint).await,
                    Ok(Lookup::OtherLaunchpad(lp, by_suffix)) => other_launchpad(lp, by_suffix),
                    Ok(Lookup::MintWithoutHistory) => mint_without_history(),
                    Ok(_) => not_found(&mint),
                    Err(e) => err_view(e),
                }
            };
            (format!("{}\n\n{}", esc(&note), inner.0), inner.1)
        }
        Ok(Lookup::OtherLaunchpad(lp, by_suffix)) => other_launchpad(lp, by_suffix),
        Ok(Lookup::MintWithoutHistory) => mint_without_history(),
        Ok(Lookup::NotDbc) => not_found(addr),
        Err(e) => err_view(e),
    }
}

async fn found_view(state: &State, found: &crate::onchain::FoundPool, addr: &str) -> View {
    match crate::onchain::store_found(&state.cfg.db_path, &state.rpc, found).await {
        Ok(()) => {
            if state.risk(&found.config).await.is_none() {
                if let Err(e) = state.refresh().await {
                    tracing::warn!("refresh failed: {e:#}");
                }
            }
            pool_view(state, &found.pool).await.unwrap_or_else(|| not_found(addr))
        }
        Err(e) => err_view(e),
    }
}

fn other_launchpad(lp: &str, by_suffix: bool) -> View {
    let how = if by_suffix { " (judging by its address)" } else { "" };
    (
        format!(
            "{}\n\n{}",
            esc(&format!("This token was launched on {lp}{how}, not on Meteora DBC.")),
            esc("DBC Radar checks launches on Meteora's Dynamic Bonding Curve: who can withdraw liquidity after graduation and whether trading is real.")
        ),
        Some(kb::back_home()),
    )
}

fn mint_without_history() -> View {
    (
        format!(
            "{}\n\n{}",
            esc("This is a token, but we could not find its launch: either it was not launched on Meteora DBC, or the launch is older than the transaction history available to us."),
            esc("If it is a DBC token, send its pool address instead — pools can be checked at any age.")
        ),
        Some(kb::back_home()),
    )
}

/// Нужно ли пересчитать кэш, чтобы ответить про этот адрес (новый конфиг ещё не оценён).
async fn needs_refresh(state: &State, addr: &str) -> bool {
    let Ok(conn) = state.db() else { return false };
    let config = match store::find_pool(&conn, addr) {
        Ok(Some(p)) => p.config,
        _ => match store::config_exists(&conn, addr) {
            Ok(true) => addr.to_string(),
            _ => return false,
        },
    };
    state.risk(&config).await.is_none()
}

/// Проверка адреса: пул/токен, иначе конфиг, иначе «не найдено».
async fn check(bot: &Bot, chat: ChatId, state: &State, addr: &str) -> HandlerResult {
    if !in_db(state, addr) {
        return check_onchain(bot, chat, state, addr).await;
    }
    if needs_refresh(state, addr).await {
        let wait = send(bot, chat, esc("⏳ New config — analysing, this takes a moment…"), None).await?;
        if let Err(e) = state.refresh().await {
            tracing::warn!("refresh failed: {e:#}");
        }
        let (text, markup) = resolve(state, addr).await;
        return edit(bot, chat, wait.id, text, markup).await;
    }
    let (text, markup) = resolve(state, addr).await;
    send(bot, chat, text, markup).await.map(|_| ())
}

async fn resolve(state: &State, addr: &str) -> View {
    if let Some(v) = pool_view(state, addr).await {
        return v;
    }
    if let Some(v) = config_view(state, addr, None).await {
        return v;
    }
    not_found(addr)
}

/// Последние запуски с вердиктами RED / RED-LINK / AMBER за 2 часа.
async fn latest_view(state: &State) -> View {
    let rows = match state.db().and_then(|c| store::latest_pools(&c, 7200, 2000)) {
        Ok(r) => r,
        Err(e) => return err_view(e),
    };
    let cache = state.cache.read().await;
    let picked: Vec<(String, String, Option<i64>, String)> = rows
        .into_iter()
        .filter_map(|(pool, config, mint, t)| {
            let v = cache.by_config.get(&config)?.verdict();
            matches!(v.as_str(), "RED" | "RED-LINK" | "AMBER").then_some((pool, mint, t, v))
        })
        .take(8)
        .collect();
    drop(cache);
    let buttons: Vec<(String, String)> = picked
        .iter()
        .map(|(pool, mint, _, v)| (format!("{} {} · {}", render::emoji(v), render::short_addr(mint), v), pool.clone()))
        .collect();
    (render::latest(&picked), Some(kb::latest(&buttons)))
}

async fn stats_view(state: &State) -> View {
    let last24 = state.db().and_then(|c| store::pools_since(&c, now() - 86_400)).unwrap_or_default();
    let cache = state.cache.read().await;
    let age = cache.refreshed_at.map(|t| t.elapsed().as_secs());
    (render::stats(&cache.by_config, &last24, age), Some(kb::back_home()))
}

/// Входящие сообщения. Команды разбираются через match по первому слову.
pub async fn handle_message(bot: Bot, msg: Message, state: Arc<State>) -> HandlerResult {
    let Some(text) = msg.text() else { return Ok(()) };
    let chat = msg.chat.id;
    let mut parts = text.trim().splitn(2, char::is_whitespace);
    let first = parts.next().unwrap_or("");
    let rest = parts.next().unwrap_or("").trim();
    // в группах команды приходят как /check@DBC_Radar_bot
    let cmd = first.split('@').next().unwrap_or(first);

    match cmd {
        "/start" => {
            // ссылки из канала оповещений: /start c_<config> или /start p_<pool>
            if let Some(cfg) = rest.strip_prefix("c_") {
                return check(&bot, chat, &state, cfg).await;
            }
            if let Some(pool) = rest.strip_prefix("p_") {
                return check(&bot, chat, &state, pool).await;
            }
            send(&bot, chat, render::start(), Some(kb::home())).await?;
        }
        "/help" => {
            send(&bot, chat, render::start(), Some(kb::home())).await?;
        }
        "/how" => {
            send(&bot, chat, render::how(), Some(kb::back_home())).await?;
        }
        "/stats" => {
            let (t, m) = stats_view(&state).await;
            send(&bot, chat, t, m).await?;
        }
        "/latest" => {
            let (t, m) = latest_view(&state).await;
            send(&bot, chat, t, m).await?;
        }
        "/check" => match store::extract_address(rest) {
            Some(addr) => check(&bot, chat, &state, &addr).await?,
            None => {
                send(&bot, chat, esc("Usage: /check <token, pool or config address>"), None).await?;
            }
        },
        _ => match store::extract_address(text) {
            Some(addr) => check(&bot, chat, &state, &addr).await?,
            None => {
                send(&bot, chat, esc("Send a token, pool or config address, or /help."), None).await?;
            }
        },
    }
    Ok(())
}

/// Нажатия инлайн-кнопок: сообщение редактируется на месте.
pub async fn handle_callback(bot: Bot, q: CallbackQuery, state: Arc<State>) -> HandlerResult {
    bot.answer_callback_query(q.id.clone()).await?;
    let (Some(data), Some(message)) = (q.data.as_deref(), q.message.as_ref()) else { return Ok(()) };
    let chat = message.chat().id;
    let id = message.id();

    let (prefix, arg) = data.split_once(':').unwrap_or((data, ""));
    let view: View = match prefix {
        "home" => (render::start(), Some(kb::home())),
        "how" => (render::how(), Some(kb::back_home())),
        "stats" => stats_view(&state).await,
        "latest" => latest_view(&state).await,
        "p" => pool_view(&state, arg).await.unwrap_or_else(|| not_found(arg)),
        "c" => config_view(&state, arg, None).await.unwrap_or_else(|| not_found(arg)),
        "cp" | "op" => {
            let pool = state.db().ok().and_then(|c| store::find_pool(&c, arg).ok().flatten());
            match pool {
                Some(p) if prefix == "cp" => {
                    config_view(&state, &p.config, Some(format!("p:{arg}"))).await.unwrap_or_else(|| not_found(&p.config))
                }
                Some(p) => match state.risk(&p.config).await {
                    Some(r) => {
                        let all = state.cache.read().await;
                        (render::operator(&r, &all.by_config), Some(kb::back_to(format!("p:{arg}"))))
                    }
                    None => not_found(&p.config),
                },
                None => not_found(arg),
            }
        }
        "o" => match state.risk(arg).await {
            Some(r) => {
                let all = state.cache.read().await;
                (render::operator(&r, &all.by_config), Some(kb::back_to(format!("c:{arg}"))))
            }
            None => not_found(arg),
        },
        "r" => match state.db().and_then(|c| store::recent_pools(&c, arg, 8)) {
            Ok(rows) => (render::recent(arg, &rows), Some(kb::back_to(format!("c:{arg}")))),
            Err(e) => err_view(e),
        },
        _ => return Ok(()),
    };
    edit(&bot, chat, id, view.0, view.1).await
}
