// MCP-tool `get_dcs_schema` — макет «Схема компоновки данных» (СКД) 1С за один вызов.
//
// Отвечает на вопросы «что считает отчёт», «какие поля отдаёт набор», «что
// попадает в запрос и какие объекты он читает», «какие параметры/варианты есть
// у схемы». Источник — таблицы `dcs_schemas` / `dcs_datasets`, которые
// заполняет `index_extras::index_dcs_schemas`, разобрав файл содержимого макета
// (`Templates/<Имя>/Ext/Template.xml` формата Конфигуратора и
// `Templates/<Имя>/Template.dcs` выгрузки 1C:EDT).
//
// Зачем отдельный инструмент, а не `bsl_sql`: схему компоновки агент иначе
// читает по кускам (файл на десятки килобайт, десятки секций, вложенные
// коллекции), а именованный инструмент под конкретный вопрос модель берёт
// уверенно. Произвольный SQL при этом остаётся — `dcs_schemas`/`dcs_datasets`
// перечислены в описании `bsl_sql`.
//
// Вход — имя макета (`Report.Отчет.Template.Схема`), имя владельца
// (`Report.Отчет`) либо имя общего макета (`CommonTemplate.Схема`). По
// владельцу отдаются ВСЕ его схемы: «основную» инструмент не помечает —
// признака `MainDataCompositionSchema` в паспортах макетов сегодня нет, и
// выдумывать его нельзя.

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;

use code_index_core::extension::{IndexTool, ToolContext};
use code_index_core::mcp::cap;
use rusqlite::params;
use serde_json::{json, Value};

/// Repo-key оффлайн-индексации: каждый репозиторий — отдельная БД, поэтому во
/// всех BSL-таблицах `repo` равно 'default' (как у соседних инструментов).
const REPO: &str = "default";

pub struct GetDcsSchemaTool;

impl IndexTool for GetDcsSchemaTool {
    fn name(&self) -> &str {
        "get_dcs_schema"
    }

