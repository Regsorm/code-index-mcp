use anyhow::{anyhow, Result};
use sha2::{Digest, Sha256};
use hex;

use super::types::{
    ParseResult, ParsedCall, ParsedClass, ParsedFunction, ParsedImport, ParsedVariable,
};
use super::LanguageParser;

/// Парсер Rust-файлов на основе tree-sitter
pub struct RustParser;

impl RustParser {
    pub fn new() -> Self {
        RustParser
    }
}

impl LanguageParser for RustParser {
    fn language_name(&self) -> &str {
        "rust"
    }

    fn file_extensions(&self) -> &[&str] {
        &["rs"]
    }

    fn parse(&self, source: &str, _file_path: &str) -> Result<ParseResult> {
        parse_rust(source)
    }
}

/// Вычислить SHA-256 хеш строки
fn sha256_hex(data: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data.as_bytes());
    hex::encode(hasher.finalize())
}

/// Получить текст узла AST из байтового среза
fn node_text<'a>(node: tree_sitter::Node<'a>, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

/// Найти первый дочерний узел с заданным kind
fn find_child_by_kind<'a>(
    node: tree_sitter::Node<'a>,
    kind: &str,
) -> Option<tree_sitter::Node<'a>> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == kind {
            return Some(child);
        }
    }
    None
}

/// Извлечь doc-комментарий, предшествующий узлу.
/// Ищем `line_comment` начинающиеся с `///` или `block_comment` с `/**`
/// среди предыдущих сестринских узлов.
fn extract_doc_comment(node: tree_sitter::Node, source: &[u8]) -> Option<String> {
    let parent = node.parent()?;
    let mut cursor = parent.walk();
    let mut doc_lines: Vec<String> = Vec::new();

    for child in parent.children(&mut cursor) {
        if child.id() == node.id() {
            break;
        }
        let kind = child.kind();
        if kind == "line_comment" {
            let text = node_text(child, source);
            if text.starts_with("///") {
                doc_lines.push(text.to_string());
            } else {
                // Обычный `//` комментарий прерывает цепочку doc-комментариев
                doc_lines.clear();
            }
        } else if kind == "block_comment" {
            let text = node_text(child, source);
            if text.starts_with("/**") {
                doc_lines.clear();
                doc_lines.push(text.to_string());
            } else {
                doc_lines.clear();
            }
        } else if !child.is_extra() {
            // Не-комментарный узел между doc-комментарием и определением — сбрасываем
            doc_lines.clear();
        }
    }

    if doc_lines.is_empty() {
        None
    } else {
        Some(doc_lines.join("\n"))
    }
}

/// Контекст обхода AST Rust
struct VisitContext<'a> {
    source: &'a [u8],
    functions: Vec<ParsedFunction>,
    classes: Vec<ParsedClass>,
    imports: Vec<ParsedImport>,
    calls: Vec<ParsedCall>,
    variables: Vec<ParsedVariable>,
}

impl<'a> VisitContext<'a> {
    fn new(source: &'a [u8]) -> Self {
        VisitContext {
            source,
            functions: Vec::new(),
            classes: Vec::new(),
            imports: Vec::new(),
            calls: Vec::new(),
            variables: Vec::new(),
        }
    }
}

/// Рекурсивный обход узла AST Rust.
/// - `impl_type` — имя типа из объемлющего `impl_item` (для qualified_name методов)
/// - `current_func` — имя ближайшей функции-контейнера (для caller у вызовов)
/// - `parent_kind` — kind родительского узла
fn visit_node(
    node: tree_sitter::Node,
    ctx: &mut VisitContext,
    impl_type: Option<&str>,
    current_func: Option<&str>,
    parent_kind: &str,
    depth: usize,
) {
    // Ограничение глубины для защиты от переполнения стека
    if depth > 80 {
        return;
    }

    match node.kind() {
        "function_item" => {
            visit_function(node, ctx, impl_type, current_func);
        }
        "impl_item" => {
            visit_impl(node, ctx, current_func, depth);
        }
        "struct_item" => {
            visit_type_decl(node, ctx, None);
        }
        "enum_item" => {
            visit_type_decl(node, ctx, Some("enum"));
        }
        "trait_item" => {
            visit_trait(node, ctx, current_func, depth);
        }
        "use_declaration" => {
            visit_use(node, ctx);
        }
        "call_expression" => {
            visit_call_expr(node, ctx, current_func);
            // Рекурсивно обходим аргументы
            if let Some(args) = node.child_by_field_name("arguments") {
                let mut cur = args.walk();
                for arg in args.children(&mut cur) {
                    visit_node(arg, ctx, impl_type, current_func, args.kind(), depth + 1);
                }
            }
        }
        "method_call_expression" => {
            visit_method_call(node, ctx, current_func);
            // Рекурсивно обходим аргументы
            if let Some(args) = node.child_by_field_name("arguments") {
                let mut cur = args.walk();
                for arg in args.children(&mut cur) {
                    visit_node(arg, ctx, impl_type, current_func, args.kind(), depth + 1);
                }
            }
        }
        "static_item" => {
            visit_static_const(node, ctx);
        }
        "const_item" => {
            visit_static_const(node, ctx);
        }
        _ => {
            // На верхнем уровне модуля обходим let_declaration
            if node.kind() == "let_declaration" && parent_kind == "source_file" {
                visit_let_decl(node, ctx);
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                visit_node(child, ctx, impl_type, current_func, node.kind(), depth + 1);
            }
        }
    }
}

