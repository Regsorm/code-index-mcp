// HTML-парсер на tree-sitter-html.
//
// HTML — менее «строгий по синтаксису» язык, чем Python/Rust. У него нет
// естественного понятия «функция»/«класс», поэтому маппинг — конвенциональный.
// Полная таблица в README.md и в `~/.claude/plans/html-parser-semantics.md`.
//
// Кратко:
//   * `<element id="X">` или `<form id|name="X">`  →  ParsedClass(name="X" / "form_X", body=outerHTML)
//   * `<form>` без id/name                          →  ParsedClass(name="form_<line>", body=outerHTML)
//   * `<input/select/textarea name="Y">`            →  ParsedVariable(name="Y", line)
//   * `<a href>` / `<link href>` / `<script src>` / `<img src>` → ParsedImport(module=URL, kind=link|stylesheet|script|image)
//   * `<script>...inline...</script>`               →  ParsedFunction(name="inline_script_<line>", body=содержимое)
//   * `<style>...inline...</style>`                 →  ParsedFunction(name="inline_style_<line>", body=содержимое)
//   * Атрибут `class="foo bar"`                     →  ParsedVariable(name="class:foo"), ParsedVariable(name="class:bar")
//
// Двойная индексация (text_files + AST) реализуется на уровне indexer — этот
// модуль возвращает только структурный ParseResult; raw-content для FTS
// сохраняет вызывающая сторона из исходных байт.

use anyhow::{anyhow, Result};

use super::types::{
    hash_ast, sha256_hex, ParseResult, ParsedClass, ParsedFunction, ParsedImport, ParsedVariable,
};
use super::LanguageParser;

pub struct HtmlParser;

impl HtmlParser {
    pub fn new() -> Self {
        HtmlParser
    }
}

impl LanguageParser for HtmlParser {
    fn language_name(&self) -> &str {
        "html"
    }

    fn file_extensions(&self) -> &[&str] {
        &["html", "htm"]
    }

    fn parse(&self, source: &str, _file_path: &str) -> Result<ParseResult> {
        parse_html(source)
    }
}

fn parse_html(source: &str) -> Result<ParseResult> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_html::LANGUAGE.into())
        .map_err(|e| anyhow!("set HTML language: {}", e))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| anyhow!("tree-sitter-html: пустое дерево"))?;

    let mut ctx = VisitContext::new(source.as_bytes());
    let root = tree.root_node();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        visit_node(child, &mut ctx);
    }

    let lines_total = source.lines().count();
    let ast_hash = hash_ast(root);
    Ok(ParseResult {
        functions: ctx.functions,
        classes: ctx.classes,
        imports: ctx.imports,
        calls: Vec::new(), // у HTML нет «вызовов»
        variables: ctx.variables,
        lines_total,
        ast_hash,
    })
}

struct VisitContext<'a> {
    source: &'a [u8],
    functions: Vec<ParsedFunction>,
    classes: Vec<ParsedClass>,
    imports: Vec<ParsedImport>,
    variables: Vec<ParsedVariable>,
}

impl<'a> VisitContext<'a> {
    fn new(source: &'a [u8]) -> Self {
        VisitContext {
            source,
            functions: Vec::new(),
            classes: Vec::new(),
            imports: Vec::new(),
            variables: Vec::new(),
        }
    }
}

fn node_text<'a>(node: tree_sitter::Node<'a>, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

/// Извлечь имя тега из start_tag/self_closing_tag.
fn tag_name<'a>(start_tag: tree_sitter::Node<'a>, source: &'a [u8]) -> Option<String> {
    let mut cursor = start_tag.walk();
    for child in start_tag.children(&mut cursor) {
        if child.kind() == "tag_name" {
            return Some(node_text(child, source).to_lowercase());
        }
    }
    None
}

