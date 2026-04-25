// Trait-API для расширения code-index новыми языками и инструментами.
//
// Назначение модуля — позволить отдельным крейтам (например, приватному
// `bsl-extension`) добавлять язык и MCP-tools без изменения core.
//
// Архитектура:
//
// * `LanguageProcessor` — описывает один язык (имя, парсер кода, эвристика
//   auto-detect, дополнительные SQLite-схемы, набор MCP-tools, специфичных
//   для этого языка).
// * `IndexTool` — описывает один MCP-инструмент: имя, описание, JSON-schema
//   входа, для каких языков применим, как выполнить.
// * `ToolContext` — то, что инструмент получает при вызове: SQLite-доступ,
//   путь репо, метаданные.
//
// Для уже встроенных в core языков (Python/Rust/JS/TS/Java/Go/BSL) есть
// `StandardLanguageProcessor` — простая обёртка вокруг существующих
// `LanguageParser`-реализаций. Она не добавляет SQLite-схем и не имеет
// специфичных tools — только базовый набор core.

pub mod context;
pub mod processor;
pub mod tool;

pub use context::ToolContext;
pub use processor::{
    LanguageProcessor, ProcessorRegistry, StandardLanguageProcessor,
};
pub use tool::IndexTool;
