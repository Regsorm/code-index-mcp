use anyhow::{anyhow, Result};
use sha2::{Digest, Sha256};
use hex;

use super::types::{
    ParseResult, ParsedCall, ParsedClass, ParsedFunction, ParsedImport, ParsedVariable,
};
use super::LanguageParser;

/// Парсер Python-файлов на основе tree-sitter
pub struct PythonParser;

impl PythonParser {
    pub fn new() -> Self {
        PythonParser
    }
}

impl LanguageParser for PythonParser {
    fn language_name(&self) -> &str {
        "python"
    }

    fn file_extensions(&self) -> &[&str] {
        &["py"]
    }

    fn parse(&self, source: &str, _file_path: &str) -> Result<ParseResult> {
        parse_python(source)
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

/// Извлечь docstring из тела функции или класса.
/// Ищет первый expression_statement в блоке body, содержащий строковой литерал.
fn extract_docstring(body_node: tree_sitter::Node, source: &[u8]) -> Option<String> {
    let mut cursor = body_node.walk();
    for child in body_node.children(&mut cursor) {
        if child.kind() == "expression_statement" {
            // Первый дочерний элемент expression_statement
            if let Some(expr) = child.child(0) {
                let kind = expr.kind();
                if kind == "string" || kind == "concatenated_string" {
                    return Some(node_text(expr, source).to_string());
                }
            }
        }
        // Заходим только в первый значимый узел body (docstring всегда первый)
        break;
    }
    None
}

/// Контекст обхода AST
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

/// Рекурсивный обход узла AST.
/// - `node` — текущий узел
/// - `ctx` — контекст сбора данных
/// - `class_name` — имя класса-контейнера (если функция является методом)
/// - `current_func` — имя ближайшей функции-контейнера (для определения caller у вызовов)
/// - `parent_kind` — kind родительского узла
fn visit_node(
    node: tree_sitter::Node,
    ctx: &mut VisitContext,
    class_name: Option<&str>,
    current_func: Option<&str>,
    parent_kind: &str,
) {
    match node.kind() {
        "function_definition" => {
            visit_function(node, ctx, class_name, current_func, parent_kind);
        }
        "decorated_definition" => {
            // Декорированное определение может содержать async function_definition
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "function_definition" {
                    visit_function(child, ctx, class_name, current_func, node.kind());
                } else if child.kind() == "class_definition" {
                    visit_class(child, ctx, current_func);
                } else {
                    // Рекурсивно обходим остальные дочерние узлы (например, декораторы)
                    visit_node(child, ctx, class_name, current_func, node.kind());
                }
            }
        }
        "class_definition" => {
            visit_class(node, ctx, current_func);
        }
        "import_statement" => {
            visit_import(node, ctx, false);
        }
        "import_from_statement" => {
            visit_import(node, ctx, true);
        }
        "call" => {
            visit_call(node, ctx, current_func);
            // Рекурсивно обходим аргументы вызова для вложенных вызовов
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() != "function" && child.kind() != "identifier" && child.kind() != "attribute" {
                    visit_node(child, ctx, class_name, current_func, node.kind());
                }
            }
        }
        "expression_statement" => {
            // Переменные на уровне модуля: parent == "module"
            if parent_kind == "module" {
                if let Some(assign) = find_child_by_kind(node, "assignment") {
                    visit_assignment(assign, ctx);
                } else if let Some(assign) = find_child_by_kind(node, "augmented_assignment") {
                    // Пропускаем += -= и т.д.
                    let _ = assign;
                }
            }
            // В любом случае рекурсивно обходим дочерние узлы для поиска вызовов
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                visit_node(child, ctx, class_name, current_func, node.kind());
            }
        }
        "assignment" => {
            // Переменные на уровне модуля
            if parent_kind == "module" {
                visit_assignment(node, ctx);
            }
            // Рекурсивно обходим правую часть для вызовов
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                visit_node(child, ctx, class_name, current_func, node.kind());
            }
        }
        _ => {
            // Рекурсивно обходим дочерние узлы
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                visit_node(child, ctx, class_name, current_func, node.kind());
            }
        }
    }
}

/// Найти первый дочерний узел с заданным kind
fn find_child_by_kind<'a>(node: tree_sitter::Node<'a>, kind: &str) -> Option<tree_sitter::Node<'a>> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == kind {
            return Some(child);
        }
    }
    None
}

