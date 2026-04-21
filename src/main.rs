// Точка входа CLI — code-index
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

use code_index_mcp::indexer::config::IndexConfig;
use code_index_mcp::indexer::Indexer;
use code_index_mcp::storage::memory::StorageConfig;
use code_index_mcp::storage::Storage;

#[derive(Parser)]
#[command(name = "code-index", version, about = "Высокопроизводительный индексатор кода с MCP-протоколом")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Запустить MCP-сервер (read-only). Индексацию ведёт отдельный демон;
    /// этот режим используется Claude Code и другими клиентами как MCP-транспорт.
    ///
    /// Multi-repo: --path можно указать несколько раз в формате `alias=dir`,
    /// тогда в каждом tool-call LLM передаёт параметр `repo=<alias>` для выбора репо.
    /// Без `=` — одиночный репо под alias `default` (старый контракт).
    ///
    /// Примеры:
    ///   code-index serve --path C:\RepoUT                          # single, alias=default
    ///   code-index serve --path ut=C:\RepoUT --path bp=C:\RepoBP   # multi, alias=ut/bp
    Serve {
        /// Корневые директории проектов. Формат: `alias=dir` или просто `dir` (alias="default").
        /// Можно указать несколько раз. Если не указан ни `--path`, ни `--config` —
        /// берётся текущая директория с alias=default.
        #[arg(short, long, value_name = "ALIAS=DIR")]
        path: Vec<String>,

        /// Транспорт: `stdio` (per-session) или `http` (shared process под mcp-supervisor).
        #[arg(short, long, default_value = "stdio")]
        transport: String,

        /// HTTP: адрес биндинга (используется только при `--transport http`).
        #[arg(long, default_value = "127.0.0.1")]
        host: String,

        /// HTTP: порт биндинга (используется только при `--transport http`).
        /// По умолчанию 8011 — следующий свободный после 8001/8002/8007/8010.
        #[arg(long, default_value_t = 8011)]
        port: u16,

        /// Путь к `daemon.toml` — подтянуть список репо и их алиасов из секции `[[paths]]`.
        /// Если указан и `--path` — CLI-пути имеют приоритет и конфиг игнорируется.
        #[arg(long, value_name = "FILE")]
        config: Option<PathBuf>,
    },

    /// Проиндексировать директорию (однократно)
    Index {
        /// Путь к директории
        path: String,

        /// Принудительная полная переиндексация (игнорировать хеши)
        #[arg(short, long)]
        force: bool,
    },

    /// Показать статистику базы данных
    Stats {
        /// Путь к корню проекта
        #[arg(short, long, default_value = ".")]
        path: String,

        /// Вывод в JSON вместо текста
        #[arg(long)]
        json: bool,
    },

    /// Быстрый поиск символа (функции, классы, переменные, импорты по точному имени)
    Query {
        /// Имя символа для поиска
        symbol: String,

        /// Путь к корню проекта
        #[arg(short, long, default_value = ".")]
        path: String,

        /// Фильтр по языку
        #[arg(short, long)]
        language: Option<String>,

        /// Вывод в JSON вместо текста
        #[arg(long)]
        json: bool,
    },

    /// Создать конфигурацию .code-index/config.json с настройками по умолчанию
    Init {
        /// Путь к проекту
        #[arg(short, long, default_value = ".")]
        path: String,
    },

    /// Удалить из индекса файлы, которых нет на диске
    Clean {
        /// Путь к проекту
        #[arg(short, long, default_value = ".")]
        path: String,
    },

    /// Полнотекстовый поиск функций по имени/телу (FTS)
    SearchFunction {
        /// Поисковый запрос
        query: String,

        /// Путь к проекту
        #[arg(short, long, default_value = ".")]
        path: String,

        /// Фильтр по языку
        #[arg(short, long)]
        language: Option<String>,

        /// Максимум результатов
        #[arg(long, default_value = "20")]
        limit: usize,
    },

    /// Полнотекстовый поиск классов по имени/телу (FTS)
    SearchClass {
        /// Поисковый запрос
        query: String,

        /// Путь к проекту
        #[arg(short, long, default_value = ".")]
        path: String,

        /// Фильтр по языку
        #[arg(short, long)]
        language: Option<String>,

        /// Максимум результатов
        #[arg(long, default_value = "20")]
        limit: usize,
    },

    /// Получить функцию по точному имени
    GetFunction {
        /// Имя функции
        name: String,

        /// Путь к проекту
        #[arg(short, long, default_value = ".")]
        path: String,

        /// Фильтр по языку (не используется при точном поиске, для совместимости)
        #[arg(short, long)]
        language: Option<String>,
    },

    /// Получить класс по точному имени
    GetClass {
        /// Имя класса
        name: String,

        /// Путь к проекту
        #[arg(short, long, default_value = ".")]
        path: String,

        /// Фильтр по языку (не используется при точном поиске, для совместимости)
        #[arg(short, long)]
        language: Option<String>,
    },

    /// Кто вызывает данную функцию (callers)
    GetCallers {
        /// Имя функции
        function_name: String,

        /// Путь к проекту
        #[arg(short, long, default_value = ".")]
        path: String,

        /// Фильтр по языку
        #[arg(short, long)]
        language: Option<String>,
    },

    /// Что вызывает данная функция (callees)
    GetCallees {
        /// Имя функции
        function_name: String,

        /// Путь к проекту
        #[arg(short, long, default_value = ".")]
        path: String,

        /// Фильтр по языку
        #[arg(short, long)]
        language: Option<String>,
    },

    /// Получить импорты файла или модуля
    GetImports {
        /// Путь к проекту
        #[arg(short, long, default_value = ".")]
        path: String,

        /// ID файла в индексе
        #[arg(long)]
        file_id: Option<i64>,

        /// Имя модуля
        #[arg(short, long)]
        module: Option<String>,

        /// Фильтр по языку
        #[arg(short, long)]
        language: Option<String>,
    },

    /// Карта файла: все функции, классы, импорты, переменные
    GetFileSummary {
        /// Путь к файлу (как в индексе)
        file: String,

        /// Путь к проекту
        #[arg(short, long, default_value = ".")]
        path: String,
    },

    /// Полнотекстовый поиск по текстовым файлам
    SearchText {
        /// Поисковый запрос
        query: String,

        /// Путь к проекту
        #[arg(short, long, default_value = ".")]
        path: String,

        /// Фильтр по языку
        #[arg(short, long)]
        language: Option<String>,

        /// Максимум результатов
        #[arg(long, default_value = "20")]
        limit: usize,
    },

    /// Поиск подстроки или regex в телах функций и классов (в отличие от FTS, поддерживает точки и спецсимволы)
    GrepBody {
        /// Путь к проекту
        #[arg(short, long, default_value = ".")]
        path: String,

        /// Буквальная подстрока для поиска (LIKE). Поддерживает точки и спецсимволы.
        #[arg(long)]
        pattern: Option<String>,

        /// Регулярное выражение для поиска (REGEXP). Альтернатива --pattern.
        #[arg(long)]
        regex: Option<String>,

        /// Фильтр по языку (bsl, python, rust, java, go, javascript, typescript)
        #[arg(short, long)]
        language: Option<String>,

        /// Максимум результатов
        #[arg(long, default_value = "100")]
        limit: usize,
    },

    /// Управление фоновым демоном индексации
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Запустить демон в foreground (вызывается Scheduled Task / systemd / launchd)
    Run,

    /// Показать статус демона (GET /health)
    Status {
        /// Вывод в JSON вместо человекочитаемого текста
        #[arg(long)]
        json: bool,
    },

    /// Попросить демон перечитать конфиг (POST /reload)
    Reload,

    /// Остановить демон (POST /stop)
    Stop,
}

