/// MCP-сервер индексатора кода
use std::sync::Arc;

use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::storage::Storage;

pub mod tools;

// ── Структуры параметров инструментов ────────────────────────────────────────

/// Параметры поиска с необязательным лимитом
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct SearchParams {
    /// Поисковый запрос (по имени, docstring, телу)
    pub query: String,
    /// Максимальное количество результатов (по умолчанию 20)
    pub limit: Option<usize>,
}

/// Параметры поиска по точному имени
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct NameParams {
    /// Точное имя (функции, класса или символа)
    pub name: String,
}

/// Параметры для поиска вызывателей/вызываемых
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct FunctionNameParams {
    /// Имя функции
    pub function_name: String,
}

/// Параметры получения импортов (file_id или имя модуля)
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ImportParams {
    /// Числовой file_id файла
    pub file_id: Option<i64>,
    /// Имя модуля для поиска импортов
    pub module: Option<String>,
}

/// Параметры получения карты файла
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct FilePathParams {
    /// Путь к файлу (как в индексе)
    pub path: String,
}

// ── Структура MCP-сервера ─────────────────────────────────────────────────────

/// Основная структура MCP-сервера индексатора кода
#[derive(Clone)]
pub struct CodeIndexServer {
    /// Хранилище — защищено мьютексом для потокобезопасного доступа
    pub storage: Arc<Mutex<Storage>>,
    /// Роутер инструментов — генерируется макросом tool_router
    tool_router: ToolRouter<Self>,
}

impl CodeIndexServer {
    /// Создать новый сервер с готовым Storage
    pub fn new(storage: Storage) -> Self {
        Self {
            storage: Arc::new(Mutex::new(storage)),
            tool_router: Self::tool_router(),
        }
    }
}

// ── Регистрация инструментов ──────────────────────────────────────────────────

#[tool_router]
impl CodeIndexServer {
    /// Полнотекстовый поиск функций по запросу (FTS5).
    /// Возвращает JSON-массив найденных функций.
    #[tool(description = "FTS поиск функций: по имени, docstring, телу. Возвращает JSON-массив FunctionRecord.")]
    async fn search_function(&self, Parameters(p): Parameters<SearchParams>) -> String {
        tools::search_function(self, p.query, p.limit).await
    }

    /// Полнотекстовый поиск классов по запросу (FTS5).
    /// Возвращает JSON-массив найденных классов.
    #[tool(description = "FTS поиск классов: по имени, docstring, телу. Возвращает JSON-массив ClassRecord.")]
    async fn search_class(&self, Parameters(p): Parameters<SearchParams>) -> String {
        tools::search_class(self, p.query, p.limit).await
    }

    /// Найти функцию по точному имени.
    /// Возвращает JSON-массив совпадений.
    #[tool(description = "Найти функцию по точному имени. Возвращает JSON-массив FunctionRecord.")]
    async fn get_function(&self, Parameters(p): Parameters<NameParams>) -> String {
        tools::get_function(self, p.name).await
    }

    /// Найти класс по точному имени.
    /// Возвращает JSON-массив совпадений.
    #[tool(description = "Найти класс по точному имени. Возвращает JSON-массив ClassRecord.")]
    async fn get_class(&self, Parameters(p): Parameters<NameParams>) -> String {
        tools::get_class(self, p.name).await
    }

    /// Найти все места, где вызывается данная функция (callers).
    /// Возвращает JSON-массив записей вызовов.
    #[tool(description = "Найти вызывателей функции (callers). Возвращает JSON-массив CallRecord.")]
    async fn get_callers(&self, Parameters(p): Parameters<FunctionNameParams>) -> String {
        tools::get_callers(self, p.function_name).await
    }

    /// Найти все функции, которые вызывает данная функция (callees).
    /// Возвращает JSON-массив записей вызовов.
    #[tool(description = "Найти что вызывает функция (callees). Возвращает JSON-массив CallRecord.")]
    async fn get_callees(&self, Parameters(p): Parameters<FunctionNameParams>) -> String {
        tools::get_callees(self, p.function_name).await
    }

    /// Универсальный поиск символа: функции + классы + переменные + импорты.
    /// Возвращает JSON-объект SymbolSearchResult.
    #[tool(description = "Универсальный поиск символа по точному имени. Возвращает JSON-объект {functions, classes, variables, imports}.")]
    async fn find_symbol(&self, Parameters(p): Parameters<NameParams>) -> String {
        tools::find_symbol(self, p.name).await
    }

    /// Получить импорты файла (по file_id) или импорты модуля (по имени).
    /// Возвращает JSON-массив ImportRecord.
    #[tool(description = "Импорты файла (file_id) или модуля (module). Возвращает JSON-массив ImportRecord.")]
    async fn get_imports(&self, Parameters(p): Parameters<ImportParams>) -> String {
        tools::get_imports(self, p.file_id, p.module).await
    }

    /// Получить сводную карту файла: все функции, классы, импорты, переменные.
    /// Возвращает JSON-объект FileSummary.
    #[tool(description = "Карта файла: все функции, классы, импорты, переменные. Возвращает JSON-объект FileSummary.")]
    async fn get_file_summary(&self, Parameters(p): Parameters<FilePathParams>) -> String {
        tools::get_file_summary(self, p.path).await
    }

    /// Статистика базы данных индекса.
    /// Возвращает JSON-объект DbStats.
    #[tool(description = "Статистика индекса: файлы, функции, классы, импорты, вызовы, переменные. Возвращает JSON-объект DbStats.")]
    async fn get_stats(&self) -> String {
        tools::get_stats(self).await
    }

    /// Полнотекстовый поиск по текстовым файлам (markdown, txt, yaml, toml и др.).
    /// Возвращает JSON-массив объектов {path, snippet}.
    #[tool(description = "FTS поиск по текстовым файлам (md, txt, yaml, toml). Возвращает JSON-массив [{path, snippet}].")]
    async fn search_text(&self, Parameters(p): Parameters<SearchParams>) -> String {
        tools::search_text(self, p.query, p.limit).await
    }
}

// ── Реализация ServerHandler ──────────────────────────────────────────────────

/// Маршрутизирует запросы к инструментам через tool_router
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
