// Сборка messages-payload для chat-completions и парсинг ответа модели в
// строку терминов для FTS-поиска.
//
// Стратегия: system-сообщение — шаблон промпта из конфигурации (по
// умолчанию см. `EnrichmentConfig::default_prompt_template`). User-сообщение
// — текст процедуры с обрамлением: имя + тело.
//
// Ответ модели нормализуем: убираем нумерацию, маркеры списка, лишние
// пробелы; всё разделители — в запятые. На выходе плоская строка
// `term1, term2, term3`. Эта строка идёт прямо в FTS — токенайзер
// `unicode61` нормализует регистр и диакритику.
//
// Не под feature: парсинг входа/выхода полезен и для тестов без HTTP.
// HTTP-клиент сам по себе живёт в `client.rs` под feature `enrichment`.

/// Длина процедуры, после которой текст обрезается перед отправкой в LLM.
/// Цель — защитить от ошибок «context length exceeded» и слишком длинных
/// запросов. По карточке 261 процедуры >30K знаков рекомендуется
/// обрабатывать через summary; пока ставим консервативную планку и
/// просто truncate'им — этого достаточно для FTS-обогащения, лишний
/// «хвост» процедуры обычно повторяет термины из её начала.
const MAX_BODY_CHARS: usize = 16_000;

/// Сообщение в формате chat-completions.
#[derive(Debug, Clone)]
pub struct Message {
    pub role: &'static str,
    pub content: String,
}

/// Собрать пару system+user-сообщений для одной процедуры.
/// `system_prompt` — это `EnrichmentConfig::prompt_template`, `proc_name`
/// и `proc_body` берутся из core::functions.
pub fn build_messages(system_prompt: &str, proc_name: &str, proc_body: &str) -> Vec<Message> {
    let truncated = if proc_body.chars().count() > MAX_BODY_CHARS {
        // ВАЖНО: считаем по char, не по byte — иначе на кириллице можно
        // перерубить multi-byte UTF-8.
        let mut iter = proc_body.char_indices();
        let cutoff = iter.nth(MAX_BODY_CHARS).map(|(i, _)| i).unwrap_or(proc_body.len());
        let mut s = String::with_capacity(cutoff + 32);
        s.push_str(&proc_body[..cutoff]);
        s.push_str("\n…(обрезано)…");
        s
    } else {
        proc_body.to_string()
    };

    let user = format!("Процедура `{}`:\n\n{}", proc_name, truncated);

    vec![
        Message { role: "system", content: system_prompt.to_string() },
        Message { role: "user", content: user },
    ]
}

/// Привести ответ LLM к плоской строке терминов через запятую.
///
/// Что нормализуется:
///   * нумерация в начале строк (`1.`, `1)`, `- `, `* `, `• `);
///   * перенос строк → запятая;
///   * двойные/висячие запятые;
///   * surrounding whitespace.
///
/// Пустая строка / только пробелы — возвращаем `None`. Это сигнал
/// «модель ничего не дала», вызывающий код может пропустить запись.
pub fn parse_response(raw: &str) -> Option<String> {
    let normalized: String = raw
        .lines()
        .map(strip_list_marker)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(", ");

    let mut compacted = String::with_capacity(normalized.len());
    let mut prev_was_comma = false;
    let mut prev_was_space = false;
    for ch in normalized.chars() {
        if ch == ',' {
            if prev_was_comma {
                continue;
            }
            // подрежем хвостовой пробел перед запятой
            if compacted.ends_with(' ') {
                compacted.pop();
            }
            compacted.push(',');
            compacted.push(' ');
            prev_was_comma = true;
            prev_was_space = true;
        } else if ch.is_whitespace() {
            if prev_was_space {
                continue;
            }
            compacted.push(' ');
            prev_was_space = true;
        } else {
            compacted.push(ch);
            prev_was_comma = false;
            prev_was_space = false;
        }
    }

    let trimmed = compacted
        .trim_matches(|c: char| c.is_whitespace() || c == ',')
        .to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Снять маркер списка в начале строки и обрезать пробелы.
/// Поддерживаемые маркеры: `-`, `*`, `•`, `1.`, `12)`, `1:`.
fn strip_list_marker(line: &str) -> String {
    let line = line.trim();
    if line.is_empty() {
        return String::new();
    }
    let bytes = line.as_bytes();

    // Начинается ли с маркера списка `-`, `*`, `•`?
    if let Some(first) = line.chars().next() {
        if matches!(first, '-' | '*' | '•') {
            return line.chars().skip(1).collect::<String>().trim().to_string();
        }
    }

    // Начинается ли с числовой нумерации `12.` / `12)` / `12:`?
    let mut i = 0usize;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i > 0 && i < bytes.len() && matches!(bytes[i], b'.' | b')' | b':') {
        return line[i + 1..].trim().to_string();
    }

    line.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_messages_includes_proc_name_and_body() {
        let msgs = build_messages("system text", "Расчёт.Старт", "// тело\nФункция Х()");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[0].content, "system text");
        assert_eq!(msgs[1].role, "user");
        assert!(msgs[1].content.contains("Расчёт.Старт"));
        assert!(msgs[1].content.contains("Функция Х()"));
    }

    #[test]
    fn build_messages_truncates_long_body() {
        let body: String = std::iter::repeat('а').take(MAX_BODY_CHARS + 100).collect();
        let msgs = build_messages("sys", "p", &body);
        let user = &msgs[1].content;
        assert!(user.contains("…(обрезано)…"));
        // Проверка что char-длина user ограничена ~MAX_BODY_CHARS + заголовок:
        let chars = user.chars().count();
        assert!(chars < MAX_BODY_CHARS + 200, "user должен быть обрезан, длина={}", chars);
    }

    #[test]
    fn parse_response_strips_numeric_list() {
        let raw = "1. товары\n2. склад\n3. проведение";
        let parsed = parse_response(raw).unwrap();
        assert_eq!(parsed, "товары, склад, проведение");
    }

    #[test]
    fn parse_response_strips_dash_and_bullet_lists() {
        let raw = "- скидки\n• оплата\n* комиссия";
        let parsed = parse_response(raw).unwrap();
        assert_eq!(parsed, "скидки, оплата, комиссия");
    }

    #[test]
    fn parse_response_keeps_inline_commas() {
        let raw = "товары, склад, проведение";
        let parsed = parse_response(raw).unwrap();
        assert_eq!(parsed, "товары, склад, проведение");
    }

    #[test]
    fn parse_response_collapses_double_commas_and_spaces() {
        let raw = "товары,  ,склад\n\nпроведение";
        let parsed = parse_response(raw).unwrap();
        assert_eq!(parsed, "товары, склад, проведение");
    }

    #[test]
    fn parse_response_empty_returns_none() {
        assert!(parse_response("").is_none());
        assert!(parse_response("   \n\n   ").is_none());
        assert!(parse_response(",,,,,,").is_none());
    }
}
