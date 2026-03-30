use anyhow::{anyhow, Result};
use sha2::{Digest, Sha256};
use hex;

use super::types::{
    ParseResult, ParsedCall, ParsedClass, ParsedFunction, ParsedImport, ParsedVariable,
};
use super::LanguageParser;

/// Парсер TypeScript/TSX-файлов на основе tree-sitter
pub struct TypeScriptParser;

impl TypeScriptParser {
    pub fn new() -> Self {
        TypeScriptParser
    }
}

impl LanguageParser for TypeScriptParser {
    fn language_name(&self) -> &str {
        "typescript"
    }

    fn file_extensions(&self) -> &[&str] {
        &["ts", "tsx"]
    }

    fn parse(&self, source: &str, file_path: &str) -> Result<ParseResult> {
        // Для .tsx используем LANGUAGE_TSX, для .ts — LANGUAGE_TYPESCRIPT
        let is_tsx = file_path.ends_with(".tsx");
        parse_typescript(source, is_tsx)
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

/// Извлечь JSDoc-комментарий перед узлом
fn extract_jsdoc(node: tree_sitter::Node, source: &[u8]) -> Option<String> {
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
            prev_comment = None;
        }
    }
    prev_comment.map(|n| node_text(n, source).to_string())
}

/// Определить, является ли функция/метод асинхронным
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

/// Контекст обхода AST TypeScript
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

/// Рекурсивный обход узла AST TypeScript
fn visit_node(
    node: tree_sitter::Node,
    ctx: &mut VisitContext,
    class_name: Option<&str>,
    current_func: Option<&str>,
    parent_kind: &str,
    depth: usize,
) {
    if depth > 50 {
        return;
    }

    match node.kind() {
        "function_declaration" => {
            visit_function_declaration(node, ctx, class_name, current_func);
        }
        "method_definition" | "method_signature" => {
            visit_method_definition(node, ctx, class_name, current_func);
        }
        "class_declaration" | "abstract_class_declaration" => {
            visit_class_node(node, ctx, current_func, "class", depth);
        }
        "interface_declaration" => {
            // Интерфейс — трактуем как класс с пометкой
            visit_class_node(node, ctx, current_func, "interface", depth);
        }
        "type_alias_declaration" => {
            // type Alias = ... — трактуем как класс
            visit_type_alias(node, ctx);
        }
        "import_statement" => {
            visit_import(node, ctx);
        }
        "call_expression" => {
            visit_call(node, ctx, current_func);
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "arguments" || child.kind() == "type_arguments" {
                    let mut arg_cursor = child.walk();
                    for arg in child.children(&mut arg_cursor) {
                        visit_node(arg, ctx, class_name, current_func, child.kind(), depth + 1);
                    }
                }
            }
        }
        "lexical_declaration" | "variable_declaration" => {
            visit_variable_declaration(node, ctx, class_name, current_func, parent_kind, depth);
        }
        "export_statement" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                match child.kind() {
                    "function_declaration" => {
                        visit_function_declaration(child, ctx, class_name, current_func);
                    }
                    "class_declaration" | "abstract_class_declaration" => {
                        visit_class_node(child, ctx, current_func, "class", depth);
                    }
                    "interface_declaration" => {
                        visit_class_node(child, ctx, current_func, "interface", depth);
                    }
                    "type_alias_declaration" => {
                        visit_type_alias(child, ctx);
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
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                visit_node(child, ctx, class_name, current_func, node.kind(), depth + 1);
            }
        }
    }
}

/// Обработать function_declaration (в том числе TypeScript с типами)
fn visit_function_declaration(
    node: tree_sitter::Node,
    ctx: &mut VisitContext,
    class_name: Option<&str>,
    _current_func: Option<&str>,
) {
    let source = ctx.source;

    let name = node.child_by_field_name("name")
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();

    if name.is_empty() {
        return;
    }

    let qualified_name = class_name.map(|cn| format!("{}.{}", cn, name));
    let line_start = node.start_position().row + 1;
    let line_end = node.end_position().row + 1;

    let args = node.child_by_field_name("parameters")
        .map(|n| node_text(n, source).to_string());

    // Тип возвращаемого значения: поле return_type
    let return_type = node.child_by_field_name("return_type")
        .map(|n| node_text(n, source).trim_start_matches(':').trim().to_string());

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
        return_type,
        docstring,
        body,
        is_async,
        node_hash,
        ..Default::default()
    });

    if let Some(body_node) = node.child_by_field_name("body") {
        let mut cursor = body_node.walk();
        for child in body_node.children(&mut cursor) {
            visit_node(child, ctx, class_name, Some(&name.clone()), body_node.kind(), 1);
        }
    }
}

