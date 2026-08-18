use anyhow::{anyhow, Result};

use super::types::{
    sha256_hex, ParseResult, ParsedCall, ParsedClass, ParsedFunction, ParsedImport,
    ParsedVariable,
};
use super::LanguageParser;
use super::types::MAX_VISIT_DEPTH;
use super::types::PARSE_TIMEOUT_MS;

/// Парсер Swift-файлов на основе tree-sitter (грамматика `tree-sitter-swift`).
///
/// Особенности грамматики 0.7:
/// - `class_declaration` — общий узел для class/struct/enum/extension/actor,
///   конкретный вид лежит в поле `declaration_kind`;
/// - у `call_expression` нет полей, вызываемое — первый именованный потомок;
/// - параметры функции — отдельные узлы `parameter`, без общей обёртки.
pub struct SwiftParser;

impl SwiftParser {
    pub fn new() -> Self {
        SwiftParser
    }
}

impl LanguageParser for SwiftParser {
    fn language_name(&self) -> &str {
        "swift"
    }

    fn file_extensions(&self) -> &[&str] {
        &["swift"]
    }

    fn parse(&self, source: &str, _file_path: &str) -> Result<ParseResult> {
        parse_swift(source)
    }
}

/// Получить текст узла AST из байтового среза
fn node_text<'a>(node: tree_sitter::Node<'a>, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

/// Извлечь документирующий комментарий (`///` либо `/** ... */`) перед объявлением
fn extract_doc(node: tree_sitter::Node, source: &[u8]) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = node.prev_sibling();
    while let Some(n) = cur {
        match n.kind() {
            "comment" => {
                let text = node_text(n, source);
                if !text.starts_with("///") {
                    break;
                }
                lines.push(text.to_string());
            }
            "multiline_comment" => {
                let text = node_text(n, source);
                if !text.starts_with("/**") {
                    break;
                }
                lines.push(text.to_string());
            }
            _ => break,
        }
        cur = n.prev_sibling();
    }
    if lines.is_empty() {
        None
    } else {
        lines.reverse();
        Some(lines.join("\n"))
    }
}

/// Собрать список параметров функции — узлы `parameter` идут прямыми потомками
fn collect_parameters(node: tree_sitter::Node, source: &[u8]) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "parameter" {
            parts.push(node_text(child, source).to_string());
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(format!("({})", parts.join(", ")))
    }
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
/// - `class_name` — имя типа-контейнера (если функция является методом)
/// - `current_func` — имя ближайшей функции-контейнера (caller у вызовов)
/// - `top_level` — находимся ли на верхнем уровне файла (для переменных)
fn visit_node(
    node: tree_sitter::Node,
    ctx: &mut VisitContext,
    class_name: Option<&str>,
    current_func: Option<&str>,
    top_level: bool,
    depth: usize,
) {
    // Предел глубины обхода — защита от переполнения стека (P-2)
    if depth > MAX_VISIT_DEPTH {
        return;
    }

    match node.kind() {
        "function_declaration"
        | "protocol_function_declaration"
        | "init_declaration"
        | "deinit_declaration" => {
            visit_function(node, ctx, class_name);
        }
        "class_declaration" | "protocol_declaration" => {
            visit_class(node, ctx);
        }
        "import_declaration" => {
            visit_import(node, ctx);
        }
        "property_declaration" => {
            if top_level {
                visit_property(node, ctx);
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                visit_node(child, ctx, class_name, current_func, false, depth + 1);
            }
        }
        "call_expression" => {
            visit_call(node, ctx, current_func);
            // Аргументы и цепочки вызовов могут содержать вложенные вызовы
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                visit_node(child, ctx, class_name, current_func, false, depth + 1);
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                visit_node(child, ctx, class_name, current_func, top_level, depth + 1);
            }
        }
    }
}

