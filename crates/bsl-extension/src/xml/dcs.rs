// Парсер макета «Схема компоновки данных» (СКД) 1С.
//
// Схема компоновки лежит в макете отчёта: в выгрузке Конфигуратора —
// `<Вид>/<Объект>/Templates/<Имя>/Ext/Template.xml`, в выгрузке 1C:EDT —
// `<Вид>/<Объект>/Templates/<Имя>/Template.dcs`. Корневой элемент —
// `<DataCompositionSchema>` (ns `v8.1c.ru/8.1/data-composition-system/schema`).
//
// Структура повторяет сериализацию коллекций платформой: каждое значение
// обёрнуто в контейнер (`<dataSets>`, `<parameters>`, `<settingsVariants>` …),
// который повторяется по числу элементов. Внутри набора данных — поля
// (`<field xsi:type="DataSetFieldField|DataSetFieldFolder">`), источник, имя
// объекта или текст запроса; вложенные наборы объединения лежат в `<items>`.
//
// Разбираем только то, что нужно индексу и инструменту `get_dcs_schema`:
// наборы (с полями), связи наборов, вычисляемые поля, итоги, параметры и
// варианты настроек. Незнакомые теги молча пропускаем — их в схеме много
// (оформление варианта, пользовательские поля, отборы), и падать на них
// нельзя. `Err` возвращаем только на непарсимом XML.

use anyhow::Result;
use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

use super::BytesTextExt;

/// Разобранная схема компоновки данных.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct DcsSchema {
    /// Заголовок схемы (`<title>`, ru-представление).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Наборы данных верхнего уровня (объединения — тоже наборы, их части в `items`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub data_sets: Vec<DcsDataSet>,
    /// Связи между наборами данных (`<dataSetLink>`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<DcsLink>,
    /// Вычисляемые поля схемы.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub calculated_fields: Vec<DcsCalculatedField>,
    /// Итоговые поля схемы.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub totals: Vec<DcsTotalField>,
    /// Параметры схемы.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<DcsParameter>,
    /// Варианты настроек (`<settingsVariant>`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub variants: Vec<DcsVariant>,
    /// Сколько макетов оформления объявлено в схеме (`<template>`): их
    /// содержимое (`.mxlx`) индексу не нужно, важен сам факт и число.
    pub templates_count: usize,
}

/// Один набор данных схемы.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct DcsDataSet {
    /// Имя набора (`<name>`).
    pub name: String,
    /// Вид набора по `xsi:type`: `query` | `object` | `union`
    /// (неизвестный `xsi:type` трактуем как `query`).
    pub kind: String,
    /// Имя источника данных (`<dataSource>`, ссылка на `<dataSources>` схемы).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_source: Option<String>,
    /// Имя объекта метаданных для набора-объекта (`<objectName>`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object_name: Option<String>,
    /// Текст запроса набора (`<query>`) как есть, с переводами строк.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// Поля набора (включая вложенные в папки — плоским списком).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<DcsField>,
    /// Вложенные наборы объединения (`<items>`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<DcsDataSet>,
}

/// Поле набора данных.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct DcsField {
    /// Путь поля (`<dataPath>`; у простого поля совпадает с `<field>`).
    pub name: String,
    /// Представление поля (`<title>`, ru-представление).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Тип значения поля человекочитаемой 1С-строкой (составной — через ` | `).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value_type: Option<String>,
    /// Роль поля: имя первого дочернего тега `<role>` со значением `true`
    /// (`dimension`, `period`, `account`, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Поле — папка полей (`DataSetFieldFolder`): своего типа значения не имеет,
    /// а поля внутри неё лежат тут же, в плоском `fields` набора.
    #[serde(skip_serializing_if = "is_false")]
    pub is_folder: bool,
}

/// Связь между наборами данных (`<dataSetLink>`).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct DcsLink {
    /// Имя набора-источника (`<sourceDataSet>`).
    pub source: String,
    /// Имя набора-приёмника (`<destinationDataSet>`).
    pub destination: String,
    /// Выражение в источнике (`<sourceExpression>`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_expression: Option<String>,
    /// Выражение в приёмнике (`<destinationExpression>`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destination_expression: Option<String>,
    /// Имя параметра запроса приёмника, в который передаётся значение связи
    /// (`<parameter>`): по нему видно, куда в тексте запроса уходит связь.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameter: Option<String>,
}

