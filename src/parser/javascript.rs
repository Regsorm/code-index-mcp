use anyhow::{anyhow, Result};
use sha2::{Digest, Sha256};
use hex;

use super::types::{
    ParseResult, ParsedCall, ParsedClass, ParsedFunction, ParsedImport, ParsedVariable,
};
use super::LanguageParser;

/// Парсер JavaScript-файлов на основе tree-sitter
pub struct JavaScriptParser;

impl JavaScriptParser {
    pub fn new() -> Self {
        JavaScriptParser
    }
}

impl LanguageParser for JavaScriptParser {
    fn language_name(&self) -> &str {
        "javascript"
    }

    fn file_extensions(&self) -> &[&str] {
        &["js", "jsx"]
    }

    fn parse(&self, source: &str, _file_path: &str) -> Result<ParseResult> {
        parse_javascript(source, false)
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
fn find_child_by_kind<'a>(node: tree_sitter::Node<'a>, kind: &str) -> Option<tree_sitter::Node<'a>> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == kind {
            return Some(child);
        }
    }
    None
}

/// Извлечь JSDoc-комментарий перед узлом (comment, начинающийся с /**)
fn extract_jsdoc(node: tree_sitter::Node, source: &[u8]) -> Option<String> {
    // Ищем предыдущий сестринский узел типа comment
    let parent = node.parent()?;
    let mut cursor = parent.walk();
    let mut prev_comment: Option<tree_sitter::Node> = None;
    for child in parent.children(&mut cursor) {
        if child.id() == node.id() {
            break;
        }
        if child.kind() == "comment" {
            let text = node_text(child, source);
            if text.starts_with("/**") {
                prev_comment = Some(child);
            } else {
                prev_comment = None;
            }
        } else if !child.is_extra() {
            // Если между комментарием и функцией есть другой узел — сбрасываем
            prev_comment = None;
        }
    }
    prev_comment.map(|n| node_text(n, source).to_string())
}

/// Контекст обхода AST JavaScript
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

/// Рекурсивный обход узла AST JavaScript
fn visit_node(
    node: tree_sitter::Node,
    ctx: &mut VisitContext,
    class_name: Option<&str>,
    current_func: Option<&str>,
    parent_kind: &str,
    depth: usize,
) {
    // Ограничение глубины во избежание переполнения стека
    if depth > 50 {
        return;
    }

    match node.kind() {
        "function_declaration" => {
            visit_function_declaration(node, ctx, class_name, current_func);
        }
        "function" => {
            // Анонимная функция — только если это выражение-значение переменной
            // (обрабатывается в visit_variable_declarator)
        }
        "arrow_function" => {
            // Стрелочные функции обрабатываются в visit_variable_declarator
            // если они на уровне модуля — здесь только вложенные
        }
        "method_definition" => {
            visit_method_definition(node, ctx, class_name, current_func);
        }
        "class_declaration" | "class" => {
            visit_class(node, ctx, current_func, depth);
        }
        "import_declaration" | "import_statement" => {
            visit_import(node, ctx);
        }
        "call_expression" => {
            visit_call(node, ctx, current_func);
            // Рекурсивно обходим аргументы вызова
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "arguments" {
                    let mut arg_cursor = child.walk();
                    for arg in child.children(&mut arg_cursor) {
                        visit_node(arg, ctx, class_name, current_func, child.kind(), depth + 1);
                    }
                }
            }
        }
        "lexical_declaration" | "variable_declaration" => {
            // const/let/var объявления на верхнем уровне программы — переменные и стрелочные функции
            visit_variable_declaration(node, ctx, class_name, current_func, parent_kind, depth);
        }
        "export_statement" => {
            // export default function / export const / export class
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                match child.kind() {
                    "function_declaration" => {
                        visit_function_declaration(child, ctx, class_name, current_func);
                    }
                    "class_declaration" => {
                        visit_class(child, ctx, current_func, depth);
                    }
                    "lexical_declaration" | "variable_declaration" => {
                        visit_variable_declaration(child, ctx, class_name, current_func, node.kind(), depth);
                    }
                    _ => {
                        visit_node(child, ctx, class_name, current_func, node.kind(), depth + 1);
                    }
                }
            }
        }
        _ => {
            // Рекурсивно обходим дочерние узлы
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                visit_node(child, ctx, class_name, current_func, node.kind(), depth + 1);
            }
        }
    }
}

