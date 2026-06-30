use anyhow::{anyhow, Result};

use super::types::{
    sha256_hex, hash_ast,
    ParseResult, ParsedCall, ParsedFunction, ParsedVariable,
};
use super::LanguageParser;

/// Парсер BSL-файлов (1С:Предприятие / OneScript) на основе tree-sitter
pub struct BslParser;

impl BslParser {
    pub fn new() -> Self {
        BslParser
    }
}

impl LanguageParser for BslParser {
    fn language_name(&self) -> &str {
        "bsl"
    }

    fn file_extensions(&self) -> &[&str] {
        &["bsl", "os"]
    }

    fn parse(&self, source: &str, _file_path: &str) -> Result<ParseResult> {
        parse_bsl(source)
    }
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

/// Проверить наличие Export/Экспорт в объявлении процедуры/функции или переменной модуля.
/// Для proc/func_declaration: грамматика не создаёт именованный узел `export`,
/// поэтому ищем через дочерний узел (для module_var_declaration) или по тексту первой строки.
fn has_export_child(node: tree_sitter::Node, source: &[u8]) -> bool {
    // Для module_var_declaration — `export` есть как именованный дочерний узел
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "export" {
            return true;
        }
    }
    // Для proc/func_declaration — ищем в тексте первой строки
    // Берём текст до первого перевода строки (сигнатура объявления)
    let full_text = node.utf8_text(source).unwrap_or("");
    let first_line = full_text.lines().next().unwrap_or("");
    // Проверяем наличие ключевого слова Экспорт/Export (регистронезависимо)
    let lower = first_line.to_lowercase();
    lower.contains("экспорт") || lower.contains("export")
}

/// Извлечь директиву компиляции из annotation-дочернего узла.
/// Ищем annotation среди непосредственных дочерних proc/func_declaration.
/// Возвращает строку вида `"&НаСервере"`.
fn extract_annotation(node: tree_sitter::Node, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "annotation" {
            // Первый identifier внутри annotation — имя директивы
            if let Some(ident) = find_child_by_kind(child, "identifier") {
                let name = node_text(ident, source);
                if !name.is_empty() {
                    return Some(format!("&{}", name));
                }
            }
        }
    }
    None
}

/// Извлечь информацию о переопределении из аннотации расширения.
/// Для &Перед("Foo"), &После("Foo"), &Вместо("Foo") возвращает (тип, цель).
fn extract_override_info(node: tree_sitter::Node, source: &[u8]) -> (Option<String>, Option<String>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "annotation" {
            if let Some(ident) = find_child_by_kind(child, "identifier") {
                let name = node_text(ident, source).to_lowercase();
                let override_type = match name.as_str() {
                    "перед" | "before" => Some("Перед"),
                    "после" | "after"  => Some("После"),
                    "вместо" | "instead" => Some("Вместо"),
                    _ => None,
                };
                if let Some(otype) = override_type {
                    // Ищем annotation_parameter → string (имя целевой процедуры)
                    if let Some(param) = find_child_by_kind(child, "annotation_parameter") {
                        if let Some(s) = find_child_by_kind(param, "string") {
                            let raw = node_text(s, source);
                            // Убираем кавычки: "Foo" → Foo
                            let target = raw.trim_matches('"').to_string();
                            if !target.is_empty() {
                                return (Some(otype.to_string()), Some(target));
                            }
                        }
                    }
                    return (Some(otype.to_string()), None);
                }
            }
        }
    }
    (None, None)
}

/// Контекст обхода AST BSL
struct VisitContext<'a> {
    source: &'a [u8],
    functions: Vec<ParsedFunction>,
    calls: Vec<ParsedCall>,
    variables: Vec<ParsedVariable>,
}

impl<'a> VisitContext<'a> {
    fn new(source: &'a [u8]) -> Self {
        VisitContext {
            source,
            functions: Vec::new(),
            calls: Vec::new(),
            variables: Vec::new(),
        }
    }
}

/// Рекурсивный обход узла AST BSL.
/// - `current_func` — имя ближайшей процедуры/функции-контейнера (для caller у вызовов)
fn visit_node(
    node: tree_sitter::Node,
    ctx: &mut VisitContext,
    current_func: Option<&str>,
    depth: usize,
) {
    // Ограничение глубины для защиты от переполнения стека
    if depth > 80 {
        return;
    }

    match node.kind() {
        "proc_declaration" => {
            visit_proc_or_func(node, ctx, "procedure");
        }
        "func_declaration" => {
            visit_proc_or_func(node, ctx, "function");
        }
        "method_block" => {
            // Блок методов — обходим дочерние proc/func
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                visit_node(child, ctx, current_func, depth + 1);
            }
        }
        "module_var_block" => {
            // Блок переменных модуля
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "module_var_declaration" {
                    visit_module_var(child, ctx);
                }
            }
        }
        "method_call" => {
            record_method_call(node, ctx, current_func);
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                visit_node(child, ctx, current_func, depth + 1);
            }
        }
        _ => {
            // Рекурсивно обходим остальные узлы
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                visit_node(child, ctx, current_func, depth + 1);
            }
        }
    }
}