/// Вычисляемое поле схемы (`<calculatedField>`).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct DcsCalculatedField {
    /// Путь поля (`<dataPath>`).
    pub name: String,
    /// Выражение (`<expression>`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expression: Option<String>,
    /// Представление (`<title>`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Тип значения вычисляемого поля, если он задан явно (`<valueType>`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value_type: Option<String>,
}

/// Итоговое поле схемы (`<totalField>`).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct DcsTotalField {
    /// Путь поля (`<dataPath>`).
    pub name: String,
    /// Выражение итога (`<expression>`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expression: Option<String>,
}

/// Параметр схемы (`<parameter>`).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct DcsParameter {
    /// Имя параметра (`<name>`).
    pub name: String,
    /// Представление (`<title>`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Тип значения человекочитаемой 1С-строкой.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value_type: Option<String>,
    /// Значение по умолчанию (`<value>`); у `xsi:nil` — отсутствует.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// Ограничение использования параметра (`<useRestriction>`): значение
    /// менять нельзя. Тега нет — ограничения нет.
    #[serde(skip_serializing_if = "is_false")]
    pub use_restriction: bool,
    /// Выражение, которым вычисляется значение параметра
    /// (`<expression>`, например `&Период.ДатаОкончания`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expression: Option<String>,
    /// Доступен ли параметр как поле компоновки (`<availableAsField>`).
    /// Платформа пишет тег только при значении, отличном от умолчания, а
    /// умолчание — `true`, поэтому отсутствие тега читается как «доступен».
    pub available_as_field: bool,
}

/// Вариант настроек схемы (`<settingsVariant>`).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct DcsVariant {
    /// Имя варианта (`<dcsset:name>`).
    pub name: String,
    /// Представление варианта (`<dcsset:presentation>`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presentation: Option<String>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Узел разбора: локальное имя тега, `xsi:type`, прямой текст и дети.
struct Node {
    name: String,
    xsi_type: Option<String>,
    text: String,
    children: Vec<Node>,
}

fn local_name(raw: &str) -> String {
    match raw.rfind(':') {
        Some(i) => raw[i + 1..].to_string(),
        None => raw.to_string(),
    }
}

fn node_from_start(e: &BytesStart<'_>) -> Node {
    let name = local_name(&String::from_utf8_lossy(e.name().as_ref()));
    let mut xsi_type = None;
    for attr in e.attributes().flatten() {
        let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
        if local_name(&key) == "type" {
            xsi_type = attr
                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .ok()
                .map(|c| c.to_string());
        }
    }
    Node {
        name,
        xsi_type,
        text: String::new(),
        children: Vec::new(),
    }
}

fn attach(node: Node, stack: &mut [Node], root: &mut Option<Node>) {
    match stack.last_mut() {
        Some(parent) => parent.children.push(node),
        None => *root = Some(node),
    }
}

/// Собрать дерево элементов. Незнакомые теги остаются в дереве как есть —
/// интерпретация их просто не находит. `Err` — только непарсимый XML.
fn build_tree(content: &str) -> Result<Node> {
    let mut reader = Reader::from_str(content);
    // Обрезку пробелов читателю не доверяем: с quick-xml 0.38 текст с
    // сущностями приходит кусками (`Код ` / `&lt;` / `&gt;` / ` 0`), и обрезка
    // каждого куска склеивала бы `Код <> 0` в `Код<>0`. Внешние пробелы у
    // значений снимает `text_of`, внутренние остаются как в файле.
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut stack: Vec<Node> = Vec::new();
    let mut root: Option<Node> = None;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => stack.push(node_from_start(&e)),
            Ok(Event::Empty(e)) => {
                let node = node_from_start(&e);
                attach(node, &mut stack, &mut root);
            }
            Ok(Event::End(_)) => {
                if let Some(node) = stack.pop() {
                    attach(node, &mut stack, &mut root);
                }
            }
            Ok(Event::Text(t)) => {
                if let Some(top) = stack.last_mut() {
                    let txt = t.unescape().map(|s| s.into_owned()).unwrap_or_default();
                    top.text.push_str(&txt);
                }
            }
            // `&amp;` и прочие сущности — отдельное событие, а не часть текста:
            // без этой ветки из выражений пропадал `&` перед именем параметра.
            Ok(Event::GeneralRef(r)) => {
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&super::general_ref_text(&r));
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "DCS XML: ошибка парсинга на позиции {}: {}",
                    reader.buffer_position(),
                    e
                ));
            }
            _ => {}
        }
        buf.clear();
    }
    root.ok_or_else(|| anyhow::anyhow!("DCS XML: документ не содержит элементов"))
}