    fn description(&self) -> &str {
        "Макет «Схема компоновки данных» (СКД) за ОДИН вызов. Передай имя макета \
         ('Report.Отчет.Template.Схема'), имя владельца ('Report.Отчет') или имя \
         общего макета ('CommonTemplate.Схема') и получи разобранную схему: наборы \
         данных с полями (имя, представление, тип, роль, папка) и текстами запросов, \
         вложенные наборы объединения, связи наборов, вычисляемые поля, итоги, \
         параметры, варианты настроек и счётчики. Секция 'reads' — объекты, которые \
         схема читает в текстах запросов (рёбра link_kind='dcs_query'). Данные уже \
         разобраны в индексе (таблицы dcs_schemas/dcs_datasets) — читать \
         Template.xml по кускам НЕ нужно, это тот же ответ ценой десятков вызовов. \
         РАЗМЕР ОТВЕТА: опознание (template_full_name/owner/content_file) и счётчики \
         сохраняются всегда; при нехватке бюджета сначала опускаются тексты запросов \
         ('query_length' + 'query_truncated'), затем поля наборов ('fields_omitted' + \
         'fields_count'), затем секции целиком ('<секция>_omitted' + '<секция>_count'). \
         Нужен полный текст — повтори вызов с большим 'max_response_bytes'; \
         'include_query=false' отдаёт только длины запросов. For BSL/1C repositories only."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo": {
                    "type": "string",
                    "description": "Алиас репозитория (из --path alias=dir или daemon.toml)"
                },
                "full_name": {
                    "type": "string",
                    "description": "Имя макета ('Report.Отчет.Template.Схема'), имя владельца ('Report.Отчет') или общий макет ('CommonTemplate.Схема'). Синонимы ключа: object, name, template."
                },
                "sections": {
                    "type": "array",
                    "items": {
                        "type": "string",
                        "enum": ["data_sets", "fields", "links", "calculated_fields", "totals", "parameters", "variants"]
                    },
                    "description": "Узкая выборка секций (как sections у get_object_structure): вернуть ТОЛЬКО указанные ключи. Без параметра — все секции. 'fields' (поля лежат внутри наборов) трактуется как 'data_sets'. Рычаг экономии контекста: ['parameters'] — только параметры, ['links'] — только связи наборов."
                },
                "include_query": {
                    "type": "boolean",
                    "description": "Отдавать ли тексты запросов наборов (по умолчанию true). false → у каждого набора только 'query_length' (число символов): дешёвый способ узнать объём запросов, не забирая их текст."
                },
                "max_response_bytes": {
                    "type": "integer",
                    "description": "Бюджет размера ЭТОГО ответа в байтах — перекрывает серверный [cap].max_response_bytes на один вызов. Применяй, когда ответ пришёл ужатым и нужны полные тексты запросов/поля: повтори тот же вызов с большим значением. Запрос сверх серверного потолка не отклоняется, а зажимается; фактически применённое значение возвращается полем response_budget_applied. Ноль (снять ограничение) на вызов не разрешён."
                }
            },
            "required": ["repo"]
        })
    }

    fn applicable_languages(&self) -> Option<&'static [&'static str]> {
        Some(&["bsl"])
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        ctx: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = Value> + Send + 'a>> {
        Box::pin(async move {
            let Some(full_name) = full_name_arg(&args) else {
                return crate::tools::wrap_error(json!({
                    "error": "укажите 'full_name' — имя макета ('Report.Отчет.Template.Схема'), \
                              имя владельца ('Report.Отчет') или общего макета ('CommonTemplate.Схема')"
                }));
            };
            let sections: Option<Vec<String>> =
                args.get("sections").and_then(|v| v.as_array()).map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                });
            let include_query = args
                .get("include_query")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let budget = cap::resolve_request_budget(
                args.get("max_response_bytes")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as usize),
            );

            let storage = match ctx.storage.get().await {
                Ok(s) => s,
                Err(e) => {
                    return crate::tools::wrap_error(json!({
                        "error": format!("storage pool: {}", e)
                    }));
                }
            };
            let conn = storage.conn();

            // Имя приводим к записи индекса: SQLite не сворачивает регистр
            // кириллицы, иначе 'report.отчет' дал бы пустоту.
            let name = crate::tools::canonical_object_name(conn, &full_name);
            let templates = resolve_templates(conn, &name);
            if templates.is_empty() {
                return crate::tools::wrap_error(json!({
                    "error": format!("Схема компоновки не найдена: {}", full_name),
                    "hint": not_found_hint(conn, &name),
                }));
            }

            // Форма ответа одна и по макету, и по владельцу: список схем и их
            // число. Модель тогда не гадает, где лежат наборы, — всегда в
            // `schemas[i]`, даже когда схема одна.
            let schemas: Vec<Value> = templates
                .iter()
                .map(|t| schema_payload(conn, t, sections.as_deref(), include_query))
                .collect();
            let payload = json!({
                "requested": name,
                "schemas_count": templates.len(),
                "schemas": schemas,
            });

            // Урезание по бюджету: сначала тексты запросов, затем поля наборов,
            // затем секции целиком. Опознание и счётчики не трогаются никогда.
            let full_bytes = payload_size(&payload);
            let (payload, shrink) = shrink(payload, budget.applied);
            let mut payload = payload;
            if shrink.any() {
                if let Some(obj) = payload.as_object_mut() {
                    obj.insert(
                        "hint".to_string(),
                        json!(shrink_hint(full_bytes, budget.applied)),
                    );
                }
            }
            let mut out =
                crate::tools::wrap_with_meta_structural(payload, Vec::new(), shrink.any());
            if let Some(obj) = out.as_object_mut() {
                if shrink.any() || budget.requested.is_some() {
                    obj.insert("response_budget_applied".to_string(), json!(budget.applied));
                }
            }
            out
        })
    }
}

