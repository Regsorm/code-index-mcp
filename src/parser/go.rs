use anyhow::{anyhow, Result};
use sha2::{Digest, Sha256};
use hex;

use super::types::{
    ParseResult, ParsedCall, ParsedClass, ParsedFunction, ParsedImport, ParsedVariable,
};
use super::LanguageParser;

/// Парсер Go-файлов на основе tree-sitter
pub struct GoParser;

impl GoParser {
    pub fn new() -> Self {
        GoParser
    }
}

impl LanguageParser for GoParser {
    fn language_name(&self) -> &str {
        "go"
    }

    fn file_extensions(&self) -> &[&str] {
        &["go"]
    }

    fn parse(&self, source: &str, _file_path: &str) -> Result<ParseResult> {
        parse_go(source)
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
fn find_child_by_kind<'a>(
    node: tree_sitter::Node<'a>,
    kind: &str,
) -> Option<tree_sitter::Node<'a>> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == kind {
            return Some(child);
        }
    }
    None
}

/// Контекст обхода AST Go
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

/// Рекурсивный обход AST Go.
/// - `current_func` — имя ближайшей функции-контейнера для вызовов
/// - `top_level` — находимся ли мы на уровне пакета
fn visit_node(
    node: tree_sitter::Node,
    ctx: &mut VisitContext,
    current_func: Option<&str>,
    top_level: bool,
    depth: usize,
) {
    // Ограничение глубины для защиты от переполнения стека
    if depth > 100 {
        return;
    }

    match node.kind() {
        "function_declaration" => {
            visit_function_decl(node, ctx, current_func);
        }
        "method_declaration" => {
            visit_method_decl(node, ctx, current_func);
        }
        "type_declaration" => {
            visit_type_decl(node, ctx);
        }
        "import_declaration" => {
            visit_import_decl(node, ctx);
        }
        "var_declaration" if top_level => {
            visit_var_or_const_decl(node, ctx);
        }
        "const_declaration" if top_level => {
            visit_var_or_const_decl(node, ctx);
        }
        "call_expression" => {
            visit_call_expr(node, ctx, current_func);
            // Рекурсивно обходим аргументы вызова
            if let Some(args) = node.child_by_field_name("arguments") {
                let mut cur = args.walk();
                for child in args.children(&mut cur) {
                    visit_node(child, ctx, current_func, false, depth + 1);
                }
            }
        }
        "short_var_declaration" => {
            // short_var_declaration на уровне пакета не бывает в Go,
            // но на всякий случай: только если top_level — индексируем.
            // По заданию — игнорировать.
        }
        _ => {
            // Для остальных узлов — рекурсивный обход дочерних
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                // На верхнем уровне оставляем top_level=true для прямых детей source_file
                visit_node(child, ctx, current_func, top_level && node.kind() == "source_file", depth + 1);
            }
        }
    }
}

/// Обработать function_declaration → functions
fn visit_function_decl(
    node: tree_sitter::Node,
    ctx: &mut VisitContext,
    _parent_func: Option<&str>,
) {
    let source = ctx.source;

    // Имя функции: поле name (identifier)
    let name = node
        .child_by_field_name("name")
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();

    if name.is_empty() {
        return;
    }

    let line_start = node.start_position().row + 1;
    let line_end = node.end_position().row + 1;

    // Параметры функции: поле parameters
    let args = node
        .child_by_field_name("parameters")
        .map(|n| node_text(n, source).to_string());

    // Тип возвращаемого значения: поле result
    let return_type = node
        .child_by_field_name("result")
        .map(|n| node_text(n, source).to_string());

    let body_text = node_text(node, source).to_string();
    let node_hash = sha256_hex(&body_text);

    let func_name = name.clone();
    ctx.functions.push(ParsedFunction {
        name,
        qualified_name: None, // Обычная функция — нет qualified_name
        line_start,
        line_end,
        args,
        return_type,
        docstring: None,
        body: body_text,
        is_async: false, // В Go нет async keyword
        node_hash,
        ..Default::default()
    });

    // Рекурсивно обходим тело функции для поиска вызовов
    if let Some(body_node) = node.child_by_field_name("body") {
        let mut cursor = body_node.walk();
        for child in body_node.children(&mut cursor) {
            visit_node(child, ctx, Some(&func_name), false, 1);
        }
    }
}

