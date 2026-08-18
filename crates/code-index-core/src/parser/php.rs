use anyhow::{anyhow, Result};

use super::types::{
    sha256_hex, ParseResult, ParsedCall, ParsedClass, ParsedFunction, ParsedImport,
    ParsedVariable,
};
use super::LanguageParser;
use super::types::MAX_VISIT_DEPTH;
use super::types::PARSE_TIMEOUT_MS;

/// Парсер PHP-файлов на основе tree-sitter.
///
/// Используем грамматику `LANGUAGE_PHP` (полную, с поддержкой инлайн-HTML между
/// `?> ... <?php`), а не `LANGUAGE_PHP_ONLY` — потому что шаблоны Битрикса
/// массово смешивают PHP и HTML в одном файле.
pub struct PhpParser;

impl PhpParser {
    pub fn new() -> Self {
        PhpParser
    }
}

impl LanguageParser for PhpParser {
    fn language_name(&self) -> &str {
        "php"
    }

    fn file_extensions(&self) -> &[&str] {
        &["php", "php5", "phtml"]
    }

    fn parse(&self, source: &str, _file_path: &str) -> Result<ParseResult> {
        parse_php(source)
    }
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

/// Найти дочерний узел по field name
fn find_child_by_field<'a>(node: tree_sitter::Node<'a>, field: &str) -> Option<tree_sitter::Node<'a>> {
    node.child_by_field_name(field)
}

/// Извлечь PHPDoc-комментарий (`/** ... */`), стоящий непосредственно перед
/// объявлением функции/класса. В PHP docstring — это предшествующий узел-комментарий,
/// а не первая инструкция тела (в отличие от Python).
fn extract_phpdoc(node: tree_sitter::Node, source: &[u8]) -> Option<String> {
    let prev = node.prev_sibling()?;
    if prev.kind() == "comment" {
        let text = node_text(prev, source);
        if text.starts_with("/**") {
            return Some(text.to_string());
        }
    }
    None
}

/// Найти тело класса/интерфейса/трейта/enum (`declaration_list` или `enum_declaration_list`).
fn find_body_node<'a>(node: tree_sitter::Node<'a>) -> Option<tree_sitter::Node<'a>> {
    find_child_by_field(node, "body")
        .or_else(|| find_child_by_kind(node, "declaration_list"))
        .or_else(|| find_child_by_kind(node, "enum_declaration_list"))
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
/// - `class_name` — имя класса-контейнера (если функция является методом)
/// - `current_func` — имя ближайшей функции-контейнера (caller у вызовов)
/// - `parent_kind` — kind родительского узла
fn visit_node(
    node: tree_sitter::Node,
    ctx: &mut VisitContext,
    class_name: Option<&str>,
    current_func: Option<&str>,
    parent_kind: &str,
    depth: usize,
) {
    // Предел глубины обхода — защита от переполнения стека (P-2)
    if depth > MAX_VISIT_DEPTH {
        return;
    }

    match node.kind() {
        "function_definition" | "method_declaration" => {
            visit_function(node, ctx, class_name);
        }
        "class_declaration" | "interface_declaration" | "trait_declaration"
        | "enum_declaration" => {
            visit_class(node, ctx);
        }
        "namespace_use_declaration" => {
            visit_use(node, ctx);
        }
        "include_expression"
        | "include_once_expression"
        | "require_expression"
        | "require_once_expression" => {
            visit_require(node, ctx);
            // require/include может содержать вложенные вызовы в выражении пути
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                visit_node(child, ctx, class_name, current_func, node.kind(), depth + 1);
            }
        }
        "function_call_expression"
        | "member_call_expression"
        | "nullsafe_member_call_expression"
        | "scoped_call_expression" => {
            visit_call(node, ctx, current_func);
            // Рекурсивно обходим детей (аргументы, цепочки вызовов) для вложенных вызовов
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                visit_node(child, ctx, class_name, current_func, node.kind(), depth + 1);
            }
        }
        "expression_statement" => {
            // Присваивания переменных на уровне модуля (parent == "program")
            if parent_kind == "program" {
                if let Some(assign) = find_child_by_kind(node, "assignment_expression") {
                    visit_assignment(assign, ctx);
                }
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                visit_node(child, ctx, class_name, current_func, node.kind(), depth + 1);
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

/// Обработать function_definition / method_declaration
fn visit_function(node: tree_sitter::Node, ctx: &mut VisitContext, class_name: Option<&str>) {
    let source = ctx.source;

    let name = find_child_by_field(node, "name")
        .or_else(|| find_child_by_kind(node, "name"))
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();

    if name.is_empty() {
        return;
    }

    // Метод класса → квалифицированное имя "Класс::метод"
    let qualified_name = class_name.map(|cn| format!("{}::{}", cn, name));

    let line_start = node.start_position().row + 1;
    let line_end = node.end_position().row + 1;

    // Параметры (formal_parameters)
    let args = find_child_by_field(node, "parameters")
        .or_else(|| find_child_by_kind(node, "formal_parameters"))
        .map(|n| node_text(n, source).to_string());

    // Тип возвращаемого значения (после ":")
    let return_type = find_child_by_field(node, "return_type").map(|n| node_text(n, source).to_string());

    // Тело (compound_statement)
    let body_node = find_child_by_field(node, "body")
        .or_else(|| find_child_by_kind(node, "compound_statement"));

    let docstring = extract_phpdoc(node, source);
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
        is_async: false, // в PHP нет async-функций
        node_hash,
        ..Default::default()
    });

    // Рекурсивно обходим тело функции — вложенные вызовы, переменные и т.д.
    if let Some(body_node) = body_node {
        let mut cursor = body_node.walk();
        for child in body_node.children(&mut cursor) {
            visit_node(child, ctx, class_name, Some(&name), body_node.kind(), 1);
        }
    }
}