/// Извлечь все пары (attr_name → attr_value) из start_tag/self_closing_tag.
fn collect_attributes(start_tag: tree_sitter::Node, source: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut cursor = start_tag.walk();
    for child in start_tag.children(&mut cursor) {
        if child.kind() != "attribute" {
            continue;
        }
        let mut name: Option<String> = None;
        let mut value: Option<String> = None;
        let mut acur = child.walk();
        for sub in child.children(&mut acur) {
            match sub.kind() {
                "attribute_name" => {
                    name = Some(node_text(sub, source).to_lowercase());
                }
                "quoted_attribute_value" => {
                    // quoted_attribute_value содержит attribute_value (или сразу строку)
                    let mut qcur = sub.walk();
                    let mut found = false;
                    for q in sub.children(&mut qcur) {
                        if q.kind() == "attribute_value" {
                            value = Some(node_text(q, source).to_string());
                            found = true;
                            break;
                        }
                    }
                    if !found {
                        // Fallback: берём текст самого quoted-узла без кавычек на краях.
                        let raw = node_text(sub, source);
                        let trimmed = raw
                            .trim_start_matches('"')
                            .trim_start_matches('\'')
                            .trim_end_matches('"')
                            .trim_end_matches('\'');
                        value = Some(trimmed.to_string());
                    }
                }
                "attribute_value" => {
                    // unquoted form
                    value = Some(node_text(sub, source).to_string());
                }
                _ => {}
            }
        }
        if let Some(n) = name {
            out.push((n, value.unwrap_or_default()));
        }
    }
    out
}

/// Получить start_tag (или self_closing_tag) ребёнка элемента.
fn find_open_tag<'a>(element: tree_sitter::Node<'a>) -> Option<tree_sitter::Node<'a>> {
    let mut cursor = element.walk();
    for child in element.children(&mut cursor) {
        match child.kind() {
            "start_tag" | "self_closing_tag" => return Some(child),
            _ => {}
        }
    }
    None
}

/// Вернуть содержимое <script>/<style>: всё что между start_tag и end_tag.
fn raw_inner_text<'a>(element: tree_sitter::Node<'a>, source: &'a [u8]) -> String {
    let mut out = String::new();
    let mut cursor = element.walk();
    for child in element.children(&mut cursor) {
        match child.kind() {
            "raw_text" | "text" => {
                out.push_str(node_text(child, source));
            }
            _ => {}
        }
    }
    out
}

/// Главный обход.
fn visit_node(node: tree_sitter::Node, ctx: &mut VisitContext) {
    match node.kind() {
        "script_element" => visit_script_element(node, ctx),
        "style_element" => visit_style_element(node, ctx),
        "element" => visit_element(node, ctx),
        // fragment / другие узлы — рекурсивно обходим детей
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                visit_node(child, ctx);
            }
        }
    }
}

fn visit_element(node: tree_sitter::Node, ctx: &mut VisitContext) {
    let line_start = node.start_position().row + 1;
    let line_end = node.end_position().row + 1;

    let open = find_open_tag(node);
    if let Some(open_tag) = open {
        let tname = tag_name(open_tag, ctx.source).unwrap_or_default();
        let attrs = collect_attributes(open_tag, ctx.source);

        // attr_get: достать значение по имени атрибута
        let attr = |name: &str| -> Option<String> {
            attrs
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        };

        let id_attr = attr("id");
        let name_attr = attr("name");
        let class_attr = attr("class");

        // ── classes-таблица ─────────────────────────────────────────────
        // <form ...> — отдельная семантика: имя формы важно, fallback на line
        if tname == "form" {
            let form_name = id_attr
                .as_deref()
                .or(name_attr.as_deref())
                .map(|s| format!("form_{}", s))
                .unwrap_or_else(|| format!("form_{}", line_start));
            let body = node_text(node, ctx.source).to_string();
            let node_hash = sha256_hex(&body);
            ctx.classes.push(ParsedClass {
                name: form_name,
                line_start,
                line_end,
                bases: Some("form".to_string()),
                docstring: None,
                body,
                node_hash,
            });
        } else if let Some(id) = id_attr.as_deref() {
            // Любой не-form элемент с id — как class
            let body = node_text(node, ctx.source).to_string();
            let node_hash = sha256_hex(&body);
            ctx.classes.push(ParsedClass {
                name: id.to_string(),
                line_start,
                line_end,
                bases: Some(tname.clone()),
                docstring: None,
                body,
                node_hash,
            });
        }

        // ── variables-таблица ──────────────────────────────────────────
        // input/select/textarea с name → переменная
        if matches!(tname.as_str(), "input" | "select" | "textarea")
            && name_attr.is_some()
        {
            ctx.variables.push(ParsedVariable {
                name: name_attr.clone().unwrap(),
                value: attr("value").or_else(|| attr("type")),
                line: line_start,
            });
        }
        // class="foo bar baz" — каждое имя как class:foo
        if let Some(cls) = class_attr.as_deref() {
            for css_class in cls.split_whitespace() {
                ctx.variables.push(ParsedVariable {
                    name: format!("class:{}", css_class),
                    value: Some(tname.clone()),
                    line: line_start,
                });
            }
        }

        // ── imports-таблица ────────────────────────────────────────────
        match tname.as_str() {
            "a" => {
                if let Some(href) = attr("href") {
                    ctx.imports.push(ParsedImport {
                        module: Some(href),
                        name: None,
                        alias: None,
                        line: line_start,
                        kind: "link".to_string(),
                    });
                }
            }
            "link" => {
                if let Some(href) = attr("href") {
                    let rel = attr("rel").unwrap_or_else(|| "stylesheet".to_string());
                    ctx.imports.push(ParsedImport {
                        module: Some(href),
                        name: None,
                        alias: None,
                        line: line_start,
                        kind: rel,
                    });
                }
            }
            "img" | "iframe" | "video" | "audio" | "source" | "embed" => {
                if let Some(src) = attr("src") {
                    ctx.imports.push(ParsedImport {
                        module: Some(src),
                        name: None,
                        alias: None,
                        line: line_start,
                        kind: tname.clone(),
                    });
                }
            }
            _ => {}
        }
    }

    // Рекурсия в детей — всегда, чтобы поймать вложенные элементы
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit_node(child, ctx);
    }
}

