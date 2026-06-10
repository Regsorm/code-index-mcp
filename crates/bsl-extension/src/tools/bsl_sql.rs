// MCP-tool `bsl_sql` — произвольный read-only SELECT по таблицам BSL-индекса.
//
// «Инструмент инструментов»: один tool закрывает весь длинный хвост запросов
// по метаданным 1С и графам, для которых нет (и не нужно) отдельного named-tool.
// Аналог `rag_query` из rag-query, но по локальному per-repo `index.db`.
// Модель сама контролирует объём вывода через список SELECT-колонок и LIMIT —
// ближайший безопасный аналог `print()`-подхода rlm без Python-песочницы.
//
// Гарантии безопасности (трёхслойная защита):
//   1. Соединение открыто read-only (SQLITE_READONLY на любую запись).
//   2. Перед выполнением — `Statement::readonly()`: отклоняем всё, что не
//      является чистым read-only запросом (ловит `WITH ... DELETE`, у которого
//      префикс SELECT/WITH, но семантика — запись).
//   3. Префикс-guard: запрос обязан начинаться с SELECT или WITH (после
//      пропуска ведущих SQL-комментариев). Быстрый понятный отказ до prepare.
// Плюс ограничители ресурсов: жёсткий row-cap (limit) и interrupt-таймаут
// (sqlite3_interrupt из отдельной задачи) против runaway-запросов.
//
// ВАЖНО про колонку `repo`: каждый репозиторий — это ОТДЕЛЬНЫЙ файл `index.db`,
// поэтому BSL-таблицы (metadata_objects, data_links, proc_call_graph, ...)
// хранят `repo` всегда равным строке 'default'. Фильтровать по `repo` НЕ нужно
// и НЕ следует (`WHERE repo='ut'` вернёт пусто). Маршрутизация по alias делается
// MCP-слоем через параметр `repo` самого tool-call, а не SQL-фильтром.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use code_index_core::extension::{IndexTool, ToolContext};
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{params_from_iter, ErrorCode};
use serde_json::{json, Value};

/// Таймаут одного запроса. По истечении вызывается sqlite3_interrupt —
/// текущий/следующий шаг возвращает SQLITE_INTERRUPT, запрос обрывается.
const QUERY_TIMEOUT_SECS: u64 = 8;
/// Лимит строк по умолчанию, если клиент не передал `limit`.
const DEFAULT_LIMIT: u64 = 500;
/// Жёсткий потолок строк (защита от выгрузки гигантских таблиц в контекст).
const MAX_LIMIT: u64 = 5000;

pub struct BslSqlTool;

impl IndexTool for BslSqlTool {
    fn name(&self) -> &str {
        "bsl_sql"
    }

