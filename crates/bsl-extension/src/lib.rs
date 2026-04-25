// bsl-extension — приватный crate code-index для конфигураций 1С.
//
// На этапе 2 здесь — скелет: реализация LanguageProcessor::name()=="bsl"
// и detects() (Configuration.xml в корне), но без специфичных tools или
// SQLite-расширений. Реальные XML-парсеры метаданных и MCP-tools
// (`get_object_structure` и т.д.) появятся на этапах 3 и 6.
//
// Этот crate НЕ публикуется в crates.io / на публичный GitHub. Он
// входит в приватный binary `bsl-indexer`, который запускается на
// VM RAG. Публичный `code-index` его не подключает — public surface
// area остаётся «универсальный индексатор без 1С-логики».

pub mod enrichment;
pub mod index_extras;
pub mod module_constants;
pub mod processor;
pub mod schema;
pub mod tools;
pub mod xml;

pub use processor::BslLanguageProcessor;
