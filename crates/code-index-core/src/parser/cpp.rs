use anyhow::Result;

use super::types::ParseResult;
use super::LanguageParser;

/// Парсер C++-файлов. Использует общий обход из `c.rs` с флагом `is_cpp = true`
/// (грамматика tree-sitter-cpp — надмножество C: те же function_definition/
/// call_expression/preproc_include плюс class_specifier с телом-методами и namespace).
pub struct CppParser;

impl CppParser {
    pub fn new() -> Self {
        CppParser
    }
}

impl LanguageParser for CppParser {
    fn language_name(&self) -> &str {
        "cpp"
    }

    fn file_extensions(&self) -> &[&str] {
        // `.h` тоже заявляем: в C++-репо заголовки почти всегда C++-ные.
        // Кто победит на `.h` — определяется порядком регистрации в ParserRegistry.
        &["cpp", "cxx", "cc", "hpp", "hxx", "hh", "h"]
    }

    fn parse(&self, source: &str, _file_path: &str) -> Result<ParseResult> {
        super::c::parse_c(source, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::LanguageParser;

    #[test]
    fn test_parse_cpp_class_method() {
        let parser = CppParser::new();
        let source = r#"
class Widget {
public:
    int compute(int x) {
        return helper(x);
    }
};
"#;
        let result = parser.parse(source, "test.cpp").unwrap();
        assert!(result.classes.iter().any(|c| c.name == "Widget"));
        assert!(result.functions.iter().any(|f| f.name == "compute"));
        assert!(result.calls.iter().any(|c| c.callee == "helper"));
    }

    #[test]
    fn test_parse_cpp_free_function() {
        let parser = CppParser::new();
        let source = r#"
namespace app {
    void start() {
        init();
    }
}
"#;
        let result = parser.parse(source, "test.cpp").unwrap();
        assert!(result.functions.iter().any(|f| f.name == "start"));
        assert!(result.calls.iter().any(|c| c.callee == "init"));
    }
}