/// Обработать function_declaration
fn visit_function_declaration(
    node: tree_sitter::Node,
    ctx: &mut VisitContext,
    class_name: Option<&str>,
    _current_func: Option<&str>,
) {
    let source = ctx.source;

    // Имя функции: поле name (identifier)
    let name = node.child_by_field_name("name")
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();

    if name.is_empty() {
        return;
    }

    let qualified_name = class_name.map(|cn| format!("{}.{}", cn, name));
    let line_start = node.start_position().row + 1;
    let line_end = node.end_position().row + 1;

    // Параметры
    let args = node.child_by_field_name("parameters")
        .map(|n| node_text(n, source).to_string());

    // is_async: ищем "async" среди дочерних узлов перед именем функции
    let is_async = is_async_node(node, source);

    // JSDoc
    let docstring = extract_jsdoc(node, source);

    let body = node_text(node, source).to_string();
    let node_hash = sha256_hex(&body);

    let func = ParsedFunction {
        name: name.clone(),
        qualified_name,
        line_start,
        line_end,
        args,
        return_type: None,
        docstring,
        body,
        is_async,
        node_hash,
    };
    ctx.functions.push(func);

    // Рекурсивно обходим тело функции
    if let Some(body_node) = node.child_by_field_name("body") {
        let mut cursor = body_node.walk();
        for child in body_node.children(&mut cursor) {
            visit_node(child, ctx, class_name, Some(&name.clone()), body_node.kind(), 1);
        }
    }
}

/// Обработать method_definition внутри класса
fn visit_method_definition(
    node: tree_sitter::Node,
    ctx: &mut VisitContext,
    class_name: Option<&str>,
    _current_func: Option<&str>,
) {
    let source = ctx.source;

    // Имя метода: поле name
    let name = node.child_by_field_name("name")
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();

    if name.is_empty() {
        return;
    }

    // qualified_name: ClassName.methodName
    let qualified_name = class_name.map(|cn| format!("{}.{}", cn, name));

    let line_start = node.start_position().row + 1;
    let line_end = node.end_position().row + 1;

    // Параметры из поля value (function)
    let args = node.child_by_field_name("value")
        .and_then(|func_node| func_node.child_by_field_name("parameters"))
        .map(|n| node_text(n, source).to_string());

    let is_async = is_async_node(node, source);

    let docstring = extract_jsdoc(node, source);
    let body = node_text(node, source).to_string();
    let node_hash = sha256_hex(&body);

    ctx.functions.push(ParsedFunction {
        name: name.clone(),
        qualified_name,
        line_start,
        line_end,
        args,
        return_type: None,
        docstring,
        body,
        is_async,
        node_hash,
    });

    // Рекурсивно обходим тело метода
    if let Some(func_val) = node.child_by_field_name("value") {
        if let Some(body_node) = func_val.child_by_field_name("body") {
            let mut cursor = body_node.walk();
            for child in body_node.children(&mut cursor) {
                visit_node(child, ctx, class_name, Some(&name.clone()), body_node.kind(), 1);
            }
        }
    }
}

/// Обработать объявление переменной (const/let/var) — извлекаем переменные и стрелочные функции
fn visit_variable_declaration(
    node: tree_sitter::Node,
    ctx: &mut VisitContext,
    class_name: Option<&str>,
    current_func: Option<&str>,
    parent_kind: &str,
    depth: usize,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "variable_declarator" {
            visit_variable_declarator(child, ctx, class_name, current_func, parent_kind, depth);
        } else {
            // Обходим другие дочерние (например, вызовы в инициализаторах)
            visit_node(child, ctx, class_name, current_func, node.kind(), depth + 1);
        }
    }
}

