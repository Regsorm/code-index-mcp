pub mod types;
pub mod python;
pub mod javascript;
pub mod typescript;
pub mod java;
pub mod rust_lang;
pub mod text;
pub mod bsl;

use std::collections::HashMap;
use std::sync::Arc;
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

/// Реестр парсеров — хранит активные парсеры, индексированные по расширению файла.
/// Используем Arc<dyn LanguageParser>, чтобы один парсер мог обслуживать
/// несколько расширений без дублирования.
pub struct ParserRegistry {
    parsers: HashMap<String, Arc<dyn LanguageParser>>,
}

impl ParserRegistry {
    /// Создать реестр со всеми доступными парсерами
    pub fn new_all() -> Self {
        let mut registry = Self { parsers: HashMap::new() };
        registry.register(Arc::new(python::PythonParser::new()));
        registry.register(Arc::new(javascript::JavaScriptParser::new()));
        registry.register(Arc::new(typescript::TypeScriptParser::new()));
        registry.register(Arc::new(java::JavaParser::new()));
        registry.register(Arc::new(rust_lang::RustParser::new()));
        registry.register(Arc::new(bsl::BslParser::new()));
        registry
    }

    /// Создать реестр только с указанными языками
    pub fn from_languages(languages: &[String]) -> Self {
        let mut registry = Self { parsers: HashMap::new() };
        for lang in languages {
            match lang.as_str() {
                "python" => registry.register(Arc::new(python::PythonParser::new())),
                "javascript" => {
                    // JS-парсер обрабатывает .js и .jsx
                    registry.register(Arc::new(javascript::JavaScriptParser::new()));
                }
                "typescript" => {
                    // TS-парсер обрабатывает .ts и .tsx
                    registry.register(Arc::new(typescript::TypeScriptParser::new()));
                }
                "java" => registry.register(Arc::new(java::JavaParser::new())),
                "rust" => registry.register(Arc::new(rust_lang::RustParser::new())),
                "bsl" => registry.register(Arc::new(bsl::BslParser::new())),
                _ => {} // Неизвестный язык — пропускаем без ошибки
            }
        }
        registry
    }

    /// Зарегистрировать парсер: добавить по всем его расширениям
    fn register(&mut self, parser: Arc<dyn LanguageParser>) {
        for ext in parser.file_extensions() {
            self.parsers.insert(ext.to_string(), Arc::clone(&parser));
        }
    }

    /// Получить парсер по расширению файла
    pub fn get_parser(&self, extension: &str) -> Option<&dyn LanguageParser> {
        self.parsers.get(extension).map(|p| p.as_ref())
    }

    /// Список всех поддерживаемых расширений
    pub fn supported_extensions(&self) -> Vec<&str> {
        self.parsers.keys().map(|k| k.as_str()).collect()
    }
}

/// Получить парсер по расширению файла (обратная совместимость).
/// Предпочтительно использовать ParserRegistry::new_all().
pub fn get_parser_for_extension(ext: &str) -> Option<Box<dyn LanguageParser>> {
    match ext {
        "py" => Some(Box::new(python::PythonParser::new())),
        "js" | "jsx" => Some(Box::new(javascript::JavaScriptParser::new())),
        "ts" | "tsx" => Some(Box::new(typescript::TypeScriptParser::new())),
        "java" => Some(Box::new(java::JavaParser::new())),
        "rs" => Some(Box::new(rust_lang::RustParser::new())),
        "bsl" | "os" => Some(Box::new(bsl::BslParser::new())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parser_registry_new_all() {
        let reg = ParserRegistry::new_all();
        assert!(reg.get_parser("py").is_some(), "Python парсер должен быть в реестре");
        assert!(reg.get_parser("js").is_some(), "JavaScript парсер должен быть в реестре");
        assert!(reg.get_parser("jsx").is_some(), "JSX парсер должен быть в реестре");
        assert!(reg.get_parser("ts").is_some(), "TypeScript парсер должен быть в реестре");
        assert!(reg.get_parser("tsx").is_some(), "TSX парсер должен быть в реестре");
        assert!(reg.get_parser("java").is_some(), "Java парсер должен быть в реестре");
        assert!(reg.get_parser("rs").is_some(), "Rust парсер должен быть в реестре");
        assert!(reg.get_parser("unknown").is_none(), "Неизвестное расширение должно давать None");
    }

    #[test]
    fn test_parser_registry_from_languages() {
        let reg = ParserRegistry::from_languages(&["python".to_string()]);
        assert!(reg.get_parser("py").is_some(), "Python должен быть при явном указании");
        assert!(reg.get_parser("js").is_none(), "JS не должен быть — не указан");
        assert!(reg.get_parser("ts").is_none(), "TS не должен быть — не указан");
        assert!(reg.get_parser("java").is_none(), "Java не должен быть — не указан");
    }

    #[test]
    fn test_parser_registry_from_languages_js_ts() {
        let reg = ParserRegistry::from_languages(&[
            "javascript".to_string(),
            "typescript".to_string(),
        ]);
        assert!(reg.get_parser("js").is_some());
        assert!(reg.get_parser("jsx").is_some());
        assert!(reg.get_parser("ts").is_some());
        assert!(reg.get_parser("tsx").is_some());
        assert!(reg.get_parser("py").is_none());
    }

    #[test]
    fn test_parser_registry_unknown_language() {
        // Неизвестный язык не должен вызывать панику; go — не поддерживается
        let reg = ParserRegistry::from_languages(&["rust".to_string(), "go".to_string()]);
        // Rust теперь поддерживается — .rs должен найтись
        assert!(reg.get_parser("rs").is_some());
        // Go не поддерживается — остаётся None
        assert!(reg.get_parser("go").is_none());
    }

    #[test]
    fn test_get_parser_for_extension_compat() {
        // Функция обратной совместимости
        assert!(get_parser_for_extension("py").is_some());
        assert!(get_parser_for_extension("js").is_some());
        assert!(get_parser_for_extension("ts").is_some());
        assert!(get_parser_for_extension("java").is_some());
        assert!(get_parser_for_extension("cpp").is_none());
    }
}
