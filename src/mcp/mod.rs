// MCP-сервер (v0.5+) — тонкий read-only слой над SQLite-индексом.
//
// Ничего не пишет. Перед каждым tool-call проверяет у демона статус папки.
// Если индекс не готов — возвращает ToolUnavailable JSON вместо данных.

use std::path::PathBuf;
use std::sync::Arc;

use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router, ServerHandler,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::storage::Storage;

pub mod tools;

// ── Параметры инструментов (без изменений) ───────────────────────────────────

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct SearchParams {
    pub query: String,
    pub limit: Option<usize>,
    pub language: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct NameParams {
    pub name: String,
    pub language: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct FunctionNameParams {
    pub function_name: String,
    pub language: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ImportParams {
    pub file_id: Option<i64>,
    pub module: Option<String>,
    pub language: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct FilePathParams {
    pub path: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GrepBodyParams {
    pub pattern: Option<String>,
    pub regex: Option<String>,
    pub language: Option<String>,
    pub limit: Option<usize>,
}

// ── Сервер ───────────────────────────────────────────────────────────────────

/// Read-only MCP-сервер индексатора. Хранит путь к проекту (для проверки статуса
/// у демона) и открытый SQLite в режиме SQLITE_OPEN_READ_ONLY.
#[derive(Clone)]
pub struct CodeIndexServer {
    /// Канонический путь к корню проекта — параметр `?path=` для /path-status у демона.
    pub root_path: PathBuf,
    /// SQLite-подключение. Mutex нужен для сериализации доступа к `Connection`
    /// (rusqlite не Sync), но БД открыта в read-only, конкуренции на запись нет.
    pub storage: Arc<Mutex<Storage>>,
    /// Роутер MCP-инструментов (генерируется макросом).
    tool_router: ToolRouter<Self>,
}

impl CodeIndexServer {
    /// Создать read-only MCP-сервер: открыть `.code-index/index.db` в read-only.
    pub fn open_readonly(root_path: PathBuf, db_path: &std::path::Path) -> anyhow::Result<Self> {
        let storage = Storage::open_file_readonly(db_path)?;
        Ok(Self {
            root_path,
            storage: Arc::new(Mutex::new(storage)),
            tool_router: Self::tool_router(),
        })
    }

    /// Конструктор для тестов/встраивания — принимает уже открытое хранилище.
    pub fn with_storage(root_path: PathBuf, storage: Storage) -> Self {
        Self {
            root_path,
            storage: Arc::new(Mutex::new(storage)),
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl CodeIndexServer {
    #[tool(description = "FTS поиск функций: по имени, docstring, телу. Возвращает JSON-массив FunctionRecord.")]
    async fn search_function(&self, Parameters(p): Parameters<SearchParams>) -> String {
        tools::search_function(self, p.query, p.limit, p.language).await
    }

    #[tool(description = "FTS поиск классов: по имени, docstring, телу. Возвращает JSON-массив ClassRecord.")]
    async fn search_class(&self, Parameters(p): Parameters<SearchParams>) -> String {
        tools::search_class(self, p.query, p.limit, p.language).await
    }

    #[tool(description = "Найти функцию по точному имени. Возвращает JSON-массив FunctionRecord.")]
    async fn get_function(&self, Parameters(p): Parameters<NameParams>) -> String {
        tools::get_function(self, p.name).await
    }

    #[tool(description = "Найти класс по точному имени. Возвращает JSON-массив ClassRecord.")]
    async fn get_class(&self, Parameters(p): Parameters<NameParams>) -> String {
        tools::get_class(self, p.name).await
    }

    #[tool(description = "Найти вызывателей функции (callers). Возвращает JSON-массив CallRecord.")]
    async fn get_callers(&self, Parameters(p): Parameters<FunctionNameParams>) -> String {
        tools::get_callers(self, p.function_name, p.language).await
    }

    #[tool(description = "Найти что вызывает функция (callees). Возвращает JSON-массив CallRecord.")]
    async fn get_callees(&self, Parameters(p): Parameters<FunctionNameParams>) -> String {
        tools::get_callees(self, p.function_name, p.language).await
    }

    #[tool(description = "Универсальный поиск символа по точному имени. Возвращает JSON-объект {functions, classes, variables, imports}.")]
    async fn find_symbol(&self, Parameters(p): Parameters<NameParams>) -> String {
        tools::find_symbol(self, p.name, p.language).await
    }

    #[tool(description = "Импорты файла (file_id) или модуля (module). Возвращает JSON-массив ImportRecord.")]
    async fn get_imports(&self, Parameters(p): Parameters<ImportParams>) -> String {
        tools::get_imports(self, p.file_id, p.module, p.language).await
    }

    #[tool(description = "Карта файла: все функции, классы, импорты, переменные. Возвращает JSON-объект FileSummary.")]
    async fn get_file_summary(&self, Parameters(p): Parameters<FilePathParams>) -> String {
        tools::get_file_summary(self, p.path).await
    }

    #[tool(description = "Статистика индекса: файлы, функции, классы, импорты, вызовы, переменные. Плюс статус демона и папки.")]
    async fn get_stats(&self) -> String {
        tools::get_stats(self).await
    }

    #[tool(description = "FTS поиск по текстовым файлам (md, txt, yaml, toml). Возвращает JSON-массив [{path, snippet}].")]
    async fn search_text(&self, Parameters(p): Parameters<SearchParams>) -> String {
        tools::search_text(self, p.query, p.limit, p.language).await
    }

    #[tool(description = "Поиск по телам функций и классов. pattern — подстрока (LIKE), regex — регулярное выражение (REGEXP). Возвращает [{file_path, name, kind, line_start, line_end, match_lines, match_count?}].")]
    async fn grep_body(&self, Parameters(p): Parameters<GrepBodyParams>) -> String {
        tools::grep_body(self, p.pattern, p.regex, p.language, p.limit).await
    }

    #[tool(description = "Проверка живости MCP-сервера и подключённого демона индексации. Возвращает JSON с информацией о MCP и демоне.")]
    async fn health(&self) -> String {
        tools::health(self).await
    }
}

#[tool_handler]
impl ServerHandler for CodeIndexServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "code-index-mcp",
                env!("CARGO_PKG_VERSION"),
            ))
    }
}
