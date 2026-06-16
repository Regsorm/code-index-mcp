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
use rusqlite::{params, params_from_iter, ErrorCode};
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
         attributes_json), metadata_forms(owner_full_name, form_name, handlers_json; \
         ФОРМАТ: owner_full_name = '<PluralFolder>.<Name>' вида 'Documents.ЗаказКлиента' \
         — папка во множ. числе, как в metadata_modules.object_name), \
         metadata_modules(full_name, object_name, module_type, object_id, property_id, \
         config_version, code_path, extension_name; ФОРМАТ значений: object_name = \
         '<PluralFolder>.<Name>' вида 'Documents.ЗаказКлиента' (папка во множ. числе!), \
         full_name = object_name + '.<ModuleType>'), event_subscriptions(name, event, \
         handler_module, handler_proc, sources_json), proc_call_graph(caller_proc_key, \
         callee_proc_name, callee_proc_key, call_type), data_links(from_object, from_path, \
         to_object, link_kind, is_composite, is_universal), role_rights(role_name, object_name, right_name), \
         metadata_code_usages(object_ref, object_ref_key, member_path, usage_kind, file_path, line; \
         фильтровать по точному object_ref='Document.X' — SQLite lower() НЕ лоуэркейсит кириллицу, \
         object_ref_key уже в нижнем регистре для поиска из приложения), procedure_enrichment(proc_key, \
         terms, signature), direct_edge_files(caller, callee, source_file). \
         link_kind в data_links: объектные attr/tabular_attr/register_dim/recorder/owner \
         (owner: подчинённый справочник → владелец); \
         конфиг-уровень subsystem_content/exchange_plan_content/defined_type_content/\
         functional_option_location/functional_option_content (from_object соответственно \
         Subsystem.X/ExchangePlan.X/DefinedType.X/FunctionalOption.X; *_content у ФО — \
         состав опции). В attributes_json у реквизитов есть synonym/required (когда заданы), \
         у объектов — секции owners/value_types/properties/enum_synonyms/commands \
         (commands — команды объекта: [{name, synonym?}]). Core-таблицы \
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
            // 'Документ.X' → 'Document.X' в строковых литералах: типы в индексе
            // хранятся только по-английски, иначе литерал с русским префиксом не находит.
            let sql_norm = crate::code_usages::normalize_sql_object_refs(sql_raw.trim());
            let sql = sql_norm.as_str();

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
                    // Схема жёсткая и известна — обогащаем ошибку prepare
                    // фактическими колонками таблиц запроса + did_you_mean,
                    // чтобы агент исправился тем же ходом (находка бенча
                    // 2026-06-11: голое «no such column» стоит хода разведки).
                    return crate::tools::wrap_error(enrich_prepare_error(
                        conn,
                        sql,
                        &e.to_string(),
                    ));
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

            // Текстовые параметры пригодятся термовому fallback'у при пустой
            // выборке (bound уходит в collect_rows по значению).
            let text_params: Vec<String> = bound
                .iter()
                .filter_map(|v| match v {
                    SqlValue::Text(s) => Some(s.clone()),
                    _ => None,
                })
                .collect();

            let result = collect_rows(&mut stmt, bound, limit);
            timer.abort();

            match result {
                Ok((columns, rows, truncated)) => {
                    let row_count = rows.len();
                    let mut payload = json!({
                        "columns": columns,
                        "rows": rows,
                        "row_count": row_count,
                        "truncated": truncated,
                        "limit": limit,
                    });
                    // W5 (0.32): SQL `LIMIT N` в тексте запроса обрезает молча —
                    // `truncated` считается только от лимита инструмента. Если
                    // строк ровно N, предупреждаем: возможно, есть ещё.
                    if !truncated {
                        if let Some(n) = sql_limit_value(sql) {
                            if row_count as u64 == n {
                                payload["hint"] = json!(format!(
                                    "строк ровно LIMIT {} из текста запроса — выдача, \
                                     возможно, обрезана вашим SQL LIMIT (truncated отражает \
                                     только лимит инструмента); поднимите LIMIT в SQL или \
                                     передайте параметр limit",
                                    n
                                ));
                            }
                        }
                    }
                    // 0.33: пустая выборка — куда идти дальше. Если запрос искал
                    // ПРОЦЕДУРУ (таблицы functions/proc_call_graph/procedure_enrichment)
                    // и не нашёл — зовём в search_terms (модель устойчиво ходит в
                    // bsl_sql LIKE по имени, а обработчик живёт в общем модуле БСП под
                    // другим именем — точный SQL промахивается; находка бенча 11.06).
                    if row_count == 0 && payload.get("hint").is_none() {
                        // Эксперимент 12.06: модель не идёт в search_terms даже по
                        // hint'у (5 живых прогонов 11.06) — поэтому при пустой
                        // выборке по таблицам процедур термовый поиск выполняется
                        // автоматически здесь же, выдача — в terms_fallback.
                        // Без hint'а: имя поля и структура самодокументируются.
                        let fb = if searched_proc_tables(sql) {
                            terms_fallback_for_sql(conn, sql, &text_params)
                        } else {
                            None
                        };
                        match fb {
                            Some(fb) => payload["terms_fallback"] = fb,
                            None => payload["hint"] = json!(empty_result_hint(sql)),
                        }
                    }
                    crate::tools::wrap_with_meta(payload, Vec::new())
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

/// Расстояние Левенштейна (классическое DP) — для did_you_mean по именам
/// колонок/таблиц. Имена короткие (< 40 символов), квадратичная сложность
/// не существенна.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// W5: значение `LIMIT N` из ТЕКСТА запроса (последнее вхождение — внешний
/// LIMIT при подзапросах чаще всего последний). Формы `LIMIT N` и
/// `LIMIT M, N`/`LIMIT N OFFSET M` — берём первое число после LIMIT, этого
/// достаточно для эвристики «строк ровно столько, сколько просили».
/// Подсказка при пустой (0 строк) выборке bsl_sql. Если запрос обращался к
/// таблицам процедур — зовём в search_terms (точный SQL по имени промахивается,
/// когда обработчик в общем модуле БСП назван иначе). Иначе — общий мягкий hint
/// (частая причина пустоты — регистрозависимость кириллицы в LIKE/=).
fn empty_result_hint(sql: &str) -> &'static str {
    if searched_proc_tables(sql) {
        "0 строк — но это, скорее всего, НЕ значит «такого нет». LIKE по name ищет ТОЧНОЕ вхождение \
         подстроки с учётом регистра (SQLite lower() кириллицу НЕ сворачивает) — он промахивается, \
         если имя в коде в другой словоформе/регистре или обработчик вынесен в общий модуль БСП под \
         иным именем. ▸ search_terms(query=…) идёт по ДРУГОМУ, триграммному FTS-индексу: ловит \
         словоформы и подстроки от 3 символов, регистр и ё/е неважны, ранжирует по числу совпавших \
         слов; термы = слова имени процедуры + СИНОНИМ объекта-владельца + комментарий. То есть \
         search_terms находит ровно то, что LIKE структурно пропускает. Дай ему 1-3 слова по смыслу."
    } else {
        "0 строк (запрос валиден, ничего не найдено). Проверьте фильтры/точность имён. \
         NB: LIKE/= по name ищет точное вхождение с учётом регистра (SQLite lower() кириллицу не \
         сворачивает) — частая причина пустоты. ▸ Если искали процедуру/функционал по СМЫСЛУ — \
         search_terms(query=…) идёт по триграммному индексу (словоформы, регистр/ё неважны, подстроки \
         ≥3) и находит то, что точный LIKE пропускает; термы = имя процедуры + синоним владельца + комментарий."
    }
}

/// Запрос обращался к таблицам процедур (functions/proc_call_graph/
/// procedure_enrichment) — значит, искали процедуру/функционал.
fn searched_proc_tables(sql: &str) -> bool {
    let lower = sql.to_lowercase();
    lower.contains("functions")
        || lower.contains("procedure_enrichment")
        || lower.contains("proc_call_graph")
}

/// Эксперимент 12.06: при пустой выборке по таблицам процедур термовый поиск
/// (тот же SQL, что у tool'а search_terms) выполняется автоматически в этом же
/// вызове — модель получает выдачу термов сразу, без второго хода (за 5 живых
/// прогонов 11.06 модели не вызвали search_terms ни разу даже по прямому hint'у).
/// Слова берём из строковых литералов SQL ('%ПоШтрихкоду%' → «по штрихкоду») и
/// текстовых params; CamelCase-сплит и нормализация — те же, что у термов
/// (terms::split_identifier: нижний регистр, ё→е, wildcard'ы % и _ отпадают как
/// не-буквенные). None — слов ≥3 символов не нашлось или термы пусты; тогда
/// поведение прежнее (только hint).
fn terms_fallback_for_sql(
    conn: &rusqlite::Connection,
    sql: &str,
    text_params: &[String],
) -> Option<Value> {
    let mut words: Vec<String> = Vec::new();
    for lit in sql_string_literals(sql) {
        words.extend(crate::terms::split_identifier(&lit));
    }
    for p in text_params {
        words.extend(crate::terms::split_identifier(p));
    }
    // Триграммы короче 3 символов не ищутся; дубли схлопываем.
    words.retain(|w| w.chars().count() >= 3);
    words.sort();
    words.dedup();
    if words.is_empty() {
        return None;
    }
    let fts_query =
        words.iter().map(|w| format!("\"{}\"", w)).collect::<Vec<_>>().join(" OR ");
    let mut stmt = conn
        .prepare(
            "SELECT pe.proc_key, pe.signature, fts.rank
             FROM fts_procedure_enrichment fts
             JOIN procedure_enrichment pe ON pe.id = fts.rowid
             WHERE pe.repo = 'default' AND fts.terms MATCH ?1
             ORDER BY fts.rank
             LIMIT 10",
        )
        .ok()?; // таблиц обогащения нет / старый индекс → молча без fallback
    let rows: Vec<Value> = stmt
        .query_map(params![&fts_query], |r| {
            Ok(json!({
                "proc_key": r.get::<_, String>(0)?,
                "signature": r.get::<_, Option<String>>(1)?,
                "score": r.get::<_, f64>(2)?,
            }))
        })
        .ok()?
        .filter_map(|r| r.ok())
        .collect();
    if rows.is_empty() {
        None
    } else {
        Some(json!({ "fts_query": fts_query, "results": rows }))
    }
}

/// Строковые литералы SQL: содержимое '…' с учётом экранированной кавычки ''.
fn sql_string_literals(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\'' {
            let mut lit = String::new();
            while let Some(c2) = chars.next() {
                if c2 == '\'' {
                    if chars.peek() == Some(&'\'') {
                        lit.push('\'');
                        chars.next();
                    } else {
                        break;
                    }
                } else {
                    lit.push(c2);
                }
            }
            out.push(lit);
        }
    }
    out
}

