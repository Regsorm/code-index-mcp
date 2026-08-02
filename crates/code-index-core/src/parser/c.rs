use anyhow::{anyhow, Result};

use super::types::{
    hash_ast, sha256_hex, ParseResult, ParsedCall, ParsedClass, ParsedFunction, ParsedImport,
    ParsedVariable,
};
use super::LanguageParser;

/// Парсер C-файлов на основе tree-sitter.
/// Функции — `function_definition`; «классы» для C — это `struct`/`union`/`enum`.
pub struct CParser;

impl CParser {
    pub fn new() -> Self {
        CParser
    }
}

impl LanguageParser for CParser {
    fn language_name(&self) -> &str {
        "c"
    }

    fn file_extensions(&self) -> &[&str] {
        &["c", "h"]
    }

    fn parse(&self, source: &str, _file_path: &str) -> Result<ParseResult> {
        parse_c(source, false)
    }
}

fn node_text<'a>(node: tree_sitter::Node<'a>, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

fn find_child_by_kind<'a>(node: tree_sitter::Node<'a>, kind: &str) -> Option<tree_sitter::Node<'a>> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == kind {
            return Some(child);
        }
    }
    None
}

/// Рекурсивно найти первый потомок с заданным kind (обход в глубину).
fn find_descendant_by_kind<'a>(
    node: tree_sitter::Node<'a>,
    kind: &str,
) -> Option<tree_sitter::Node<'a>> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == kind {
            return Some(child);
        }
        if let Some(found) = find_descendant_by_kind(child, kind) {
            return Some(found);
        }
    }
    None
}

/// Извлечь имя из декларатора C: спускаемся через
/// function_declarator / pointer_declarator / parenthesized_declarator / array_declarator
/// до identifier / field_identifier.
fn declarator_name(node: tree_sitter::Node, source: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" | "field_identifier" | "type_identifier" => {
            Some(node_text(node, source).to_string())
        }
        _ => {
            if let Some(d) = node.child_by_field_name("declarator") {
                if let Some(name) = declarator_name(d, source) {
                    return Some(name);
                }
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if let Some(name) = declarator_name(child, source) {
                    return Some(name);
                }
            }
            None
        }
    }
}

struct VisitContext<'a> {
    source: &'a [u8],
    functions: Vec<ParsedFunction>,
    classes: Vec<ParsedClass>,
    imports: Vec<ParsedImport>,
    calls: Vec<ParsedCall>,
    variables: Vec<ParsedVariable>,
    is_cpp: bool,
}

impl<'a> VisitContext<'a> {
    fn new(source: &'a [u8], is_cpp: bool) -> Self {
        VisitContext {
            source,
            functions: Vec::new(),
            classes: Vec::new(),
            imports: Vec::new(),
            calls: Vec::new(),
            variables: Vec::new(),
            is_cpp,
        }
    }
}

fn visit_node(
    node: tree_sitter::Node,
    ctx: &mut VisitContext,
    class_name: Option<&str>,
    current_func: Option<&str>,
) {
    match node.kind() {
        "function_definition" => {
            visit_function(node, ctx, class_name);
        }
        "struct_specifier" | "union_specifier" | "enum_specifier" | "class_specifier" => {
            visit_record(node, ctx);
        }
        "call_expression" => {
            visit_call(node, ctx, current_func);
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                visit_node(child, ctx, class_name, current_func);
            }
        }
        "preproc_include" => {
            visit_include(node, ctx);
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                visit_node(child, ctx, class_name, current_func);
            }
        }
    }
}

fn visit_function(node: tree_sitter::Node, ctx: &mut VisitContext, class_name: Option<&str>) {
    let source = ctx.source;

    let declarator = node.child_by_field_name("declarator");
    let name = declarator
        .and_then(|d| declarator_name(d, source))
        .unwrap_or_default();

    if name.is_empty() {
        return;
    }

    let qualified_name = class_name.map(|cn| format!("{}::{}", cn, name));

    let line_start = node.start_position().row + 1;
    let line_end = node.end_position().row + 1;

    // Параметры — parameter_list внутри function_declarator
    let args = declarator
        .and_then(|d| find_descendant_by_kind(d, "parameter_list"))
        .map(|n| node_text(n, source).to_string());

    let return_type = node.child_by_field_name("type").map(|n| node_text(n, source).to_string());

    let body_node = node.child_by_field_name("body");
    let body = node_text(node, source).to_string();
    let node_hash = sha256_hex(&body);

    ctx.functions.push(ParsedFunction {
        name: name.clone(),
        qualified_name,
        line_start,
        line_end,
        args,
        return_type,
        docstring: None,
        body,
        is_async: false,
        node_hash,
        ..Default::default()
    });

    if let Some(body_node) = body_node {
        let mut cursor = body_node.walk();
        for child in body_node.children(&mut cursor) {
            visit_node(child, ctx, class_name, Some(&name));
        }
    }
}

