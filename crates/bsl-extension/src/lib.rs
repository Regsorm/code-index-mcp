// bsl-extension — crate code-index с поддержкой конфигураций 1С.
//
// Реализует `LanguageProcessor` для языка "bsl": XML-парсеры выгрузки
// (Configuration.xml, формы, объекты), SQLite-расширения схемы
// (metadata_objects / metadata_forms / event_subscriptions /
// proc_call_graph / data_links) и BSL-специфичные MCP-tools
// (`get_object_structure`, `get_form_handlers`, `get_event_subscriptions`,
// `find_path`, `search_terms`, `get_data_links`, `find_data_path`).
//
// Подключается к ядру (`code-index-core`) в сборке бинарника `bsl-indexer`
// (core + эта надстройка). Публичный бинарник `code-index` этот crate НЕ
// линкует — его public surface остаётся «универсальный индексатор без
// 1С-логики». При этом ИСХОДНИКИ crate'а лежат в том же публичном репо и
// распространяются вместе с проектом (полный функционал открыт).

pub mod enrichment;
pub mod index_extras;
pub mod module_constants;
pub mod processor;
pub mod schema;
pub mod tools;
pub mod xml;

pub use processor::BslLanguageProcessor;
