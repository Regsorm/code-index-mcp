// Точка входа CLI — code-index
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

use code_index_mcp::indexer::Indexer;
use code_index_mcp::mcp::CodeIndexServer;
use code_index_mcp::storage::Storage;
use rmcp::ServiceExt;

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
    },

    /// Быстрый поиск символа
    Query {
        /// Имя символа для поиска
        symbol: String,

        /// Путь к корню проекта
        #[arg(short, long, default_value = ".")]
        path: String,
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
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Serve { path, transport } => {
            tracing::info!("Запуск MCP-сервера: path={}, transport={}", path, transport);

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

            // Открыть хранилище
            let mut storage = Storage::open_file(&db_path)?;

            // Начальная индексация проекта
            let mut indexer = Indexer::new(&mut storage);
            let index_result = indexer.full_reindex(&root, false)?;
            eprintln!(
                "Индексация завершена: {} файлов за {} мс",
                index_result.files_indexed, index_result.elapsed_ms
            );

            // Запуск MCP-сервера через stdio-транспорт
            let server = CodeIndexServer::new(storage);
            let service = server.serve(rmcp::transport::io::stdio()).await?;
            service.waiting().await?;
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

            // 3. Открыть Storage
            let db_path = db_dir.join("index.db");
            let mut storage = Storage::open_file(&db_path)?;

            // 4. Создать Indexer
            let mut indexer = Indexer::new(&mut storage);

            // 5. Запустить индексацию
            let result = indexer.full_reindex(&abs_path, force)?;

            // 6. Вывести результат
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

        Commands::Stats { path } => {
            tracing::info!("Статистика: path={}", path);

            // 1. Открыть БД
            let db_path = get_db_path(&path);
            let storage = Storage::open_file(&db_path)?;

            // 2. Получить статистику
            let stats = storage.get_stats()?;

            // 3. Вывести таблицу
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

        Commands::Query { symbol, path } => {
            tracing::info!("Поиск символа '{}': path={}", symbol, path);

            // 1. Открыть БД
            let db_path = get_db_path(&path);
            let storage = Storage::open_file(&db_path)?;

            // 2. Поиск символа
            let result = storage.find_symbol(&symbol)?;

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
    }

    Ok(())
}