/// Обработать variable_declarator: const add = (a, b) => a + b
fn visit_variable_declarator(
    node: tree_sitter::Node,
    ctx: &mut VisitContext,
    class_name: Option<&str>,
    current_func: Option<&str>,
    parent_kind: &str,
    depth: usize,
) {
    let source = ctx.source;

    let name_node = node.child_by_field_name("name");
    let value_node = node.child_by_field_name("value");

    let var_name = name_node
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();

    if var_name.is_empty() {
        return;
    }

    if let Some(val) = value_node {
        match val.kind() {
            "arrow_function" | "function" => {
                // Стрелочная функция или function expression — трактуем как функцию
                let line_start = node.start_position().row + 1;
                let line_end = node.end_position().row + 1;

                let qualified_name = class_name.map(|cn| format!("{}.{}", cn, var_name));

                let args = val.child_by_field_name("parameters")
                    .map(|n| node_text(n, source).to_string())
                    // Одиночный параметр стрелочной функции: `x => x+1`
                    .or_else(|| val.child_by_field_name("parameter")
                        .map(|n| node_text(n, source).to_string()));

                let is_async = is_async_node(val, source);
                let docstring = extract_jsdoc(node.parent().unwrap_or(node), source);
                let body = node_text(node, source).to_string();
                let node_hash = sha256_hex(&body);

                ctx.functions.push(ParsedFunction {
                    name: var_name.clone(),
                    qualified_name,
                    line_start,
                    line_end,
                    args,
                    return_type: None,
                    docstring,
                    body,
                    is_async,
                    node_hash,
                });

                // Рекурсивно обходим тело
                if let Some(body_node) = val.child_by_field_name("body") {
                    let mut cursor = body_node.walk();
                    for child in body_node.children(&mut cursor) {
                        visit_node(child, ctx, class_name, Some(&var_name.clone()), body_node.kind(), depth + 1);
                    }
                }
            }
            _ => {
                // Обычная переменная — сохраняем только на верхнем уровне программы
                if parent_kind == "program" || parent_kind == "module" || parent_kind == "export_statement" {
                    let value_text = node_text(val, source);
                    let value = if value_text.chars().count() > 200 {
                        value_text.chars().take(200).collect()
                    } else {
                        value_text.to_string()
                    };
                    ctx.variables.push(ParsedVariable {
                        name: var_name,
                        value: Some(value),
                        line: node.start_position().row + 1,
                    });
                }
                // Ищем вызовы внутри значения
                visit_node(val, ctx, class_name, current_func, node.kind(), depth + 1);
            }
        }
    }
}

/// Обработать class_declaration / class
fn visit_class(
    node: tree_sitter::Node,
    ctx: &mut VisitContext,
    current_func: Option<&str>,
    depth: usize,
) {
    let source = ctx.source;

    // Имя класса
    let name = node.child_by_field_name("name")
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();

    if name.is_empty() {
        return;
    }

    let line_start = node.start_position().row + 1;
    let line_end = node.end_position().row + 1;

    // Базовые классы: узел class_heritage (extends X)
    // В tree-sitter-javascript это не поле, а дочерний узел типа "class_heritage"
    let bases = find_child_by_kind(node, "class_heritage")
        .map(|n| node_text(n, source).to_string());

    let docstring = extract_jsdoc(node, source);
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

    // Рекурсивно обходим тело класса
    if let Some(class_body) = node.child_by_field_name("body") {
        let mut cursor = class_body.walk();
        for child in class_body.children(&mut cursor) {
            visit_node(child, ctx, Some(&name), current_func, class_body.kind(), depth + 1);
        }
    }
}

