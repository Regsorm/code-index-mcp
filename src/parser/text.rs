use super::types::TextParseResult;

/// Простой текстовый парсер без AST
pub struct TextParser;

impl TextParser {
    /// Парсит текстовый файл — возвращает содержимое и количество строк
    pub fn parse(source: &str) -> TextParseResult {
        TextParseResult {
            content: source.to_string(),
            lines_total: source.lines().count(),
        }
    }
}
