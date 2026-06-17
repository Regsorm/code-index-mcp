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
        "repo", "depth", "limit", "direction", "sections", "language", "max_depth",
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