/// Первый прямой ребёнок с таким локальным именем (регистр неважен).
fn child<'a>(node: &'a Node, name: &str) -> Option<&'a Node> {
    node.children
        .iter()
        .find(|c| c.name.eq_ignore_ascii_case(name))
}

/// Текст узла без внешних пробелов; пустой текст — `None`.
fn text_of(node: &Node) -> Option<String> {
    let t = node.text.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// Текст первого прямого ребёнка с таким именем.
fn child_text(node: &Node, name: &str) -> Option<String> {
    child(node, name).and_then(text_of)
}

/// Текст первого найденного из перечисленных детей.
fn first_child_text(node: &Node, names: &[&str]) -> Option<String> {
    names.iter().find_map(|n| child_text(node, n))
}

/// Все узлы-потомки с таким локальным именем (рекурсивно, без самого `node`).
fn descendants<'a>(node: &'a Node, name: &str) -> Vec<&'a Node> {
    fn walk<'a>(n: &'a Node, name: &str, out: &mut Vec<&'a Node>) {
        for c in &n.children {
            if c.name.eq_ignore_ascii_case(name) {
                out.push(c);
            }
            walk(c, name, out);
        }
    }
    let mut out = Vec::new();
    walk(node, name, &mut out);
    out
}

/// Элементы коллекции по имени в единственном числе: платформа сериализует их
/// либо повтором самого элемента (`<dataSet>`), либо повтором контейнера
/// (`<dataSets>` — по одному элементу внутри). Узнаём оба варианта.
fn collect_elems<'a>(parent: &'a Node, singular: &str) -> Vec<&'a Node> {
    let plural = format!("{}s", singular);
    let mut out = Vec::new();
    for c in &parent.children {
        let n = c.name.to_lowercase();
        if n == singular {
            out.push(c);
        } else if n == plural {
            let inner: Vec<&Node> = c
                .children
                .iter()
                .filter(|g| g.name.to_lowercase() == singular)
                .collect();
            if !inner.is_empty() && inner.len() == c.children.len() {
                out.extend(inner);
            } else {
                out.push(c);
            }
        }
    }
    out
}

/// Представление из `<v8:item>`: берём `v8:lang == "ru"`, иначе — первый
/// `v8:content`. Используется и для `<title>`, и для `dcsset:presentation`.
fn presentation_of(node: &Node) -> Option<String> {
    let items = descendants(node, "item");
    let content = |it: &Node| child(it, "content").and_then(text_of);
    for it in &items {
        if child_text(it, "lang").as_deref() == Some("ru") {
            if let Some(v) = content(it) {
                return Some(v);
            }
        }
    }
    for it in &items {
        if let Some(v) = content(it) {
            return Some(v);
        }
    }
    // Вариант без `<v8:item>` (одно поле `<content>`) — тоже принимаем.
    if let Some(v) = descendants(node, "content").into_iter().find_map(text_of) {
        return Some(v);
    }
    // Простая строка прямо в теге: так платформа пишет представление варианта
    // настроек (`<dcsset:presentation xsi:type="xs:string">Основной</…>`).
    text_of(node)
}

/// Типы поля человекочитаемой строкой. Типы из `.dcs` EDT идут в платформенной
/// нотации без префикса — их нормализует `edt_type_to_cfg`; дальше общий для
/// проекта `pretty_types` склеивает составной тип через ` | `.
fn pretty_value_type(types: &[String]) -> Option<String> {
    if types.is_empty() {
        return None;
    }
    let mapped: Vec<String> = types
        .iter()
        .map(|t| match t.split_once(':') {
            None => crate::xml::edt_mdo::edt_type_to_cfg(t),
            // Любой префикс, кроме `xs`/`v8`, — пространство имён текущей
            // конфигурации: в `.dcs` EDT оно объявляется прямо на теге под
            // случайным именем (`xmlns:d4p1=".../enterprise/current-config"`),
            // и тип приходит как `d4p1:CatalogRef.X`. Для `pretty_types` это
            // тот же `cfg:CatalogRef.X`; без замены псевдоним утекал в ответ.
            Some((prefix, rest)) if prefix != "xs" && prefix != "v8" => format!("cfg:{rest}"),
            Some(_) => t.clone(),
        })
        .collect();
    Some(crate::xml::object_attributes::pretty_types(&mapped))
}