/// Обработать method_declaration → functions с qualified_name ReceiverType.method
fn visit_method_decl(
    node: tree_sitter::Node,
    ctx: &mut VisitContext,
    _parent_func: Option<&str>,
) {
    let source = ctx.source;

    // Имя метода: поле name (field_identifier или identifier)
    let name = node
        .child_by_field_name("name")
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();

    if name.is_empty() {
        return;
    }

    // Тип получателя: поле receiver содержит parameter_list вида (s *Server)
    // Нужно найти тип внутри: убираем *, берём type_identifier
    let receiver_type = extract_receiver_type(node, source);

    let qualified_name = receiver_type.as_deref().map(|r| format!("{}.{}", r, name));

    let line_start = node.start_position().row + 1;
    let line_end = node.end_position().row + 1;

    // Параметры метода: поле parameters
    let args = node
        .child_by_field_name("parameters")
        .map(|n| node_text(n, source).to_string());

    // Тип возвращаемого значения: поле result
    let return_type = node
        .child_by_field_name("result")
        .map(|n| node_text(n, source).to_string());

    let body_text = node_text(node, source).to_string();
    let node_hash = sha256_hex(&body_text);

    let func_name = name.clone();
    ctx.functions.push(ParsedFunction {
        name,
        qualified_name,
        line_start,
        line_end,
        args,
        return_type,
        docstring: None,
        body: body_text,
        is_async: false,
        node_hash,
        ..Default::default()
    });

    // Рекурсивно обходим тело для вызовов
    if let Some(body_node) = node.child_by_field_name("body") {
        let mut cursor = body_node.walk();
        for child in body_node.children(&mut cursor) {
            visit_node(child, ctx, Some(&func_name), false, 1);
        }
    }
}

/// Извлечь имя типа получателя из поля receiver метода.
/// Пример: `(s *Server)` → "Server", `(h Handler)` → "Handler"
fn extract_receiver_type(method_node: tree_sitter::Node, source: &[u8]) -> Option<String> {
    // Поле receiver — это parameter_list: (s *Server) или (s Server)
    let receiver = method_node.child_by_field_name("receiver")?;

    // Внутри parameter_list ищем parameter_declaration
    let param_decl = find_child_by_kind(receiver, "parameter_declaration")?;

    // В parameter_declaration тип может быть:
    // - pointer_type → type_identifier (для *Server)
    // - type_identifier (для Server)
    let type_node = param_decl.child_by_field_name("type")
        .or_else(|| {
            // Если поле "type" не найдено — ищем среди дочерних
            let mut cursor = param_decl.walk();
            let found = param_decl.children(&mut cursor).find(|c| {
                matches!(c.kind(), "type_identifier" | "pointer_type" | "qualified_type")
            });
            found
        })?;

    if type_node.kind() == "pointer_type" {
        // *Server → находим вложенный type_identifier
        find_child_by_kind(type_node, "type_identifier")
            .map(|n| node_text(n, source).to_string())
    } else {
        Some(node_text(type_node, source).to_string())
    }
}

/// Обработать type_declaration → classes (struct или interface)
fn visit_type_decl(node: tree_sitter::Node, ctx: &mut VisitContext) {
    let source = ctx.source;

    // type_declaration может содержать несколько type_spec
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "type_spec" {
            visit_type_spec(child, ctx, source);
        }
    }
}

/// Обработать type_spec внутри type_declaration
fn visit_type_spec(node: tree_sitter::Node, ctx: &mut VisitContext, source: &[u8]) {
    // Имя типа: поле name (type_identifier)
    let name = node
        .child_by_field_name("name")
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();

    if name.is_empty() {
        return;
    }

    // Тип: поле type — проверяем struct_type или interface_type
    let type_node = match node.child_by_field_name("type") {
        Some(n) => n,
        None => return,
    };

    let bases = match type_node.kind() {
        "struct_type" => None,
        "interface_type" => Some("interface".to_string()),
        _ => return, // Алиасы, generics и прочее — пропускаем
    };

    let line_start = node.start_position().row + 1;
    let line_end = node.end_position().row + 1;

    let body_text = node_text(node, source).to_string();
    let node_hash = sha256_hex(&body_text);

    ctx.classes.push(ParsedClass {
        name,
        line_start,
        line_end,
        bases,
        docstring: None,
        body: body_text,
        node_hash,
    });
}

