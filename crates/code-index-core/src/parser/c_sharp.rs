use anyhow::{anyhow, Result};

use super::types::{
    sha256_hex, ParseResult, ParsedCall, ParsedClass, ParsedFunction, ParsedImport,
    ParsedVariable,
};
use super::LanguageParser;
use super::callee::callee_name;
use super::types::MAX_VISIT_DEPTH;
use super::types::PARSE_TIMEOUT_MS;

/// Парсер C#-файлов на основе tree-sitter (грамматика `tree-sitter-c-sharp`).
///
/// Классами считаем class/interface/struct/enum/record — всё, что образует
/// именованный контейнер с телом. Пространства имён (`namespace`) отдельными
/// сущностями не записываем, только рекурсивно спускаемся внутрь.
pub struct CSharpParser;

impl CSharpParser {
    pub fn new() -> Self {
        CSharpParser
    }
}

impl LanguageParser for CSharpParser {
    fn language_name(&self) -> &str {
        "csharp"
    }

    fn file_extensions(&self) -> &[&str] {
        &["cs"]
    }

    fn parse(&self, source: &str, _file_path: &str) -> Result<ParseResult> {
        parse_csharp(source)
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

/// Извлечь XML-документацию (`/// ...`), стоящую непосредственно перед объявлением.
/// Комментарии идут отдельными строками-узлами — собираем подряд идущие вверх.
fn extract_doc(node: tree_sitter::Node, source: &[u8]) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = node.prev_sibling();
    while let Some(n) = cur {
        if n.kind() != "comment" {
            break;
        }
        let text = node_text(n, source);
        if !text.starts_with("///") && !text.starts_with("/**") {
            break;
        }
        lines.push(text.to_string());
        cur = n.prev_sibling();
    }
    if lines.is_empty() {
        None
    } else {
        lines.reverse();
        Some(lines.join("\n"))
    }
}

/// Есть ли среди модификаторов объявления `async`
fn has_async_modifier(node: tree_sitter::Node, source: &[u8]) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "modifier" && node_text(child, source) == "async" {
            return true;
        }
    }
    false
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
fn visit_node(
    node: tree_sitter::Node,
    ctx: &mut VisitContext,
    class_name: Option<&str>,
    current_func: Option<&str>,
    depth: usize,
) {
    // Предел глубины обхода — защита от переполнения стека (P-2)
    if depth > MAX_VISIT_DEPTH {
        return;
    }

    match node.kind() {
        "method_declaration"
        | "constructor_declaration"
        | "destructor_declaration"
        | "local_function_statement" => {
            visit_function(node, ctx, class_name);
        }
        "class_declaration"
        | "interface_declaration"
        | "struct_declaration"
        | "enum_declaration"
        | "record_declaration" => {
            visit_class(node, ctx);
        }
        "using_directive" => {
            visit_using(node, ctx);
        }
        "field_declaration" => {
            visit_field(node, ctx);
        }
        "invocation_expression" => {
            visit_call(node, ctx, current_func);
            // Аргументы и цепочки вызовов могут содержать вложенные вызовы
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                visit_node(child, ctx, class_name, current_func, depth + 1);
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                visit_node(child, ctx, class_name, current_func, depth + 1);
            }
        }
    }
}

/// Обработать method_declaration / constructor_declaration / local_function_statement
fn visit_function(node: tree_sitter::Node, ctx: &mut VisitContext, class_name: Option<&str>) {
    let source = ctx.source;

    let name = node
        .child_by_field_name("name")
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();

    if name.is_empty() {
        return;
    }

    let qualified_name = class_name.map(|cn| format!("{}.{}", cn, name));

    let line_start = node.start_position().row + 1;
    let line_end = node.end_position().row + 1;

    let args = node
        .child_by_field_name("parameters")
        .map(|n| node_text(n, source).to_string());

    // Тип результата: у методов — поле "returns", у локальных функций — "type"
    let return_type = node
        .child_by_field_name("returns")
        .or_else(|| node.child_by_field_name("type"))
        .map(|n| node_text(n, source).to_string());

    let body_node = node.child_by_field_name("body");
    let docstring = extract_doc(node, source);
    let is_async = has_async_modifier(node, source);
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

    if let Some(body_node) = body_node {
        let mut cursor = body_node.walk();
        for child in body_node.children(&mut cursor) {
            visit_node(child, ctx, class_name, Some(&name), 1);
        }
    }
}

/// Обработать class/interface/struct/enum/record
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

    // Базовые типы и интерфейсы — единый base_list (`: Base, IFoo`)
    let bases = find_child_by_kind(node, "base_list").map(|n| node_text(n, source).to_string());

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
            visit_node(child, ctx, Some(&name), None, 1);
        }
    }
}