/// struct / union / enum / (C++) class — как «класс»
fn visit_record(node: tree_sitter::Node, ctx: &mut VisitContext) {
    let source = ctx.source;

    let name = node
        .child_by_field_name("name")
        .or_else(|| find_child_by_kind(node, "type_identifier"))
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();

    if name.is_empty() {
        return; // анонимные записи пропускаем
    }

    let line_start = node.start_position().row + 1;
    let line_end = node.end_position().row + 1;
    let body = node_text(node, source).to_string();
    let node_hash = sha256_hex(&body);

    ctx.classes.push(ParsedClass {
        name: name.clone(),
        line_start,
        line_end,
        bases: None,
        docstring: None,
        body,
        node_hash,
    });

    // В C++ у класса/структуры есть тело с методами — рекурсивно обходим
    if ctx.is_cpp {
        if let Some(body_node) = node.child_by_field_name("body") {
            let mut cursor = body_node.walk();
            for child in body_node.children(&mut cursor) {
                visit_node(child, ctx, Some(&name), None);
            }
        }
    }
}

fn visit_call(node: tree_sitter::Node, ctx: &mut VisitContext, current_func: Option<&str>) {
    let source = ctx.source;
    let line = node.start_position().row + 1;

    let callee = match node.child_by_field_name("function") {
        Some(n) => node_text(n, source).to_string(),
        None => return,
    };
    if callee.is_empty() {
        return;
    }

    let caller = current_func.unwrap_or("<module>").to_string();
    ctx.calls.push(ParsedCall { caller, callee, line });
}

fn visit_include(node: tree_sitter::Node, ctx: &mut VisitContext) {
    let source = ctx.source;
    let line = node.start_position().row + 1;

    let path = node
        .child_by_field_name("path")
        .or_else(|| find_child_by_kind(node, "string_literal"))
        .or_else(|| find_child_by_kind(node, "system_lib_string"))
        .map(|n| node_text(n, source).to_string());

    ctx.imports.push(ParsedImport {
        module: path,
        name: None,
        alias: None,
        line,
        kind: "include".to_string(),
    });
}

/// Общая точка входа для C и C++ (`is_cpp` управляет обходом тел классов).
pub(crate) fn parse_c(source: &str, is_cpp: bool) -> Result<ParseResult> {
    let mut ts_parser = tree_sitter::Parser::new();
    let lang = if is_cpp {
        tree_sitter_cpp::LANGUAGE.into()
    } else {
        tree_sitter_c::LANGUAGE.into()
    };
    ts_parser
        .set_language(&lang)
        .map_err(|e| anyhow!("Ошибка установки языка tree-sitter (C/C++): {}", e))?;

    let tree = ts_parser
        .parse(source, None)
        .ok_or_else(|| anyhow!("tree-sitter не смог распарсить файл"))?;

    let root = tree.root_node();
    let source_bytes = source.as_bytes();
    let ast_hash = hash_ast(root);
    let lines_total = source.lines().count();

    let mut ctx = VisitContext::new(source_bytes, is_cpp);
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        visit_node(child, &mut ctx, None, None);
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
    fn test_parse_c_function() {
        let parser = CParser::new();
        let source = r#"
int add(int a, int b) {
    return a + b;
}
"#;
        let result = parser.parse(source, "test.c").unwrap();
        assert_eq!(result.functions.len(), 1);
        assert_eq!(result.functions[0].name, "add");
    }

    #[test]
    fn test_parse_c_struct_and_call() {
        let parser = CParser::new();
        let source = r#"
struct Point {
    int x;
    int y;
};

void run() {
    do_work();
}
"#;
        let result = parser.parse(source, "test.c").unwrap();
        assert!(result.classes.iter().any(|c| c.name == "Point"));
        assert!(result.calls.iter().any(|c| c.callee == "do_work"));
    }

    #[test]
    fn test_parse_c_include() {
        let parser = CParser::new();
        let source = "#include <stdio.h>\n#include \"local.h\"\n";
        let result = parser.parse(source, "test.c").unwrap();
        assert!(result.imports.len() >= 2);
        assert_eq!(result.imports[0].kind, "include");
    }
}