fn visit_script_element(node: tree_sitter::Node, ctx: &mut VisitContext) {
    let line_start = node.start_position().row + 1;
    let line_end = node.end_position().row + 1;

    let open = find_open_tag(node);
    let attrs = open.map(|t| collect_attributes(t, ctx.source)).unwrap_or_default();
    let attr = |name: &str| -> Option<String> {
        attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    };

    // <script src="..."> — это импорт, не функция (нет inline-кода)
    if let Some(src) = attr("src") {
        ctx.imports.push(ParsedImport {
            module: Some(src),
            name: None,
            alias: None,
            line: line_start,
            kind: "script".to_string(),
        });
        return;
    }

    // Inline-script: содержимое тега → ParsedFunction
    let body = raw_inner_text(node, ctx.source);
    if body.trim().is_empty() {
        return;
    }
    let name = format!("inline_script_{}", line_start);
    let node_hash = sha256_hex(&body);
    ctx.functions.push(ParsedFunction {
        name,
        qualified_name: None,
        line_start,
        line_end,
        args: None,
        return_type: None,
        docstring: None,
        body,
        is_async: false,
        node_hash,
        ..Default::default()
    });
}

fn visit_style_element(node: tree_sitter::Node, ctx: &mut VisitContext) {
    let line_start = node.start_position().row + 1;
    let line_end = node.end_position().row + 1;
    let body = raw_inner_text(node, ctx.source);
    if body.trim().is_empty() {
        return;
    }
    let name = format!("inline_style_{}", line_start);
    let node_hash = sha256_hex(&body);
    ctx.functions.push(ParsedFunction {
        name,
        qualified_name: None,
        line_start,
        line_end,
        args: None,
        return_type: None,
        docstring: None,
        body,
        is_async: false,
        node_hash,
        ..Default::default()
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> ParseResult {
        HtmlParser::new().parse(src, "test.html").unwrap()
    }

    #[test]
    fn extract_element_with_id_as_class() {
        let html = r#"<!doctype html><html><body><div id="cart">items</div></body></html>"#;
        let r = parse(html);
        assert_eq!(r.classes.len(), 1);
        assert_eq!(r.classes[0].name, "cart");
        assert_eq!(r.classes[0].bases.as_deref(), Some("div"));
        assert!(r.classes[0].body.contains("items"));
    }

    #[test]
    fn extract_form_with_id() {
        let html = r#"<form id="login"><input name="user"></form>"#;
        let r = parse(html);
        // У формы id, поэтому в classes по двум правилам? — проверяем что только одна запись
        let form_records: Vec<_> = r.classes.iter().filter(|c| c.name.starts_with("form_")).collect();
        assert_eq!(form_records.len(), 1, "ровно одна form-запись");
        assert_eq!(form_records[0].name, "form_login");
    }

    #[test]
    fn extract_form_with_name_only() {
        let html = r#"<form name="signup"><input name="email"></form>"#;
        let r = parse(html);
        assert!(r.classes.iter().any(|c| c.name == "form_signup"));
    }

    #[test]
    fn extract_form_without_id_or_name_uses_line() {
        let html = r#"<html><body>
<form>
  <input name="x">
</form>
</body></html>"#;
        let r = parse(html);
        let forms: Vec<_> = r.classes.iter().filter(|c| c.name.starts_with("form_")).collect();
        assert_eq!(forms.len(), 1);
        // Форма на строке 2 (1-based)
        assert_eq!(forms[0].name, "form_2");
    }

    #[test]
    fn extract_inputs_as_variables() {
        let html = r#"<form>
  <input name="user" type="text">
  <select name="country"><option>RU</option></select>
  <textarea name="comment"></textarea>
</form>"#;
        let r = parse(html);
        let names: Vec<&str> = r.variables.iter().map(|v| v.name.as_str()).collect();
        assert!(names.contains(&"user"));
        assert!(names.contains(&"country"));
        assert!(names.contains(&"comment"));
    }

    #[test]
    fn extract_links_as_imports() {
        let html = r#"<a href="/foo">x</a>
<link href="/style.css" rel="stylesheet">
<script src="/app.js"></script>
<img src="/logo.png">"#;
        let r = parse(html);
        let kinds: Vec<&str> = r.imports.iter().map(|i| i.kind.as_str()).collect();
        assert!(kinds.contains(&"link"));
        assert!(kinds.contains(&"stylesheet"));
        assert!(kinds.contains(&"script"));
        assert!(kinds.contains(&"img"));
        let modules: Vec<&str> = r
            .imports
            .iter()
            .filter_map(|i| i.module.as_deref())
            .collect();
        assert!(modules.contains(&"/foo"));
        assert!(modules.contains(&"/style.css"));
        assert!(modules.contains(&"/app.js"));
        assert!(modules.contains(&"/logo.png"));
    }

    #[test]
    fn extract_inline_script_as_function() {
        let html = r#"<script>const x = 42; fetch('/api');</script>"#;
        let r = parse(html);
        assert_eq!(r.functions.len(), 1);
        assert!(r.functions[0].name.starts_with("inline_script_"));
        assert!(r.functions[0].body.contains("fetch"));
    }

    #[test]
    fn extract_inline_style_as_function() {
        let html = r#"<style>.foo { color: red; }</style>"#;
        let r = parse(html);
        assert_eq!(r.functions.len(), 1);
        assert!(r.functions[0].name.starts_with("inline_style_"));
        assert!(r.functions[0].body.contains("color: red"));
    }

    #[test]
    fn external_script_is_import_not_function() {
        let html = r#"<script src="/cdn/lib.js"></script>"#;
        let r = parse(html);
        assert_eq!(r.functions.len(), 0, "external <script src> не должен быть функцией");
        assert!(r.imports.iter().any(|i| i.kind == "script"));
    }

    #[test]
    fn extract_classes_attribute_as_multiple_variables() {
        let html = r#"<div class="btn primary large">click</div>"#;
        let r = parse(html);
        let css_vars: Vec<&str> = r
            .variables
            .iter()
            .filter(|v| v.name.starts_with("class:"))
            .map(|v| v.name.as_str())
            .collect();
        assert!(css_vars.contains(&"class:btn"));
        assert!(css_vars.contains(&"class:primary"));
        assert!(css_vars.contains(&"class:large"));
    }

    #[test]
    fn handles_jinja_template_tolerantly() {
        // Шаблонные участки — это text-content для tree-sitter-html, парсер
        // не должен падать, и обычные элементы вокруг должны парситься.
        let html = r#"<html><body>
{% if user %}
  <div id="profile">{{ user.name }}</div>
{% endif %}
</body></html>"#;
        let r = parse(html);
        // Должны найти div#profile несмотря на шаблонные обвязки
        assert!(r.classes.iter().any(|c| c.name == "profile"));
    }

    #[test]
    fn empty_html_does_not_crash() {
        let r = parse("");
        assert!(r.functions.is_empty());
        assert!(r.classes.is_empty());
    }

    #[test]
    fn nested_elements_are_visited() {
        let html = r#"<div id="outer"><div id="inner">x</div></div>"#;
        let r = parse(html);
        let names: Vec<&str> = r.classes.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"outer"));
        assert!(names.contains(&"inner"));
    }
}
