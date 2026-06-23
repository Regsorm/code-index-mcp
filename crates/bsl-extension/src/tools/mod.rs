// MCP-инструменты, специфичные для конфигураций 1С.
//
// Все четыре инструмента опираются на таблицы, заполняемые
// `index_extras::run_index_extras` (этап 4):
//
// - `get_object_structure` — читает строку из `metadata_objects` по
//   full_name и возвращает meta_type/name/synonym/attributes.
// - `get_form_handlers` — читает запись из `metadata_forms` по
//   (owner_full_name, form_name) и возвращает массив (event, handler).
// - `get_event_subscriptions` — отдаёт все подписки репо из
//   `event_subscriptions` (с опциональной фильтрацией по handler-модулю).
// - `find_path_bsl` — проходит по `proc_call_graph` через recursive CTE
//   и возвращает первый путь из caller в callee длиной до max_depth
//   (BSL-вариант универсального `find_path` ядра по таблице `calls`).
//
// Регистрируются в `BslLanguageProcessor::additional_tools()` и
// попадают в MCP `tools/list` только если хотя бы у одного репо
// `language = "bsl"` (этап 1.5/1.6 → conditional registration).

pub mod bsl_sql;
pub mod find_data_path;
pub mod find_references;
pub mod get_object_profile;
pub mod find_path_bsl;
pub mod get_data_links;
pub mod get_event_subscriptions;
pub mod get_form_handlers;
pub mod get_object_structure;
pub mod get_register_writers;
pub mod search_terms;

pub use bsl_sql::BslSqlTool;
pub use find_data_path::FindDataPathTool;
pub use find_path_bsl::FindPathBslTool;
pub use find_references::FindReferencesTool;
pub use get_data_links::GetDataLinksTool;
pub use get_event_subscriptions::GetEventSubscriptionsTool;
pub use get_form_handlers::GetFormHandlersTool;
pub use get_object_profile::GetObjectProfileTool;
pub use get_object_structure::GetObjectStructureTool;
pub use get_register_writers::GetRegisterWritersTool;
pub use search_terms::SearchTermsTool;

use serde_json::{json, Value};

/// Завернуть результат BSL-tool'а в `{result, _meta: {dependent_files: [...]}}`
/// для cache-ci event-based invalidation (Phase 2). BSL-tools пока не вычисляют
/// dependent_files (XML-парсер метаданных хранит данные о объектах конфигурации
/// не как файлы а как records в SQLite) — отдаём пустой массив. Entry попадёт
/// в кэш без file-зависимостей и будет чиститься только по TTL (как раньше).
/// Включение реальных dependent_files для BSL — задача следующей итерации.
pub(crate) fn wrap_with_meta(tool: &str, result: Value, dependent_files: Vec<String>) -> Value {
    // cap_response (обрез массивов с сэмплом) применяется ТОЛЬКО если инструмент
    // в списке `[mcp].cap_tools` (параметр сервера; дефолт — cap::DEFAULT_CAP_TOOLS).
    // Иначе ответ как есть. Серверная нода ужимает ДО federation-провода и клиента
    // (не давая harness'у сбросить громадный tool_result на диск).
    let (result, truncated) = if code_index_core::mcp::cap::cap_applies(tool) {
        code_index_core::mcp::cap::cap_response(result, code_index_core::mcp::cap::response_cap())
    } else {
        (result, false)
    };
    let mut out = json!({
        "result": result,
        "_meta": { "dependent_files": dependent_files },
    });
    if truncated {
        if let Some(obj) = out.as_object_mut() {
            obj.insert("response_truncated".to_string(), json!(true));
            obj.insert(
                "response_truncated_hint".to_string(),
                json!(code_index_core::mcp::cap::CAP_HINT),
            );
        }
    }
    out
}

/// Обёртка для СТРУКТУРНЫХ инструментов (get_object_structure и др. из
/// `cap::STRUCTURAL_TOOLS`): `{result, _meta}` БЕЗ `cap_response` — слепой обрез
/// массивов исказил бы авторитетную структуру объекта 1С (получишь «1 значение
/// перечисления из 816»). Размером такие tools управляют сами через
/// `cap::omit_oversize_sections` (тяжёлую секцию целиком) ДО этой обёртки.
/// `omitted` → добавить верхнеуровневый маркер + hint.
pub(crate) fn wrap_with_meta_structural(
    result: Value,
    dependent_files: Vec<String>,
    omitted: bool,
) -> Value {
    let mut out = json!({
        "result": result,
        "_meta": { "dependent_files": dependent_files },
    });
    if omitted {
        if let Some(obj) = out.as_object_mut() {
            obj.insert("response_sections_omitted".to_string(), json!(true));
            obj.insert(
                "response_sections_omitted_hint".to_string(),
                json!(code_index_core::mcp::cap::OMIT_HINT),
            );
        }
    }
    out
}

