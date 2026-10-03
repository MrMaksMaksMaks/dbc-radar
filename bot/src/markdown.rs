//! Экранирование MarkdownV2 в одном месте (как в Starter Kit Bot): все динамические данные
//! и статический текст сообщений проходят через эти функции, чтобы набор спецсимволов
//! не разошёлся между модулями.

/// Экранирует спецсимволы MarkdownV2 в обычном тексте.
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

/// Короткое имя функции для частого использования в шаблонах.
pub fn esc(text: &str) -> String {
    escape_markdown_v2(text)
}

/// Моноширинный фрагмент (адреса): внутри `...` экранируются только ` и \.
pub fn code(text: &str) -> String {
    let inner: String = text
        .chars()
        .flat_map(|c| if c == '`' || c == '\\' { vec!['\\', c] } else { vec![c] })
        .collect();
    format!("`{inner}`")
}

/// Жирный текст.
pub fn bold(text: &str) -> String {
    format!("*{}*", esc(text))
}

/// Курсив.
pub fn italic(text: &str) -> String {
    format!("_{}_", esc(text))
}

/// Ссылка: в URL экранируются только ) и \.
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
