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
