use anyhow::{anyhow, Result};

use super::types::{
    sha256_hex, ParseResult, ParsedCall, ParsedClass, ParsedFunction, ParsedImport,
    ParsedVariable,
};
use super::LanguageParser;
use super::types::MAX_VISIT_DEPTH;
use super::types::PARSE_TIMEOUT_MS;

/// Парсер Ruby-файлов на основе tree-sitter (грамматика `tree-sitter-ruby`).
///
/// Классами считаем `class` и `module` — оба образуют именованный контейнер.
/// Вызовы без скобок (`puts "x"`) в грамматике 0.23 — это тот же узел `call`,
/// отдельного узла `command` нет.
pub struct RubyParser;

impl RubyParser {
    pub fn new() -> Self {
        RubyParser
    }
}

impl LanguageParser for RubyParser {
    fn language_name(&self) -> &str {
        "ruby"
    }

    fn file_extensions(&self) -> &[&str] {
        &["rb"]
    }

    fn parse(&self, source: &str, _file_path: &str) -> Result<ParseResult> {
        parse_ruby(source)
    }
}

/// Получить текст узла AST из байтового среза
fn node_text<'a>(node: tree_sitter::Node<'a>, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

/// Извлечь комментарий-документацию (`# ...`) перед объявлением.
/// В Ruby это подряд идущие строки-комментарии непосредственно над `def`/`class`.
fn extract_doc(node: tree_sitter::Node, source: &[u8]) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = node.prev_sibling();
    while let Some(n) = cur {
        if n.kind() != "comment" {
            break;
        }
        lines.push(node_text(n, source).to_string());
        cur = n.prev_sibling();
    }
    if lines.is_empty() {
        None
    } else {
        lines.reverse();
        Some(lines.join("\n"))
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
/// - `class_name` — имя класса/модуля-контейнера
/// - `current_func` — имя ближайшего метода-контейнера (caller у вызовов)
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
        "method" | "singleton_method" => {
            visit_method(node, ctx, class_name);
        }
        "class" | "module" | "singleton_class" => {
            visit_class(node, ctx);
        }
        "call" => {
            visit_call(node, ctx, current_func);
            // Аргументы и блоки могут содержать вложенные вызовы
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                visit_node(child, ctx, class_name, current_func, false, depth + 1);
            }
        }
        "assignment" => {
            if top_level {
                visit_assignment(node, ctx);
            }
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

/// Обработать method / singleton_method
fn visit_method(node: tree_sitter::Node, ctx: &mut VisitContext, class_name: Option<&str>) {
    let source = ctx.source;

    let name = node
        .child_by_field_name("name")
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();

    if name.is_empty() {
        return;
    }

    // Идиоматика Ruby: `Класс#метод` для обычных, `Класс.метод` для singleton
    let separator = if node.kind() == "singleton_method" { "." } else { "#" };
    let qualified_name = class_name.map(|cn| format!("{}{}{}", cn, separator, name));

    let line_start = node.start_position().row + 1;
    let line_end = node.end_position().row + 1;

    let args = node
        .child_by_field_name("parameters")
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
        return_type: None, // в Ruby нет объявления типа результата
        docstring,
        body,
        is_async: false,
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

/// Обработать class / module / singleton_class
fn visit_class(node: tree_sitter::Node, ctx: &mut VisitContext) {
    let source = ctx.source;

    // У singleton_class (`class << self`) имени нет — берём значение после `<<`
    let name = node
        .child_by_field_name("name")
        .or_else(|| node.child_by_field_name("value"))
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();

    if name.is_empty() {
        return;
    }

    let line_start = node.start_position().row + 1;
    let line_end = node.end_position().row + 1;

    let bases = node
        .child_by_field_name("superclass")
        .map(|n| node_text(n, source).to_string());

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

/// Обработать call: обычный вызов либо `require`/`require_relative`/`load` как импорт.
///
/// Ограничение грамматики: вызов без скобок, без получателя и без аргументов
/// (`prepare`) разбирается как `identifier`, а не `call` — он синтаксически
/// неотличим от чтения локальной переменной, поэтому в граф вызовов не идёт.
fn visit_call(node: tree_sitter::Node, ctx: &mut VisitContext, current_func: Option<&str>) {
    let source = ctx.source;
    let line = node.start_position().row + 1;

    let callee = match node.child_by_field_name("method") {
        Some(n) => node_text(n, source).to_string(),
        None => return,
    };

    if callee.is_empty() {
        return;
    }

    // require / require_relative / load — это подключение файла, а не обычный вызов
    if matches!(callee.as_str(), "require" | "require_relative" | "load") {
        let module = node
            .child_by_field_name("arguments")
            .and_then(|a| a.named_child(0))
            .map(|n| {
                node_text(n, source)
                    .trim_matches(|c| c == '"' || c == '\'')
                    .to_string()
            });
        ctx.imports.push(ParsedImport {
            module,
            name: None,
            alias: None,
            line,
            kind: callee,
        });
        return;
    }

    let caller = current_func.unwrap_or("<module>").to_string();
    ctx.calls.push(ParsedCall { caller, callee, line });
}

/// Обработать присваивание на верхнем уровне файла (`X = ...`)
fn visit_assignment(node: tree_sitter::Node, ctx: &mut VisitContext) {
    let source = ctx.source;
    let line = node.start_position().row + 1;

    let name = match node.child_by_field_name("left") {
        Some(n) => node_text(n, source).to_string(),
        None => return,
    };
    if name.is_empty() {
        return;
    }

    let value = node.child_by_field_name("right").map(|n| {
        let text = node_text(n, source);
        if text.chars().count() > 200 {
            text.chars().take(200).collect::<String>()
        } else {
            text.to_string()
        }
    });

    ctx.variables.push(ParsedVariable { name, value, line });
}

/// Главная функция парсинга Ruby-файла
fn parse_ruby(source: &str) -> Result<ParseResult> {
    let mut ts_parser = tree_sitter::Parser::new();
    ts_parser
        .set_language(&tree_sitter_ruby::LANGUAGE.into())
        .map_err(|e| anyhow!("Ошибка установки языка tree-sitter-ruby: {}", e))?;

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
        let parser = RubyParser::new();
        let source = r#"
# Сервис заказов.
class OrderService < BaseService
  def initialize(repo)
    @repo = repo
  end

  def self.build
    new(Repo.new)
  end
end
"#;
        let result = parser.parse(source, "test.rb").unwrap();
        assert_eq!(result.classes.len(), 1);
        assert_eq!(result.classes[0].name, "OrderService");
        assert!(result.classes[0].docstring.is_some());
        assert!(result.classes[0]
            .bases
            .as_deref()
            .unwrap()
            .contains("BaseService"));

        assert!(result
            .functions
            .iter()
            .any(|f| f.qualified_name.as_deref() == Some("OrderService#initialize")));
        assert!(result
            .functions
            .iter()
            .any(|f| f.qualified_name.as_deref() == Some("OrderService.build")));
    }

    #[test]
    fn test_parse_module_and_calls() {
        let parser = RubyParser::new();
        let source = r#"
module Utils
  def self.run
    prepare()
    bare_call
    puts "done"
  end
end

Utils.run
"#;
        let result = parser.parse(source, "test.rb").unwrap();
        assert!(result.classes.iter().any(|c| c.name == "Utils"));
        assert!(result
            .calls
            .iter()
            .any(|c| c.callee == "prepare" && c.caller == "run"));
        assert!(result.calls.iter().any(|c| c.callee == "puts"));
        // Вызов без скобок, без получателя и без аргументов грамматика Ruby
        // разбирает как `identifier` — он неотличим от чтения локальной
        // переменной, поэтому в граф вызовов не попадает (см. visit_call)
        assert!(!result.calls.iter().any(|c| c.callee == "bare_call"));
        assert!(result
            .calls
            .iter()
            .any(|c| c.callee == "run" && c.caller == "<module>"));
    }

    #[test]
    fn test_parse_requires() {
        let parser = RubyParser::new();
        let source = r#"
require 'json'
require_relative "../lib/helper"
"#;
        let result = parser.parse(source, "test.rb").unwrap();
        assert!(result
            .imports
            .iter()
            .any(|i| i.kind == "require" && i.module.as_deref() == Some("json")));
        assert!(result
            .imports
            .iter()
            .any(|i| i.kind == "require_relative"
                && i.module.as_deref() == Some("../lib/helper")));
        // require не должен попасть в обычные вызовы
        assert!(!result.calls.iter().any(|c| c.callee == "require"));
    }

    #[test]
    fn test_parse_top_level_assignment() {
        let parser = RubyParser::new();
        let source = r#"
MAX_RETRIES = 5

def retry_count
  local = 1
  MAX_RETRIES
end
"#;
        let result = parser.parse(source, "test.rb").unwrap();
        assert!(result.variables.iter().any(|v| v.name == "MAX_RETRIES"));
        // локальная переменная внутри метода не должна попасть в module-level
        assert!(!result.variables.iter().any(|v| v.name == "local"));
        assert!(result.functions.iter().any(|f| f.name == "retry_count"));
    }
}