/// Обработать function_item (функция или метод)
fn visit_function(
    node: tree_sitter::Node,
    ctx: &mut VisitContext,
    impl_type: Option<&str>,
    _current_func: Option<&str>,
) {
    let source = ctx.source;

    // Имя функции: поле name (identifier)
    let name = node
        .child_by_field_name("name")
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();

    if name.is_empty() {
        return;
    }

    // qualified_name: для методов в impl — TypeName::method_name
    let qualified_name = impl_type.map(|t| format!("{}::{}", t, name));

    let line_start = node.start_position().row + 1;
    let line_end = node.end_position().row + 1;

    // Параметры: поле parameters
    let args = node
        .child_by_field_name("parameters")
        .map(|n| node_text(n, source).to_string());

    // Тип возвращаемого значения: поле return_type
    let return_type = node
        .child_by_field_name("return_type")
        .map(|n| node_text(n, source).to_string());

    // is_async: проверяем наличие "async" keyword среди дочерних узлов
    let is_async = {
        let mut found = false;
        let mut cur = node.walk();
        for child in node.children(&mut cur) {
            if node_text(child, source) == "async" {
                found = true;
                break;
            }
        }
        found
    };

    // Doc-комментарий перед функцией
    let docstring = extract_doc_comment(node, source);

    // Полный текст функции
    let body = node_text(node, source).to_string();
    let node_hash = sha256_hex(&body);

    let func_name = name.clone();
    ctx.functions.push(ParsedFunction {
        name,
        qualified_name,
        line_start,
        line_end,
        args,
        return_type,
        docstring,
        body,
        is_async,
        node_hash,
    });

    // Рекурсивно обходим тело функции
    if let Some(body_node) = node.child_by_field_name("body") {
        let mut cursor = body_node.walk();
        for child in body_node.children(&mut cursor) {
            visit_node(
                child,
                ctx,
                impl_type,
                Some(&func_name),
                body_node.kind(),
                1,
            );
        }
    }
}

/// Обработать impl_item — определяет контекст типа для методов
fn visit_impl(
    node: tree_sitter::Node,
    ctx: &mut VisitContext,
    current_func: Option<&str>,
    depth: usize,
) {
    let source = ctx.source;

    // Имя типа: поле type в impl_item
    // В tree-sitter-rust это поле называется "type", содержит type_identifier
    let type_name = node
        .child_by_field_name("type")
        .map(|n| node_text(n, source).to_string())
        // Если поле "type" не найдено — ищем первый type_identifier
        .or_else(|| {
            find_child_by_kind(node, "type_identifier")
                .map(|n| node_text(n, source).to_string())
        })
        .unwrap_or_default();

    let impl_type = if type_name.is_empty() {
        None
    } else {
        Some(type_name.as_str())
    };

    // Обходим тело impl
    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            if child.kind() == "function_item" {
                visit_function(child, ctx, impl_type, current_func);
            } else {
                visit_node(child, ctx, impl_type, current_func, body.kind(), depth + 1);
            }
        }
    }
}

/// Обработать struct_item или enum_item → classes
fn visit_type_decl(node: tree_sitter::Node, ctx: &mut VisitContext, kind_hint: Option<&str>) {
    let source = ctx.source;

    // Имя типа: поле name (type_identifier)
    let name = node
        .child_by_field_name("name")
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();

    if name.is_empty() {
        return;
    }

    let line_start = node.start_position().row + 1;
    let line_end = node.end_position().row + 1;

    // bases: для enum → "enum", для struct → None
    let bases = kind_hint.map(|s| s.to_string());

    let docstring = extract_doc_comment(node, source);
    let body = node_text(node, source).to_string();
    let node_hash = sha256_hex(&body);

    ctx.classes.push(ParsedClass {
        name,
        line_start,
        line_end,
        bases,
        docstring,
        body,
        node_hash,
    });
}