    fn description(&self) -> &str {
        "Произвольный read-only SQL (SELECT/WITH) по таблицам BSL-индекса репо 1С. \
         Один tool на весь длинный хвост запросов по метаданным и графам, где нет \
         отдельного named-tool: фильтры, join'ы, агрегации, выборка по колонкам. \
         Только SELECT/WITH — запись/PRAGMA/ATTACH отклоняются (соединение read-only \
         + проверка Statement::readonly()). \
         Параметры: repo (alias репо), sql (текст запроса), limit (потолок строк, \
         default 500, max 5000), params (опц. массив скаляров для ?1,?2,…). \
         ВАЖНО: каждый репо — отдельная БД, колонка repo во всех BSL-таблицах всегда \
         'default' — фильтровать по repo НЕ нужно. \
         Ключевые таблицы: metadata_objects(full_name, meta_type, name, synonym, \
         attributes_json), metadata_forms(owner_full_name, form_name, handlers_json), \
         metadata_modules(full_name, object_name, module_type, object_id, property_id, \
         config_version, code_path, extension_name), event_subscriptions(name, event, \
         handler_module, handler_proc, sources_json), proc_call_graph(caller_proc_key, \
         callee_proc_name, callee_proc_key, call_type), data_links(from_object, from_path, \
         to_object, link_kind, is_composite, is_universal), role_rights(role_name, object_name, right_name), \
         metadata_code_usages(object_ref, object_ref_key, member_path, usage_kind, file_path, line; \
         фильтровать по точному object_ref='Document.X' — SQLite lower() НЕ лоуэркейсит кириллицу, \
         object_ref_key уже в нижнем регистре для поиска из приложения), procedure_enrichment(proc_key, \
         terms, signature), direct_edge_files(caller, callee, source_file). \
         link_kind в data_links: объектные attr/tabular_attr/register_dim/recorder/owner; \
         конфиг-уровень subsystem_content/exchange_plan_content/defined_type_content/\
         functional_option_location (from_object соответственно Subsystem.X/ExchangePlan.X/\
         DefinedType.X/FunctionalOption.X). Core-таблицы \
         (без колонки repo): files(path, language, lines_total, mtime, file_size), \
         functions(file_id, name, qualified_name, line_start, line_end, args, return_type, \
         body, override_type, override_target), classes, imports, calls, variables. \
         Схему можно интроспектировать: SELECT name, sql FROM sqlite_master WHERE type='table'. \
         Пример (перехваты расширений): SELECT f.name, f.override_type, f.override_target, \
         fl.path FROM functions f JOIN files fl ON fl.id=f.file_id WHERE f.override_type \
         IS NOT NULL LIMIT 100. Blob-колонки (zstd-контент) отдаются как {_blob_bytes: N}, \
         текст брать через get_function/grep_body/read_file. \n         Формат результата: {columns:[имена], rows:[[значения по порядку columns], ...], row_count, truncated, limit}; \n         rows COLUMNAR — массивы значений по позициям columns, имена колонок не дублируются (экономия контекста). \n         For BSL/1C repositories only."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo": { "type": "string", "description": "Алиас репозитория" },
                "sql": {
                    "type": "string",
                    "description": "Read-only SQL: должен начинаться с SELECT или WITH. Фильтр по колонке repo не нужен (в каждой БД она всегда 'default')."
                },
                "limit": {
                    "type": "integer",
                    "description": "Потолок строк в ответе (default 500, max 5000). Лишние строки обрезаются с truncated=true.",
                    "minimum": 1
                },
                "params": {
                    "type": "array",
                    "description": "Опциональные позиционные параметры для ?1, ?2, … Только скаляры: null/bool/number/string.",
                    "items": {}
                }
            },
            "required": ["repo", "sql"]
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
            // ── Параметры ─────────────────────────────────────────────────
            let sql_raw = match args.get("sql").and_then(|v| v.as_str()) {
                Some(s) => s,
                None => {
                    return crate::tools::wrap_error(json!({
                        "error": "missing required parameter 'sql' (string)"
                    }));
                }
            };
            let sql = sql_raw.trim();

            // Префикс-guard: только SELECT/WITH (после пропуска ведущих комментариев).
            if !starts_with_select_or_with(sql) {
                return crate::tools::wrap_error(json!({
                    "error": "only read-only SELECT/WITH queries are allowed (after leading comments)"
                }));
            }

            let limit = args
                .get("limit")
                .and_then(|v| v.as_u64())
                .map(|n| n.clamp(1, MAX_LIMIT))
                .unwrap_or(DEFAULT_LIMIT);

            // Опциональные позиционные параметры (?1, ?2, …).
            let bound = match parse_params(args.get("params")) {
                Ok(b) => b,
                Err(msg) => {
                    return crate::tools::wrap_error(json!({ "error": msg }));
                }
            };

            // ── Выполнение ────────────────────────────────────────────────
            let storage = match ctx.storage.get().await {
                Ok(s) => s,
                Err(e) => {
                    return crate::tools::wrap_error(serde_json::json!({
                        "error": format!("storage pool: {}", e)
                    }));
                }
            };
            let conn = storage.conn();

            let mut stmt = match conn.prepare(sql) {
                Ok(s) => s,
                Err(e) => {
                    return crate::tools::wrap_error(json!({
                        "error": format!("SQL prepare error: {}", e)
                    }));
                }
            };

            // Авторитетный guard: запрос не должен ничего менять.
            if !stmt.readonly() {
                return crate::tools::wrap_error(json!({
                    "error": "statement is not read-only — only SELECT/WITH queries are allowed"
                }));
            }

            // interrupt-таймаут: handle живёт в отдельной задаче, по истечении
            // дёргает sqlite3_interrupt. После сбора строк задачу гасим.
            let handle = conn.get_interrupt_handle();
            let timer = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(QUERY_TIMEOUT_SECS)).await;
                handle.interrupt();
            });

            let result = collect_rows(&mut stmt, bound, limit);
            timer.abort();

            match result {
                Ok((columns, rows, truncated)) => {
                    let row_count = rows.len();
                    crate::tools::wrap_with_meta(
                        json!({
                            "columns": columns,
                            "rows": rows,
                            "row_count": row_count,
                            "truncated": truncated,
                            "limit": limit,
                        }),
                        Vec::new(),
                    )
                }
                Err(e) => {
                    let interrupted = matches!(
                        &e,
                        rusqlite::Error::SqliteFailure(err, _)
                            if err.code == ErrorCode::OperationInterrupted
                    );
                    let msg = if interrupted {
                        format!("query timed out after {}s and was interrupted", QUERY_TIMEOUT_SECS)
                    } else {
                        format!("SQL execution error: {}", e)
                    };
                    crate::tools::wrap_error(json!({
                        "error": msg,
                        "interrupted": interrupted,
                    }))
                }
            }
        })
    }
}