/// Получить путь к БД для проекта
fn get_db_path(project_path: &str) -> PathBuf {
    let root = Path::new(project_path)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(project_path));
    root.join(".code-index").join("index.db")
}

/// Собрать список (alias, root, db_path) для MCP-сервера.
///
/// Порядок источников:
/// 1. Если передан `--path` — используем CLI-аргументы (старый контракт).
/// 2. Иначе если указан `--config` — берём секцию `[[paths]]` из daemon.toml,
///    алиас вычисляется через [`PathEntry::effective_alias`].
/// 3. Иначе — текущая директория под alias=default.
///
/// Параллельно создаём пустую `.code-index/index.db` со схемой, чтобы MCP-сервер
/// мог открыть read-only до того, как демон проиндексирует путь.
fn build_repo_entries(
    cli_paths: Vec<String>,
    config_path: Option<&Path>,
) -> anyhow::Result<Vec<(String, PathBuf, PathBuf)>> {
    // (alias, dir)
    let pairs: Vec<(String, String)> = if !cli_paths.is_empty() {
        let mut out = Vec::with_capacity(cli_paths.len());
        for raw in cli_paths {
            if let Some(eq_idx) = raw.find('=') {
                let alias = raw[..eq_idx].trim().to_string();
                let dir = raw[eq_idx + 1..].to_string();
                if alias.is_empty() {
                    return Err(anyhow::anyhow!(
                        "Пустой alias в --path '{}'. Формат: alias=dir.",
                        raw
                    ));
                }
                out.push((alias, dir));
            } else {
                out.push(("default".to_string(), raw));
            }
        }
        out
    } else if let Some(cfg_path) = config_path {
        let cfg = code_index_mcp::daemon_core::config::load_from(cfg_path)?;
        if cfg.paths.is_empty() {
            return Err(anyhow::anyhow!(
                "В {} нет ни одной секции [[paths]] — укажите --path или добавьте пути в конфиг.",
                cfg_path.display()
            ));
        }
        cfg.paths
            .iter()
            .map(|p| (p.effective_alias(), p.path.to_string_lossy().into_owned()))
            .collect()
    } else {
        vec![("default".to_string(), ".".to_string())]
    };

    let mut entries: Vec<(String, PathBuf, PathBuf)> = Vec::with_capacity(pairs.len());
    let mut seen_aliases = std::collections::HashSet::new();
    for (alias, dir) in pairs {
        if !seen_aliases.insert(alias.clone()) {
            return Err(anyhow::anyhow!(
                "Алиас репо '{}' указан дважды — алиасы должны быть уникальны.",
                alias
            ));
        }

        let root = Path::new(&dir)
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(&dir));
        let db_path = root.join(".code-index").join("index.db");

        // Если БД ещё нет — создаём пустую со схемой, чтобы сервер мог стартовать.
        // Данные появятся, когда демон проиндексирует путь.
        if !db_path.exists() {
            std::fs::create_dir_all(db_path.parent().unwrap())?;
            let storage = Storage::open_file(&db_path)?;
            drop(storage);
        }

        tracing::info!("MCP repo: {} -> {}", alias, root.display());
        entries.push((alias, root, db_path));
    }

    Ok(entries)
}

