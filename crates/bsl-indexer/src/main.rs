// Приватный wrapper code-index с поддержкой 1С через bsl-extension.
//
// Регистрация процессоров отличается от code-index ровно одной строкой:
// вместо `StandardLanguageProcessor::bsl()` подключается полнофункциональный
// `bsl_extension::BslLanguageProcessor`, который (на этапе 6) принесёт
// специфичные MCP-tools (`get_object_structure`, `get_form_handlers` и т.д.)
// и XML-парсер метаданных конфигурации.
//
// На этапе 2 эти tools ещё пусты — bsl-indexer ведёт себя как code-index
// плюс обещание расшириться. Главная цель этапа 2 — убедиться, что
// workspace правильно собирается со вторым bin'ом и оба используют одну
// и ту же CLI-обёртку без дублирования кода.

use std::sync::Arc;

use bsl_extension::BslLanguageProcessor;
use code_index_core::cli;
use code_index_core::extension::{ProcessorRegistry, StandardLanguageProcessor};

fn build_registry() -> ProcessorRegistry {
    let mut reg = ProcessorRegistry::new();
    // Универсальные процессоры — те же, что в публичном code-index.
    reg.register(Arc::new(StandardLanguageProcessor::python()));
    reg.register(Arc::new(StandardLanguageProcessor::rust()));
    reg.register(Arc::new(StandardLanguageProcessor::go()));
    reg.register(Arc::new(StandardLanguageProcessor::java()));
    reg.register(Arc::new(StandardLanguageProcessor::javascript()));
    reg.register(Arc::new(StandardLanguageProcessor::typescript()));
    // 1С — полнофункциональный процессор из bsl-extension.
    // ВАЖНО: BslLanguageProcessor ИДЁТ РАНЬШЕ StandardLanguageProcessor::bsl(),
    // если бы мы захотели зарегистрировать оба. Сейчас регистрируем только
    // bsl-extension, потому что StandardLanguageProcessor::bsl() — это
    // upstream-фолбэк для публичного code-index.
    reg.register(Arc::new(BslLanguageProcessor::new()));
    reg
}

/// Имена 1С-специфичных подкоманд, которые мы перехватываем ДО `core::cli::run`.
/// Делаем это через простой match по argv[1], чтобы не размывать публичную
/// CLI-обёртку core 1С-специфичной командой `enrich` (она имеет смысл только
/// при сборке с feature `enrichment` или приватной сборке `bsl-indexer`).
const BSL_SUBCOMMANDS: &[&str] = &["enrich"];

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let argv: Vec<String> = std::env::args().collect();

    // Если пользователь набрал `bsl-indexer enrich [...]` — обходим core::cli
    // и идём в bsl_extension::enrichment::run_cli. Делегат сам разберёт
    // оставшийся хвост через clap. Никакой initialization tracing_subscriber
    // делегат не делает — это наша забота тут.
    if let Some(sub) = argv.get(1) {
        if BSL_SUBCOMMANDS.contains(&sub.as_str()) {
            init_tracing();
            // Передаём всё, что после `enrich`, в обработчик подкоманды.
            let rest: Vec<String> = argv[2..].to_vec();
            return match sub.as_str() {
                "enrich" => bsl_extension::enrichment::run_cli(rest).await,
                _ => unreachable!("BSL_SUBCOMMANDS должен покрывать ветки match"),
            };
        }
    }

    cli::run(build_registry()).await
}

/// Инициализация tracing_subscriber. core::cli делает то же самое идемпотентно
/// при своём вызове, но при перехвате `enrich` мы туда не заходим — поэтому
/// инициализируем явно. `try_init` тихо возвращает Err, если глобальный
/// dispatcher уже установлен (например, в тестах) — это нормально.
fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .with_writer(std::io::stderr)
        .try_init();
}
