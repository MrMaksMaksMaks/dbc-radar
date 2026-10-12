//! Inline keyboards. Button data (callback data, up to 64 bytes):
//!   p:<pool>    — pool card
//!   c:<config>  — config report
//!   cp:<pool>   — config report for a pool (with a "back" button to the pool)
//!   o:<config>  — operator (cluster)
//!   r:<config>  — latest launches of a config
//!   op:<pool>   — operator of a pool (with a "back" button to the pool)
//!   stats | how | home | latest

use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};

fn cb(text: &str, data: String) -> InlineKeyboardButton {
    InlineKeyboardButton::callback(text.to_string(), data)
}

fn url(text: &str, u: String) -> Option<InlineKeyboardButton> {
    u.parse().ok().map(|u| InlineKeyboardButton::url(text.to_string(), u))
}

pub fn home() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![cb("🚨 Latest risky launches", "latest".into())],
        vec![cb("📊 Stats", "stats".into()), cb("❓ How it works", "how".into())],
    ])
}

/// List of latest launches: one button per launch, plus refresh and menu.
pub fn latest(items: &[(String, String)]) -> InlineKeyboardMarkup {
    let mut rows: Vec<Vec<InlineKeyboardButton>> = items.iter().map(|(label, pool)| vec![cb(label, format!("p:{pool}"))]).collect();
    rows.push(vec![cb("🔁 Refresh", "latest".into()), cb("« Menu", "home".into())]);
    InlineKeyboardMarkup::new(rows)
}

pub fn back_home() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![cb("« Menu", "home".into())]])
}

pub fn pool(pool: &str) -> InlineKeyboardMarkup {
    let mut row2 = vec![cb("🔁 Refresh", format!("p:{pool}"))];
    if let Some(b) = url("🔗 Solscan", format!("https://solscan.io/account/{pool}")) {
        row2.insert(0, b);
    }
    InlineKeyboardMarkup::new(vec![
        vec![cb("📋 Config details", format!("cp:{pool}")), cb("🧭 Operator", format!("op:{pool}"))],
        row2,
    ])
}

/// `back` — callback data for the "back" button (e.g. to the pool card).
pub fn config(config: &str, back: Option<String>) -> InlineKeyboardMarkup {
    let mut rows = vec![vec![cb("🧭 Operator", format!("o:{config}")), cb("🕒 Recent launches", format!("r:{config}"))]];
    let mut last = Vec::new();
    if let Some(b) = url("🔗 Solscan", format!("https://solscan.io/account/{config}")) {
        last.push(b);
    }
    if let Some(back) = back {
        last.push(cb("« Back", back));
    }
    if !last.is_empty() {
        rows.push(last);
    }
    InlineKeyboardMarkup::new(rows)
}

pub fn back_to(data: String) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![cb("« Back", data)]])
}

/// For the alerts channel: links only (callback buttons in a channel would change the message for everyone).
pub fn open_in_bot(bot_username: &str, config: &str) -> InlineKeyboardMarkup {
    let mut row = Vec::new();
    if let Some(b) = url("🔎 Open in bot", format!("https://t.me/{bot_username}?start=c_{config}")) {
        row.push(b);
    }
    if let Some(b) = url("🔗 Solscan", format!("https://solscan.io/account/{config}")) {
        row.push(b);
    }
    InlineKeyboardMarkup::new(vec![row])
}