/// Запуск MCP-сервера по HTTP (Streamable HTTP) на `host:port`.
///
/// Роут `/mcp` — это точка подключения клиента (соответствует url'у в .mcp.json).
/// `LocalSessionManager` держит сессии in-memory. На каждую сессию фабрика
/// клонирует уже собранный `CodeIndexServer` (он реализует `Clone`), так что
/// все сессии разделяют общий набор открытых SQLite-баз.
async fn serve_http(
    server: code_index_mcp::mcp::CodeIndexServer,
    host: &str,
    port: u16,
) -> anyhow::Result<()> {
    use rmcp::transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    };
    use std::sync::Arc;

    let session_manager = Arc::new(LocalSessionManager::default());
    let svc_server = server.clone();
    let http_service = StreamableHttpService::new(
        move || Ok(svc_server.clone()),
        session_manager,
        StreamableHttpServerConfig::default(),
    );

    let app = axum::Router::new().nest_service("/mcp", http_service);

    let addr: std::net::SocketAddr = format!("{}:{}", host, port)
        .parse()
        .map_err(|e| anyhow::anyhow!("Некорректный host:port '{}:{}': {}", host, port, e))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow::anyhow!("Не удалось привязаться к {}: {}", addr, e))?;

    tracing::info!("MCP HTTP слушает http://{}/mcp", addr);
    axum::serve(listener, app)
        .await
        .map_err(|e| anyhow::anyhow!("axum serve error: {}", e))?;
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Инициализация логирования
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Serve { path, transport, host, port, config } => {
            let entries = build_repo_entries(path, config.as_deref())?;

            let aliases: Vec<&str> = entries.iter().map(|(a, _, _)| a.as_str()).collect();
            tracing::info!("MCP read-only ({}), репо: {:?}", transport, aliases);

            use code_index_mcp::mcp::CodeIndexServer;
            let server = CodeIndexServer::open_readonly_multi(entries)?;

            match transport.as_str() {
                "stdio" => {
                    use rmcp::ServiceExt;
                    let service = server
                        .serve(rmcp::transport::io::stdio())
                        .await
                        .map_err(|e| anyhow::anyhow!("MCP serve error: {}", e))?;
                    service
                        .waiting()
                        .await
                        .map_err(|e| anyhow::anyhow!("MCP wait error: {}", e))?;
                }
                "http" => {
                    serve_http(server, &host, port).await?;
                }
                other => {
                    return Err(anyhow::anyhow!(
                        "Транспорт '{}' не поддерживается. Используйте 'stdio' или 'http'.",
                        other
                    ));
                }
            }
        }

        Commands::Index { path, force } => {
            tracing::info!("Индексация: path={}, force={}", path, force);

            // 1. Разрешить путь до абсолютного
            let abs_path = Path::new(&path)
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from(&path));

            // 2. Создать директорию .code-index/ внутри проекта
            let db_dir = abs_path.join(".code-index");
            std::fs::create_dir_all(&db_dir)
                .map_err(|e| anyhow::anyhow!("Не удалось создать директорию {:?}: {}", db_dir, e))?;

            // 3. Загрузить конфигурацию проекта
            let db_path = db_dir.join("index.db");
            let config = IndexConfig::load(&abs_path)?;

            // 4. Открыть Storage с автоопределением режима
            let storage_config = StorageConfig {
                mode: config.storage_mode.clone(),
                memory_max_percent: config.memory_max_percent,
            };
            let mut storage = Storage::open_auto(&db_path, &storage_config)?;

            // 5. Создать Indexer с конфигом
            let mut indexer = Indexer::with_config(&mut storage, config);

            // 6. Запустить индексацию
            let result = indexer.full_reindex(&abs_path, force)?;

            // 7. Если работаем в in-memory режиме — сохранить результаты на диск
            storage.flush_to_disk(&db_path)?;

            // 8. Вывести результат
            println!("Индексация завершена за {} мс", result.elapsed_ms);
            println!("  Найдено файлов:        {}", result.files_scanned);
            println!("  Проиндексировано:      {}", result.files_indexed);
            println!("  Пропущено (без изм.):  {}", result.files_skipped);
            println!("  Удалено из индекса:    {}", result.files_deleted);

            if !result.errors.is_empty() {
                println!("  Ошибок:                {}", result.errors.len());
                for (file, err) in &result.errors {
                    println!("    [ERR] {}: {}", file, err);
                }
            }
        }

        Commands::Stats { path, json } => {
            tracing::info!("Статистика: path={}", path);

            // 1. Открыть БД (только чтение — не конкурирует с MCP-демоном)
            let db_path = get_db_path(&path);
            let storage = Storage::open_file_readonly(&db_path)?;

            // 2. Получить статистику
            let stats = storage.get_stats()?;

            if json {
                // JSON-формат для программного использования
                println!("{}", serde_json::to_string_pretty(&stats)?);
            } else {
                // Текстовый формат для человека
                println!("Статистика индекса: {}", db_path.display());
                println!("─────────────────────────────────────");
                println!("  Файлов:        {}", stats.total_files);
                println!("  Функций:       {}", stats.total_functions);
                println!("  Классов:       {}", stats.total_classes);
                println!("  Импортов:      {}", stats.total_imports);
                println!("  Вызовов:       {}", stats.total_calls);
                println!("  Переменных:    {}", stats.total_variables);
                println!("  Текст. файлов: {}", stats.total_text_files);
            }
        }

        Commands::Query { symbol, path, language, json } => {
            tracing::info!("Поиск символа '{}': path={}", symbol, path);

            // 1. Открыть БД (только чтение — не конкурирует с MCP-демоном)
            let db_path = get_db_path(&path);
            let storage = Storage::open_file_readonly(&db_path)?;

            // 2. Поиск символа
            let result = storage.find_symbol(&symbol, language.as_deref())?;

            if json {
                // JSON-формат для программного использования
                println!("{}", serde_json::to_string_pretty(&result)?);
                return Ok(());
            }

            let total = result.functions.len()
                + result.classes.len()
                + result.variables.len()
                + result.imports.len();

            if total == 0 {
                println!("Символ '{}' не найден в индексе.", symbol);
                return Ok(());
            }

            println!("Результаты поиска символа '{}':", symbol);

            // 3. Функции
            if !result.functions.is_empty() {
                println!("\n  Функции ({}):", result.functions.len());
                for f in &result.functions {
                    let qname = f.qualified_name.as_deref().unwrap_or(&f.name);
                    let async_mark = if f.is_async { " [async]" } else { "" };
                    let args = f.args.as_deref().unwrap_or("()");
                    println!(
                        "    {}{}  {}  строки {}-{}  (file_id={})",
                        qname, async_mark, args, f.line_start, f.line_end, f.file_id
                    );
                }
            }

            // 4. Классы
            if !result.classes.is_empty() {
                println!("\n  Классы ({}):", result.classes.len());
                for c in &result.classes {
                    let bases = c.bases.as_deref().unwrap_or("");
                    let bases_str = if bases.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", bases)
                    };
                    println!(
                        "    {}{}  строки {}-{}  (file_id={})",
                        c.name, bases_str, c.line_start, c.line_end, c.file_id
                    );
                }
            }

            // 5. Переменные
            if !result.variables.is_empty() {
                println!("\n  Переменные ({}):", result.variables.len());
                for v in &result.variables {
                    let val = v.value.as_deref().unwrap_or("<нет значения>");
                    println!(
                        "    {}  =  {}  строка {}  (file_id={})",
                        v.name, val, v.line, v.file_id
                    );
                }
            }

            // 6. Импорты
            if !result.imports.is_empty() {
                println!("\n  Импорты ({}):", result.imports.len());
                for i in &result.imports {
                    let module = i.module.as_deref().unwrap_or("?");
                    let name = i.name.as_deref().unwrap_or("*");
                    let alias_str = match &i.alias {
                        Some(a) => format!(" as {}", a),
                        None => String::new(),
                    };
                    println!(
                        "    {} from {}{}  строка {}  (file_id={})",
                        name, module, alias_str, i.line, i.file_id
                    );
                }
            }
        }

        Commands::Clean { path } => {
            tracing::info!("Очистка индекса: path={}", path);

            // 1. Открыть БД
            let db_path = get_db_path(&path);
            let storage = Storage::open_file(&db_path)?;

            // 2. Разрешить корневой путь проекта
            let project_root = std::path::Path::new(&path)
                .canonicalize()
                .unwrap_or_else(|_| std::path::PathBuf::from(&path));

            // 3. Получить все файлы из индекса
            let files = storage.get_all_files()?;
            let total = files.len();
            let mut deleted = 0usize;

            // 4. Для каждого файла проверить существование на диске
            for file in files {
                // Путь в индексе может быть абсолютным или относительным от корня проекта
                let on_disk = if std::path::Path::new(&file.path).is_absolute() {
                    std::path::PathBuf::from(&file.path)
                } else {
                    project_root.join(&file.path)
                };

                if !on_disk.exists() {
                    if let Some(id) = file.id {
                        storage.delete_file(id)?;
                        deleted += 1;
                        println!("  Удалён: {}", file.path);
                    }
                }
            }

            // 5. Итог
            println!(
                "Очистка завершена: проверено {} файлов, удалено {} записей.",
                total, deleted
            );
        }

        Commands::Init { path } => {
            // 1. Разрешить путь до абсолютного
            let abs_path = Path::new(&path)
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from(&path));

            let config_path = abs_path.join(".code-index").join("config.json");

            if config_path.exists() {
                println!("Конфигурация уже существует: {}", config_path.display());
                println!("Для перезаписи удалите файл вручную.");
                return Ok(());
            }

            // 2. Создать конфиг по умолчанию
            let config = IndexConfig::default();
            config.save(&abs_path)?;

            println!("Создан файл конфигурации: {}", config_path.display());
            println!("Отредактируйте его при необходимости:");
            println!("  exclude_dirs          — дополнительные директории для исключения");
            println!("  extra_text_extensions — дополнительные расширения для FTS-индексации");
            println!("  max_file_size         — макс. размер текстового файла в байтах (по умолчанию 1 МБ, не влияет на код)");
            println!("  max_files             — лимит файлов (0 = без лимита)");
        }

        // ── Новые команды: JSON-вывод ─────────────────────────────────────────

        Commands::SearchFunction { query, path, language, limit } => {
            let db_path = get_db_path(&path);
            let storage = Storage::open_file_readonly(&db_path)?;
            let results = storage.search_functions(&query, limit, language.as_deref())?;
            println!("{}", serde_json::to_string_pretty(&results)?);
        }

        Commands::SearchClass { query, path, language, limit } => {
            let db_path = get_db_path(&path);
            let storage = Storage::open_file_readonly(&db_path)?;
            let results = storage.search_classes(&query, limit, language.as_deref())?;
            println!("{}", serde_json::to_string_pretty(&results)?);
        }

        Commands::GetFunction { name, path, language: _ } => {
            let db_path = get_db_path(&path);
            let storage = Storage::open_file_readonly(&db_path)?;
            let results = storage.get_function_by_name(&name)?;
            println!("{}", serde_json::to_string_pretty(&results)?);
        }

        Commands::GetClass { name, path, language: _ } => {
            let db_path = get_db_path(&path);
            let storage = Storage::open_file_readonly(&db_path)?;
            let results = storage.get_class_by_name(&name)?;
            println!("{}", serde_json::to_string_pretty(&results)?);
        }

        Commands::GetCallers { function_name, path, language } => {
            let db_path = get_db_path(&path);
            let storage = Storage::open_file_readonly(&db_path)?;
            let results = storage.get_callers(&function_name, language.as_deref())?;
            println!("{}", serde_json::to_string_pretty(&results)?);
        }

        Commands::GetCallees { function_name, path, language } => {
            let db_path = get_db_path(&path);
            let storage = Storage::open_file_readonly(&db_path)?;
            let results = storage.get_callees(&function_name, language.as_deref())?;
            println!("{}", serde_json::to_string_pretty(&results)?);
        }

        Commands::GetImports { path, file_id, module, language } => {
            let db_path = get_db_path(&path);
            let storage = Storage::open_file_readonly(&db_path)?;

            // Приоритет: file_id > module; если ничего не указано — ошибка
            let results = if let Some(fid) = file_id {
                storage.get_imports_by_file(fid)?
            } else if let Some(mod_name) = &module {
                storage.get_imports_by_module(mod_name, language.as_deref())?
            } else {
                return Err(anyhow::anyhow!(
                    "Укажите --file-id <ID> или --module <имя_модуля>"
                ));
            };
            println!("{}", serde_json::to_string_pretty(&results)?);
        }

        Commands::GetFileSummary { file, path } => {
            let db_path = get_db_path(&path);
            let storage = Storage::open_file_readonly(&db_path)?;
            let result = storage.get_file_summary(&file)?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }

        Commands::SearchText { query, path, language, limit } => {
            let db_path = get_db_path(&path);
            let storage = Storage::open_file_readonly(&db_path)?;
            let results = storage.search_text(&query, limit, language.as_deref())?;

            // Результат — Vec<(String, String)>: путь + сниппет
            // Преобразуем в удобный JSON-массив объектов
            let json_results: Vec<serde_json::Value> = results
                .into_iter()
                .map(|(file_path, snippet)| {
                    serde_json::json!({
                        "path": file_path,
                        "snippet": snippet
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&json_results)?);
        }

        Commands::GrepBody { path, pattern, regex, language, limit } => {
            if pattern.is_none() && regex.is_none() {
                return Err(anyhow::anyhow!(
                    "Укажите --pattern <подстрока> или --regex <выражение>"
                ));
            }
            let db_path = get_db_path(&path);
            let storage = Storage::open_file_readonly(&db_path)?;
            let results = storage.grep_body(
                pattern.as_deref(),
                regex.as_deref(),
                language.as_deref(),
                limit,
            )?;
            println!("{}", serde_json::to_string_pretty(&results)?);
        }

        Commands::Daemon { action } => handle_daemon(action).await?,
    }

    Ok(())
}