/// Полное имя из синонимов ключа (имя ключа не значимо — параметр один).
fn full_name_arg(args: &Value) -> Option<String> {
    ["full_name", "object", "name", "template"]
        .iter()
        .find_map(|k| args.get(*k).and_then(|v| v.as_str()))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// Полные имена схем: сперва точное совпадение по имени макета, иначе — все
/// схемы владельца (сортировка по имени; «основная» НЕ помечается и порядок не
/// меняется — признака `MainDataCompositionSchema` в паспортах макетов нет).
fn resolve_templates(conn: &rusqlite::Connection, full_name: &str) -> Vec<String> {
    let exact: Option<String> = conn
        .query_row(
            "SELECT template_full_name FROM dcs_schemas \
             WHERE repo = ?1 AND template_full_name = ?2",
            params![REPO, full_name],
            |r| r.get(0),
        )
        .ok();
    if let Some(v) = exact {
        return vec![v];
    }
    let mut stmt = match conn.prepare(
        "SELECT template_full_name FROM dcs_schemas \
         WHERE repo = ?1 AND owner_full_name = ?2 ORDER BY template_full_name",
    ) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("get_dcs_schema: {}", e);
            return Vec::new();
        }
    };
    // Выборка забирается отдельным оператором: `MappedRows` заимствует `stmt`,
    // и «хвостовым» выражением блока она бы пережила его.
    let mut out: Vec<String> = Vec::new();
    match stmt.query_map(params![REPO, full_name], |r| r.get::<_, String>(0)) {
        Ok(rows) => out.extend(rows.flatten()),
        Err(e) => tracing::warn!("get_dcs_schema: {}", e),
    }
    out
}

/// Разобранная схема одного макета вместе с её наборами и прочитанными
/// объектами.
fn schema_payload(
    conn: &rusqlite::Connection,
    template_full_name: &str,
    sections: Option<&[String]>,
    include_query: bool,
) -> Value {
    let row = conn.query_row(
        "SELECT owner_full_name, title, content_file, links_json, calculated_fields_json, \
                totals_json, parameters_json, variants_json, templates_count, data_sets_count, \
                fields_count, links_count, calculated_fields_count, totals_count, \
                parameters_count, variants_count \
         FROM dcs_schemas WHERE repo = ?1 AND template_full_name = ?2",
        params![REPO, template_full_name],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, i64>(8)?,
                r.get::<_, i64>(9)?,
                r.get::<_, i64>(10)?,
                r.get::<_, i64>(11)?,
                r.get::<_, i64>(12)?,
                r.get::<_, i64>(13)?,
                r.get::<_, i64>(14)?,
                r.get::<_, i64>(15)?,
            ))
        },
    );
    let (
        owner,
        title,
        content_file,
        links_json,
        calculated_json,
        totals_json,
        parameters_json,
        variants_json,
        templates_count,
        data_sets_count,
        fields_count,
        links_count,
        calculated_count,
        totals_count,
        parameters_count,
        variants_count,
    ) = match row {
        Ok(v) => v,
        Err(e) => {
            return json!({
                "template_full_name": template_full_name,
                "error": format!("database error: {}", e)
            });
        }
    };

    let mut map = serde_json::Map::new();
    map.insert("template_full_name".into(), json!(template_full_name));
    map.insert("owner".into(), json!(owner));
    if let Some(t) = &title {
        map.insert("title".into(), json!(t));
    }
    if let Some(c) = &content_file {
        map.insert("content_file".into(), json!(c));
    }
    map.insert("templates_count".into(), json!(templates_count));
    map.insert("data_sets_count".into(), json!(data_sets_count));
    map.insert("fields_count".into(), json!(fields_count));
    map.insert("links_count".into(), json!(links_count));
    map.insert("calculated_fields_count".into(), json!(calculated_count));
    map.insert("totals_count".into(), json!(totals_count));
    map.insert("parameters_count".into(), json!(parameters_count));
    map.insert("variants_count".into(), json!(variants_count));

    // Объекты, которые схема читает в текстах запросов (рёбра dcs_query).
    let (_, template_name) = crate::index_extras::split_template_full_name(template_full_name);
    let read_objects = reads(conn, &owner, &template_name);
    if !read_objects.is_empty() {
        map.insert("reads".into(), json!(read_objects));
    }

    if want_data_sets(sections) {
        // `sections=['data_sets']` без 'fields' — наборы без полей (у каждого
        // остаётся `fields_count`); без параметра `sections` поля всегда есть.
        map.insert(
            "data_sets".into(),
            load_datasets(
                conn,
                template_full_name,
                include_query,
                want_key(sections, "fields"),
            ),
        );
    }
    if want_key(sections, "links") {
        map.insert("links".into(), json_from_column(links_json.as_deref()));
    }
    if want_key(sections, "calculated_fields") {
        map.insert(
            "calculated_fields".into(),
            json_from_column(calculated_json.as_deref()),
        );
    }
    if want_key(sections, "totals") {
        map.insert("totals".into(), json_from_column(totals_json.as_deref()));
    }
    if want_key(sections, "parameters") {
        map.insert(
            "parameters".into(),
            json_from_column(parameters_json.as_deref()),
        );
    }
    if want_key(sections, "variants") {
        map.insert(
            "variants".into(),
            json_from_column(variants_json.as_deref()),
        );
    }

    Value::Object(map)
}