/// Обработать proc_declaration или func_declaration
fn visit_proc_or_func(
    node: tree_sitter::Node,
    ctx: &mut VisitContext,
    proc_type: &str,
) {
    let source = ctx.source;

    // Имя: поля proc_name / func_name
    let name_field = if proc_type == "procedure" { "proc_name" } else { "func_name" };
    let name = node
        .child_by_field_name(name_field)
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();

    if name.is_empty() {
        return;
    }

    let line_start = node.start_position().row + 1;
    let line_end = node.end_position().row + 1;

    // Параметры из argument_list
    let arg_list_text = find_child_by_kind(node, "argument_list")
        .map(|n| node_text(n, source).to_string());

    // Экспорт: есть ли дочерний узел export
    let is_export = has_export_child(node, source);

    // Формируем строку аргументов: текст argument_list + суффикс "Экспорт" если нужно
    let args = match (arg_list_text, is_export) {
        (Some(args_str), true) => Some(format!("{} Экспорт", args_str)),
        (Some(args_str), false) => Some(args_str),
        (None, true) => Some("() Экспорт".to_string()),
        (None, false) => None,
    };

    // Директива компиляции (annotation) → сохраняем в return_type
    let directive = extract_annotation(node, source);

    // Аннотация переопределения расширения (&Перед/&После/&Вместо)
    let (override_type, override_target) = extract_override_info(node, source);

    // Метаинформация BSL в docstring: тип + директива + экспорт
    let docstring = {
        let mut parts = vec![proc_type.to_string()];
        if let Some(ref d) = directive {
            parts.push(d.clone());
        }
        if is_export {
            parts.push("export".to_string());
        }
        if let (Some(ref ot), Some(ref otgt)) = (&override_type, &override_target) {
            parts.push(format!("override:{ot}({otgt})"));
        }
        Some(parts.join(" "))
    };

    // Полный текст
    let body = node_text(node, source).to_string();
    let node_hash = sha256_hex(&body);

    let func_name = name.clone();

    ctx.functions.push(ParsedFunction {
        name,
        qualified_name: None, // В BSL нет вложенных классов
        line_start,
        line_end,
        args,
        return_type: directive,
        docstring,
        body,
        is_async: false, // BSL не имеет async
        node_hash,
        override_type,
        override_target,
    });

    // Рекурсивно обходим тело для извлечения вызовов (method_call в любых
    // выражениях: присваивания, условия, аргументы, операторы-вызовы).
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "proc_declaration" | "func_declaration" | "argument_list" | "annotation" | "export" => {
                // Не рекурсируем в определения и служебные узлы
            }
            _ => {
                visit_body_for_calls(child, ctx, Some(&func_name), 1);
            }
        }
    }
}

/// Рекурсивный обход тела процедуры/функции для извлечения вызовов.
/// Ловит КАЖДЫЙ узел `method_call` — это реальный вызов `Имя(args)` в грамматике
/// onescript; встречается и как оператор-вызов (внутри call_statement), и внутри
/// любых выражений (присваивания, условия, аргументы). Имя вызываемой процедуры/
/// функции — первый `identifier` внутри method_call; квалификатор (`Модуль`)
/// лежит соседом и приклеивается в record_method_call → `Модуль.Функция`.
/// TASK-1: раньше ловился только call_statement → вызовы функций в выражениях
/// (основная масса обращений к общим модулям) терялись, граф вызовов был почти пуст.
fn visit_body_for_calls(
    node: tree_sitter::Node,
    ctx: &mut VisitContext,
    current_func: Option<&str>,
    depth: usize,
) {
    if depth > 80 {
        return;
    }
    if node.kind() == "method_call" {
        record_method_call(node, ctx, current_func);
    }
    // Всегда обходим детей: вложенные вызовы в аргументах (Ф(Г(х))) и любые
    // подвыражения тела.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit_body_for_calls(child, ctx, current_func, depth + 1);
    }
}

