pub mod types;
pub mod python;
pub mod text;

use anyhow::Result;
use types::ParseResult;

/// Универсальный интерфейс парсера языка программирования
pub trait LanguageParser: Send + Sync {
    /// Название языка
    fn language_name(&self) -> &str;

    /// Расширения файлов, поддерживаемые парсером
    fn file_extensions(&self) -> &[&str];

    /// Парсинг исходного кода файла
    fn parse(&self, source: &str, file_path: &str) -> Result<ParseResult>;
}

/// Получить парсер по расширению файла.
/// Возвращает None, если язык не поддерживается.
pub fn get_parser_for_extension(ext: &str) -> Option<Box<dyn LanguageParser>> {
    match ext {
        "py" => Some(Box::new(python::PythonParser::new())),
        _ => None,
    }
}