/// Обработать using_directive (`using System.Text;`, `using X = Foo.Bar;`)
fn visit_using(node: tree_sitter::Node, ctx: &mut VisitContext) {
    let source = ctx.source;
    let line = node.start_position().row + 1;

    // Псевдоним задаётся полем "name" (только у формы `using X = ...`)
    let alias_node = node.child_by_field_name("name");
    let alias = alias_node.map(|n| node_text(n, source).to_string());

    // Сам импортируемый путь — последний именованный дочерний узел (не псевдоним)
    let mut module: Option<String> = None;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.is_named() && Some(child.id()) != alias_node.map(|n| n.id()) {
            module = Some(node_text(child, source).to_string());
        }
    }

    ctx.imports.push(ParsedImport {
        module,
        name: None,
        alias,
        line,
        kind: "using".to_string(),
    });
}

/// Обработать field_declaration — поля класса как «переменные»
fn visit_field(node: tree_sitter::Node, ctx: &mut VisitContext) {
    let source = ctx.source;
    let line = node.start_position().row + 1;

    let decl = match find_child_by_kind(node, "variable_declaration") {
        Some(d) => d,
        None => return,
    };

    let mut cursor = decl.walk();
    for child in decl.children(&mut cursor) {
        if child.kind() != "variable_declarator" {
            continue;
        }
        let name_node = match child.child_by_field_name("name") {
            Some(n) => n,
            None => continue,
        };
        let name = node_text(name_node, source).to_string();

        // Значение инициализации — последний именованный узел, если это не само имя
        let value = child
            .named_child(child.named_child_count().saturating_sub(1))
            .filter(|n| n.id() != name_node.id())
            .map(|n| truncate(node_text(n, source)));

        ctx.variables.push(ParsedVariable { name, value, line });
    }
}

/// Обрезать значение до 200 символов (значения бывают многострочными)
fn truncate(text: &str) -> String {
    if text.chars().count() > 200 {
        text.chars().take(200).collect()
    } else {
        text.to_string()
    }
}

/// Обработать invocation_expression
fn visit_call(node: tree_sitter::Node, ctx: &mut VisitContext, current_func: Option<&str>) {
    let source = ctx.source;
    let line = node.start_position().row + 1;

    let func_node = match node.child_by_field_name("function") {
        Some(n) => n,
        None => return,
    };

    // Для `obj.Method()` берём только имя метода, для `Method()` — сам идентификатор
    let callee = match callee_name(func_node, source) {
        Some(name) => name,
        None => return,
    };

    let caller = current_func.unwrap_or("<module>").to_string();
    ctx.calls.push(ParsedCall { caller, callee, line });
}

/// Главная функция парсинга C#-файла
fn parse_csharp(source: &str) -> Result<ParseResult> {
    let mut ts_parser = tree_sitter::Parser::new();
    ts_parser
        .set_language(&tree_sitter_c_sharp::LANGUAGE.into())
        .map_err(|e| anyhow!("Ошибка установки языка tree-sitter-c-sharp: {}", e))?;

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
        visit_node(child, &mut ctx, None, None, 0);
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
        let parser = CSharpParser::new();
        let source = r#"
namespace App.Core
{
    /// <summary>Сервис заказов.</summary>
    public class OrderService : BaseService, IOrderService
    {
        public int Total { get; set; }

        public OrderService(IRepo repo) { _repo = repo; }

        public async Task<int> CalculateAsync(int id)
        {
            var order = _repo.Find(id);
            return Sum(order);
        }
    }
}
"#;
        let result = parser.parse(source, "test.cs").unwrap();
        assert_eq!(result.classes.len(), 1);
        assert_eq!(result.classes[0].name, "OrderService");
        assert!(result.classes[0].docstring.is_some());
        assert!(result.classes[0].bases.as_deref().unwrap().contains("IOrderService"));

        assert!(result
            .functions
            .iter()
            .any(|f| f.name == "CalculateAsync" && f.is_async));
        assert!(result
            .functions
            .iter()
            .any(|f| f.qualified_name.as_deref() == Some("OrderService.OrderService")));
    }

    #[test]
    fn test_parse_calls() {
        let parser = CSharpParser::new();
        let source = r#"
class C {
    void Run() {
        Helper.Log("x");
        Process();
    }
}
"#;
        let result = parser.parse(source, "test.cs").unwrap();
        assert!(result.calls.iter().any(|c| c.callee == "Log" && c.caller == "Run"));
        assert!(result.calls.iter().any(|c| c.callee == "Process"));
    }

    #[test]
    fn test_parse_usings() {
        let parser = CSharpParser::new();
        let source = r#"
using System.Text;
using Alias = Foo.Bar;
"#;
        let result = parser.parse(source, "test.cs").unwrap();
        assert!(result
            .imports
            .iter()
            .any(|i| i.module.as_deref() == Some("System.Text")));
        assert!(result
            .imports
            .iter()
            .any(|i| i.alias.as_deref() == Some("Alias")));
    }

    #[test]
    fn test_parse_interface_struct_enum() {
        let parser = CSharpParser::new();
        let source = r#"
interface IFoo { void Do(); }
struct Point { public int X; }
enum Color { Red, Green }
"#;
        let result = parser.parse(source, "test.cs").unwrap();
        assert!(result.classes.iter().any(|c| c.name == "IFoo"));
        assert!(result.classes.iter().any(|c| c.name == "Point"));
        assert!(result.classes.iter().any(|c| c.name == "Color"));
        assert!(result.variables.iter().any(|v| v.name == "X"));
    }
}