/// Записать ребро вызова из узла `method_call`. callee — имя метода (первый
/// `identifier` внутри method_call), СКЛЕЕННОЕ с квалификатором-receiver, если
/// вызов идёт через точку (`Модуль.Метод` → `Модуль.Метод`). Так BSL хранит
/// вызовы единообразно с остальными языками (Python `obj.method`, Rust
/// `obj.method`). Голый вызов (точки нет) остаётся голым именем. caller — имя
/// процедуры-контейнера. Используется и для вызовов в теле процедуры
/// (visit_body_for_calls), и для кода модуля верхнего уровня (visit_node).
fn record_method_call(
    node: tree_sitter::Node,
    ctx: &mut VisitContext,
    current_func: Option<&str>,
) {
    if let Some(ident) = find_child_by_kind(node, "identifier") {
        let method = node_text(ident, ctx.source).to_string();
        if method.is_empty() {
            return;
        }
        // Квалификатор вызова `Модуль.Метод()`: в грамматике onescript левый
        // операнд лежит СОСЕДОМ от method_call — method_call.prev_sibling() это
        // токен `.`, а его prev_sibling() — receiver (member_access/identifier).
        // Склеиваем `receiver.method`; если точки нет — оставляем голое имя.
        let callee = match node.prev_sibling() {
            Some(dot) if dot.kind() == "." => match dot.prev_sibling() {
                Some(recv) => {
                    let q = node_text(recv, ctx.source);
                    if q.is_empty() {
                        method
                    } else {
                        format!("{q}.{method}")
                    }
                }
                None => method,
            },
            _ => method,
        };
        ctx.calls.push(ParsedCall {
            caller: current_func.unwrap_or("<module>").to_string(),
            callee,
            line: node.start_position().row + 1,
        });
    }
}

/// Обработать module_var_declaration — объявление переменной модуля
fn visit_module_var(node: tree_sitter::Node, ctx: &mut VisitContext) {
    let source = ctx.source;
    let line = node.start_position().row + 1;

    // Имя переменной: первый identifier
    if let Some(ident) = find_child_by_kind(node, "identifier") {
        let name = node_text(ident, source).to_string();
        if !name.is_empty() {
            // Значение: признак экспорта
            let value = if has_export_child(node, source) {
                Some("Экспорт".to_string())
            } else {
                None
            };
            ctx.variables.push(ParsedVariable { name, value, line });
        }
    }
}

/// Главная функция парсинга BSL-файла
/// Порог времени на парсинг одного BSL-файла — страховка от патологий tree-sitter.
/// 10 с даёт ~5-кратный запас над самым медленным легитимным модулем (≈2 с на 8 МБ),
/// при этом обрывает деградацию на аномальном вводе за секунды вместо минут.
const PARSE_TIMEOUT_MS: u64 = 10_000;

/// Признак, что под расширением `.bsl` лежит не исходник, а двоичные данные.
/// EDT выгружает защищённые модули поставщика как `ObjectModule.bsl` с двоичным
/// образом 1С (сигнатура `FF FF FF 7F`) вместо текста — конфигуратор для тех же
/// модулей использует `.bin`. NUL-байт в первых килобайтах — надёжный маркер
/// не-текста (как в git/file): валидный BSL-исходник его не содержит, а на таком
/// вводе tree-sitter уходит в нелинейную деградацию (квадратично по размеру).
fn looks_binary(source: &str) -> bool {
    source.as_bytes().iter().take(8192).any(|&b| b == 0)
}

/// Пустой результат — для файлов, пропущенных защитой (двоичные либо превысившие таймаут).
fn empty_parse_result(source: &str) -> ParseResult {
    ParseResult {
        functions: Vec::new(),
        classes: Vec::new(),
        imports: Vec::new(),
        calls: Vec::new(),
        variables: Vec::new(),
        lines_total: source.lines().count(),
        ast_hash: String::new(),
    }
}

