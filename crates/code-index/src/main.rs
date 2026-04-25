// Публичный wrapper code-index. Регистрирует только встроенные в core
// процессоры (Python/Rust/Go/Java/JavaScript/TypeScript/BSL) и зовёт
// общую CLI-обёртку из core.
//
// BSL-процессор тут регистрируется в режиме `StandardLanguageProcessor`
// (без специфичных tools и без XML-парсера метаданных). Полную
// поддержку 1С даёт приватный binary `bsl-indexer`, который добавляет
// `bsl_extension::BslLanguageProcessor` поверх этого набора.

use std::sync::Arc;

use code_index_core::cli;
use code_index_core::extension::{ProcessorRegistry, StandardLanguageProcessor};

fn build_registry() -> ProcessorRegistry {
    let mut reg = ProcessorRegistry::new();
    reg.register(Arc::new(StandardLanguageProcessor::python()));
    reg.register(Arc::new(StandardLanguageProcessor::rust()));
    reg.register(Arc::new(StandardLanguageProcessor::go()));
    reg.register(Arc::new(StandardLanguageProcessor::java()));
    reg.register(Arc::new(StandardLanguageProcessor::javascript()));
    reg.register(Arc::new(StandardLanguageProcessor::typescript()));
    // BSL без специфичных tools и без XML-парсера метаданных. Для
    // полноценного индексирования 1С используется bsl-indexer (приватный).
    reg.register(Arc::new(StandardLanguageProcessor::bsl()));
    reg
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    cli::run(build_registry()).await
}
