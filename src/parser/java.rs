use anyhow::{anyhow, Result};
use sha2::{Digest, Sha256};
use hex;

use super::types::{
    ParseResult, ParsedCall, ParsedClass, ParsedFunction, ParsedImport, ParsedVariable,
};
use super::LanguageParser;

/// Парсер Java-файлов на основе tree-sitter
pub struct JavaParser;

impl JavaParser {
    pub fn new() -> Self {
        JavaParser
    }
}

impl LanguageParser for JavaParser {
    fn language_name(&self) -> &str {
        "java"
    }

    fn file_extensions(&self) -> &[&str] {
        &["java"]
    }

    fn parse(&self, source: &str, _file_path: &str) -> Result<ParseResult> {
        parse_java(source)
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

/// Найти все дочерние узлы с заданным kind
fn find_children_by_kind<'a>(node: tree_sitter::Node<'a>, kind: &str) -> Vec<tree_sitter::Node<'a>> {
    let mut result = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == kind {
            result.push(child);
        }
    }
    result
}

/// Извлечь Javadoc-комментарий перед узлом (block_comment /** ... */)
fn extract_javadoc(node: tree_sitter::Node, source: &[u8]) -> Option<String> {
    let parent = node.parent()?;
    let mut cursor = parent.walk();
    let mut prev_comment: Option<tree_sitter::Node> = None;
    for child in parent.children(&mut cursor) {
        if child.id() == node.id() {
            break;
        }
        if child.kind() == "block_comment" || child.kind() == "line_comment" {
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

/// Контекст обхода AST Java
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

/// Рекурсивный обход узла AST Java
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
        "class_declaration" => {
            visit_class(node, ctx, "class", current_func, depth);
        }
        "interface_declaration" => {
            visit_class(node, ctx, "interface", current_func, depth);
        }
        "enum_declaration" => {
            visit_class(node, ctx, "enum", current_func, depth);
        }
        "method_declaration" => {
            visit_method(node, ctx, class_name, current_func);
        }
        "constructor_declaration" => {
            visit_constructor(node, ctx, class_name, current_func);
        }
        "import_declaration" => {
            visit_import(node, ctx);
        }
        "method_invocation" => {
            visit_call(node, ctx, current_func);
            // Рекурсивно обходим аргументы
            if let Some(args_node) = node.child_by_field_name("arguments") {
                let mut cursor = args_node.walk();
                for arg in args_node.children(&mut cursor) {
                    visit_node(arg, ctx, class_name, current_func, args_node.kind(), depth + 1);
                }
            }
        }
        "field_declaration" => {
            // static/final поля — извлекаем как переменные
            visit_field(node, ctx, parent_kind);
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                visit_node(child, ctx, class_name, current_func, node.kind(), depth + 1);
            }
        }
    }
}

/// Обработать class_declaration / interface_declaration / enum_declaration
fn visit_class(
    node: tree_sitter::Node,
    ctx: &mut VisitContext,
    kind_hint: &str,
    current_func: Option<&str>,
    depth: usize,
) {
    let source = ctx.source;

    // Имя класса: поле name
    let name = node.child_by_field_name("name")
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();

    if name.is_empty() {
        return;
    }

    let line_start = node.start_position().row + 1;
    let line_end = node.end_position().row + 1;

    // Базовые классы/интерфейсы
    let bases = {
        let mut parts = Vec::new();

        // extends SuperClass
        if let Some(superclass) = node.child_by_field_name("superclass") {
            parts.push(format!("extends {}", node_text(superclass, source)));
        }

        // implements Interface1, Interface2
        if let Some(interfaces) = node.child_by_field_name("interfaces") {
            parts.push(node_text(interfaces, source).to_string());
        }

        // extends SuperInterface (для interface declaration)
        if let Some(ext_interfaces) = node.child_by_field_name("extends_interfaces") {
            parts.push(node_text(ext_interfaces, source).to_string());
        }

        if kind_hint == "interface" {
            parts.insert(0, "interface".to_string());
        } else if kind_hint == "enum" {
            parts.insert(0, "enum".to_string());
        }

        if parts.is_empty() {
            None
        } else {
            Some(parts.join(" "))
        }
    };

    let docstring = extract_javadoc(node, source);
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

/// Обработать method_declaration
fn visit_method(
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

    // Параметры: поле parameters
    let args = node.child_by_field_name("parameters")
        .map(|n| node_text(n, source).to_string());

    // Тип возвращаемого значения: поле type
    let return_type = node.child_by_field_name("type")
        .map(|n| node_text(n, source).to_string());

    // Модификаторы (для определения async — в Java нет, но есть в некоторых фреймворках)
    // В Java нет нативного async, is_async = false
    let is_async = false;

    let docstring = extract_javadoc(node, source);
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
    });

    // Рекурсивно обходим тело метода
    if let Some(body_node) = node.child_by_field_name("body") {
        let mut cursor = body_node.walk();
        for child in body_node.children(&mut cursor) {
            visit_node(child, ctx, class_name, Some(&name.clone()), body_node.kind(), 1);
        }
    }
}