/// Нужна ли секция при заданном `sections` (без параметра — нужны все).
fn want_key(sections: Option<&[String]>, key: &str) -> bool {
    match sections {
        None => true,
        Some(s) => s.iter().any(|x| x == key),
    }
}

/// Наборы данных нужны, если запрошены сами (`data_sets`) ИЛИ их поля
/// (`fields`): поля лежат ВНУТРИ наборов, отдельной секции у них нет.
fn want_data_sets(sections: Option<&[String]>) -> bool {
    match sections {
        None => true,
        Some(s) => s.iter().any(|x| x == "data_sets" || x == "fields"),
    }
}

/// JSON-секция из колонки `*_json`; пусто/битый JSON → пустой массив.
fn json_from_column(raw: Option<&str>) -> Value {
    match raw {
        Some(s) if !s.trim().is_empty() => serde_json::from_str(s).unwrap_or_else(|_| json!([])),
        _ => json!([]),
    }
}

/// Строка одного набора данных до сборки дерева.
struct DsRow {
    name: String,
    kind: String,
    data_source: Option<String>,
    object_name: Option<String>,
    query: Option<String>,
    fields: Value,
    parent: String,
}

/// Собрать наборы схемы в дерево: верхний уровень (`parent_set = ''`) и
/// вложенные наборы объединения под своим родителем. Порядок — по rowid, т.е.
/// по порядку в файле.
fn load_datasets(
    conn: &rusqlite::Connection,
    template_full_name: &str,
    include_query: bool,
    with_fields: bool,
) -> Value {
    let mut stmt = match conn.prepare(
        "SELECT data_set_name, kind, data_source, object_name, query_text, fields_json, parent_set \
         FROM dcs_datasets WHERE repo = ?1 AND template_full_name = ?2 ORDER BY rowid",
    ) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("get_dcs_schema: {}", e);
            return json!([]);
        }
    };
    let rows: Vec<DsRow> = match stmt.query_map(params![REPO, template_full_name], |r| {
        Ok(DsRow {
            name: r.get(0)?,
            kind: r.get(1)?,
            data_source: r.get(2)?,
            object_name: r.get(3)?,
            query: r.get(4)?,
            fields: json_from_column(r.get::<_, Option<String>>(5)?.as_deref()),
            parent: r.get(6)?,
        })
    }) {
        Ok(rows) => rows.flatten().collect(),
        Err(e) => {
            tracing::warn!("get_dcs_schema: {}", e);
            return json!([]);
        }
    };
    let mut used = vec![false; rows.len()];
    json!(build_sets(
        &rows,
        None,
        &mut used,
        include_query,
        with_fields
    ))
}

