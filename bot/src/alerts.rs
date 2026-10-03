//! Фоновые задачи: пересчёт оценок по таймеру, оповещения о новых рискованных конфигах
//! и периодическая сводка в канал.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use teloxide::prelude::*;
use teloxide::types::Recipient;

use crate::markdown::{bold, esc};
use crate::render;
use crate::store::{self, now, State};
use crate::{handlers, keyboards as kb};

const ALERT_VERDICTS: &[&str] = &["RED", "RED-LINK", "AMBER"];
/// Не больше стольких оповещений за один пересчёт, чтобы не заваливать канал.
const MAX_ALERTS_PER_ROUND: usize = 5;

fn recipient(s: &str) -> Recipient {
    match s.parse::<i64>() {
        Ok(id) => Recipient::Id(ChatId(id)),
        Err(_) => Recipient::ChannelUsername(s.to_string()),
    }
}

fn load_alerted(path: &str) -> Option<HashSet<String>> {
    std::fs::read_to_string(path).ok().map(|t| t.lines().map(str::to_owned).filter(|l| !l.is_empty()).collect())
}

fn save_alerted(path: &str, set: &HashSet<String>) {
    let mut v: Vec<&String> = set.iter().collect();
    v.sort();
    let body = v.into_iter().cloned().collect::<Vec<_>>().join("\n");
    if let Err(e) = std::fs::write(path, body) {
        tracing::warn!("save {path}: {e}");
    }
}

/// Пересчёт кэша по таймеру и оповещения о новых конфигах после каждого пересчёта.
pub async fn run_refresher(bot: Bot, state: Arc<State>) {
    let mut alerted = load_alerted(&state.cfg.alerted_file);
    let mut first = true;
    loop {
        // первый пересчёт уже сделан при старте, поэтому на первом круге пересчёт пропускаем
        let res = if first { Ok(()) } else { state.refresh().await };
        first = false;
        if let Err(e) = res {
            tracing::warn!("refresh failed: {e:#}");
        } else if let Some(chat) = state.cfg.alert_chat.clone() {
            let cache = state.cache.read().await;
            let risky: Vec<(String, store::ConfigRisk)> = cache
                .by_config
                .iter()
                .filter(|(_, r)| ALERT_VERDICTS.contains(&r.verdict().as_str()))
                .map(|(c, r)| (c.clone(), r.clone()))
                .collect();
            drop(cache);
            match alerted.as_mut() {
                // первый запуск: всё уже известное считается «оповещённым», чтобы не было лавины
                None => {
                    let set: HashSet<String> = risky.iter().map(|(c, _)| c.clone()).collect();
                    save_alerted(&state.cfg.alerted_file, &set);
                    tracing::info!(baseline = set.len(), "alert baseline saved");
                    alerted = Some(set);
                }
                Some(set) => {
                    let mut sent = 0;
                    let fresh: Vec<&(String, store::ConfigRisk)> = risky.iter().filter(|(c, _)| !set.contains(c)).collect();
                    for (c, r) in fresh {
                        if sent >= MAX_ALERTS_PER_ROUND {
                            break;
                        }
                        let header = match r.verdict().as_str() {
                            "RED-LINK" => "🆕 New config of a known synthetic-launch operator",
                            "RED" => "🆕 Synthetic launches detected on a config",
                            _ => "🆕 New risky config",
                        };
                        let text = format!("{}\n\n{}", bold(header), render::config_report(r));
                        let to = recipient(&chat);
                        let res = bot
                            .send_message(to, text)
                            .parse_mode(teloxide::types::ParseMode::MarkdownV2)
                            .reply_markup(kb::open_in_bot(&state.cfg.bot_username, c))
                            .await;
                        match res {
                            Ok(_) => {
                                set.insert(c.clone());
                                sent += 1;
                            }
                            Err(e) => {
                                tracing::warn!("alert failed: {e}");
                                break;
                            }
                        }
                    }
                    if sent > 0 {
                        save_alerted(&state.cfg.alerted_file, set);
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(state.cfg.refresh_secs.max(60))).await;
    }
}

/// Сводка за период в канал оповещений.
pub async fn run_digest(bot: Bot, state: Arc<State>) {
    let Some(chat) = state.cfg.alert_chat.clone() else { return };
    let period = state.cfg.digest_secs.max(600);
    loop {
        tokio::time::sleep(Duration::from_secs(period)).await;
        let since = now() - period as i64;
        let counts = match state.db().and_then(|c| store::pools_since(&c, since)) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("digest: {e:#}");
                continue;
            }
        };
        let cache = state.cache.read().await;
        let body = render::stats(&cache.by_config, &counts, cache.refreshed_at.map(|t| t.elapsed().as_secs()));
        drop(cache);
        let text = format!("{}\n\n{}", bold(&format!("Digest for the last {} h", period / 3600)), body)
            .replace(&esc("Launches in the last 24h"), &esc("Launches in this period"));
        let chat_id = match chat.parse::<i64>() {
            Ok(id) => ChatId(id),
            Err(_) => {
                // для @username используем отдельный запрос без helper-а
                let _ = bot
                    .send_message(Recipient::ChannelUsername(chat.clone()), text)
                    .parse_mode(teloxide::types::ParseMode::MarkdownV2)
                    .await
                    .map_err(|e| tracing::warn!("digest failed: {e}"));
                continue;
            }
        };
        if let Err(e) = handlers::send(&bot, chat_id, text, None).await {
            tracing::warn!("digest failed: {e}");
        }
    }
}