/// Обработать function_declaration / init_declaration / deinit_declaration
fn visit_function(node: tree_sitter::Node, ctx: &mut VisitContext, class_name: Option<&str>) {
    let source = ctx.source;

    // У deinit имени нет — используем само ключевое слово
    let name = node
        .child_by_field_name("name")
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_else(|| {
            if node.kind() == "deinit_declaration" {
                "deinit".to_string()
            } else {
                String::new()
            }
        });

    if name.is_empty() {
        return;
    }

    let qualified_name = class_name.map(|cn| format!("{}.{}", cn, name));

    let line_start = node.start_position().row + 1;
    let line_end = node.end_position().row + 1;

    let args = collect_parameters(node, source);
    let return_type = node
        .child_by_field_name("return_type")
        .map(|n| node_text(n, source).to_string());

    let body_node = node.child_by_field_name("body");
    let docstring = extract_doc(node, source);
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
        is_async: false, // `async` в Swift — анонимный токен сигнатуры, не выделяем
        node_hash,
        ..Default::default()
    });

    if let Some(body_node) = body_node {
        let mut cursor = body_node.walk();
        for child in body_node.children(&mut cursor) {
            visit_node(child, ctx, class_name, Some(&name), false, 1);
        }
    }
}

/// Обработать class_declaration (class/struct/enum/extension/actor) и protocol_declaration
fn visit_class(node: tree_sitter::Node, ctx: &mut VisitContext) {
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

    // Базовые типы и протоколы — узлы inheritance_specifier
    let mut base_parts: Vec<String> = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "inheritance_specifier" {
            base_parts.push(node_text(child, source).to_string());
        }
    }
    let bases = if base_parts.is_empty() {
        None
    } else {
        Some(base_parts.join(", "))
    };

    let docstring = extract_doc(node, source);
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

    if let Some(body_node) = node.child_by_field_name("body") {
        let mut cursor = body_node.walk();
        for child in body_node.children(&mut cursor) {
            visit_node(child, ctx, Some(&name), None, false, 1);
        }
    }
}

/// Обработать import_declaration (`import Foundation`)
fn visit_import(node: tree_sitter::Node, ctx: &mut VisitContext) {
    let source = ctx.source;
    let line = node.start_position().row + 1;

    let module = node
        .named_child(node.named_child_count().saturating_sub(1))
        .map(|n| node_text(n, source).to_string());

    ctx.imports.push(ParsedImport {
        module,
        name: None,
        alias: None,
        line,
        kind: "import".to_string(),
    });
}

/// Обработать call_expression — вызываемое лежит в первом именованном потомке
fn visit_call(node: tree_sitter::Node, ctx: &mut VisitContext, current_func: Option<&str>) {
    let source = ctx.source;
    let line = node.start_position().row + 1;

    let target = match node.named_child(0) {
        Some(n) => n,
        None => return,
    };

    // `obj.method()` → navigation_expression, берём только имя после точки
    let callee = if target.kind() == "navigation_expression" {
        target
            .child_by_field_name("suffix")
            .and_then(|s| s.child_by_field_name("suffix"))
            .map(|n| node_text(n, source).to_string())
            .unwrap_or_default()
    } else {
        node_text(target, source).to_string()
    };

    if callee.is_empty() || callee.contains('\n') {
        return;
    }

    let caller = current_func.unwrap_or("<module>").to_string();
    ctx.calls.push(ParsedCall { caller, callee, line });
}

/// Обработать property_declaration на верхнем уровне файла (`let x = ...`)
fn visit_property(node: tree_sitter::Node, ctx: &mut VisitContext) {
    let source = ctx.source;
    let line = node.start_position().row + 1;

    let name = match node.child_by_field_name("name") {
        Some(n) => node_text(n, source).to_string(),
        None => return,
    };
    if name.is_empty() {
        return;
    }

    let value = node.child_by_field_name("value").map(|n| {
        let text = node_text(n, source);
        if text.chars().count() > 200 {
            text.chars().take(200).collect::<String>()
        } else {
            text.to_string()
        }
    });

    ctx.variables.push(ParsedVariable { name, value, line });
}