/// Типы значения узла `<valueType>` (или `None`, если их нет).
fn value_type_of(node: &Node) -> Option<String> {
    let vt = child(node, "valueType")?;
    let types: Vec<String> = descendants(vt, "Type")
        .into_iter()
        .filter_map(text_of)
        .collect();
    pretty_value_type(&types)
}

/// Имя роли поля: первый дочерний тег `<role>`, у которого текст `true`.
fn role_of(node: &Node) -> Option<String> {
    let role = child(node, "role")?;
    role.children
        .iter()
        .find(|c| c.text.trim().eq_ignore_ascii_case("true"))
        .map(|c| c.name.clone())
}

fn parse_field(node: &Node) -> DcsField {
    let is_folder = node
        .xsi_type
        .as_deref()
        .map(|t| t.contains("DataSetFieldFolder"))
        .unwrap_or(false);
    DcsField {
        name: first_child_text(node, &["dataPath", "field"]).unwrap_or_default(),
        title: child(node, "title").and_then(presentation_of),
        value_type: if is_folder { None } else { value_type_of(node) },
        role: role_of(node),
        is_folder,
    }
}

fn parse_dataset(node: &Node) -> DcsDataSet {
    let kind = match node.xsi_type.as_deref() {
        Some(t) if t.contains("DataSetObject") => "object",
        Some(t) if t.contains("DataSetUnion") => "union",
        _ => "query",
    }
    .to_string();
    let mut ds = DcsDataSet {
        name: first_child_text(node, &["name", "dataPath"]).unwrap_or_default(),
        kind,
        data_source: first_child_text(node, &["dataSource", "dataSources"]),
        object_name: first_child_text(node, &["objectName", "object"]),
        query: child(node, "query").map(|c| c.text.trim_end().to_string()),
        fields: Vec::new(),
        items: Vec::new(),
    };
    for f in collect_elems(node, "field") {
        ds.fields.push(parse_field(f));
    }
    // Части объединения платформа пишет либо повтором `<item>` (выгрузка
    // Конфигуратора), либо повтором контейнера `<items>` (выгрузка 1C:EDT) —
    // `collect_elems` узнаёт обе формы, поэтому набор-объединение не теряет
    // свои части ни в одном формате.
    for it in collect_elems(node, "item") {
        ds.items.push(parse_dataset(it));
    }
    ds
}

fn parse_link(node: &Node) -> DcsLink {
    DcsLink {
        source: first_child_text(node, &["sourceDataSet", "source"]).unwrap_or_default(),
        destination: first_child_text(node, &["destinationDataSet", "destination"])
            .unwrap_or_default(),
        source_expression: first_child_text(node, &["sourceExpression"]),
        destination_expression: first_child_text(node, &["destinationExpression"]),
        parameter: child_text(node, "parameter"),
    }
}

fn parse_calculated_field(node: &Node) -> DcsCalculatedField {
    DcsCalculatedField {
        name: first_child_text(node, &["dataPath", "name"]).unwrap_or_default(),
        expression: child_text(node, "expression"),
        title: child(node, "title").and_then(presentation_of),
        value_type: value_type_of(node),
    }
}

fn parse_total_field(node: &Node) -> DcsTotalField {
    DcsTotalField {
        name: first_child_text(node, &["dataPath", "name"]).unwrap_or_default(),
        expression: child_text(node, "expression"),
    }
}

fn parse_parameter(node: &Node) -> DcsParameter {
    DcsParameter {
        name: child_text(node, "name").unwrap_or_default(),
        title: child(node, "title").and_then(presentation_of),
        value_type: value_type_of(node),
        value: child(node, "value").and_then(text_of),
        use_restriction: is_true(child_text(node, "useRestriction").as_deref()),
        expression: child_text(node, "expression"),
        // Тега нет → умолчание платформы (`true`), см. комментарий поля.
        available_as_field: child_text(node, "availableAsField")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(true),
    }
}

/// Булев тег схемы: текст `true` (регистр неважен) — истина, всё прочее и
/// отсутствие тега — ложь.
fn is_true(text: Option<&str>) -> bool {
    matches!(text, Some(t) if t.eq_ignore_ascii_case("true"))
}