/// На Windows Rust собирается как console-subsystem приложение. При запуске
/// в пользовательской сессии (Scheduled Task LogonType=Interactive, ручной
/// вызов в cmd/powershell) процесс получает консольное окно и становится
/// привязанным к нему: закрытие окна шлёт CTRL_CLOSE_EVENT и убивает демон.
///
/// Чтобы демон переживал любой способ запуска, при `daemon run` смотрим
/// переменную окружения `CODE_INDEX_DAEMON_DETACHED`. Если её нет —
/// перезапускаем себя с флагами DETACHED_PROCESS | CREATE_NO_WINDOW
/// и немедленно выходим; detached-клон живёт без консоли до явного
/// `daemon stop` / `daemon reload`.
///
/// На Unix self-detach не нужен — демонизацией управляет systemd/launchd.
#[cfg(windows)]
fn detach_from_console_if_needed() -> anyhow::Result<bool> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const ENV_FLAG: &str = "CODE_INDEX_DAEMON_DETACHED";

    if std::env::var_os(ENV_FLAG).is_some() {
        return Ok(false);
    }

    let exe = std::env::current_exe()?;
    std::process::Command::new(exe)
        .arg("daemon")
        .arg("run")
        .env(ENV_FLAG, "1")
        .creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW)
        .spawn()?;
    Ok(true)
}