/// Найти дочерний узел по field name
fn find_child_by_field<'a>(node: tree_sitter::Node<'a>, field: &str) -> Option<tree_sitter::Node<'a>> {
    node.child_by_field_name(field)
}

/// Обработать function_definition
fn visit_function(
    node: tree_sitter::Node,
    ctx: &mut VisitContext,
    class_name: Option<&str>,
    _current_func: Option<&str>,
    parent_kind: &str,
) {
    let source = ctx.source;

    // Имя функции
    let name = find_child_by_field(node, "name")
        .or_else(|| find_child_by_kind(node, "identifier"))
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();

    if name.is_empty() {
        return;
    }

    // Qualified name: "ClassName.func_name" если метод класса
    let qualified_name = class_name.map(|cn| format!("{}.{}", cn, name));

    // Позиция
    let line_start = node.start_position().row + 1;
    let line_end = node.end_position().row + 1;

    // Аргументы
    let args = find_child_by_field(node, "parameters")
        .or_else(|| find_child_by_kind(node, "parameters"))
        .map(|n| node_text(n, source).to_string());

    // Тип возвращаемого значения (аннотация после ->)
    let return_type = node.child_by_field_name("return_type")
        .map(|n| node_text(n, source).to_string());

    // Тело функции
    let body_node = find_child_by_field(node, "body")
        .or_else(|| find_child_by_kind(node, "block"));

    // Docstring
    let docstring = body_node.and_then(|b| extract_docstring(b, source));

    // Полный текст функции
    let body = node_text(node, source).to_string();

    // is_async: проверяем наличие "async" keyword перед def
    // В tree-sitter-python async_def — это "function_definition" с первым дочерним "async"
    // ИЛИ parent == "decorated_definition" и нода содержит async
    let is_async = is_async_function(node, source, parent_kind);

    // Хеш тела функции
    let node_hash = sha256_hex(&body);

    // Вставляем функцию
    let func = ParsedFunction {
        name: name.clone(),
        qualified_name,
        line_start,
        line_end,
        args,
        return_type,
        docstring,
        body,
        is_async,
        node_hash,
    };
    ctx.functions.push(func);

    // Рекурсивно обходим тело функции (для вложенных вызовов, переменных и т.д.)
    if let Some(body_node) = find_child_by_field(node, "body")
        .or_else(|| find_child_by_kind(node, "block"))
    {
        let mut cursor = body_node.walk();
        for child in body_node.children(&mut cursor) {
            visit_node(child, ctx, class_name, Some(&name.clone()), body_node.kind());
        }
    }
}

/// Определить, является ли функция асинхронной.
/// В tree-sitter-python async def парсится как function_definition,
/// у которого первый именованный дочерний элемент — "async".
fn is_async_function(node: tree_sitter::Node, source: &[u8], _parent_kind: &str) -> bool {
    // Ищем "async" среди дочерних узлов до "def"
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let kind = child.kind();
        if kind == "async" {
            return true;
        }
        if kind == "def" || kind == "identifier" {
            break;
        }
        // Проверяем текст узла типа keyword
        if node_text(child, source) == "async" {
            return true;
        }
    }
    false
}

/// Обработать class_definition
fn visit_class(
    node: tree_sitter::Node,
    ctx: &mut VisitContext,
    current_func: Option<&str>,
) {
    let source = ctx.source;

    // Имя класса
    let name = find_child_by_field(node, "name")
        .or_else(|| find_child_by_kind(node, "identifier"))
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();

    if name.is_empty() {
        return;
    }

    let line_start = node.start_position().row + 1;
    let line_end = node.end_position().row + 1;

    // Базовые классы (argument_list)
    let bases = find_child_by_field(node, "superclasses")
        .or_else(|| find_child_by_kind(node, "argument_list"))
        .map(|n| node_text(n, source).to_string());

    // Тело класса
    let body_node = find_child_by_field(node, "body")
        .or_else(|| find_child_by_kind(node, "block"));

    // Docstring
    let docstring = body_node.and_then(|b| extract_docstring(b, source));

    // Полный текст класса
    let body = node_text(node, source).to_string();
    let node_hash = sha256_hex(&body);

    ctx.classes.push(ParsedClass {
        name: name.clone(),
        line_start,
        line_end,
        bases,
        docstring,
        body,
        node_hash,
    });

    // Рекурсивно обходим тело класса, передавая имя класса
    if let Some(body_node) = find_child_by_field(node, "body")
        .or_else(|| find_child_by_kind(node, "block"))
    {
        let mut cursor = body_node.walk();
        for child in body_node.children(&mut cursor) {
            visit_node(child, ctx, Some(&name), current_func, body_node.kind());
        }
    }
}