fn parse_bsl(source: &str) -> Result<ParseResult> {
    // Защита 1: двоичный .bsl (EDT-защищённые модули) — не отдаём в tree-sitter,
    // иначе он деградирует на бесструктурном вводе и вешает индексацию.
    if looks_binary(source) {
        return Ok(empty_parse_result(source));
    }

    let mut ts_parser = tree_sitter::Parser::new();
    ts_parser
        .set_language(&tree_sitter_onescript::LANGUAGE.into())
        .map_err(|e| anyhow!("Ошибка установки языка tree-sitter-onescript: {}", e))?;

    // Защита 2: дедлайн на парсинг — страховка от любой будущей патологии.
    // При превышении parse() возвращает None; трактуем как пустой результат,
    // чтобы один файл не ронял ошибкой и не вешал всю индексацию.
    #[allow(deprecated)]
    ts_parser.set_timeout_micros(PARSE_TIMEOUT_MS * 1000);

    let tree = match ts_parser.parse(source, None) {
        Some(t) => t,
        None => return Ok(empty_parse_result(source)),
    };

    let root = tree.root_node();
    let source_bytes = source.as_bytes();

    // Хеш AST
    let ast_hash = hash_ast(root);

    // Количество строк
    let lines_total = source.lines().count();

    // Обход AST
    let mut ctx = VisitContext::new(source_bytes);
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        visit_node(child, &mut ctx, None, 0);
    }

    Ok(ParseResult {
        functions: ctx.functions,
        classes: Vec::new(), // BSL не имеет классов
        imports: Vec::new(),  // BSL не имеет импортов
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
    fn test_parse_bsl_procedure() {
        let parser = BslParser::new();
        let source = "Процедура ПриОткрытии()\n    Сообщить(\"Привет\");\nКонецПроцедуры";
        let result = parser.parse(source, "test.bsl").unwrap();
        assert_eq!(result.functions.len(), 1);
        assert_eq!(result.functions[0].name, "ПриОткрытии");
    }

    #[test]
    fn test_parse_bsl_function() {
        let parser = BslParser::new();
        let source = "Функция ПолучитьДанные(Параметр1, Параметр2) Экспорт\n    Возврат Параметр1 + Параметр2;\nКонецФункции";
        let result = parser.parse(source, "test.bsl").unwrap();
        assert_eq!(result.functions.len(), 1);
        assert_eq!(result.functions[0].name, "ПолучитьДанные");
    }

    #[test]
    fn test_parse_bsl_with_directive() {
        let parser = BslParser::new();
        let source = "&НаСервере\nПроцедура ОбработатьНаСервере()\n    // код\nКонецПроцедуры";
        let result = parser.parse(source, "test.bsl").unwrap();
        assert_eq!(result.functions.len(), 1);
        assert_eq!(result.functions[0].name, "ОбработатьНаСервере");
    }

    #[test]
    fn test_parse_bsl_calls() {
        let parser = BslParser::new();
        let source = "Процедура Тест()\n    Сообщить(\"Привет\");\n    Рез = ОбщегоНазначения.ЗначениеРеквизита(Объект);\n    ОбщийМодуль.МетодМодуля();\nКонецПроцедуры";
        let result = parser.parse(source, "test.bsl").unwrap();
        let names: Vec<&str> = result.calls.iter().map(|c| c.callee.as_str()).collect();
        // Неквалифицированный вызов-оператор — остаётся голым именем (точки нет).
        assert!(names.contains(&"Сообщить"), "Сообщить не найден: {:?}", names);
        // Главный кейс: вызов ФУНКЦИИ в ВЫРАЖЕНИИ через общий модуль —
        // квалификатор приклеен: `ОбщегоНазначения.ЗначениеРеквизита`.
        assert!(names.contains(&"ОбщегоНазначения.ЗначениеРеквизита"),
            "склеенный вызов функции в выражении не найден: {:?}", names);
        // Квалифицированный вызов-процедура (call_statement) — тоже склеен.
        assert!(names.contains(&"ОбщийМодуль.МетодМодуля"),
            "склеенный МетодМодуля не найден: {:?}", names);
        // Голый метод без модуля не должен появляться отдельно для
        // квалифицированных вызовов (квалификатор приклеен).
        assert!(!names.contains(&"ЗначениеРеквизита") && !names.contains(&"МетодМодуля"),
            "квалифицированный вызов попал в callee голым: {:?}", names);
        // Имена модулей-приёмников не должны попадать в callee как отдельные «вызовы».
        assert!(!names.contains(&"ОбщегоНазначения") && !names.contains(&"ОбщийМодуль"),
            "имя модуля ошибочно записано как отдельный вызов: {:?}", names);
    }

    #[test]
    fn test_parse_bsl_module_vars() {
        let parser = BslParser::new();
        let source = "Перем МояПеременная Экспорт;\nПерем ВтораяПеременная;\n\nПроцедура Тест()\nКонецПроцедуры";
        let result = parser.parse(source, "test.bsl").unwrap();
        assert!(result.variables.len() >= 2);
    }

    #[test]
    fn test_parse_bsl_english_keywords() {
        let parser = BslParser::new();
        let source = "Procedure OnOpen() Export\n    Message(\"Hello\");\nEndProcedure";
        let result = parser.parse(source, "test.bsl").unwrap();
        assert_eq!(result.functions.len(), 1);
        assert_eq!(result.functions[0].name, "OnOpen");
    }

    #[test]
    fn test_parse_bsl_export_marker() {
        let parser = BslParser::new();
        // Функция с Экспорт — в args должен быть суффикс "Экспорт"
        let source = "Функция ПолучитьДанные(Пар1) Экспорт\n    Возврат Пар1;\nКонецФункции";
        let result = parser.parse(source, "test.bsl").unwrap();
        assert_eq!(result.functions.len(), 1);
        let f = &result.functions[0];
        assert!(f.args.as_deref().unwrap_or("").contains("Экспорт"));
    }

    #[test]
    fn test_parse_bsl_directive_in_return_type() {
        let parser = BslParser::new();
        let source = "&НаКлиенте\nПроцедура НаКлиенте()\nКонецПроцедуры";
        let result = parser.parse(source, "test.bsl").unwrap();
        assert_eq!(result.functions.len(), 1);
        // Директива должна быть сохранена в return_type
        assert!(result.functions[0].return_type.is_some());
        assert!(result.functions[0].return_type.as_deref().unwrap().contains("НаКлиенте"));
    }

    #[test]
    fn test_parse_bsl_docstring_contains_type() {
        let parser = BslParser::new();
        let source = "Процедура МояПроц()\nКонецПроцедуры";
        let result = parser.parse(source, "test.bsl").unwrap();
        assert_eq!(result.functions.len(), 1);
        // docstring содержит "procedure"
        let doc = result.functions[0].docstring.as_deref().unwrap_or("");
        assert!(doc.contains("procedure"));
    }

    #[test]
    fn test_parse_bsl_override_before() {
        let parser = BslParser::new();
        let source = "&Перед(\"ОригинальнаяПроцедура\")\nПроцедура Ext_ОригинальнаяПроцедура()\nКонецПроцедуры";
        let result = parser.parse(source, "test.bsl").unwrap();
        assert_eq!(result.functions.len(), 1);
        let f = &result.functions[0];
        assert_eq!(f.override_type.as_deref(), Some("Перед"));
        assert_eq!(f.override_target.as_deref(), Some("ОригинальнаяПроцедура"));
        // Директива тоже должна быть извлечена
        assert_eq!(f.return_type.as_deref(), Some("&Перед"));
    }

    #[test]
    fn test_parse_bsl_override_instead() {
        let parser = BslParser::new();
        let source = "&Вместо(\"ПолучитьДанные\")\nФункция Ext_ПолучитьДанные()\n    Возврат 42;\nКонецФункции";
        let result = parser.parse(source, "test.bsl").unwrap();
        assert_eq!(result.functions.len(), 1);
        let f = &result.functions[0];
        assert_eq!(f.override_type.as_deref(), Some("Вместо"));
        assert_eq!(f.override_target.as_deref(), Some("ПолучитьДанные"));
    }

    #[test]
    fn test_parse_bsl_override_after() {
        let parser = BslParser::new();
        let source = "&После(\"ПриЗаписи\")\nПроцедура Ext_ПриЗаписи()\nКонецПроцедуры";
        let result = parser.parse(source, "test.bsl").unwrap();
        assert_eq!(result.functions.len(), 1);
        let f = &result.functions[0];
        assert_eq!(f.override_type.as_deref(), Some("После"));
        assert_eq!(f.override_target.as_deref(), Some("ПриЗаписи"));
    }

    #[test]
    fn test_parse_bsl_no_override_for_regular_directive() {
        let parser = BslParser::new();
        let source = "&НаСервере\nПроцедура Тест()\nКонецПроцедуры";
        let result = parser.parse(source, "test.bsl").unwrap();
        assert_eq!(result.functions.len(), 1);
        let f = &result.functions[0];
        assert!(f.override_type.is_none());
        assert!(f.override_target.is_none());
        assert_eq!(f.return_type.as_deref(), Some("&НаСервере"));
    }

    #[test]
    fn test_binary_source_yields_empty_not_hang() {
        // .bsl с двоичным содержимым (EDT-защищённый модуль поставщика) не должен
        // парситься tree-sitter'ом: возвращаем пустой результат, без зависания.
        let parser = BslParser::new();
        let binary = "\u{0}\u{2}binary\u{0}image content";
        let result = parser.parse(binary, "ObjectModule.bsl").unwrap();
        assert_eq!(result.functions.len(), 0);
    }
}