/// Обработать method_definition/method_signature
fn visit_method_definition(
    node: tree_sitter::Node,
    ctx: &mut VisitContext,
    class_name: Option<&str>,
    _current_func: Option<&str>,
) {
    let source = ctx.source;

    let name = node.child_by_field_name("name")
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();

    if name.is_empty() {
        return;
    }

    let qualified_name = class_name.map(|cn| format!("{}.{}", cn, name));
    let line_start = node.start_position().row + 1;
    let line_end = node.end_position().row + 1;

    // Параметры и return_type могут быть напрямую или через поле value (function)
    let args = node.child_by_field_name("parameters")
        .or_else(|| node.child_by_field_name("value")
            .and_then(|v| v.child_by_field_name("parameters")))
        .map(|n| node_text(n, source).to_string());

    let return_type = node.child_by_field_name("return_type")
        .map(|n| node_text(n, source).trim_start_matches(':').trim().to_string());

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
        return_type,
        docstring,
        body,
        is_async,
        node_hash,
        ..Default::default()
    });

    // Тело метода
    let body_node = node.child_by_field_name("value")
        .and_then(|v| v.child_by_field_name("body"))
        .or_else(|| node.child_by_field_name("body"));

    if let Some(body_node) = body_node {
        let mut cursor = body_node.walk();
        for child in body_node.children(&mut cursor) {
            visit_node(child, ctx, class_name, Some(&name.clone()), body_node.kind(), 1);
        }
    }
}

/// Обработать class_declaration / interface_declaration / abstract_class_declaration
fn visit_class_node(
    node: tree_sitter::Node,
    ctx: &mut VisitContext,
    current_func: Option<&str>,
    kind_hint: &str,
    depth: usize,
) {
    let source = ctx.source;

    let name = node.child_by_field_name("name")
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();

    if name.is_empty() {
        return;
    }

    let line_start = node.start_position().row + 1;
    let line_end = node.end_position().row + 1;

    // Базовые классы: heritage clause (extends/implements)
    let bases = if kind_hint == "interface" {
        node.child_by_field_name("extends_clause")
            .map(|n| node_text(n, source).to_string())
    } else {
        node.child_by_field_name("superclass")
            .map(|n| {
                let extends_text = node_text(n, source).to_string();
                // Добавляем implements если есть
                if let Some(impl_clause) = node.child_by_field_name("implements_clause") {
                    format!("{} {}", extends_text, node_text(impl_clause, source))
                } else {
                    extends_text
                }
            })
            .or_else(|| node.child_by_field_name("implements_clause")
                .map(|n| node_text(n, source).to_string()))
    };

    // Для интерфейсов добавляем пометку в bases
    let bases = if kind_hint == "interface" {
        if let Some(b) = bases {
            Some(format!("interface {}", b))
        } else {
            Some("interface".to_string())
        }
    } else {
        bases
    };

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

    // Рекурсивно обходим тело класса/интерфейса
    if let Some(class_body) = node.child_by_field_name("body") {
        let mut cursor = class_body.walk();
        for child in class_body.children(&mut cursor) {
            visit_node(child, ctx, Some(&name), current_func, class_body.kind(), depth + 1);
        }
    }
}

/// Обработать type_alias_declaration: type Alias = SomeType
fn visit_type_alias(node: tree_sitter::Node, ctx: &mut VisitContext) {
    let source = ctx.source;

    let name = node.child_by_field_name("name")
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();

    if name.is_empty() {
        return;
    }

    let line_start = node.start_position().row + 1;
    let line_end = node.end_position().row + 1;

    // Значение типа
    let bases = node.child_by_field_name("value")
        .map(|n| node_text(n, source).to_string());

    let body = node_text(node, source).to_string();
    let node_hash = sha256_hex(&body);

    ctx.classes.push(ParsedClass {
        name,
        line_start,
        line_end,
        bases,
        docstring: None,
        body,
        node_hash,
    });
}

/// Обработать объявление переменной (const/let/var)
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
            visit_node(child, ctx, class_name, current_func, node.kind(), depth + 1);
        }
    }
}

/// Обработать variable_declarator
fn visit_variable_declarator(
    node: tree_sitter::Node,
    ctx: &mut VisitContext,
    class_name: Option<&str>,
    current_func: Option<&str>,
    parent_kind: &str,
    depth: usize,
) {
    let source = ctx.source;

    let var_name = node.child_by_field_name("name")
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();

    if var_name.is_empty() {
        return;
    }

    let value_node = node.child_by_field_name("value");

    if let Some(val) = value_node {
        match val.kind() {
            "arrow_function" | "function" => {
                let line_start = node.start_position().row + 1;
                let line_end = node.end_position().row + 1;

                let qualified_name = class_name.map(|cn| format!("{}.{}", cn, var_name));

                let args = val.child_by_field_name("parameters")
                    .map(|n| node_text(n, source).to_string())
                    .or_else(|| val.child_by_field_name("parameter")
                        .map(|n| node_text(n, source).to_string()));

                let return_type = val.child_by_field_name("return_type")
                    .map(|n| node_text(n, source).trim_start_matches(':').trim().to_string());

                let is_async = is_async_node(val, source);
                let docstring = None;
                let body = node_text(node, source).to_string();
                let node_hash = sha256_hex(&body);

                ctx.functions.push(ParsedFunction {
                    name: var_name.clone(),
                    qualified_name,
                    line_start,
                    line_end,
                    args,
                    return_type,
                    docstring,
                    body,
                    is_async,
                    node_hash,
                    ..Default::default()
                });

                if let Some(body_node) = val.child_by_field_name("body") {
                    let mut cursor = body_node.walk();
                    for child in body_node.children(&mut cursor) {
                        visit_node(child, ctx, class_name, Some(&var_name.clone()), body_node.kind(), depth + 1);
                    }
                }
            }
            _ => {
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
                visit_node(val, ctx, class_name, current_func, node.kind(), depth + 1);
            }
        }
    }
}

