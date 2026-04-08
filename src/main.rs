// Точка входа CLI — code-index
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

use code_index_mcp::daemon::DaemonConfig;
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
    /// Запустить MCP-сервер (режим daemon)
    Serve {
        /// Корневая директория проекта
        #[arg(short, long, default_value = ".")]
        path: String,

        /// Транспорт: stdio или http
        #[arg(short, long, default_value = "stdio")]
        transport: String,

        /// Запустить без file watcher (только startup индексация + MCP)
        #[arg(long)]
        no_watch: bool,

        /// Интервал периодической записи БД на диск (в секундах)
        #[arg(long, default_value = "30")]
        flush_interval: u64,
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
}

/// Получить путь к БД для проекта
fn get_db_path(project_path: &str) -> PathBuf {
    let root = Path::new(project_path)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(project_path));
    root.join(".code-index").join("index.db")
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
        Commands::Serve { path, transport, no_watch, flush_interval } => {
            tracing::info!(
                "Запуск MCP-сервера: path={}, transport={}, no_watch={}, flush_interval={}s",
                path, transport, no_watch, flush_interval
            );

            // Разрешить путь до абсолютного
            let root = Path::new(&path)
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from(&path));
            let db_path = root.join(".code-index").join("index.db");

            // Создать директорию .code-index/ если не существует
            if let Some(parent) = db_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| anyhow::anyhow!("Не удалось создать директорию: {}", e))?;
            }

            // Загрузить конфигурацию проекта
            let index_config = IndexConfig::load(&root)?;

            // Подготовить конфигурацию хранилища
            let storage_config = StorageConfig {
                mode: index_config.storage_mode.clone(),
                memory_max_percent: index_config.memory_max_percent,
            };

            // Запустить daemon (startup scan + MCP + watcher)
            let daemon_config = DaemonConfig {
                root,
                db_path,
                index_config,
                storage_config,
                no_watch,
                flush_interval_sec: flush_interval,
            };
            code_index_mcp::daemon::run_daemon(daemon_config).await?;
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
    }

    Ok(())
}