/// Обработать constructor_declaration
fn visit_constructor(
    node: tree_sitter::Node,
    ctx: &mut VisitContext,
    class_name: Option<&str>,
    _current_func: Option<&str>,
) {
    let source = ctx.source;

    // Имя конструктора = имя класса
    let name = node.child_by_field_name("name")
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_else(|| class_name.unwrap_or("").to_string());

    if name.is_empty() {
        return;
    }

    let qualified_name = class_name.map(|cn| format!("{}.{}", cn, name));

    let line_start = node.start_position().row + 1;
    let line_end = node.end_position().row + 1;

    let args = node.child_by_field_name("parameters")
        .map(|n| node_text(n, source).to_string());

    let docstring = extract_javadoc(node, source);
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
        is_async: false,
        node_hash,
    });

    // Рекурсивно обходим тело конструктора
    if let Some(body_node) = node.child_by_field_name("body") {
        let mut cursor = body_node.walk();
        for child in body_node.children(&mut cursor) {
            visit_node(child, ctx, class_name, Some(&name.clone()), body_node.kind(), 1);
        }
    }
}

/// Обработать import_declaration
fn visit_import(node: tree_sitter::Node, ctx: &mut VisitContext) {
    let source = ctx.source;
    let line = node.start_position().row + 1;

    // import java.util.List; → module = "java.util", name = "List"
    // import java.util.*; → module = "java.util", name = "*"
    // import static java.lang.Math.PI; → kind = "static"

    let is_static = node.child_by_field_name("static")
        .map(|n| node_text(n, source) == "static")
        .unwrap_or(false);

    // Полное имя импортируемого пути
    let full_path = {
        let mut path = String::new();
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "scoped_identifier" | "identifier" => {
                    path = node_text(child, source).to_string();
                }
                "asterisk" => {
                    path.push_str(".*");
                }
                _ => {}
            }
        }
        path
    };

    if full_path.is_empty() {
        return;
    }

    // Разбиваем на модуль и имя
    let (module, name) = if let Some(pos) = full_path.rfind('.') {
        let mod_part = &full_path[..pos];
        let name_part = &full_path[pos + 1..];
        (Some(mod_part.to_string()), Some(name_part.to_string()))
    } else {
        (None, Some(full_path))
    };

    let kind = if is_static { "static".to_string() } else { "import".to_string() };

    ctx.imports.push(ParsedImport {
        module,
        name,
        alias: None,
        line,
        kind,
    });
}

/// Обработать вызов метода (method_invocation)
fn visit_call(node: tree_sitter::Node, ctx: &mut VisitContext, current_func: Option<&str>) {
    let source = ctx.source;
    let line = node.start_position().row + 1;

    // Callee: поле name (имя метода) + необязательное поле object
    let method_name = node.child_by_field_name("name")
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();

    if method_name.is_empty() {
        return;
    }

    // Объект, на котором вызывается метод
    let callee = if let Some(obj) = node.child_by_field_name("object") {
        format!("{}.{}", node_text(obj, source), method_name)
    } else {
        method_name
    };

    let caller = current_func.unwrap_or("<module>").to_string();
    ctx.calls.push(ParsedCall { caller, callee, line });
}