fn sql_limit_value(sql: &str) -> Option<u64> {
    let tokens: Vec<&str> = sql
        .split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == ';')
        .filter(|t| !t.is_empty())
        .collect();
    let mut last: Option<u64> = None;
    for w in tokens.windows(2) {
        if w[0].eq_ignore_ascii_case("limit") {
            if let Ok(n) = w[1].trim_end_matches(',').parse::<u64>() {
                last = Some(n);
            }
        }
    }
    last
}

/// Имена таблиц, упомянутых в запросе после FROM/JOIN (без regex: токенизация
/// по пробелам/скобкам, идентификатор — ASCII-буквы/цифры/подчёркивание).
fn tables_in_query(sql: &str) -> Vec<String> {
    let toks: Vec<String> = sql
        .split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | ',' | ';'))
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect();
    let mut out: Vec<String> = Vec::new();
    for w in toks.windows(2) {
        let kw = w[0].to_ascii_lowercase();
        if kw == "from" || kw == "join" {
            let ident: String =
                w[1].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
            if !ident.is_empty() && !out.contains(&ident) {
                out.push(ident);
            }
        }
    }
    out
}

/// Обогатить ошибку prepare схемной подсказкой. Для «no such column/table»:
/// фактические колонки таблиц запроса (PRAGMA table_info), `did_you_mean`
/// по Левенштейну и список таблиц, где такая колонка реально существует.
/// Прочие ошибки prepare возвращаются как есть.
fn enrich_prepare_error(conn: &rusqlite::Connection, sql: &str, err: &str) -> Value {
    let base = format!("SQL prepare error: {}", err);

    // Недостающий идентификатор из текста ошибки SQLite; alias-префикс ('mm.')
    // отрезаем — модель назвала колонку, alias для подсказки не важен.
    let missing = ["no such column: ", "no such table: "]
        .iter()
        .find_map(|m| err.split(m).nth(1))
        .and_then(|s| s.split_whitespace().next())
        .map(|s| s.rsplit('.').next().unwrap_or(s).to_string());
    let Some(missing) = missing else {
        return json!({ "error": base });
    };

    let columns_of = |t: &str| -> Vec<String> {
        conn.prepare(&format!("PRAGMA table_info({})", t))
            .ok()
            .and_then(|mut st| {
                st.query_map([], |r| r.get::<_, String>(1))
                    .ok()
                    .map(|rows| rows.flatten().collect())
            })
            .unwrap_or_default()
    };

    let all_tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' AND name NOT LIKE 'fts_%'")
        .ok()
        .and_then(|mut st| {
            st.query_map([], |r| r.get::<_, String>(0))
                .ok()
                .map(|rows| rows.flatten().collect())
        })
        .unwrap_or_default();

    if err.contains("no such table") {
        // Подсказка по таблицам: ближайшие имена из всей БД.
        let mut cand: Vec<(usize, &String)> =
            all_tables.iter().map(|t| (levenshtein(&missing, t), t)).collect();
        cand.sort();
        let dym: Vec<&String> = cand.iter().take(3).map(|(_, t)| *t).collect();
        return json!({ "error": base, "did_you_mean": dym, "tables": all_tables });
    }

    // no such column: колонки таблиц запроса + где колонка реально живёт.
    let mut schema = serde_json::Map::new();
    let mut candidates: Vec<String> = Vec::new();
    for t in tables_in_query(sql) {
        let cols = columns_of(&t);
        if !cols.is_empty() {
            candidates.extend(cols.iter().cloned());
            schema.insert(t, json!(cols));
        }
    }
    let found_in: Vec<&String> = all_tables
        .iter()
        .filter(|t| columns_of(t).iter().any(|c| c == &missing))
        .collect();
    candidates.sort();
    candidates.dedup();
    let mut cand: Vec<(usize, &String)> =
        candidates.iter().map(|c| (levenshtein(&missing, c), c)).collect();
    cand.sort();
    let dym: Vec<&String> = cand.iter().filter(|(d, _)| *d <= 6).take(3).map(|(_, c)| *c).collect();
    json!({
        "error": base,
        "did_you_mean": dym,
        "column_exists_in_tables": found_in,
        "query_tables_columns": schema
    })
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
    fn enrich_error_suggests_column_and_table() {
        let conn = mem_db();
        // Перепутанная колонка: meta_type не из metadata_modules (там module_type).
        let sql = "SELECT mm.meta_type FROM metadata_modules mm";
        let err = conn.prepare(sql).unwrap_err().to_string();
        let v = enrich_prepare_error(&conn, sql, &err);
        let dym: Vec<&str> =
            v["did_you_mean"].as_array().unwrap().iter().filter_map(|x| x.as_str()).collect();
        assert!(dym.contains(&"module_type"), "did_you_mean: {dym:?}");
        // Колонка meta_type реально живёт в metadata_objects.
        let hosts: Vec<&str> = v["column_exists_in_tables"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|x| x.as_str())
            .collect();
        assert!(hosts.contains(&"metadata_objects"), "hosts: {hosts:?}");
        // Колонки таблицы запроса приложены.
        assert!(v["query_tables_columns"]["metadata_modules"].is_array());

        // Несуществующая таблица → ближайшие имена таблиц.
        let sql2 = "SELECT * FROM metadata_object";
        let err2 = conn.prepare(sql2).unwrap_err().to_string();
        let v2 = enrich_prepare_error(&conn, sql2, &err2);
        let dym2: Vec<&str> =
            v2["did_you_mean"].as_array().unwrap().iter().filter_map(|x| x.as_str()).collect();
        assert!(dym2.contains(&"metadata_objects"), "did_you_mean: {dym2:?}");
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
    fn empty_result_hint_routes_proc_queries_to_search_terms() {
        // Запрос к таблицам процедур → зовёт в search_terms.
        for q in [
            "SELECT name FROM functions WHERE name LIKE '%Цены%'",
            "SELECT * FROM proc_call_graph WHERE caller_proc_key='x'",
            "SELECT terms FROM procedure_enrichment WHERE proc_key='y'",
        ] {
            let h = empty_result_hint(q);
            assert!(h.contains("search_terms"), "proc-запрос должен звать в search_terms");
            assert!(h.contains("триграмм"), "должна быть триграммная аргументация (выгода)");
        }
        // Запрос к метаданным/связям → общий hint (тоже упоминает search_terms,
        // но как «если искали процедуру», а не как основной совет).
        let generic = empty_result_hint("SELECT * FROM data_links WHERE link_kind='owner'");
        assert!(generic.contains("ничего не найдено"));
        assert!(generic.contains("регистр"));
    }

    #[test]
    fn sql_limit_value_finds_last_limit() {
        // W5: простые формы.
        assert_eq!(sql_limit_value("SELECT 1 FROM t LIMIT 30"), Some(30));
        assert_eq!(sql_limit_value("select 1 from t limit 30;"), Some(30));
        assert_eq!(sql_limit_value("SELECT 1 FROM t LIMIT 10 OFFSET 5"), Some(10));
        // Подзапрос: берём последний LIMIT (внешний).
        assert_eq!(
            sql_limit_value("SELECT * FROM (SELECT 1 FROM t LIMIT 100) LIMIT 7"),
            Some(7)
        );
        // Без LIMIT — None; LIMIT с параметром — не число, None.
        assert_eq!(sql_limit_value("SELECT 1 FROM t"), None);
        assert_eq!(sql_limit_value("SELECT 1 FROM t LIMIT ?1"), None);
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

    #[test]
    fn sql_string_literals_extracts_and_unescapes() {
        // Обычные литералы, включая wildcards LIKE.
        assert_eq!(
            sql_string_literals("SELECT * FROM functions WHERE name LIKE '%ПоШтрихкоду%'"),
            vec!["%ПоШтрихкоду%"]
        );
        // Несколько литералов + экранированная кавычка ''.
        assert_eq!(
            sql_string_literals("WHERE a = 'один' AND b = 'д''ва'"),
            vec!["один", "д'ва"]
        );
        // Без литералов — пусто.
        assert!(sql_string_literals("SELECT 1 FROM t WHERE id = ?1").is_empty());
    }

    #[test]
    fn searched_proc_tables_detects_procedure_queries() {
        assert!(searched_proc_tables("SELECT * FROM functions WHERE name LIKE '%X%'"));
        assert!(searched_proc_tables("select count(*) from PROC_CALL_GRAPH"));
        assert!(searched_proc_tables("SELECT terms FROM procedure_enrichment"));
        assert!(!searched_proc_tables("SELECT * FROM metadata_objects"));
        assert!(!searched_proc_tables("SELECT * FROM data_links"));
    }

    #[test]
    fn terms_fallback_finds_procs_by_sql_literals() {
        let conn = mem_db();
        conn.execute(
            "INSERT INTO procedure_enrichment (repo, proc_key, terms, signature, updated_at) \
             VALUES ('default', 'ОбщегоНазначения.bsl::НайтиПоШтрихкоду', \
                     'найти по штрихкоду, поиск номенклатуры', 'mech:v1', 0)",
            [],
        )
        .unwrap();
        // Точный LIKE промахнулся (другая словоформа) → fallback находит по термам.
        let fb = terms_fallback_for_sql(
            &conn,
            "SELECT name FROM functions WHERE name LIKE '%ПоискПоШтрихкодам%'",
            &[],
        )
        .expect("fallback должен вернуть выдачу");
        let results = fb["results"].as_array().unwrap();
        assert!(!results.is_empty());
        assert!(results[0]["proc_key"].as_str().unwrap().contains("НайтиПоШтрихкоду"));
        // Текстовые params тоже участвуют в термах.
        let fb2 = terms_fallback_for_sql(
            &conn,
            "SELECT name FROM functions WHERE name LIKE ?1",
            &["%штрихкод%".to_string()],
        );
        assert!(fb2.is_some());
    }

    #[test]
    fn terms_fallback_none_when_no_words_or_no_match() {
        let conn = mem_db();
        // Термов в БД нет вообще → None (поведение прежнее, только hint).
        assert!(terms_fallback_for_sql(
            &conn,
            "SELECT name FROM functions WHERE name LIKE '%Штрихкод%'",
            &[],
        )
        .is_none());
        // Слов ≥3 символов не нашлось → None ещё до запроса.
        assert!(terms_fallback_for_sql(&conn, "SELECT name FROM functions WHERE id = 5", &[])
            .is_none());
    }
}
