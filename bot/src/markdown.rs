//! MarkdownV2 escaping in one place (as in Starter Kit Bot): all dynamic data
//! and static message text go through these functions, so the set of special characters
//! does not drift between modules.

/// Escapes MarkdownV2 special characters in plain text.
pub fn escape_markdown_v2(text: &str) -> String {
    const SPECIAL: &[char] = &[
        '_', '*', '[', ']', '(', ')', '~', '`', '>', '#', '+', '-', '=',
        '|', '{', '}', '.', '!', '\\',
    ];
    let mut escaped = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        if SPECIAL.contains(&c) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

/// Short function name for frequent use in templates.
pub fn esc(text: &str) -> String {
    escape_markdown_v2(text)
}

/// Monospace fragment (addresses): inside `...` only ` and \ are escaped.
pub fn code(text: &str) -> String {
    let inner: String = text
        .chars()
        .flat_map(|c| if c == '`' || c == '\\' { vec!['\\', c] } else { vec![c] })
        .collect();
    format!("`{inner}`")
}

/// Bold text.
pub fn bold(text: &str) -> String {
    format!("*{}*", esc(text))
}

/// Italic.
pub fn italic(text: &str) -> String {
    format!("_{}_", esc(text))
}

/// Link: in the URL only ) and \ are escaped.
pub fn link(text: &str, url: &str) -> String {
    let u: String = url
        .chars()
        .flat_map(|c| if c == ')' || c == '\\' { vec!['\\', c] } else { vec![c] })
        .collect();
    format!("[{}]({u})", esc(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_all_special_chars() {
        assert_eq!(esc("a.b-c(d)!"), "a\\.b\\-c\\(d\\)\\!");
        assert_eq!(code("Ab1`x"), "`Ab1\\`x`");
        assert_eq!(link("Solscan", "https://solscan.io/a(b)"), "[Solscan](https://solscan.io/a(b\\))");
    }
}