/// Сохранить _meta даже на ошибке, чтобы клиенты всегда получали единый формат
/// `{result, _meta}`. Tool сам помещает в `result` что нужно (включая `{error: ...}`).
pub(crate) fn wrap_error(error_value: Value) -> Value {
    // Ошибки крошечные и капу не подлежат — без cap, без hint.
    wrap_with_meta_structural(error_value, Vec::new(), false)
}

/// Имя объекта для single-object инструмента — берётся ЗНАЧЕНИЕ без оглядки на имя
/// ключа. Агент мог назвать параметр `object`/`full_name`/`name`/как угодно — не
/// важно: у такого инструмента ровно один объект, поэтому имя ключа не анализируем.
/// Пропускаются служебные ключи (repo и общие модификаторы), первое непустое
/// строковое значение трактуется как имя объекта.
///
/// НЕ применять в multi-object инструментах (`find_data_path` from/to,
/// `get_form_handlers` owner+form_name) — там имя ключа значимо.
pub(crate) fn object_value(args: &Value) -> Option<&str> {
    const SERVICE: &[&str] = &[
        "repo", "depth", "limit", "direction", "sections", "language", "max_depth", "name_like",
        "meta_type",
    ];
    args.as_object()?
        .iter()
        .filter(|(k, _)| !SERVICE.contains(&k.as_str()))
        .find_map(|(_, v)| v.as_str().filter(|s| !s.trim().is_empty()))
}

/// singular meta_type → имя папки выгрузки (plural), под которым хранятся
/// формы (`metadata_forms.owner_full_name`) и модули (`metadata_modules.full_name`).
/// Возвращает `None` для пустого типа. Покрывает все типы, у которых бывают
/// формы или модули; общий хелпер get_object_profile и get_form_handlers.
pub(crate) fn meta_type_to_folder(meta_type: &str) -> Option<String> {
    let folder = match meta_type {
        "Catalog" => "Catalogs",
        "Document" => "Documents",
        "DocumentJournal" => "DocumentJournals",
        "Enum" => "Enums",
        "Report" => "Reports",
        "DataProcessor" => "DataProcessors",
        "InformationRegister" => "InformationRegisters",
        "AccumulationRegister" => "AccumulationRegisters",
        "AccountingRegister" => "AccountingRegisters",
        "CalculationRegister" => "CalculationRegisters",
        "ChartOfCharacteristicTypes" => "ChartsOfCharacteristicTypes",
        "ChartOfAccounts" => "ChartsOfAccounts",
        "ChartOfCalculationTypes" => "ChartsOfCalculationTypes",
        "ExchangePlan" => "ExchangePlans",
        "BusinessProcess" => "BusinessProcesses",
        "Task" => "Tasks",
        "SettingsStorage" => "SettingsStorages",
        "CommonForm" => "CommonForms",
        "Constant" => "Constants",
        "FilterCriterion" => "FilterCriteria",
        "Sequence" => "Sequences",
        // Незнакомый тип — эвристика 1С «+s» (Document→Documents, Report→Reports);
        // покрывает регулярные случаи, нерегулярные (ChartOf*) перечислены явно выше.
        other if !other.is_empty() => return Some(format!("{}s", other)),
        _ => return None,
    };
    Some(folder.to_string())
}