fn parse_variant(node: &Node) -> DcsVariant {
    DcsVariant {
        name: child_text(node, "name").unwrap_or_default(),
        presentation: child(node, "presentation")
            .and_then(presentation_of)
            .or_else(|| child(node, "title").and_then(presentation_of)),
    }
}

/// Разобрать схему компоновки данных. `Err` — только непарсимый XML;
/// на постороннем корне возвращается пустая схема.
pub fn parse_dcs(content: &str) -> Result<DcsSchema> {
    let root = build_tree(content)?;
    if root.name != "DataCompositionSchema" {
        return Ok(DcsSchema::default());
    }
    let mut schema = DcsSchema {
        title: child(&root, "title").and_then(presentation_of),
        templates_count: collect_elems(&root, "template").len(),
        ..Default::default()
    };
    // Источники данных отдельной секцией не храним: наборы ссылаются на них
    // по имени, а само имя источника индексу не нужно.
    for ds in collect_elems(&root, "dataset") {
        schema.data_sets.push(parse_dataset(ds));
    }
    for l in collect_elems(&root, "datasetlink") {
        schema.links.push(parse_link(l));
    }
    for f in collect_elems(&root, "calculatedfield") {
        schema.calculated_fields.push(parse_calculated_field(f));
    }
    for t in collect_elems(&root, "totalfield") {
        schema.totals.push(parse_total_field(t));
    }
    for p in collect_elems(&root, "parameter") {
        schema.parameters.push(parse_parameter(p));
    }
    for v in collect_elems(&root, "settingsvariant") {
        schema.variants.push(parse_variant(v));
    }
    Ok(schema)
}

/// Является ли содержимое схеме компоновки данных: первый `Event::Start`
/// имеет локальное имя `DataCompositionSchema`.
///
/// Нужна для общих макетов (по пути вид макета не виден) и для ветки
/// наблюдателя формата Конфигуратора, где файл содержимого не читается ни
/// паспортом, ни описанием.
pub fn is_dcs_content(content: &str) -> bool {
    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                return local_name(&String::from_utf8_lossy(e.name().as_ref()))
                    == "DataCompositionSchema";
            }
            Ok(Event::Eof) => break,
            Err(_) => return false,
            _ => {}
        }
        buf.clear();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<DataCompositionSchema xmlns="http://v8.1c.ru/8.1/data-composition-system/schema"
    xmlns:dcsset="http://v8.1c.ru/8.1/data-composition-system/settings"
    xmlns:v8="http://v8.1c.ru/8.1/data/core"
    xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <dataSources>
    <name>ИсточникДанных1</name>
    <dataSourceType>Local</dataSourceType>
  </dataSources>
  <dataSets>
    <name>НаборДанных1</name>
    <field xsi:type="DataSetFieldField">
      <dataPath>Организация</dataPath>
      <title xsi:type="v8:LocalStringType">
        <v8:item><v8:lang>ru</v8:lang><v8:content>Организация</v8:content></v8:item>
      </title>
      <role><dimension>true</dimension></role>
      <valueType><v8:Type>cfg:CatalogRef.Организации</v8:Type></valueType>
    </field>
    <field xsi:type="DataSetFieldFolder">
      <dataPath>Папка</dataPath>
    </field>
    <field xsi:type="DataSetFieldField">
      <dataPath>Сумма</dataPath>
      <valueType><v8:Type>xs:decimal</v8:Type></valueType>
    </field>
    <dataSource>ИсточникДанных1</dataSource>
    <query>ВЫБРАТЬ
	Организация,
	Сумма
ИЗ
	РегистрНакопления.Выручка.Остатки</query>
  </dataSets>
  <dataSets>
    <name>НаборДанных2</name>
    <field xsi:type="DataSetFieldField">
      <dataPath>Дата</dataPath>
    </field>
  </dataSets>
  <dataSetLinks>
    <sourceDataSet>НаборДанных1</sourceDataSet>
    <destinationDataSet>НаборДанных2</destinationDataSet>
    <sourceExpression>Организация</sourceExpression>
    <destinationExpression>Организация</destinationExpression>
  </dataSetLinks>
  <calculatedFields>
    <dataPath>Поле1</dataPath>
    <expression>Организация.Наименование</expression>
  </calculatedFields>
  <totalFields>
    <dataPath>Сумма</dataPath>
    <expression>Сумма(Сумма)</expression>
  </totalFields>
  <parameters>
    <name>НачалоПериода</name>
    <valueType><v8:Type>xs:dateTime</v8:Type></valueType>
  </parameters>
  <settingsVariants>
    <dcsset:name>Основной</dcsset:name>
    <dcsset:presentation xsi:type="v8:LocalStringType">
      <v8:item><v8:lang>ru</v8:lang><v8:content>Основной вариант</v8:content></v8:item>
    </dcsset:presentation>
    <dcsset:settings>что-то очень тяжёлое, внутрь не ходим</dcsset:settings>
  </settingsVariants>
  <templates>
    <name>Макет1</name>
  </templates>