/// Обработать class_declaration / interface_declaration / trait_declaration / enum_declaration
fn visit_class(node: tree_sitter::Node, ctx: &mut VisitContext) {
    let source = ctx.source;

    let name = find_child_by_field(node, "name")
        .or_else(|| find_child_by_kind(node, "name"))
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();

    if name.is_empty() {
        return;
    }

    let line_start = node.start_position().row + 1;
    let line_end = node.end_position().row + 1;

    // Наследование: base_clause (extends) + class_interface_clause (implements)
    let extends = find_child_by_kind(node, "base_clause").map(|n| node_text(n, source).to_string());
    let implements =
        find_child_by_kind(node, "class_interface_clause").map(|n| node_text(n, source).to_string());
    let bases = match (extends, implements) {
        (Some(e), Some(i)) => Some(format!("{} {}", e, i)),
        (Some(e), None) => Some(e),
        (None, Some(i)) => Some(i),
        (None, None) => None,
    };

    let docstring = extract_phpdoc(node, source);
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

    // Рекурсивно обходим тело класса, передавая имя класса (для методов)
    if let Some(body_node) = find_body_node(node) {
        let mut cursor = body_node.walk();
        for child in body_node.children(&mut cursor) {
            visit_node(child, ctx, Some(&name), None, body_node.kind(), 1);
        }
    }
}

/// Обработать namespace_use_declaration (`use Foo\Bar as Baz;`)
fn visit_use(node: tree_sitter::Node, ctx: &mut VisitContext) {
    let source = ctx.source;
    let line = node.start_position().row + 1;

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "namespace_use_clause" {
            // Имя импортируемого символа (qualified_name или name)
            let module = find_child_by_kind(child, "qualified_name")
                .or_else(|| find_child_by_kind(child, "name"))
                .map(|n| node_text(n, source).to_string());
            // Псевдоним (as X) — в грамматике 0.23 это поле `alias` самого clause
            let alias = find_child_by_field(child, "alias")
                .map(|n| node_text(n, source).to_string());
            ctx.imports.push(ParsedImport {
                module,
                name: None,
                alias,
                line,
                kind: "use".to_string(),
            });
        }
    }
}

/// Обработать include/require — путь как импорт
fn visit_require(node: tree_sitter::Node, ctx: &mut VisitContext) {
    let source = ctx.source;
    let line = node.start_position().row + 1;

    let kind = if node.kind().starts_with("require") {
        "require"
    } else {
        "include"
    };

    // Путь — последний именованный дочерний узел (само выражение пути)
    let mut path: Option<String> = None;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.is_named() {
            path = Some(node_text(child, source).to_string());
        }
    }

    ctx.imports.push(ParsedImport {
        module: path,
        name: None,
        alias: None,
        line,
        kind: kind.to_string(),
    });
}

/// Обработать вызов: function_call / member_call / scoped_call / nullsafe_member_call
fn visit_call(node: tree_sitter::Node, ctx: &mut VisitContext, current_func: Option<&str>) {
    let source = ctx.source;
    let line = node.start_position().row + 1;

    // Для обычного вызова callee — поле "function"; для методов — поле "name".
    let callee_node = node
        .child_by_field_name("function")
        .or_else(|| node.child_by_field_name("name"));

    let callee = match callee_node {
        Some(n) => node_text(n, source).to_string(),
        None => return,
    };

    if callee.is_empty() {
        return;
    }

    let caller = current_func.unwrap_or("<module>").to_string();
    ctx.calls.push(ParsedCall { caller, callee, line });
}

/// Обработать присваивание переменной на уровне модуля (`$x = ...;`)
fn visit_assignment(node: tree_sitter::Node, ctx: &mut VisitContext) {
    let source = ctx.source;
    let line = node.start_position().row + 1;

    let name_node = node.child_by_field_name("left").or_else(|| node.child(0));
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

    let value = node
        .child_by_field_name("right")
        .or_else(|| node.child(2))
        .map(|n| {
            let text = node_text(n, source).to_string();
            if text.chars().count() > 200 {
                text.chars().take(200).collect::<String>()
            } else {
                text
            }
        });

    ctx.variables.push(ParsedVariable { name, value, line });
}