/// Обработать import_declaration → imports
fn visit_import_decl(node: tree_sitter::Node, ctx: &mut VisitContext) {
    let source = ctx.source;
    let line = node.start_position().row + 1;

    // import_declaration содержит либо один import_spec, либо import_spec_list
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "import_spec" => {
                collect_import_spec(child, source, line, ctx);
            }
            "import_spec_list" => {
                let mut list_cursor = child.walk();
                for spec in child.children(&mut list_cursor) {
                    if spec.kind() == "import_spec" {
                        collect_import_spec(spec, source, spec.start_position().row + 1, ctx);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Разобрать import_spec: [alias] "path"
fn collect_import_spec(
    node: tree_sitter::Node,
    source: &[u8],
    line: usize,
    ctx: &mut VisitContext,
) {
    // Путь пакета: поле path (interpreted_string_literal), снимаем кавычки
    let path_raw = node
        .child_by_field_name("path")
        .map(|n| node_text(n, source))
        .unwrap_or("");

    // Убираем обрамляющие кавычки "..."
    let path = path_raw.trim_matches('"').to_string();
    if path.is_empty() {
        return;
    }

    // Псевдоним: поле name (identifier или ".")
    let alias = node
        .child_by_field_name("name")
        .map(|n| node_text(n, source).to_string())
        .filter(|s| !s.is_empty());

    // Модуль — весь путь, name — последний сегмент (базовое имя пакета)
    let (module, pkg_name) = if let Some(pos) = path.rfind('/') {
        (Some(path[..pos].to_string()), path[pos + 1..].to_string())
    } else {
        (None, path.clone())
    };

    ctx.imports.push(ParsedImport {
        module,
        name: Some(pkg_name),
        alias,
        line,
        kind: "import".to_string(),
    });
}

/// Обработать var_declaration или const_declaration на уровне пакета → variables
fn visit_var_or_const_decl(node: tree_sitter::Node, ctx: &mut VisitContext) {
    let source = ctx.source;

    // var_declaration содержит var_spec; const_declaration содержит const_spec
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if matches!(child.kind(), "var_spec" | "const_spec") {
            collect_var_spec(child, source, ctx);
        }
    }
}

/// Собрать переменные из var_spec или const_spec
fn collect_var_spec(node: tree_sitter::Node, source: &[u8], ctx: &mut VisitContext) {
    let line = node.start_position().row + 1;

    // Имена: поле name (identifier_list) или непосредственно identifier
    // В tree-sitter-go поле называется "name"
    let names: Vec<String> = if let Some(name_node) = node.child_by_field_name("name") {
        // Может быть несколько идентификаторов в identifier_list
        if name_node.kind() == "identifier" {
            vec![node_text(name_node, source).to_string()]
        } else {
            // identifier_list
            let mut ids = Vec::new();
            let mut cur = name_node.walk();
            for id in name_node.children(&mut cur) {
                if id.kind() == "identifier" {
                    ids.push(node_text(id, source).to_string());
                }
            }
            ids
        }
    } else {
        // Альтернатива: ищем identifier напрямую среди дочерних
        let mut ids = Vec::new();
        let mut cur = node.walk();
        for child in node.children(&mut cur) {
            if child.kind() == "identifier" {
                ids.push(node_text(child, source).to_string());
            }
        }
        ids
    };

    // Значение: поле value (expression_list или одно выражение)
    let value = node.child_by_field_name("value").map(|n| {
        let text = node_text(n, source);
        if text.chars().count() > 200 {
            text.chars().take(200).collect()
        } else {
            text.to_string()
        }
    });

    for name in names {
        if !name.is_empty() {
            ctx.variables.push(ParsedVariable {
                name,
                value: value.clone(),
                line,
            });
        }
    }
}

/// Обработать call_expression → calls
fn visit_call_expr(
    node: tree_sitter::Node,
    ctx: &mut VisitContext,
    current_func: Option<&str>,
) {
    let source = ctx.source;
    let line = node.start_position().row + 1;

    // В Go call_expression: поле function — выражение-функция
    let callee = if let Some(func_node) = node.child_by_field_name("function") {
        node_text(func_node, source).to_string()
    } else {
        return;
    };

    if callee.is_empty() {
        return;
    }

    let caller = current_func.unwrap_or("<module>").to_string();
    ctx.calls.push(ParsedCall { caller, callee, line });
}

/// Главная функция парсинга Go-файла
fn parse_go(source: &str) -> Result<ParseResult> {
    let mut ts_parser = tree_sitter::Parser::new();
    ts_parser
        .set_language(&tree_sitter_go::LANGUAGE.into())
        .map_err(|e| anyhow!("Ошибка установки языка tree-sitter-go: {}", e))?;

    let tree = ts_parser
        .parse(source, None)
        .ok_or_else(|| anyhow!("tree-sitter не смог распарсить Go-файл"))?;

    let root = tree.root_node();
    let source_bytes = source.as_bytes();

    // Хеш AST
    let ast_hash = sha256_hex(&root.to_sexp());

    // Количество строк
    let lines_total = source.lines().count();

    // Обход AST: обходим прямых потомков source_file
    let mut ctx = VisitContext::new(source_bytes);
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        visit_node(child, &mut ctx, None, true, 0);
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
    fn test_parse_go_function() {
        let parser = GoParser::new();
        let source =
            "package main\n\nfunc hello(name string) string {\n\treturn \"Hello \" + name\n}\n";
        let result = parser.parse(source, "test.go").unwrap();
        assert_eq!(result.functions.len(), 1, "Должна быть одна функция");
        assert_eq!(result.functions[0].name, "hello");
    }

    #[test]
    fn test_parse_go_method() {
        let parser = GoParser::new();
        let source = "package main\n\ntype Server struct {\n\tPort int\n}\n\nfunc (s *Server) Start() error {\n\treturn nil\n}\n";
        let result = parser.parse(source, "test.go").unwrap();

        // Должна быть структура Server
        assert!(
            result.classes.iter().any(|c| c.name == "Server"),
            "Должна быть структура Server"
        );

        // Должен быть метод Start с qualified_name Server.Start
        let method = result.functions.iter().find(|f| f.name == "Start");
        assert!(method.is_some(), "Должен быть метод Start");
        assert_eq!(
            method.unwrap().qualified_name,
            Some("Server.Start".to_string()),
            "qualified_name должен быть Server.Start"
        );
    }

    #[test]
    fn test_parse_go_interface() {
        let parser = GoParser::new();
        let source =
            "package main\n\ntype Handler interface {\n\tHandle(req Request) Response\n}\n";
        let result = parser.parse(source, "test.go").unwrap();

        let iface = result.classes.iter().find(|c| c.name == "Handler");
        assert!(iface.is_some(), "Должен быть интерфейс Handler");
        assert_eq!(
            iface.unwrap().bases,
            Some("interface".to_string()),
            "bases должен быть \"interface\""
        );
    }

    #[test]
    fn test_parse_go_imports() {
        let parser = GoParser::new();
        let source = "package main\n\nimport (\n\t\"fmt\"\n\t\"os\"\n\tlog \"github.com/sirupsen/logrus\"\n)\n";
        let result = parser.parse(source, "test.go").unwrap();
        assert!(
            result.imports.len() >= 3,
            "Должно быть минимум 3 импорта, найдено: {}",
            result.imports.len()
        );
    }

    #[test]
    fn test_parse_go_const_var() {
        let parser = GoParser::new();
        let source = "package main\n\nconst MaxRetries = 3\nvar Debug = false\n";
        let result = parser.parse(source, "test.go").unwrap();
        assert!(
            result.variables.len() >= 2,
            "Должно быть минимум 2 переменные, найдено: {}",
            result.variables.len()
        );
    }

    #[test]
    fn test_parse_go_calls() {
        let parser = GoParser::new();
        let source =
            "package main\n\nfunc main() {\n\tfmt.Println(\"hello\")\n\tprocess()\n}\n";
        let result = parser.parse(source, "test.go").unwrap();
        assert!(
            result.calls.len() >= 2,
            "Должно быть минимум 2 вызова, найдено: {}",
            result.calls.len()
        );
    }
}
