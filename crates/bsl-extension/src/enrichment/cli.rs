// Подкоманда `bsl-indexer enrich`. Перехватывается в `bsl-indexer/src/main.rs`
// до вызова `core::cli::run`, чтобы не размывать публичную CLI-обёртку
// 1С-специфичной командой.
//
// Когда фича `enrichment` включена — реальная реализация ниже.
// Когда выключена — `bsl-indexer/src/main.rs` использует stub из `super::run_stub`,
// который печатает понятную ошибку и выходит с ненулевым кодом.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use code_index_core::daemon_core::config::EnrichmentConfig;
use code_index_core::storage::Storage;

use super::batch::{run as run_batch, RunOptions};
use super::client::{ChatClient, ReqwestChatClient};

/// Параметры подкоманды `enrich`.
#[derive(Parser, Debug)]
#[command(
    name = "enrich",
    about = "Обогатить процедуры 1С бизнес-терминами через LLM (FTS-канал поиска)"
)]
pub struct EnrichArgs {
    /// Путь к репозиторию 1С (с уже выполненным `bsl-indexer index`).
    #[arg(short, long, default_value = ".")]
    pub path: String,

    /// Путь к daemon.toml с секцией `[enrichment]`. Если не указан —
    /// ищется `$CODE_INDEX_HOME/daemon.toml`.
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Обогатить максимум N процедур (для smoke-test'а).
    #[arg(long)]
    pub limit: Option<usize>,

    /// Принудительно переобогатить даже те процедуры, у которых уже есть terms.
    #[arg(long)]
    pub reenrich: bool,
}

/// Точка входа подкоманды `enrich`. `argv` — аргументы ПОСЛЕ `enrich`
/// (то есть `argv[0]` — это первый флаг команды). См.
/// `bsl-indexer/src/main.rs` для перехвата.
///
/// Инициализацию tracing_subscriber делает вызывающий бинарник до
/// этого вызова — у bsl-extension в зависимостях нет tracing_subscriber,
/// чтобы не размазывать инициализацию логов между крейтами.
pub async fn run_cli(argv: Vec<String>) -> Result<()> {
    // clap ожидает имя бинарника в argv[0]. Префиксуем «bsl-indexer enrich»,
    // чтобы help-сообщения смотрелись осмысленно.
    let mut full_argv = Vec::with_capacity(argv.len() + 1);
    full_argv.push("bsl-indexer enrich".to_string());
    full_argv.extend(argv);
    let args = match EnrichArgs::try_parse_from(full_argv) {
        Ok(a) => a,
        Err(e) => {
            // --help / --version — clap возвращает Err, но это не ошибка
            // пользователя: печатаем в stdout и выходим с 0.
            use clap::error::ErrorKind;
            if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) {
                let _ = e.print();
                return Ok(());
            }
            return Err(anyhow!(e.to_string()));
        }
    };

    let cfg = load_enrichment_config(args.config.as_deref())?;
    if !cfg.enabled {
        return Err(anyhow!(
            "В daemon.toml [enrichment].enabled=false — фича выключена. \
             Включите её и повторите команду."
        ));
    }
    if cfg.url.is_empty() || cfg.model.is_empty() {
        return Err(anyhow!(
            "В daemon.toml [enrichment] обязательны поля url и model."
        ));
    }

    let signature = cfg.signature();
    tracing::info!("enrichment: signature={}", signature);

    // Сверка подписи с уже сохранённой в БД. На warning-mismatch уведомляем
    // оператора, но продолжаем (он сам решит, нужен ли --reenrich).
    let abs_path = Path::new(&args.path)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(&args.path));
    let db_path = abs_path.join(".code-index").join("index.db");
    if !db_path.exists() {
        return Err(anyhow!(
            "БД индекса не найдена: {}. Сначала запустите `bsl-indexer index {}`.",
            db_path.display(),
            args.path
        ));
    }
    let mut storage = Storage::open_file(&db_path)?;
    storage
        .apply_schema_extensions(crate::schema::SCHEMA_EXTENSIONS)
        .context("apply_schema_extensions для enrichment")?;

    match super::signature::check_signature(storage.conn(), &signature)? {
        super::signature::SignatureCheck::Mismatch { stored } => {
            if !args.reenrich {
                tracing::warn!(
                    "enrichment_signature в БД ({}) != конфиг ({}). \
                     Старые termы остаются и будут смешиваться с новыми; \
                     запустите `bsl-indexer enrich --reenrich` для пересборки.",
                    stored,
                    signature
                );
            } else {
                tracing::info!(
                    "enrichment_signature mismatch (stored={}, current={}); --reenrich \
                     перезапишет всё под новую подпись.",
                    stored,
                    signature
                );
            }
        }
        super::signature::SignatureCheck::Fresh => {
            tracing::info!("enrichment_signature: впервые — записываем после прогона.");
        }
        super::signature::SignatureCheck::Match => {
            tracing::info!("enrichment_signature: совпадает с конфигом.");
        }
    }

    let client: Arc<dyn ChatClient> = Arc::new(ReqwestChatClient::new(
        &cfg.url,
        &cfg.model,
        cfg.api_key_env.as_deref(),
        3,
    )?);

    let opts = RunOptions {
        prompt_template: cfg.prompt_template.clone(),
        signature,
        limit: args.limit,
        reenrich: args.reenrich,
        batch_size: cfg.batch_size as usize,
    };
    let stats = run_batch(&mut storage, client, &opts).await?;
    println!(
        "enrichment: attempted={}, written={}, empty={}, failed={}",
        stats.attempted, stats.written, stats.empty, stats.failed
    );
    Ok(())
}

/// Прочитать `[enrichment]` из явно указанного daemon.toml или из
/// `$CODE_INDEX_HOME/daemon.toml`.
fn load_enrichment_config(custom: Option<&Path>) -> Result<EnrichmentConfig> {
    let cfg = match custom {
        Some(p) => code_index_core::daemon_core::config::load_from(p)
            .with_context(|| format!("чтение {}", p.display()))?,
        None => code_index_core::daemon_core::config::load_or_default()
            .context("чтение daemon.toml по умолчанию")?,
    };
    cfg.enrichment.ok_or_else(|| {
        anyhow!(
            "В daemon.toml нет секции [enrichment] — добавьте её и установите enabled=true."
        )
    })
}