/// Главная функция парсинга PHP-файла
fn parse_php(source: &str) -> Result<ParseResult> {
    let mut ts_parser = tree_sitter::Parser::new();
    ts_parser
        .set_language(&tree_sitter_php::LANGUAGE_PHP.into())
        .map_err(|e| anyhow!("Ошибка установки языка tree-sitter-php: {}", e))?;

    // Дедлайн разбора — страховка от нелинейной деградации tree-sitter на
    // патологическом вводе (минифицированные и сгенерированные файлы).
    #[allow(deprecated)]
    ts_parser.set_timeout_micros(PARSE_TIMEOUT_MS * 1000);

    let tree = ts_parser
        .parse(source, None)
        .ok_or_else(|| anyhow!("tree-sitter не смог распарсить файл"))?;

    let root = tree.root_node();
    let source_bytes = source.as_bytes();

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
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::LanguageParser;

    #[test]
    fn test_parse_simple_function() {
        let parser = PhpParser::new();
        let source = r#"<?php
/** Приветствие. */
function hello(string $name): string {
    return "Hello, {$name}!";
}
"#;
        let result = parser.parse(source, "test.php").unwrap();
        assert_eq!(result.functions.len(), 1);
        assert_eq!(result.functions[0].name, "hello");
        assert!(result.functions[0].docstring.is_some());
        assert!(!result.functions[0].is_async);
    }

    #[test]
    fn test_parse_class_with_methods() {
        let parser = PhpParser::new();
        let source = r#"<?php
class MyClass extends Base {
    public function methodOne() {
        return 1;
    }

    private function methodTwo($x) {
        return $x * 2;
    }
}
"#;
        let result = parser.parse(source, "test.php").unwrap();
        assert_eq!(result.classes.len(), 1);
        assert_eq!(result.classes[0].name, "MyClass");
        assert_eq!(result.functions.len(), 2);
        assert_eq!(
            result.functions[0].qualified_name,
            Some("MyClass::methodOne".to_string())
        );
        assert_eq!(
            result.functions[1].qualified_name,
            Some("MyClass::methodTwo".to_string())
        );
    }

    #[test]
    fn test_parse_interface_and_trait() {
        let parser = PhpParser::new();
        let source = r#"<?php
interface Runnable {
    public function run();
}

trait Loggable {
    public function log($msg) {}
}
"#;
        let result = parser.parse(source, "test.php").unwrap();
        // interface + trait = 2 «класса»
        assert_eq!(result.classes.len(), 2);
        assert!(result.classes.iter().any(|c| c.name == "Runnable"));
        assert!(result.classes.iter().any(|c| c.name == "Loggable"));
    }

    #[test]
    fn test_parse_use_imports() {
        let parser = PhpParser::new();
        let source = r#"<?php
use enterego\EnteregoUser;
use Bitrix\Main\Loader as BxLoader;
"#;
        let result = parser.parse(source, "test.php").unwrap();
        assert!(result.imports.iter().any(|i| i.kind == "use"
            && i.module.as_deref() == Some("enterego\\EnteregoUser")));
        assert!(result
            .imports
            .iter()
            .any(|i| i.alias.as_deref() == Some("BxLoader")));
    }

    #[test]
    fn test_parse_calls() {
        let parser = PhpParser::new();
        let source = r#"<?php
function process() {
    $data = fetchData();
    $result = transform($data);
    save($result);
}

print_r("done");
"#;
        let result = parser.parse(source, "test.php").unwrap();
        assert!(result.calls.iter().any(|c| c.callee == "fetchData"));
        assert!(result.calls.iter().any(|c| c.callee == "transform"));
        assert!(result.calls.iter().any(|c| c.callee == "save"));
        // Вызов на уровне модуля
        let module_call = result.calls.iter().find(|c| c.callee == "print_r");
        assert!(module_call.is_some());
        assert_eq!(module_call.unwrap().caller, "<module>");
    }

    #[test]
    fn test_parse_method_call_callee() {
        let parser = PhpParser::new();
        let source = r#"<?php
function run() {
    $obj->doWork();
    Helper::staticCall();
}
"#;
        let result = parser.parse(source, "test.php").unwrap();
        assert!(result.calls.iter().any(|c| c.callee == "doWork"));
        assert!(result.calls.iter().any(|c| c.callee == "staticCall"));
    }

    #[test]
    fn test_parse_inline_html_mixed() {
        // Битрикс-стиль: PHP вперемешку с HTML
        let parser = PhpParser::new();
        let source = r#"<?php
function header() { return "h"; }
?>
<div>Some HTML</div>
<?php
function footer() { return "f"; }
"#;
        let result = parser.parse(source, "template.php").unwrap();
        assert!(result.functions.iter().any(|f| f.name == "header"));
        assert!(result.functions.iter().any(|f| f.name == "footer"));
    }
}