</DataCompositionSchema>"#;

    /// Сущности внутри текста приходят отдельным событием (quick-xml ≥ 0.38):
    /// `&amp;` перед именем параметра и представление варианта простой
    /// строкой должны доходить до результата.
    #[test]
    fn сущности_и_строковое_представление_сохраняются() {
        let xml = r#"<?xml version="1.0"?>
<DataCompositionSchema xmlns="http://v8.1c.ru/8.1/data-composition-system/schema"
    xmlns:dcsset="http://v8.1c.ru/8.1/data-composition-system/settings"
    xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <calculatedField>
    <dataPath>Выбран</dataPath>
    <expression>Код в (&amp;ВыбранныеОКВЭД) и Код &lt;&gt; &quot;&quot;</expression>
  </calculatedField>
  <parameter>
    <name>КонецПериода</name>
    <expression>&amp;Период.ДатаОкончания</expression>
  </parameter>
  <settingsVariant>
    <dcsset:name>Основной</dcsset:name>
    <dcsset:presentation xsi:type="xs:string">Основной вариант</dcsset:presentation>
  </settingsVariant>
</DataCompositionSchema>"#;
        let s = parse_dcs(xml).unwrap();
        assert_eq!(
            s.calculated_fields[0].expression.as_deref(),
            Some("Код в (&ВыбранныеОКВЭД) и Код <> \"\"")
        );
        assert_eq!(
            s.parameters[0].expression.as_deref(),
            Some("&Период.ДатаОкончания")
        );
        assert_eq!(
            s.variants[0].presentation.as_deref(),
            Some("Основной вариант")
        );
    }

    /// Псевдоним пространства имён конфигурации в `.dcs` EDT произвольный
    /// (`d4p1:`), а `xs:`/`v8:` — платформенные и остаются как есть.
    #[test]
    fn тип_с_локальным_псевдонимом_конфигурации_читается_как_cfg() {
        assert_eq!(
            pretty_value_type(&["d4p1:CatalogRef.Организации".to_string()]).as_deref(),
            Some("СправочникСсылка.Организации")
        );
        assert_eq!(
            pretty_value_type(&["xs:decimal".to_string()]).as_deref(),
            Some("Число")
        );
    }

    #[test]
    fn разбирает_все_секции() {
        let s = parse_dcs(FULL).unwrap();
        assert_eq!(s.data_sets.len(), 2);
        assert_eq!(s.data_sets[0].name, "НаборДанных1");
        assert_eq!(s.data_sets[0].kind, "query");
        assert_eq!(
            s.data_sets[0].data_source.as_deref(),
            Some("ИсточникДанных1")
        );
        let f = &s.data_sets[0].fields;
        assert_eq!(f.len(), 3);
        assert_eq!(f[0].name, "Организация");
        assert_eq!(f[0].title.as_deref(), Some("Организация"));
        assert_eq!(f[0].role.as_deref(), Some("dimension"));
        // Ссылочный тип отдаётся так же, как у реквизитов объектов
        // (`pretty_types`): `СправочникСсылка.<Имя>`, второго разбора типов нет.
        assert_eq!(
            f[0].value_type.as_deref(),
            Some("СправочникСсылка.Организации")
        );
        assert!(f[1].is_folder, "DataSetFieldFolder → is_folder");
        assert_eq!(f[1].value_type, None);
        assert_eq!(f[2].value_type.as_deref(), Some("Число"));
        let q = s.data_sets[0].query.clone().unwrap_or_default();
        assert!(q.starts_with("ВЫБРАТЬ"), "текст запроса сохранён: {q:?}");
        assert!(q.contains('\n'), "переводы строк сохранены");
        assert_eq!(s.links.len(), 1);
        assert_eq!(s.links[0].source, "НаборДанных1");
        assert_eq!(s.links[0].destination, "НаборДанных2");
        assert_eq!(s.calculated_fields.len(), 1);
        assert_eq!(s.calculated_fields[0].name, "Поле1");
        assert_eq!(s.totals.len(), 1);
        assert_eq!(s.totals[0].expression.as_deref(), Some("Сумма(Сумма)"));
        assert_eq!(s.parameters.len(), 1);
        assert_eq!(s.parameters[0].name, "НачалоПериода");
        assert_eq!(s.variants.len(), 1);
        assert_eq!(s.variants[0].name, "Основной");
        assert_eq!(
            s.variants[0].presentation.as_deref(),
            Some("Основной вариант")
        );
        assert_eq!(s.templates_count, 1);
    }

    #[test]
    fn разбирает_объединение_с_вложенными_наборами() {
        let xml = r#"<DataCompositionSchema xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <dataSets xsi:type="DataSetUnion">
    <name>Объединение</name>
    <items xsi:type="DataSetQuery">
      <name>Часть1</name>
      <query>ВЫБРАТЬ * ИЗ Справочник.Организации</query>
    </items>
    <items xsi:type="DataSetQuery">
      <name>Часть2</name>
      <query>ВЫБРАТЬ * ИЗ Справочник.Контрагенты</query>
    </items>
  </dataSets>
</DataCompositionSchema>"#;
        let s = parse_dcs(xml).unwrap();
        assert_eq!(s.data_sets.len(), 1);
        assert_eq!(s.data_sets[0].kind, "union");
        assert_eq!(s.data_sets[0].items.len(), 2);
        assert_eq!(s.data_sets[0].items[0].name, "Часть1");
        assert_eq!(
            s.data_sets[0].items[1].query.as_deref(),
            Some("ВЫБРАТЬ * ИЗ Справочник.Контрагенты")
        );
    }

    #[test]
    fn схема_без_наборов() {
        let xml = r#"<DataCompositionSchema>
  <settingsVariants>
    <name>Вариант</name>
  </settingsVariants>
</DataCompositionSchema>"#;
        let s = parse_dcs(xml).unwrap();
        assert!(s.data_sets.is_empty());
        assert_eq!(s.variants.len(), 1);
    }

    #[test]
    fn незнакомые_теги_между_известными_пропускаются() {
        let xml = r#"<DataCompositionSchema>
  <какойтоНезнакомыйТег><внутри><ещё>текст</ещё></внутри></какойтоНезнакомыйТег>
  <dataSets><name>Н</name></dataSets>
  <что-тоДругое/>
  <parameters><name>П</name></parameters>
</DataCompositionSchema>"#;
        let s = parse_dcs(xml).unwrap();
        assert_eq!(s.data_sets.len(), 1);
        assert_eq!(s.data_sets[0].name, "Н");
        assert_eq!(s.parameters.len(), 1);
        assert_eq!(s.parameters[0].name, "П");
    }

    #[test]
    fn заголовок_без_ru_берётся_первый() {
        let xml = r#"<DataCompositionSchema>
  <title><v8:item xmlns:v8="http://v8.1c.ru/8.1/data/core">
    <v8:lang>en</v8:lang><v8:content>Report</v8:content></v8:item></title>
  <dataSets><name>Н</name></dataSets>
</DataCompositionSchema>"#;
        let s = parse_dcs(xml).unwrap();
        assert_eq!(s.title.as_deref(), Some("Report"));
    }

    #[test]
    fn не_схема_компоновки_на_корне() {
        let xml = r#"<MetaDataObject><Template><Properties/></Template></MetaDataObject>"#;
        let s = parse_dcs(xml).unwrap();
        assert_eq!(s, DcsSchema::default());
    }

    #[test]
    fn битый_xml_даёт_ошибку() {
        assert!(parse_dcs("<DataCompositionSchema><dataSets>").is_err());
    }

    #[test]
    fn is_dcs_content_отличает_схему_от_табличного_макета() {
        assert!(is_dcs_content(FULL));
        let mxl = r#"<?xml version="1.0"?><document xmlns="http://v8.1c.ru/8.2/data/spreadsheet">
  <rowsItem/><format/></document>"#;
        assert!(!is_dcs_content(mxl));
        assert!(!is_dcs_content(""));
    }
}