/// Рекурсивно собрать наборы под родителем `parent` (индекс строки; `None` —
/// верхний уровень). Родитель задан только именем, а вложенный набор
/// объединения может называться так же, как само объединение (типовой
/// `Report.Запасы`: объединение `НаборДанных3` содержит запрос `НаборДанных3`).
/// Поэтому каждая строка попадает в дерево ровно один раз (`used`), а дети
/// ищутся только после родителя по порядку файла — иначе рекурсия по
/// одноимённому набору бесконечна и роняет сервер переполнением стека.
fn build_sets(
    rows: &[DsRow],
    parent: Option<usize>,
    used: &mut [bool],
    include_query: bool,
    with_fields: bool,
) -> Vec<Value> {
    let (parent_name, start) = match parent {
        Some(p) => (rows[p].name.as_str(), p + 1),
        None => ("", 0),
    };
    // Разобрать детей этого уровня целиком до спуска вглубь, чтобы одноимённый
    // вложенный набор не забрал себе соседей.
    let children: Vec<usize> = (start..rows.len())
        .filter(|&i| !used[i] && rows[i].parent == parent_name)
        .collect();
    for &i in &children {
        used[i] = true;
    }
    let mut out = Vec::new();
    for i in children {
        let r = &rows[i];
        let mut m = serde_json::Map::new();
        m.insert("name".into(), json!(r.name.as_str()));
        m.insert("kind".into(), json!(r.kind.as_str()));
        if let Some(ds) = &r.data_source {
            m.insert("data_source".into(), json!(ds));
        }
        if let Some(o) = &r.object_name {
            m.insert("object_name".into(), json!(o));
        }
        match (&r.query, include_query) {
            (Some(q), true) => {
                m.insert("query".into(), json!(q));
            }
            // Без текста запроса — только его длина: объём видно, контекст не тратится.
            (Some(q), false) => {
                m.insert("query_length".into(), json!(q.chars().count()));
            }
            (None, _) => {}
        }
        if with_fields {
            m.insert("fields".into(), r.fields.clone());
        } else {
            let n = r.fields.as_array().map(|a| a.len()).unwrap_or(0);
            m.insert("fields_count".into(), json!(n));
        }
        let items = build_sets(rows, Some(i), used, include_query, with_fields);
        if !items.is_empty() {
            m.insert("items".into(), json!(items));
        }
        out.push(Value::Object(m));
    }
    out
}

/// Объекты, которые макет читает в текстах своих запросов: DISTINCT `to_object`
/// рёбер `link_kind='dcs_query'` владельца по путям этого макета
/// (`from_path` = `«<ИмяМакета>.<ИмяНабора>»`).
fn reads(conn: &rusqlite::Connection, owner: &str, template_name: &str) -> Vec<String> {
    let mut stmt = match conn.prepare(
        "SELECT from_path, to_object FROM data_links \
         WHERE repo = ?1 AND link_kind = 'dcs_query' AND from_object = ?2",
    ) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("get_dcs_schema: {}", e);
            return Vec::new();
        }
    };
    let prefix = format!("{}.", template_name);
    let mut out: BTreeSet<String> = BTreeSet::new();
    if let Ok(rows) = stmt.query_map(params![REPO, owner], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    }) {
        for (from_path, to_object) in rows.flatten() {
            if from_path == template_name || from_path.starts_with(&prefix) {
                out.insert(to_object);
            }
        }
    }
    out.into_iter().collect()
}