/// Проверить, что запрос начинается с `SELECT` или `WITH` (case-insensitive),
/// пропустив ведущие SQL-комментарии (`-- …` до конца строки и `/* … */`).
fn starts_with_select_or_with(sql: &str) -> bool {
    let rest = skip_leading_comments(sql);
    let upper = rest.trim_start();
    let head: String = upper.chars().take(6).collect::<String>().to_ascii_uppercase();
    head.starts_with("SELECT") || head.starts_with("WITH ") || head.starts_with("WITH\t")
        || head.starts_with("WITH\n") || head == "WITH" || head.starts_with("WITH(")
}

/// Срезать ведущие пробелы и SQL-комментарии, вернуть остаток.
fn skip_leading_comments(input: &str) -> &str {
    let mut s = input.trim_start();
    loop {
        if let Some(rest) = s.strip_prefix("--") {
            // Строковый комментарий до конца строки.
            match rest.find('\n') {
                Some(nl) => s = rest[nl + 1..].trim_start(),
                None => return "", // весь хвост — комментарий
            }
        } else if let Some(rest) = s.strip_prefix("/*") {
            // Блочный комментарий до `*/`.
            match rest.find("*/") {
                Some(end) => s = rest[end + 2..].trim_start(),
                None => return "", // незакрытый блок
            }
        } else {
            return s;
        }
    }
}

/// Разобрать опциональный массив `params` в позиционные SQL-значения.
/// Допустимы только скаляры (null/bool/number/string).
fn parse_params(v: Option<&Value>) -> Result<Vec<SqlValue>, String> {
    let Some(v) = v else { return Ok(Vec::new()) };
    if v.is_null() {
        return Ok(Vec::new());
    }
    let arr = v
        .as_array()
        .ok_or_else(|| "'params' must be an array of scalars".to_string())?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, item) in arr.iter().enumerate() {
        let sv = match item {
            Value::Null => SqlValue::Null,
            Value::Bool(b) => SqlValue::Integer(*b as i64),
            Value::Number(n) => {
                if let Some(int) = n.as_i64() {
                    SqlValue::Integer(int)
                } else if let Some(f) = n.as_f64() {
                    SqlValue::Real(f)
                } else {
                    return Err(format!("params[{}]: unsupported numeric value", i));
                }
            }
            Value::String(s) => SqlValue::Text(s.clone()),
            _ => {
                return Err(format!(
                    "params[{}]: only scalars allowed (null/bool/number/string)",
                    i
                ))
            }
        };
        out.push(sv);
    }
    Ok(out)
}

/// Выполнить prepared-запрос и собрать до `limit` строк в COLUMNAR-формате:
/// каждая строка — массив значений `[v0, v1, …]` в порядке `columns` (имена
/// колонок НЕ дублируются в каждой строке — экономия контекста на широких
/// результатах). Возвращает (имена колонок, строки-массивы, truncated).
fn collect_rows(
    stmt: &mut rusqlite::Statement<'_>,
    bound: Vec<SqlValue>,
    limit: u64,
) -> rusqlite::Result<(Vec<String>, Vec<Value>, bool)> {
    let columns: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let col_count = columns.len();

    let mut rows = stmt.query(params_from_iter(bound.iter()))?;
    let mut out: Vec<Value> = Vec::new();
    let mut truncated = false;

    while let Some(row) = rows.next()? {
        if out.len() as u64 >= limit {
            // Есть ещё хотя бы одна строка сверх лимита — отмечаем обрезку.
            truncated = true;
            break;
        }
        let mut arr = Vec::with_capacity(col_count);
        for i in 0..col_count {
            arr.push(valueref_to_json(row.get_ref(i)?));
        }
        out.push(Value::Array(arr));
    }

    Ok((columns, out, truncated))
}

