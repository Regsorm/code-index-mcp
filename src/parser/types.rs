use serde::{Deserialize, Serialize};

/// Извлечённая функция из AST
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ParsedFunction {
    pub name: String,
    pub qualified_name: Option<String>,
    pub line_start: usize,
    pub line_end: usize,
    pub args: Option<String>,
    pub return_type: Option<String>,
    pub docstring: Option<String>,
    pub body: String,
    pub is_async: bool,
    pub node_hash: String,
    /// Тип переопределения: "Перед", "После", "Вместо" (только BSL-расширения)
    pub override_type: Option<String>,
    /// Имя оригинальной процедуры, которую переопределяет аннотация
    pub override_target: Option<String>,
}

/// Извлечённый класс
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedClass {
    pub name: String,
    pub line_start: usize,
    pub line_end: usize,
    pub bases: Option<String>,
    pub docstring: Option<String>,
    pub body: String,
    pub node_hash: String,
}

/// Извлечённый импорт
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedImport {
    pub module: Option<String>,
    pub name: Option<String>,
    pub alias: Option<String>,
    pub line: usize,
    /// Тип импорта: "import" или "from"
    pub kind: String,
}

/// Извлечённый вызов функции
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedCall {
    pub caller: String,
    pub callee: String,
    pub line: usize,
}

/// Извлечённая переменная
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedVariable {
    pub name: String,
    pub value: Option<String>,
    pub line: usize,
}

/// Результат парсинга одного файла
#[derive(Debug, Clone)]
pub struct ParseResult {
    pub functions: Vec<ParsedFunction>,
    pub classes: Vec<ParsedClass>,
    pub imports: Vec<ParsedImport>,
    pub calls: Vec<ParsedCall>,
    pub variables: Vec<ParsedVariable>,
    pub lines_total: usize,
    pub ast_hash: String,
}

/// Результат парсинга текстового файла
#[derive(Debug, Clone)]
pub struct TextParseResult {
    pub content: String,
    pub lines_total: usize,
}
