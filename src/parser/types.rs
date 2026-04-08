use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Вычислить SHA-256 хеш строки → hex
pub fn sha256_hex(data: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data.as_bytes());
    hex::encode(hasher.finalize())
}

/// Инкрементальный SHA-256 хеш дерева AST — без материализации S-expression.
/// Обходит дерево рекурсивно, кормит хешер kind + позициями каждого узла.
/// Для файла 80K строк: ~100x быстрее чем to_sexp() + sha256.
pub fn hash_ast(node: tree_sitter::Node) -> String {
    let mut hasher = Sha256::new();
    hash_ast_node(node, &mut hasher);
    hex::encode(hasher.finalize())
}

fn hash_ast_node(node: tree_sitter::Node, hasher: &mut Sha256) {
    // Кормим kind узла + границы (start_byte, end_byte)
    hasher.update(node.kind().as_bytes());
    hasher.update(&node.start_byte().to_le_bytes());
    hasher.update(&node.end_byte().to_le_bytes());
    // Рекурсивно обходим дочерние узлы
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        hash_ast_node(child, hasher);
    }
}

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