/// Обработать import_statement (TypeScript)
fn visit_import(node: tree_sitter::Node, ctx: &mut VisitContext) {
    let source = ctx.source;
    let line = node.start_position().row + 1;

    let module = node.child_by_field_name("source")
        .map(|n| {
            let text = node_text(n, source);
            text.trim_matches(|c| c == '"' || c == '\'').to_string()
        });

    let mut cursor = node.walk();
    let mut found_names = false;

    for child in node.children(&mut cursor) {
        if child.kind() == "import_clause" {
            found_names = true;
            let mut clause_cursor = child.walk();
            for clause_child in child.children(&mut clause_cursor) {
                match clause_child.kind() {
                    "identifier" => {
                        ctx.imports.push(ParsedImport {
                            module: module.clone(),
                            name: Some(node_text(clause_child, source).to_string()),
                            alias: None,
                            line,
                            kind: "import".to_string(),
                        });
                    }
                    "namespace_import" => {
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
    }

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

    let callee = if let Some(func_node) = node.child_by_field_name("function") {
        node_text(func_node, source).to_string()
    } else {
        return;
    };

    let caller = current_func.unwrap_or("<module>").to_string();
    ctx.calls.push(ParsedCall { caller, callee, line });
}

/// Главная функция парсинга TypeScript/TSX
fn parse_typescript(source: &str, is_tsx: bool) -> Result<ParseResult> {
    let mut ts_parser = tree_sitter::Parser::new();

    if is_tsx {
        ts_parser
            .set_language(&tree_sitter_typescript::LANGUAGE_TSX.into())
            .map_err(|e| anyhow!("Ошибка установки языка tree-sitter-tsx: {}", e))?;
    } else {
        ts_parser
            .set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
            .map_err(|e| anyhow!("Ошибка установки языка tree-sitter-typescript: {}", e))?;
    }

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
    fn test_parse_ts_function() {
        let parser = TypeScriptParser::new();
        let source = "function greet(name: string): string {\n  return `Hello, ${name}`;\n}\n";
        let result = parser.parse(source, "test.ts").unwrap();
        assert_eq!(result.functions.len(), 1);
        assert_eq!(result.functions[0].name, "greet");
    }

    #[test]
    fn test_parse_ts_interface() {
        let parser = TypeScriptParser::new();
        let source = "interface User {\n  name: string;\n  age: number;\n}\n";
        let result = parser.parse(source, "test.ts").unwrap();
        assert_eq!(result.classes.len(), 1);
        assert_eq!(result.classes[0].name, "User");
        // Интерфейс должен иметь пометку
        assert!(result.classes[0].bases.as_deref().unwrap_or("").contains("interface"));
    }

    #[test]
    fn test_parse_ts_class() {
        let parser = TypeScriptParser::new();
        let source = "class Service {\n  private name: string;\n  constructor(name: string) {\n    this.name = name;\n  }\n  getName(): string {\n    return this.name;\n  }\n}\n";
        let result = parser.parse(source, "test.ts").unwrap();
        assert_eq!(result.classes.len(), 1);
        assert_eq!(result.classes[0].name, "Service");
        assert!(result.functions.len() >= 1);
    }

    #[test]
    fn test_parse_ts_type_alias() {
        let parser = TypeScriptParser::new();
        let source = "type UserId = string | number;\n";
        let result = parser.parse(source, "test.ts").unwrap();
        assert_eq!(result.classes.len(), 1);
        assert_eq!(result.classes[0].name, "UserId");
    }

    #[test]
    fn test_parse_ts_arrow() {
        let parser = TypeScriptParser::new();
        let source = "const multiply = (a: number, b: number): number => a * b;\n";
        let result = parser.parse(source, "test.ts").unwrap();
        assert_eq!(result.functions.len(), 1);
        assert_eq!(result.functions[0].name, "multiply");
    }

    #[test]
    fn test_parse_tsx_file() {
        let parser = TypeScriptParser::new();
        let source = "import React from 'react';\nfunction App() {\n  return <div>Hello</div>;\n}\n";
        let result = parser.parse(source, "test.tsx").unwrap();
        assert!(result.functions.len() >= 1);
        assert!(result.functions.iter().any(|f| f.name == "App"));
    }
}
