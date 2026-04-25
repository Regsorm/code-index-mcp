// Корневой модуль `enrichment` для bsl-extension.
//
// Архитектура — две части:
//
//  * `signature` и `prompt` доступны ВСЕГДА. signature нужен для записи
//    отпечатка модели даже без работающего HTTP (мы его пишем при первом
//    `bsl-indexer enrich`, а читает его кто угодно). prompt полезен для
//    тестов парсинга ответа без сети.
//
//  * `client`, `batch`, `cli` доступны ТОЛЬКО под cargo feature `enrichment`.
//    Они тащат `reqwest` и сетевую логику. Без feature `bsl-indexer enrich`
//    отдаёт ошибку «фича не собрана» (см. `run_stub` ниже).

pub mod prompt;
pub mod signature;

#[cfg(feature = "enrichment")]
pub mod client;
#[cfg(feature = "enrichment")]
pub mod batch;
#[cfg(feature = "enrichment")]
pub mod cli;

#[cfg(feature = "enrichment")]
pub use cli::run_cli;

/// Stub-реализация подкоманды `enrich` для сборок без feature `enrichment`.
/// Печатает руководство пользователя и возвращает ненулевой код выхода
/// через `Err`. Сам бинарник `bsl-indexer` не падает с panic — оборачивает
/// результат в обычный `anyhow::Result<()>`.
#[cfg(not(feature = "enrichment"))]
pub async fn run_cli(_argv: Vec<String>) -> anyhow::Result<()> {
    Err(anyhow::anyhow!(
        "Подкоманда `enrich` доступна только в сборке с cargo feature `enrichment`. \
         Соберите бинарник заново: `cargo build --release -p bsl-indexer --features enrichment`."
    ))
}
