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
// - `find_path` — проходит по `proc_call_graph` через recursive CTE
//   и возвращает первый путь из caller в callee длиной до max_depth.
//
// Регистрируются в `BslLanguageProcessor::additional_tools()` и
// попадают в MCP `tools/list` только если хотя бы у одного репо
// `language = "bsl"` (этап 1.5/1.6 → conditional registration).

pub mod find_path;
pub mod get_event_subscriptions;
pub mod get_form_handlers;
pub mod get_object_structure;
pub mod search_terms;

pub use find_path::FindPathTool;
pub use get_event_subscriptions::GetEventSubscriptionsTool;
pub use get_form_handlers::GetFormHandlersTool;
pub use get_object_structure::GetObjectStructureTool;
pub use search_terms::SearchTermsTool;

use serde_json::{json, Value};

/// Завернуть результат BSL-tool'а в `{result, _meta: {dependent_files: [...]}}`
/// для cache-ci event-based invalidation (Phase 2). BSL-tools пока не вычисляют
/// dependent_files (XML-парсер метаданных хранит данные о объектах конфигурации
/// не как файлы а как records в SQLite) — отдаём пустой массив. Entry попадёт
/// в кэш без file-зависимостей и будет чиститься только по TTL (как раньше).
/// Включение реальных dependent_files для BSL — задача следующей итерации.
pub(crate) fn wrap_with_meta(result: Value, dependent_files: Vec<String>) -> Value {
    json!({
        "result": result,
        "_meta": { "dependent_files": dependent_files },
    })
}

/// Сохранить _meta даже на ошибке, чтобы клиенты всегда получали единый формат
/// `{result, _meta}`. Tool сам помещает в `result` что нужно (включая `{error: ...}`).
pub(crate) fn wrap_error(error_value: Value) -> Value {
    wrap_with_meta(error_value, Vec::new())
}