/// Обработать import_statement и import_from_statement
fn visit_import(node: tree_sitter::Node, ctx: &mut VisitContext, is_from: bool) {
    let source = ctx.source;
    let line = node.start_position().row + 1;

    if !is_from {
        // import os, sys, os.path as op
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "dotted_name" => {
                    ctx.imports.push(ParsedImport {
                        module: Some(node_text(child, source).to_string()),
                        name: None,
                        alias: None,
                        line,
                        kind: "import".to_string(),
                    });
                }
                "aliased_import" => {
                    // aliased_import: dotted_name "as" identifier
                    let module = find_child_by_kind(child, "dotted_name")
                        .map(|n| node_text(n, source).to_string());
                    let alias = find_child_by_field(child, "alias")
                        .or_else(|| find_child_by_kind(child, "identifier"))
                        .map(|n| node_text(n, source).to_string());
                    ctx.imports.push(ParsedImport {
                        module,
                        name: None,
                        alias,
                        line,
                        kind: "import".to_string(),
                    });
                }
                _ => {}
            }
        }
    } else {
        // from os.path import join, exists
        // from django.db import models as db_models
        let module = node.child_by_field_name("module_name")
            .map(|n| node_text(n, source).to_string());

        // Импортируемые имена
        let mut cursor = node.walk();
        let mut found_name = false;
        for child in node.children(&mut cursor) {
            match child.kind() {
                "dotted_name" => {
                    // Имя (не модуль — модуль уже извлечён через field)
                    if found_name {
                        ctx.imports.push(ParsedImport {
                            module: module.clone(),
                            name: Some(node_text(child, source).to_string()),
                            alias: None,
                            line,
                            kind: "from".to_string(),
                        });
                    }
                    found_name = true; // первый dotted_name — это модуль, остальные — имена
                }
                "import" => {
                    // Ключевое слово import — после него идут имена
                    found_name = true;
                }
                "aliased_import" => {
                    let name_node = find_child_by_kind(child, "dotted_name")
                        .or_else(|| find_child_by_kind(child, "identifier"));
                    let name = name_node.map(|n| node_text(n, source).to_string());
                    let alias = child.child_by_field_name("alias")
                        .map(|n| node_text(n, source).to_string());
                    ctx.imports.push(ParsedImport {
                        module: module.clone(),
                        name,
                        alias,
                        line,
                        kind: "from".to_string(),
                    });
                }
                "identifier" => {
                    if found_name {
                        ctx.imports.push(ParsedImport {
                            module: module.clone(),
                            name: Some(node_text(child, source).to_string()),
                            alias: None,
                            line,
                            kind: "from".to_string(),
                        });
                    }
                }
                "wildcard_import" => {
                    ctx.imports.push(ParsedImport {
                        module: module.clone(),
                        name: Some("*".to_string()),
                        alias: None,
                        line,
                        kind: "from".to_string(),
                    });
                }
                _ => {}
            }
        }
    }
}

/// Обработать вызов функции (call)
fn visit_call(node: tree_sitter::Node, ctx: &mut VisitContext, current_func: Option<&str>) {
    let source = ctx.source;
    let line = node.start_position().row + 1;

    // Callee: поле function узла call
    let callee = if let Some(func_node) = node.child_by_field_name("function") {
        node_text(func_node, source).to_string()
    } else {
        return;
    };

    // Caller: имя ближайшей функции-контейнера или "<module>"
    let caller = current_func.unwrap_or("<module>").to_string();

    ctx.calls.push(ParsedCall { caller, callee, line });
}