/// Обработать trait_item → classes (bases = "trait")
fn visit_trait(
    node: tree_sitter::Node,
    ctx: &mut VisitContext,
    current_func: Option<&str>,
    depth: usize,
) {
    let source = ctx.source;

    let name = node
        .child_by_field_name("name")
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();

    if name.is_empty() {
        return;
    }

    let line_start = node.start_position().row + 1;
    let line_end = node.end_position().row + 1;

    let docstring = extract_doc_comment(node, source);
    let body = node_text(node, source).to_string();
    let node_hash = sha256_hex(&body);

    ctx.classes.push(ParsedClass {
        name: name.clone(),
        line_start,
        line_end,
        bases: Some("trait".to_string()),
        docstring,
        body,
        node_hash,
    });

    // Обходим методы трейта
    if let Some(body_node) = node.child_by_field_name("body") {
        let mut cursor = body_node.walk();
        for child in body_node.children(&mut cursor) {
            if child.kind() == "function_item" {
                // Методы трейта с реализацией по умолчанию — qualified_name TypeName::method
                visit_function(child, ctx, Some(&name), current_func);
            } else {
                visit_node(
                    child,
                    ctx,
                    Some(&name),
                    current_func,
                    body_node.kind(),
                    depth + 1,
                );
            }
        }
    }
}

/// Обработать use_declaration — разобрать use_tree рекурсивно
fn visit_use(node: tree_sitter::Node, ctx: &mut VisitContext) {
    let source = ctx.source;
    let line = node.start_position().row + 1;

    // Ищем use_tree — дочерний узел use_declaration
    if let Some(use_tree) = node.child_by_field_name("argument")
        .or_else(|| find_child_by_kind(node, "use_tree"))
        .or_else(|| find_child_by_kind(node, "scoped_use_list"))
        .or_else(|| find_child_by_kind(node, "identifier"))
        .or_else(|| find_child_by_kind(node, "scoped_identifier"))
    {
        collect_use_tree(use_tree, source, line, None, ctx);
    }
}

/// Рекурсивный обход use_tree для извлечения импортов.
/// `prefix` — накопленный путь (например "std::path")
fn collect_use_tree(
    node: tree_sitter::Node,
    source: &[u8],
    line: usize,
    prefix: Option<&str>,
    ctx: &mut VisitContext,
) {
    match node.kind() {
        "use_tree" => {
            // use_tree может быть:
            // 1. простым путём: `std::path::Path`
            // 2. группой: `std::io::{Read, Write}`
            // 3. псевдонимом: `std::path::Path as P`
            // 4. звёздочкой: `std::prelude::*`

            // Ищем alias (as Ident)
            let alias = node
                .child_by_field_name("alias")
                .map(|n| node_text(n, source).to_string());

            // Путь (поле path или первый scoped_identifier/identifier)
            let path_node = node.child_by_field_name("path")
                .or_else(|| find_child_by_kind(node, "scoped_identifier"))
                .or_else(|| find_child_by_kind(node, "identifier"));

            // Список вложенных деревьев (группа в {})
            let list_node = node.child_by_field_name("list")
                .or_else(|| find_child_by_kind(node, "use_tree_list"));

            // Звёздочка
            let has_star = find_child_by_kind(node, "use_wildcard").is_some()
                || {
                    let mut cur = node.walk();
                    let found = node.children(&mut cur).any(|c| node_text(c, source) == "*");
                    found
                };

            // Формируем накопленный путь для данного узла
            let current_path = if let Some(pn) = path_node {
                let p = node_text(pn, source).to_string();
                if let Some(pref) = prefix {
                    format!("{}::{}", pref, p)
                } else {
                    p
                }
            } else {
                prefix.unwrap_or("").to_string()
            };

            if let Some(list) = list_node {
                // Групповой импорт: std::io::{Read, Write}
                let mut cur = list.walk();
                for child in list.children(&mut cur) {
                    if child.kind() == "use_tree" {
                        collect_use_tree(child, source, line, Some(&current_path), ctx);
                    }
                }
            } else if has_star {
                // use std::prelude::*
                let (module, name) = split_use_path(&current_path);
                let _ = name;
                ctx.imports.push(ParsedImport {
                    module: if module.is_empty() { None } else { Some(module) },
                    name: Some("*".to_string()),
                    alias,
                    line,
                    kind: "use".to_string(),
                });
            } else if !current_path.is_empty() {
                // Простой путь: use std::path::Path
                let (module, name) = split_use_path(&current_path);
                ctx.imports.push(ParsedImport {
                    module: if module.is_empty() { None } else { Some(module) },
                    name: if name.is_empty() { None } else { Some(name) },
                    alias,
                    line,
                    kind: "use".to_string(),
                });
            }
        }
        "scoped_identifier" | "scoped_use_list" => {
            // Это верхний узел вида `std::path::Path` или `std::io::{Read, Write}`
            let text = node_text(node, source).to_string();
            let full_path = if let Some(pref) = prefix {
                format!("{}::{}", pref, text)
            } else {
                text
            };
            let (module, name) = split_use_path(&full_path);
            ctx.imports.push(ParsedImport {
                module: if module.is_empty() { None } else { Some(module) },
                name: if name.is_empty() { None } else { Some(name) },
                alias: None,
                line,
                kind: "use".to_string(),
            });
        }
        "identifier" => {
            // Простой идентификатор без пути
            let name = node_text(node, source).to_string();
            let module = prefix.map(|p| p.to_string());
            ctx.imports.push(ParsedImport {
                module,
                name: Some(name),
                alias: None,
                line,
                kind: "use".to_string(),
            });
        }
        _ => {
            // Для неожиданных типов — пробуем обработать дочерние
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if matches!(child.kind(), "use_tree" | "scoped_identifier" | "identifier") {
                    collect_use_tree(child, source, line, prefix, ctx);
                }
            }
        }
    }
}