/// Главная функция парсинга Swift-файла.
///
/// Окончания строк приводим к `\n`: грамматика 0.7 не разбирает многострочный
/// литерал `"""` с переносом через `\`, если строки заканчиваются на CRLF —
/// ошибка «размазывается» по восстановлению и теряется всё последующее
/// содержимое файла. На выгрузке swift-nio под Windows (git переводит LF в
/// CRLF) так терялось около 12% символов. Номера строк от замены не меняются.
fn parse_swift(source: &str) -> Result<ParseResult> {
    let normalized;
    let source = if source.contains("\r\n") {
        normalized = source.replace("\r\n", "\n");
        normalized.as_str()
    } else {
        source
    };

    let mut ts_parser = tree_sitter::Parser::new();
    ts_parser
        .set_language(&tree_sitter_swift::LANGUAGE.into())
        .map_err(|e| anyhow!("Ошибка установки языка tree-sitter-swift: {}", e))?;

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
        visit_node(child, &mut ctx, None, None, true, 0);
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
    fn test_parse_class_with_methods() {
        let parser = SwiftParser::new();
        let source = r#"
import Foundation

/// Сервис заказов.
class OrderService: BaseService, Reloadable {
    init(repo: Repo) {
        self.repo = repo
    }

    func calculate(id: Int, retries: Int = 3) -> Int {
        let order = repo.find(id)
        return sum(order)
    }
}
"#;
        let result = parser.parse(source, "test.swift").unwrap();
        assert_eq!(result.classes.len(), 1);
        assert_eq!(result.classes[0].name, "OrderService");
        assert!(result.classes[0].docstring.is_some());
        assert!(result.classes[0]
            .bases
            .as_deref()
            .unwrap()
            .contains("Reloadable"));

        let calc = result
            .functions
            .iter()
            .find(|f| f.name == "calculate")
            .expect("метод calculate должен быть найден");
        assert_eq!(calc.qualified_name.as_deref(), Some("OrderService.calculate"));
        assert!(calc.args.as_deref().unwrap().contains("retries"));
        assert_eq!(calc.return_type.as_deref(), Some("Int"));

        assert!(result.functions.iter().any(|f| f.name == "init"));
        assert!(result
            .imports
            .iter()
            .any(|i| i.module.as_deref() == Some("Foundation")));
    }

    #[test]
    fn test_parse_calls() {
        let parser = SwiftParser::new();
        let source = r#"
func run() {
    Logger.log("x")
    process()
}
"#;
        let result = parser.parse(source, "test.swift").unwrap();
        assert!(result
            .calls
            .iter()
            .any(|c| c.callee == "log" && c.caller == "run"));
        assert!(result.calls.iter().any(|c| c.callee == "process"));
    }

    #[test]
    fn test_parse_struct_enum_protocol() {
        let parser = SwiftParser::new();
        let source = r#"
struct Point {
    var x: Int
}

enum Color {
    case red
}

protocol Runnable {
    func run()
}
"#;
        let result = parser.parse(source, "test.swift").unwrap();
        assert!(result.classes.iter().any(|c| c.name == "Point"));
        assert!(result.classes.iter().any(|c| c.name == "Color"));
        assert!(result.classes.iter().any(|c| c.name == "Runnable"));
        assert!(result
            .functions
            .iter()
            .any(|f| f.qualified_name.as_deref() == Some("Runnable.run")));
    }

    #[test]
    fn test_parse_crlf_multiline_string() {
        // Регресс: с окончаниями строк CRLF грамматика теряла всё, что идёт
        // после многострочного литерала с переносом через `\`
        let parser = SwiftParser::new();
        let source = concat!(
            "public func before() -> Int { return 1 }\n",
            "public func makeMsg(_ x: Int) -> String {\n",
            "    let s = \"\"\"\n",
            "        Value (\\(x)) is more than allowed \\\n",
            "        and the text continues here.\n",
            "        \"\"\"\n",
            "    return s\n",
            "}\n",
            "public func after() -> Int { return 2 }\n",
        );
        let crlf = source.replace('\n', "\r\n");

        for (tag, text) in [("LF", source.to_string()), ("CRLF", crlf)] {
            let result = parser.parse(&text, "test.swift").unwrap();
            let names: Vec<&str> = result.functions.iter().map(|f| f.name.as_str()).collect();
            assert!(names.contains(&"before"), "{}: before", tag);
            assert!(names.contains(&"makeMsg"), "{}: makeMsg", tag);
            assert!(names.contains(&"after"), "{}: after — теряется при CRLF", tag);
        }
    }

    #[test]
    fn test_parse_top_level_property() {
        let parser = SwiftParser::new();
        let source = r#"
let maxRetries = 5

func retryCount() -> Int {
    let local = 1
    return maxRetries
}
"#;
        let result = parser.parse(source, "test.swift").unwrap();
        assert!(result.variables.iter().any(|v| v.name == "maxRetries"));
        assert!(!result.variables.iter().any(|v| v.name == "local"));
        assert!(result.functions.iter().any(|f| f.name == "retryCount"));
    }
}