/// Обработать field_declaration (static/final поля → переменные)
fn visit_field(node: tree_sitter::Node, ctx: &mut VisitContext, _parent_kind: &str) {
    let source = ctx.source;
    let line = node.start_position().row + 1;

    // Проверяем наличие модификаторов static/final
    let modifiers_node = find_child_by_kind(node, "modifiers");
    let is_static_or_final = modifiers_node.map(|m| {
        let mods_text = node_text(m, source);
        mods_text.contains("static") || mods_text.contains("final")
    }).unwrap_or(false);

    if !is_static_or_final {
        return;
    }

    // variable_declarator внутри field_declaration
    let declarators = find_children_by_kind(node, "variable_declarator");
    for decl in declarators {
        let name = decl.child_by_field_name("name")
            .map(|n| node_text(n, source).to_string())
            .unwrap_or_default();

        if name.is_empty() {
            continue;
        }

        let value = decl.child_by_field_name("value")
            .map(|n| {
                let text = node_text(n, source);
                if text.chars().count() > 200 {
                    text.chars().take(200).collect()
                } else {
                    text.to_string()
                }
            });

        ctx.variables.push(ParsedVariable { name, value, line });
    }
}

/// Главная функция парсинга Java-файла
fn parse_java(source: &str) -> Result<ParseResult> {
    let mut ts_parser = tree_sitter::Parser::new();
    ts_parser
        .set_language(&tree_sitter_java::LANGUAGE.into())
        .map_err(|e| anyhow!("Ошибка установки языка tree-sitter-java: {}", e))?;

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
    fn test_parse_java_class() {
        let parser = JavaParser::new();
        let source = "public class Main {\n  public static void main(String[] args) {\n    System.out.println(\"Hello\");\n  }\n}\n";
        let result = parser.parse(source, "Main.java").unwrap();
        assert_eq!(result.classes.len(), 1);
        assert_eq!(result.classes[0].name, "Main");
        assert_eq!(result.functions.len(), 1);
        assert_eq!(result.functions[0].name, "main");
    }

    #[test]
    fn test_parse_java_interface() {
        let parser = JavaParser::new();
        let source = "public interface Runnable {\n  void run();\n}\n";
        let result = parser.parse(source, "Runnable.java").unwrap();
        assert_eq!(result.classes.len(), 1);
        assert_eq!(result.classes[0].name, "Runnable");
        assert!(result.classes[0].bases.as_deref().unwrap_or("").contains("interface"));
    }

    #[test]
    fn test_parse_java_import() {
        let parser = JavaParser::new();
        let source = "import java.util.List;\nimport java.util.ArrayList;\npublic class Test {}\n";
        let result = parser.parse(source, "Test.java").unwrap();
        assert!(result.imports.len() >= 2);
        assert!(result.imports.iter().any(|i| i.name.as_deref() == Some("List")));
    }

    #[test]
    fn test_parse_java_qualified_name() {
        let parser = JavaParser::new();
        let source = "public class Calculator {\n  public int add(int a, int b) {\n    return a + b;\n  }\n  public int subtract(int a, int b) {\n    return a - b;\n  }\n}\n";
        let result = parser.parse(source, "Calculator.java").unwrap();
        assert_eq!(result.classes.len(), 1);
        assert_eq!(result.functions.len(), 2);
        // qualified_name должен содержать имя класса
        let add = result.functions.iter().find(|f| f.name == "add").unwrap();
        assert_eq!(add.qualified_name, Some("Calculator.add".to_string()));
    }

    #[test]
    fn test_parse_java_static_field() {
        let parser = JavaParser::new();
        let source = "public class Config {\n  public static final String HOST = \"localhost\";\n  private int port;\n}\n";
        let result = parser.parse(source, "Config.java").unwrap();
        // Только static/final поля — port не должен быть в variables
        assert!(result.variables.len() >= 1);
        assert!(result.variables.iter().any(|v| v.name == "HOST"));
    }

    #[test]
    fn test_parse_java_calls() {
        let parser = JavaParser::new();
        let source = "public class App {\n  public void run() {\n    System.out.println(\"start\");\n    doWork();\n  }\n  private void doWork() {}\n}\n";
        let result = parser.parse(source, "App.java").unwrap();
        assert!(result.calls.len() >= 1);
    }
}