/// Разбить путь Rust на модуль и имя.
/// Например: "std::path::Path" → ("std::path", "Path")
///           "anyhow" → ("", "anyhow")
fn split_use_path(path: &str) -> (String, String) {
    if let Some(pos) = path.rfind("::") {
        (path[..pos].to_string(), path[pos + 2..].to_string())
    } else {
        ("".to_string(), path.to_string())
    }
}

/// Обработать call_expression: func(args)
fn visit_call_expr(
    node: tree_sitter::Node,
    ctx: &mut VisitContext,
    current_func: Option<&str>,
) {
    let source = ctx.source;
    let line = node.start_position().row + 1;

    // Callee: поле function
    let callee = if let Some(func_node) = node.child_by_field_name("function") {
        node_text(func_node, source).to_string()
    } else {
        return;
    };

    if callee.is_empty() {
        return;
    }

    let caller = current_func.unwrap_or("<module>").to_string();
    ctx.calls.push(ParsedCall { caller, callee, line });
}

/// Обработать method_call_expression: receiver.method(args)
fn visit_method_call(
    node: tree_sitter::Node,
    ctx: &mut VisitContext,
    current_func: Option<&str>,
) {
    let source = ctx.source;
    let line = node.start_position().row + 1;

    // Имя метода: поле name (field_identifier)
    let method_name = if let Some(name_node) = node.child_by_field_name("name") {
        node_text(name_node, source).to_string()
    } else {
        return;
    };

    // Получатель: поле receiver
    let callee = if let Some(recv) = node.child_by_field_name("receiver") {
        format!("{}.{}", node_text(recv, source), method_name)
    } else {
        method_name
    };

    let caller = current_func.unwrap_or("<module>").to_string();
    ctx.calls.push(ParsedCall { caller, callee, line });
}

/// Обработать static_item и const_item → variables
fn visit_static_const(node: tree_sitter::Node, ctx: &mut VisitContext) {
    let source = ctx.source;
    let line = node.start_position().row + 1;

    // Имя: поле name (identifier)
    let name = node
        .child_by_field_name("name")
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();

    if name.is_empty() {
        return;
    }

    // Значение: поле value
    let value = node.child_by_field_name("value").map(|n| {
        let text = node_text(n, source);
        if text.chars().count() > 200 {
            text.chars().take(200).collect()
        } else {
            text.to_string()
        }
    });

    ctx.variables.push(ParsedVariable { name, value, line });
}