/// Перевести значение ячейки SQLite в JSON. Blob не выгружаем в контекст
/// (это zstd-контент) — отдаём маркер длины.
fn valueref_to_json(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => json!(i),
        ValueRef::Real(f) => serde_json::Number::from_f64(f)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => json!({ "_blob_bytes": b.len() }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn mem_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        for ddl in crate::schema::SCHEMA_EXTENSIONS {
            conn.execute_batch(ddl).unwrap();
        }
        conn.execute(
            "INSERT INTO metadata_objects (repo, full_name, meta_type, name, synonym, attributes_json) \
             VALUES ('default', 'Catalog.Контрагенты', 'Catalog', 'Контрагенты', 'Контрагенты', '[]')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO metadata_objects (repo, full_name, meta_type, name, synonym, attributes_json) \
             VALUES ('default', 'Document.Реализация', 'Document', 'Реализация', 'Реализация', '[]')",
            [],
        )
        .unwrap();
        conn
    }

    #[test]
    fn prefix_guard_accepts_select_and_with() {
        assert!(starts_with_select_or_with("SELECT 1"));
        assert!(starts_with_select_or_with("  select * from x"));
        assert!(starts_with_select_or_with("WITH cte AS (SELECT 1) SELECT * FROM cte"));
        assert!(starts_with_select_or_with("with(1)")); // редкий, но валидный синтаксис
    }

    #[test]
    fn prefix_guard_skips_leading_comments() {
        assert!(starts_with_select_or_with("-- комментарий\nSELECT 1"));
        assert!(starts_with_select_or_with("/* блок */ SELECT 1"));
        assert!(starts_with_select_or_with("/* a */ -- b\n  WITH cte AS (SELECT 1) SELECT 1"));
    }

    #[test]
    fn prefix_guard_rejects_writes() {
        assert!(!starts_with_select_or_with("DELETE FROM metadata_objects"));
        assert!(!starts_with_select_or_with("INSERT INTO x VALUES (1)"));
        assert!(!starts_with_select_or_with("PRAGMA table_info(files)"));
        assert!(!starts_with_select_or_with("DROP TABLE x"));
        assert!(!starts_with_select_or_with("UPDATE x SET y=1"));
    }

    #[test]
    fn readonly_check_blocks_with_delete() {
        // WITH … DELETE имеет префикс WITH, но НЕ read-only — ловит stmt.readonly().
        let conn = mem_db();
        let sql = "WITH c AS (SELECT id FROM metadata_objects) DELETE FROM metadata_objects WHERE id IN (SELECT id FROM c)";
        let stmt = conn.prepare(sql).unwrap();
        assert!(!stmt.readonly(), "WITH ... DELETE не должен считаться read-only");
    }

    #[test]
    fn collect_rows_returns_columnar_arrays() {
        let conn = mem_db();
        let mut stmt = conn
            .prepare("SELECT full_name, meta_type FROM metadata_objects ORDER BY full_name")
            .unwrap();
        assert!(stmt.readonly());
        let (cols, rows, truncated) = collect_rows(&mut stmt, Vec::new(), 100).unwrap();
        assert_eq!(cols, vec!["full_name".to_string(), "meta_type".to_string()]);
        assert_eq!(rows.len(), 2);
        assert!(!truncated);
        // COLUMNAR: строка — массив значений по позициям columns (без имён колонок).
        assert_eq!(rows[0][0], json!("Catalog.Контрагенты"));
        assert_eq!(rows[0][1], json!("Catalog"));
    }

    #[test]
    fn collect_rows_enforces_limit_and_sets_truncated() {
        let conn = mem_db();
        let mut stmt = conn.prepare("SELECT full_name FROM metadata_objects").unwrap();
        let (_, rows, truncated) = collect_rows(&mut stmt, Vec::new(), 1).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(truncated, "при лимите 1 и двух строках должно быть truncated=true");
    }

    #[test]
    fn collect_rows_binds_positional_params() {
        let conn = mem_db();
        let mut stmt = conn
            .prepare("SELECT full_name FROM metadata_objects WHERE meta_type = ?1")
            .unwrap();
        let (_, rows, _) =
            collect_rows(&mut stmt, vec![SqlValue::Text("Document".into())], 100).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], json!("Document.Реализация"));
    }

    #[test]
    fn parse_params_accepts_scalars_rejects_compound() {
        let ok = parse_params(Some(&json!([1, "a", true, null, 3.5]))).unwrap();
        assert_eq!(ok.len(), 5);
        assert!(parse_params(Some(&json!([[1, 2]]))).is_err());
        assert!(parse_params(Some(&json!([{"k": 1}]))).is_err());
        assert!(parse_params(None).unwrap().is_empty());
        assert!(parse_params(Some(&Value::Null)).unwrap().is_empty());
    }

    #[test]
    fn valueref_blob_returns_length_marker() {
        let v = valueref_to_json(ValueRef::Blob(&[1, 2, 3, 4]));
        assert_eq!(v, json!({ "_blob_bytes": 4 }));
    }
}