/// Обработать import_declaration
fn visit_import(node: tree_sitter::Node, ctx: &mut VisitContext) {
    let source = ctx.source;
    let line = node.start_position().row + 1;

    // Источник импорта: поле source (string-literal "module-name")
    let module = node.child_by_field_name("source")
        .map(|n| {
            // Убираем кавычки
            let text = node_text(n, source);
            text.trim_matches(|c| c == '"' || c == '\'').to_string()
        });

    // Импортируемые имена: import_clause
    let mut cursor = node.walk();
    let mut found_names = false;

    for child in node.children(&mut cursor) {
        match child.kind() {
            "import_clause" => {
                found_names = true;
                // import DefaultExport, { Named1, Named2 as Alias } from "..."
                let mut clause_cursor = child.walk();
                for clause_child in child.children(&mut clause_cursor) {
                    match clause_child.kind() {
                        "identifier" => {
                            // default import
                            ctx.imports.push(ParsedImport {
                                module: module.clone(),
                                name: Some(node_text(clause_child, source).to_string()),
                                alias: None,
                                line,
                                kind: "import".to_string(),
                            });
                        }
                        "namespace_import" => {
                            // import * as ns
                            let alias = find_child_by_kind(clause_child, "identifier")
                                .map(|n| node_text(n, source).to_string());
                            ctx.imports.push(ParsedImport {
                                module: module.clone(),
                                name: Some("*".to_string()),
                                alias,
                                line,
                                kind: "import".to_string(),
                            });
                        }
                        "named_imports" => {
                            // { Named1, Named2 as Alias }
                            let mut ni_cursor = clause_child.walk();
                            for import_spec in clause_child.children(&mut ni_cursor) {
                                if import_spec.kind() == "import_specifier" {
                                    let spec_name = import_spec.child_by_field_name("name")
                                        .map(|n| node_text(n, source).to_string());
                                    let spec_alias = import_spec.child_by_field_name("alias")
                                        .map(|n| node_text(n, source).to_string());
                                    ctx.imports.push(ParsedImport {
                                        module: module.clone(),
                                        name: spec_name,
                                        alias: spec_alias,
                                        line,
                                        kind: "from".to_string(),
                                    });
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    // Если нет import_clause — это `import "side-effect"`
    if !found_names {
        ctx.imports.push(ParsedImport {
            module,
            name: None,
            alias: None,
            line,
            kind: "import".to_string(),
        });
    }
}

/// Обработать call_expression
fn visit_call(node: tree_sitter::Node, ctx: &mut VisitContext, current_func: Option<&str>) {
    let source = ctx.source;
    let line = node.start_position().row + 1;

    // Callee: поле function
    let callee = if let Some(func_node) = node.child_by_field_name("function") {
        node_text(func_node, source).to_string()
    } else {
        return;
    };

    let caller = current_func.unwrap_or("<module>").to_string();
    ctx.calls.push(ParsedCall { caller, callee, line });
}

/// Определить, является ли функция асинхронной (наличие "async" keyword)
fn is_async_node(node: tree_sitter::Node, source: &[u8]) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "async" {
            return true;
        }
        if node_text(child, source) == "async" {
            return true;
        }
    }
    false
}

/// Главная функция парсинга JavaScript/JSX
fn parse_javascript(source: &str, _is_jsx: bool) -> Result<ParseResult> {
    let mut ts_parser = tree_sitter::Parser::new();
    ts_parser
        .set_language(&tree_sitter_javascript::LANGUAGE.into())
        .map_err(|e| anyhow!("Ошибка установки языка tree-sitter-javascript: {}", e))?;

    let tree = ts_parser
        .parse(source, None)
        .ok_or_else(|| anyhow!("tree-sitter не смог распарсить файл"))?;

    let root = tree.root_node();
    let source_bytes = source.as_bytes();

    let ast_hash = sha256_hex(&root.to_sexp());
    let lines_total = source.lines().count();

    let mut ctx = VisitContext::new(source_bytes);
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        visit_node(child, &mut ctx, None, None, "program", 0);
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
    fn test_parse_js_function() {
        let parser = JavaScriptParser::new();
        let source = "function hello(name) {\n  return `Hello ${name}`;\n}\n";
        let result = parser.parse(source, "test.js").unwrap();
        assert_eq!(result.functions.len(), 1);
        assert_eq!(result.functions[0].name, "hello");
        assert!(!result.functions[0].is_async);
    }

    #[test]
    fn test_parse_js_async_function() {
        let parser = JavaScriptParser::new();
        let source = "async function fetchData(url) {\n  const res = await fetch(url);\n  return res.json();\n}\n";
        let result = parser.parse(source, "test.js").unwrap();
        assert_eq!(result.functions.len(), 1);
        assert_eq!(result.functions[0].name, "fetchData");
        assert!(result.functions[0].is_async);
    }

    #[test]
    fn test_parse_js_class() {
        let parser = JavaScriptParser::new();
        let source = "class Animal {\n  constructor(name) {\n    this.name = name;\n  }\n  speak() {\n    return this.name;\n  }\n}\n";
        let result = parser.parse(source, "test.js").unwrap();
        assert_eq!(result.classes.len(), 1);
        assert_eq!(result.classes[0].name, "Animal");
        // Методы класса должны быть извлечены
        assert!(result.functions.len() >= 1, "ожидаем методы класса: {:?}", result.functions);
    }

    #[test]
    fn test_parse_js_arrow() {
        let parser = JavaScriptParser::new();
        let source = "const add = (a, b) => a + b;\n";
        let result = parser.parse(source, "test.js").unwrap();
        assert_eq!(result.functions.len(), 1, "стрелочная функция должна быть в functions");
        assert_eq!(result.functions[0].name, "add");
    }

    #[test]
    fn test_parse_js_import() {
        let parser = JavaScriptParser::new();
        let source = "import React from 'react';\nimport { useState, useEffect } from 'react';\n";
        let result = parser.parse(source, "test.js").unwrap();
        assert!(result.imports.len() >= 2, "ожидаем минимум 2 импорта, получили: {:?}", result.imports);
    }

    #[test]
    fn test_parse_js_class_with_extends() {
        let parser = JavaScriptParser::new();
        let source = "class Dog extends Animal {\n  speak() {\n    return 'Woof!';\n  }\n}\n";
        let result = parser.parse(source, "test.js").unwrap();
        assert_eq!(result.classes.len(), 1);
        assert_eq!(result.classes[0].name, "Dog");
        assert!(result.classes[0].bases.is_some());
    }

    #[test]
    fn test_parse_js_calls() {
        let parser = JavaScriptParser::new();
        let source = "function process() {\n  fetch('url');\n  console.log('done');\n}\n";
        let result = parser.parse(source, "test.js").unwrap();
        assert!(result.calls.len() >= 1, "ожидаем вызовы функций");
    }
}