/// Подсказка к «не найдено». Три разных случая — три разных действия:
/// владелец без схем компоновки, общий макет и имя, которого нет вовсе.
fn not_found_hint(conn: &rusqlite::Connection, input: &str) -> String {
    let object_exists = conn
        .query_row(
            "SELECT 1 FROM metadata_objects WHERE repo = ?1 AND full_name = ?2",
            params![REPO, input],
            |_| Ok(()),
        )
        .is_ok();
    if object_exists {
        return format!(
            "Объект '{}' есть в перечне, но схем компоновки у него в индексе нет. \
             Перечислите его макеты запросом bsl_sql \
             \"SELECT full_name FROM metadata_objects WHERE meta_type='Template' \
             AND full_name LIKE '{}.Template.%'\" — схемой компоновки будет тот, у которого \
             в attributes_json template_type='DataCompositionSchema'.",
            input, input
        );
    }
    if input.starts_with("CommonTemplate.") {
        return format!(
            "Общий макет '{}' не найден среди схем компоновки. Проверьте имя либо \
             посмотрите, что вообще попало в индекс — bsl_sql \"SELECT template_full_name, \
             content_file FROM dcs_schemas\" (в таблицу идут только макеты вида \
             «Схема компоновки данных»; у общего макета владельцем записан он сам).",
            input
        );
    }
    format!(
        "Ни макета, ни объекта-владельца '{}' в индексе нет. Уточните имя: \
         'Report.Отчет.Template.Схема' — макет, 'Report.Отчет' — владелец. \
         Перечень владельцев отдаёт bsl_sql \"SELECT DISTINCT owner_full_name FROM dcs_schemas\".",
        input
    )
}

/// Что именно пришлось урезать, чтобы уложиться в бюджет.
#[derive(Default)]
struct ShrinkState {
    queries_truncated: bool,
    fields_omitted: bool,
    sections_omitted: bool,
}

impl ShrinkState {
    fn any(&self) -> bool {
        self.queries_truncated || self.fields_omitted || self.sections_omitted
    }
}

fn payload_size(value: &Value) -> usize {
    serde_json::to_string(value).map(|s| s.len()).unwrap_or(0)
}

/// Ужать ответ под `budget` байт. Порядок ступеней задан: (1) тексты запросов
/// до `query_length` + `query_truncated`; (2) `fields` внутри наборов →
/// `fields_omitted` + `fields_count`; (3) секции `calculated_fields` / `totals`
/// / `parameters` / `variants` целиком → `<секция>_omitted` + `<секция>_count`.
/// Опознание (`template_full_name`, `owner`, `content_file`) и счётчики не
/// урезаются никогда. `budget == 0` → no-op.
fn shrink(mut value: Value, budget: usize) -> (Value, ShrinkState) {
    let mut state = ShrinkState::default();
    if budget == 0 || payload_size(&value) <= budget {
        return (value, state);
    }
    state.queries_truncated = truncate_queries(&mut value);
    if payload_size(&value) <= budget {
        return (value, state);
    }
    state.fields_omitted = omit_fields(&mut value);
    if payload_size(&value) <= budget {
        return (value, state);
    }
    state.sections_omitted = omit_sections(&mut value);
    (value, state)
}

/// Ступень 1: у каждого объекта ключ `query` (строка) заменяется парой
/// `query_length` + `query_truncated`.
fn truncate_queries(value: &mut Value) -> bool {
    match value {
        Value::Object(map) => {
            let mut changed = false;
            if let Some(Value::String(q)) = map.remove("query") {
                map.insert("query_length".into(), json!(q.chars().count()));
                map.insert("query_truncated".into(), json!(true));
                changed = true;
            }
            for child in map.values_mut() {
                changed |= truncate_queries(child);
            }
            changed
        }
        Value::Array(arr) => {
            let mut changed = false;
            for e in arr.iter_mut() {
                changed |= truncate_queries(e);
            }
            changed
        }
        _ => false,
    }
}