#[cfg(not(windows))]
fn detach_from_console_if_needed() -> anyhow::Result<bool> {
    Ok(false)
}

async fn handle_daemon(action: DaemonAction) -> anyhow::Result<()> {
    use code_index_mcp::daemon_core::{client, runner};

    match action {
        DaemonAction::Run => {
            if detach_from_console_if_needed()? {
                return Ok(());
            }
            tracing::info!("Запуск фонового демона code-index");
            runner::run().await?;
        }
        DaemonAction::Status { json } => match client::health().await {
            Ok(h) => {
                if json {
                    println!("{}", serde_json::to_string_pretty(&h)?);
                } else {
                    print_status_text(&h);
                }
            }
            Err(e) => {
                eprintln!("Демон недоступен: {}", e);
                std::process::exit(1);
            }
        },
        DaemonAction::Reload => {
            let r = client::reload().await?;
            println!("{}", serde_json::to_string_pretty(&r)?);
        }
        DaemonAction::Stop => {
            let r = client::stop().await?;
            println!("{}", serde_json::to_string_pretty(&r)?);
        }
    }
    Ok(())
}

fn print_status_text(h: &code_index_mcp::daemon_core::ipc::HealthResponse) {
    println!("Демон code-index");
    println!("  статус:    {}", h.status);
    println!("  версия:    {}", h.version);
    println!("  PID:       {}", h.pid);
    println!("  старт:     {}", h.started_at);
    println!("  uptime:    {}с", h.uptime_sec);
    println!("  папок:     {}", h.paths.len());
    for p in &h.paths {
        let status_s = serde_json::to_string(&p.status)
            .unwrap_or_else(|_| "\"?\"".into());
        let status_s = status_s.trim_matches('"');
        let progress_s = match &p.progress {
            Some(pr) => match pr.percent {
                Some(pct) => format!(" {}/{} ({}%)", pr.files_done, pr.files_total, pct),
                None => format!(" {}/{}", pr.files_done, pr.files_total),
            },
            None => String::new(),
        };
        let err_s = p.error.as_ref().map(|e| format!(" err: {}", e)).unwrap_or_default();
        println!("    - [{}] {}{}{}", status_s, p.path.display(), progress_s, err_s);
    }
}