/// Обработать let_declaration на уровне модуля (редкий случай)
fn visit_let_decl(node: tree_sitter::Node, ctx: &mut VisitContext) {
    let source = ctx.source;
    let line = node.start_position().row + 1;

    // Паттерн: поле pattern
    let name = node
        .child_by_field_name("pattern")
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();

    if name.is_empty() {
        return;
    }

    // Значение: поле value
    let value = node.child_by_field_name("value").map(|n| {
        let text = node_text(n, source);
        if text.chars().count() > 200 {
            text.chars().take(200).collect()
        } else {
            text.to_string()
        }
    });

    ctx.variables.push(ParsedVariable { name, value, line });
}

/// Главная функция парсинга Rust-файла
fn parse_rust(source: &str) -> Result<ParseResult> {
    let mut ts_parser = tree_sitter::Parser::new();
    ts_parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .map_err(|e| anyhow!("Ошибка установки языка tree-sitter-rust: {}", e))?;

    let tree = ts_parser
        .parse(source, None)
        .ok_or_else(|| anyhow!("tree-sitter не смог распарсить Rust-файл"))?;

    let root = tree.root_node();
    let source_bytes = source.as_bytes();

    // Хеш AST
    let ast_hash = sha256_hex(&root.to_sexp());

    // Количество строк
    let lines_total = source.lines().count();

    // Обход AST
    let mut ctx = VisitContext::new(source_bytes);
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        visit_node(child, &mut ctx, None, None, "source_file", 0);
    }

    Ok(ParseResult {
        functions: ctx.functions,
        classes: ctx.classes,
        imports: ctx.imports,
        calls: ctx.calls,
        variables: ctx.variables,
        lines_total,
        ast_hash,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::LanguageParser;

    #[test]
    fn test_parse_rust_function() {
        let parser = RustParser::new();
        let source = r#"
/// Вычислить сумму
fn add(a: i32, b: i32) -> i32 {
    a + b
}
"#;
        let result = parser.parse(source, "test.rs").unwrap();
        assert_eq!(result.functions.len(), 1);
        assert_eq!(result.functions[0].name, "add");
        assert!(result.functions[0].docstring.is_some());
    }

    #[test]
    fn test_parse_rust_struct_and_impl() {
        let parser = RustParser::new();
        let source = r#"
pub struct Storage {
    conn: Connection,
}

impl Storage {
    pub fn new() -> Self {
        Self { conn: Connection::open_in_memory().unwrap() }
    }

    pub fn get_stats(&self) -> Result<Stats> {
        todo!()
    }
}
"#;
        let result = parser.parse(source, "test.rs").unwrap();
        assert!(result.classes.iter().any(|c| c.name == "Storage"));
        assert!(result.functions.len() >= 2);
        // Методы имеют qualified_name
        assert!(result
            .functions
            .iter()
            .any(|f| f.qualified_name == Some("Storage::new".to_string())));
    }

    #[test]
    fn test_parse_rust_use() {
        let parser = RustParser::new();
        let source = r#"
use std::path::Path;
use crate::storage::Storage;
use anyhow::Result;
"#;
        let result = parser.parse(source, "test.rs").unwrap();
        assert!(result.imports.len() >= 3);
    }

    #[test]
    fn test_parse_rust_async() {
        let parser = RustParser::new();
        let source = r#"
async fn fetch_data(url: &str) -> Result<String> {
    let resp = reqwest::get(url).await?;
    Ok(resp.text().await?)
}
"#;
        let result = parser.parse(source, "test.rs").unwrap();
        assert_eq!(result.functions.len(), 1);
        assert!(result.functions[0].is_async);
    }

    #[test]
    fn test_parse_rust_enum_and_trait() {
        let parser = RustParser::new();
        let source = r#"
pub enum FileCategory {
    Code(String),
    Text,
    Binary,
}

pub trait LanguageParser: Send + Sync {
    fn language_name(&self) -> &str;
    fn parse(&self, source: &str) -> Result<()>;
}
"#;
        let result = parser.parse(source, "test.rs").unwrap();
        assert!(result.classes.iter().any(|c| c.name == "FileCategory"));
        assert!(result.classes.iter().any(|c| c.name == "LanguageParser"));
    }

    #[test]
    fn test_parse_rust_const_static() {
        let parser = RustParser::new();
        let source = r#"
const MAX_SIZE: usize = 1024;
static COUNTER: AtomicU64 = AtomicU64::new(0);
"#;
        let result = parser.parse(source, "test.rs").unwrap();
        assert!(result.variables.len() >= 2);
        assert!(result.variables.iter().any(|v| v.name == "MAX_SIZE"));
    }
}