/// Ступень 2: `fields` набора заменяется парой `fields_omitted` + `fields_count`.
fn omit_fields(value: &mut Value) -> bool {
    match value {
        Value::Object(map) => {
            let mut changed = false;
            if map.contains_key("fields") {
                let n = map
                    .get("fields")
                    .and_then(|f| f.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                map.remove("fields");
                map.insert("fields_count".into(), json!(n));
                map.insert("fields_omitted".into(), json!(true));
                changed = true;
            }
            for child in map.values_mut() {
                changed |= omit_fields(child);
            }
            changed
        }
        Value::Array(arr) => {
            let mut changed = false;
            for e in arr.iter_mut() {
                changed |= omit_fields(e);
            }
            changed
        }
        _ => false,
    }
}

/// Ступень 3: целые секции — с их счётчиком рядом.
const OMIT_SECTIONS: &[&str] = &["calculated_fields", "totals", "parameters", "variants"];

fn omit_sections(value: &mut Value) -> bool {
    match value {
        Value::Object(map) => {
            let mut changed = false;
            for section in OMIT_SECTIONS {
                if !map.contains_key(*section) {
                    continue;
                }
                let n = map
                    .get(*section)
                    .and_then(|x| x.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                map.remove(*section);
                map.insert(format!("{}_count", section), json!(n));
                map.insert(format!("{}_omitted", section), json!(true));
                changed = true;
            }
            for child in map.values_mut() {
                changed |= omit_sections(child);
            }
            changed
        }
        Value::Array(arr) => {
            let mut changed = false;
            for e in arr.iter_mut() {
                changed |= omit_sections(e);
            }
            changed
        }
        _ => false,
    }
}

/// Подсказка при ужатом ответе: называет ОБА числа (полный размер и бюджет) и
/// конкретную ручку `max_response_bytes=<с запасом>`. Общий совет без чисел
/// слабая модель не выполняет.
fn shrink_hint(full_bytes: usize, budget: usize) -> String {
    let kb = |b: usize| (b + 512) / 1024;
    let suggested = (full_bytes / 1000 + 2) * 1000;
    format!(
        "Ответ ужат под бюджет: полный ответ ≈{} КБ ({} байт) при бюджете {} КБ ({} байт). \
         Повторите тот же вызов с max_response_bytes={} — потолок сервера выше. Порядок \
         урезания: сначала тексты запросов (query_length + query_truncated), затем поля \
         наборов (fields_omitted + fields_count), затем секции целиком (<секция>_omitted + \
         <секция>_count). Опознание (template_full_name/owner/content_file) и счётчики не \
         урезаются никогда; узкую выборку даёт sections=['parameters'] и т.п.",
        kb(full_bytes),
        full_bytes,
        kb(budget),
        budget,
        suggested
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn имя_принимается_из_всех_синонимов_ключа() {
        for key in ["full_name", "object", "name", "template"] {
            let args = json!({ "repo": "ut", key: "Report.Отчет.Template.Схема" });
            assert_eq!(
                full_name_arg(&args).as_deref(),
                Some("Report.Отчет.Template.Схема"),
                "ключ {key}"
            );
        }
        assert_eq!(
            full_name_arg(&json!({ "repo": "ut", "object": "   " })),
            None
        );
        assert_eq!(full_name_arg(&json!({ "repo": "ut" })), None);
    }

    #[test]
    fn fields_трактуются_как_наборы() {
        assert!(want_data_sets(None));
        assert!(want_data_sets(Some(&["fields".to_string()])));
        assert!(want_data_sets(Some(&["data_sets".to_string()])));
        assert!(!want_data_sets(Some(&["parameters".to_string()])));
        assert!(want_key(Some(&["parameters".to_string()]), "parameters"));
        assert!(!want_key(Some(&["parameters".to_string()]), "links"));
    }

    #[test]
    fn урезание_идёт_по_порядку_и_бережёт_опознание() {
        let value = json!({
            "template_full_name": "Report.От.Template.С",
            "owner": "Report.От",
            "content_file": "Reports/От/Templates/С/Ext/Template.xml",
            "parameters_count": 1,
            "data_sets": [
                { "name": "Н", "query": "ОЧЕНЬ ДЛИННЫЙ ТЕКСТ ЗАПРОСА", "fields": [ {"name": "A"} ] }
            ],
            "parameters": [ { "name": "П" } ]
        });
        let (v, st) = shrink(value, 1);
        assert!(st.any());
        assert!(st.queries_truncated);
        assert!(st.fields_omitted);
        assert!(st.sections_omitted);
        // Опознание неприкосновенно.
        assert_eq!(
            v["template_full_name"].as_str(),
            Some("Report.От.Template.С")
        );
        assert_eq!(v["owner"].as_str(), Some("Report.От"));
        assert!(v["content_file"].is_string());
        // Ступени отработали:
        assert!(v["data_sets"][0]["query_length"].is_number());
        assert_eq!(v["data_sets"][0]["query_truncated"].as_bool(), Some(true));
        assert_eq!(v["data_sets"][0]["fields_omitted"].as_bool(), Some(true));
        assert_eq!(v["parameters_omitted"].as_bool(), Some(true));
        assert_eq!(v["parameters_count"].as_u64(), Some(1));
    }

    fn ds(name: &str, kind: &str, parent: &str) -> DsRow {
        DsRow {
            name: name.into(),
            kind: kind.into(),
            data_source: None,
            object_name: None,
            query: None,
            fields: json!([]),
            parent: parent.into(),
        }
    }

    #[test]
    fn вложенный_набор_с_именем_объединения_не_зацикливает() {
        // Порядок строк как в индексе для типового Report.Запасы.
        let rows = vec![
            ds("НаборДанных3", "union", ""),
            ds("НаборДанных1", "query", "НаборДанных3"),
            ds("НаборДанных3", "query", "НаборДанных3"),
            ds("НаборДанных2", "query", ""),
        ];
        let mut used = vec![false; rows.len()];
        let v = json!(build_sets(&rows, None, &mut used, true, true));
        assert_eq!(v.as_array().map(|a| a.len()), Some(2), "{v}");
        assert_eq!(v[0]["kind"].as_str(), Some("union"));
        let items = v[0]["items"].as_array().expect("items у объединения");
        assert_eq!(items.len(), 2, "{v}");
        assert_eq!(items[0]["name"].as_str(), Some("НаборДанных1"));
        assert_eq!(items[1]["name"].as_str(), Some("НаборДанных3"));
        assert_eq!(items[1]["kind"].as_str(), Some("query"));
        assert!(items[1].get("items").is_none(), "{v}");
        assert_eq!(v[1]["name"].as_str(), Some("НаборДанных2"));
        assert!(used.iter().all(|&u| u));
    }

    #[test]
    fn вложенное_объединение_собирает_своих_детей() {
        let rows = vec![
            ds("О", "union", ""),
            ds("Вн", "union", "О"),
            ds("А", "query", "Вн"),
            ds("Б", "query", "О"),
        ];
        let mut used = vec![false; rows.len()];
        let v = json!(build_sets(&rows, None, &mut used, true, true));
        assert_eq!(v[0]["items"][0]["name"].as_str(), Some("Вн"));
        assert_eq!(v[0]["items"][0]["items"][0]["name"].as_str(), Some("А"));
        assert_eq!(v[0]["items"][1]["name"].as_str(), Some("Б"));
    }

    #[test]
    fn бюджет_ноль_ничего_не_трогает() {
        let value = json!({ "data_sets": [ { "query": "X" } ] });
        let (v, st) = shrink(value, 0);
        assert!(!st.any());
        assert_eq!(v["data_sets"][0]["query"].as_str(), Some("X"));
    }

    #[test]
    fn подсказка_называет_оба_числа_и_ручку() {
        let h = shrink_hint(123456, 16384);
        assert!(h.contains("123456"), "{h}");
        assert!(h.contains("16384"), "{h}");
        assert!(h.contains("max_response_bytes=125000"), "{h}");
    }
}