/// Обработать присваивание переменной на уровне модуля
fn visit_assignment(node: tree_sitter::Node, ctx: &mut VisitContext) {
    let source = ctx.source;
    let line = node.start_position().row + 1;

    // Левая часть: поле left или первый дочерний узел
    let name_node = node.child_by_field_name("left")
        .or_else(|| node.child(0));
    let name = match name_node {
        Some(n) => {
            let text = node_text(n, source).to_string();
            if text.is_empty() || text == "=" {
                return;
            }
            text
        }
        None => return,
    };

    // Правая часть: поле right или третий дочерний элемент
    let value = node.child_by_field_name("right")
        .or_else(|| {
            // assignment: left "=" right — right это child(2)
            node.child(2)
        })
        .map(|n| {
            let text = node_text(n, source).to_string();
            // Обрезаем до 200 символов (безопасно для UTF-8)
            if text.chars().count() > 200 {
                text.chars().take(200).collect::<String>()
            } else {
                text
            }
        });

    ctx.variables.push(ParsedVariable { name, value, line });
}

/// Главная функция парсинга Python-файла
fn parse_python(source: &str) -> Result<ParseResult> {
    let mut ts_parser = tree_sitter::Parser::new();
    ts_parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .map_err(|e| anyhow!("Ошибка установки языка tree-sitter-python: {}", e))?;

    let tree = ts_parser
        .parse(source, None)
        .ok_or_else(|| anyhow!("tree-sitter не смог распарсить файл"))?;

    let root = tree.root_node();
    let source_bytes = source.as_bytes();

    // Хеш всего AST
    let ast_hash = sha256_hex(&root.to_sexp());

    // Количество строк
    let lines_total = source.lines().count();

    // Обход AST
    let mut ctx = VisitContext::new(source_bytes);
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        visit_node(child, &mut ctx, None, None, "module");
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
    fn test_parse_simple_function() {
        let parser = PythonParser::new();
        let source = r#"
def hello(name: str) -> str:
    """Приветствие."""
    return f"Hello, {name}!"
"#;
        let result = parser.parse(source, "test.py").unwrap();
        assert_eq!(result.functions.len(), 1);
        assert_eq!(result.functions[0].name, "hello");
        assert!(result.functions[0].docstring.is_some());
        assert!(!result.functions[0].is_async);
    }

    #[test]
    fn test_parse_class_with_methods() {
        let parser = PythonParser::new();
        let source = r#"
class MyClass(Base):
    """Мой класс."""

    def method_one(self):
        pass

    def method_two(self, x):
        return x * 2
"#;
        let result = parser.parse(source, "test.py").unwrap();
        assert_eq!(result.classes.len(), 1);
        assert_eq!(result.classes[0].name, "MyClass");
        assert_eq!(result.functions.len(), 2);
        // Методы должны иметь qualified_name
        assert_eq!(
            result.functions[0].qualified_name,
            Some("MyClass.method_one".to_string())
        );
        assert_eq!(
            result.functions[1].qualified_name,
            Some("MyClass.method_two".to_string())
        );
    }

    #[test]
    fn test_parse_imports() {
        let parser = PythonParser::new();
        let source = r#"
import os
import sys
from os.path import join, exists
from django.db import models as db_models
"#;
        let result = parser.parse(source, "test.py").unwrap();
        assert!(result.imports.len() >= 4);
    }

    #[test]
    fn test_parse_calls() {
        let parser = PythonParser::new();
        let source = r#"
def process():
    data = fetch_data()
    result = transform(data)
    save(result)

print("done")
"#;
        let result = parser.parse(source, "test.py").unwrap();
        // Вызовы внутри process: fetch_data, transform, save
        // Вызов на уровне модуля: print
        assert!(result.calls.len() >= 4);
        let module_call = result.calls.iter().find(|c| c.callee == "print");
        assert!(module_call.is_some());
        assert_eq!(module_call.unwrap().caller, "<module>");
    }

    #[test]
    fn test_parse_async_function() {
        let parser = PythonParser::new();
        let source = r#"
async def fetch(url):
    response = await get(url)
    return response
"#;
        let result = parser.parse(source, "test.py").unwrap();
        assert_eq!(result.functions.len(), 1);
        assert!(result.functions[0].is_async);
    }

    #[test]
    fn test_parse_module_variables() {
        let parser = PythonParser::new();
        let source = r#"
MAX_RETRIES = 3
API_URL = "https://api.example.com"
DEBUG = True

def main():
    local_var = 42
"#;
        let result = parser.parse(source, "test.py").unwrap();
        // Только модульные переменные, не локальные
        assert!(result.variables.len() >= 3);
        assert!(result.variables.iter().any(|v| v.name == "MAX_RETRIES"));
    }
}