/// Развернуть плоский критерий-селектор в список `full_name` по индексу
/// метаданных. ОБЩАЯ конвенция объектно-ключевых инструментов: вместо одного
/// имени модель передаёт предикат (`name_like` — подстрока имени объекта +
/// опц. `meta_type`), сервер сам разворачивает его в набор за один SQL по
/// `metadata_objects` (`repo='default'` — как в `resolve_one`), дальше набор
/// уходит в `mass_map`. Это форма ПРЕДИКАТА (высокий adoption, как sections=),
/// а не форма списка (`full_names[]` модель спонтанно не собирает).
///
/// `cap` — потолок объектов (защита от широкого критерия вроде LIKE '%а%').
/// Берём `cap + 1` строк, чтобы отличить «ровно cap» от «больше cap».
/// Возвращает `(full_names ≤ cap, truncated)`. Регистр LIKE для кириллицы
/// значим (SQLite case-insensitive только для ASCII).
pub(crate) fn expand_object_criterion(
    conn: &rusqlite::Connection,
    name_like: &str,
    meta_type: Option<&str>,
    cap: usize,
) -> rusqlite::Result<(Vec<String>, bool)> {
    // meta_type канонизируем (RU→EN, как resolve_one); неизвестный тип оставляем
    // как есть — SQL просто вернёт 0 строк (критерий ни с чем не совпал).
    let canon = meta_type.map(|t| match crate::code_usages::canonical_meta_type(t) {
        Some(c) => c.to_string(),
        None => t.to_string(),
    });
    let limit = (cap + 1) as i64;
    let rows: Vec<String> = if let Some(mt) = &canon {
        let mut stmt = conn.prepare(
            "SELECT full_name FROM metadata_objects \
             WHERE repo = 'default' AND name LIKE '%' || ?1 || '%' AND meta_type = ?2 \
             ORDER BY full_name LIMIT ?3",
        )?;
        let it = stmt.query_map(rusqlite::params![name_like, mt, limit], |r| {
            r.get::<_, String>(0)
        })?;
        it.collect::<rusqlite::Result<Vec<String>>>()?
    } else {
        let mut stmt = conn.prepare(
            "SELECT full_name FROM metadata_objects \
             WHERE repo = 'default' AND name LIKE '%' || ?1 || '%' \
             ORDER BY full_name LIMIT ?2",
        )?;
        let it = stmt.query_map(rusqlite::params![name_like, limit], |r| {
            r.get::<_, String>(0)
        })?;
        it.collect::<rusqlite::Result<Vec<String>>>()?
    };
    let truncated = rows.len() > cap;
    let full_names = rows.into_iter().take(cap).collect();
    Ok((full_names, truncated))
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
        let rows = [
            ("Catalog.НастройкиЭДО", "Catalog", "НастройкиЭДО"),
            ("Document.СообщениеЭДО", "Document", "СообщениеЭДО"),
            ("InformationRegister.АбонентыЭДО", "InformationRegister", "АбонентыЭДО"),
            ("Catalog.Контрагенты", "Catalog", "Контрагенты"),
        ];
        for (fqn, mt, nm) in rows {
            conn.execute(
                "INSERT INTO metadata_objects (repo, full_name, meta_type, name) \
                 VALUES ('default', ?, ?, ?)",
                rusqlite::params![fqn, mt, nm],
            )
            .unwrap();
        }
        conn
    }

    #[test]
    fn expand_filters_by_name_substring() {
        let conn = mem_db();
        let (names, truncated) = expand_object_criterion(&conn, "ЭДО", None, 50).unwrap();
        assert_eq!(names.len(), 3);
        assert!(!truncated);
        assert!(names.iter().all(|n| n.contains("ЭДО")));
        assert!(!names.contains(&"Catalog.Контрагенты".to_string()));
    }

    #[test]
    fn expand_filters_by_meta_type() {
        let conn = mem_db();
        let (names, _) = expand_object_criterion(&conn, "ЭДО", Some("Catalog"), 50).unwrap();
        assert_eq!(names, vec!["Catalog.НастройкиЭДО".to_string()]);
    }

    #[test]
    fn expand_meta_type_accepts_russian_singular() {
        let conn = mem_db();
        // RU singular «Документ» канонизируется в Document.
        let (names, _) = expand_object_criterion(&conn, "ЭДО", Some("Документ"), 50).unwrap();
        assert_eq!(names, vec!["Document.СообщениеЭДО".to_string()]);
    }

    #[test]
    fn expand_cap_sets_truncated() {
        let conn = mem_db();
        // cap=2 при 3 совпадениях → ровно 2 имени + truncated.
        let (names, truncated) = expand_object_criterion(&conn, "ЭДО", None, 2).unwrap();
        assert_eq!(names.len(), 2);
        assert!(truncated);
    }

    #[test]
    fn expand_empty_when_no_match() {
        let conn = mem_db();
        let (names, truncated) =
            expand_object_criterion(&conn, "НесуществующаяТема", None, 50).unwrap();
        assert!(names.is_empty());
        assert!(!truncated);
    }
}
