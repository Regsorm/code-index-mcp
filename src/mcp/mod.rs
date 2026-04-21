// MCP-сервер (v0.5+) — тонкий read-only слой над SQLite-индексом.
//
// Multi-repo: один stdio-процесс держит открытыми несколько SQLite-баз
// (по одной на репозиторий), диспатч по параметру `repo` в каждом tool-call.
// Перед каждым tool-call проверяет у демона статус папки для конкретного репо.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
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

// ── Один репозиторий, обслуживаемый сервером ───────────────────────────────

/// Одна запись в репо-карте: путь к корню + открытый SQLite в read-only.
pub struct RepoEntry {
    /// Канонический путь к корню проекта (параметр `?path=` для /path-status у демона).
    pub root_path: PathBuf,
    /// SQLite-подключение. Mutex нужен для сериализации доступа к Connection
    /// (rusqlite не Sync). БД открыта read-only, конкуренции на запись нет.
    pub storage: Arc<Mutex<Storage>>,
}

// ── Параметры инструментов ─────────────────────────────────────────────────
//
// Везде добавлен `repo: String` — алиас репозитория, выбранный при старте сервера
// (см. `code-index serve --path <alias>=<dir>`). Если передан неизвестный alias —
// возвращается ToolUnavailable::NotStarted.

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct SearchParams {
    /// Алиас репозитория (из --path alias=dir при запуске сервера).
    pub repo: String,
    pub query: String,
    pub limit: Option<usize>,
    pub language: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct NameParams {
    /// Алиас репозитория (из --path alias=dir при запуске сервера).
    pub repo: String,
    pub name: String,
    pub language: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct FunctionNameParams {
    /// Алиас репозитория (из --path alias=dir при запуске сервера).
    pub repo: String,
    pub function_name: String,
    pub language: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ImportParams {
    /// Алиас репозитория (из --path alias=dir при запуске сервера).
    pub repo: String,
    pub file_id: Option<i64>,
    pub module: Option<String>,
    pub language: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct FilePathParams {
    /// Алиас репозитория (из --path alias=dir при запуске сервера).
    pub repo: String,
    pub path: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GrepBodyParams {
    /// Алиас репозитория (из --path alias=dir при запуске сервера).
    pub repo: String,
    pub pattern: Option<String>,
    pub regex: Option<String>,
    pub language: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct StatsParams {
    /// Алиас репозитория. Если не указан — возвращается статистика по всем подключённым репо.
    pub repo: Option<String>,
}

// ── Сервер ───────────────────────────────────────────────────────────────────

/// Read-only MCP-сервер индексатора. Держит N открытых репозиториев
/// (по одному на alias), диспатч по параметру `repo` в каждом tool-call.
#[derive(Clone)]
pub struct CodeIndexServer {
    /// Карта alias → RepoEntry. BTreeMap для детерминированного порядка в логах и /health.
    pub repos: Arc<BTreeMap<String, RepoEntry>>,
    /// Роутер MCP-инструментов (генерируется макросом).
    tool_router: ToolRouter<Self>,
}

impl CodeIndexServer {
    /// Создать сервер из уже собранной карты репо.
    pub fn with_repos(repos: BTreeMap<String, RepoEntry>) -> Self {
        Self {
            repos: Arc::new(repos),
            tool_router: Self::tool_router(),
        }
    }

    /// Удобство: собрать сервер из массива (alias, root_path, db_path), открывая все БД read-only.
    pub fn open_readonly_multi(entries: Vec<(String, PathBuf, PathBuf)>) -> anyhow::Result<Self> {
        let mut map = BTreeMap::new();
        for (alias, root_path, db_path) in entries {
            let storage = Storage::open_file_readonly(&db_path)?;
            map.insert(alias, RepoEntry {
                root_path,
                storage: Arc::new(Mutex::new(storage)),
            });
        }
        Ok(Self::with_repos(map))
    }

    /// Legacy-совместимый конструктор: одно репо под алиасом `default`.
    pub fn open_readonly(root_path: PathBuf, db_path: &Path) -> anyhow::Result<Self> {
        Self::open_readonly_multi(vec![("default".to_string(), root_path, db_path.to_path_buf())])
    }

    /// Конструктор для тестов/встраивания — принимает уже открытое хранилище под alias.
    pub fn with_storage(alias: impl Into<String>, root_path: PathBuf, storage: Storage) -> Self {
        let mut map = BTreeMap::new();
        map.insert(alias.into(), RepoEntry {
            root_path,
            storage: Arc::new(Mutex::new(storage)),
        });
        Self::with_repos(map)
    }

    /// Список алиасов для описаний и диагностики.
    pub fn repo_aliases(&self) -> Vec<String> {
        self.repos.keys().cloned().collect()
    }

    /// Получить RepoEntry по alias или вернуть ToolUnavailable::NotStarted JSON.
    fn resolve_repo(&self, alias: &str) -> Result<&RepoEntry, String> {
        self.repos.get(alias).ok_or_else(|| {
            tools::format_unavailable(crate::daemon_core::ipc::ToolUnavailable::NotStarted {
                message: format!(
                    "Неизвестный repo '{}'. Доступные: {:?}. Укажите один из алиасов, переданных в --path alias=dir при запуске сервера.",
                    alias,
                    self.repo_aliases()
                ),
            })
        })
    }
}

// ── MCP tools ──────────────────────────────────────────────────────────────
//
// В каждом handler'е: resolve_repo → entry → tools::foo(entry, ...). Ошибка
// ресолва возвращается как ToolUnavailable JSON (совместимо с существующими
// клиентами, которые уже умеют парсить `status:"not_started"`).

#[tool_router]
impl CodeIndexServer {
    #[tool(description = "FTS поиск функций по указанному репо: по имени, docstring, телу. Возвращает JSON-массив FunctionRecord.")]
    async fn search_function(&self, Parameters(p): Parameters<SearchParams>) -> String {
        let entry = match self.resolve_repo(&p.repo) { Ok(e) => e, Err(j) => return j };
        tools::search_function(entry, p.query, p.limit, p.language).await
    }

    #[tool(description = "FTS поиск классов по указанному репо: по имени, docstring, телу. Возвращает JSON-массив ClassRecord.")]
    async fn search_class(&self, Parameters(p): Parameters<SearchParams>) -> String {
        let entry = match self.resolve_repo(&p.repo) { Ok(e) => e, Err(j) => return j };
        tools::search_class(entry, p.query, p.limit, p.language).await
    }

    #[tool(description = "Найти функцию по точному имени в указанном репо. Возвращает JSON-массив FunctionRecord.")]
    async fn get_function(&self, Parameters(p): Parameters<NameParams>) -> String {
        let entry = match self.resolve_repo(&p.repo) { Ok(e) => e, Err(j) => return j };
        tools::get_function(entry, p.name).await
    }

    #[tool(description = "Найти класс по точному имени в указанном репо. Возвращает JSON-массив ClassRecord.")]
    async fn get_class(&self, Parameters(p): Parameters<NameParams>) -> String {
        let entry = match self.resolve_repo(&p.repo) { Ok(e) => e, Err(j) => return j };
        tools::get_class(entry, p.name).await
    }

    #[tool(description = "Найти вызывателей функции (callers) в указанном репо. Возвращает JSON-массив CallRecord.")]
    async fn get_callers(&self, Parameters(p): Parameters<FunctionNameParams>) -> String {
        let entry = match self.resolve_repo(&p.repo) { Ok(e) => e, Err(j) => return j };
        tools::get_callers(entry, p.function_name, p.language).await
    }

    #[tool(description = "Найти что вызывает функция (callees) в указанном репо. Возвращает JSON-массив CallRecord.")]
    async fn get_callees(&self, Parameters(p): Parameters<FunctionNameParams>) -> String {
        let entry = match self.resolve_repo(&p.repo) { Ok(e) => e, Err(j) => return j };
        tools::get_callees(entry, p.function_name, p.language).await
    }

    #[tool(description = "Универсальный поиск символа по точному имени в указанном репо. Возвращает JSON-объект {functions, classes, variables, imports}.")]
    async fn find_symbol(&self, Parameters(p): Parameters<NameParams>) -> String {
        let entry = match self.resolve_repo(&p.repo) { Ok(e) => e, Err(j) => return j };
        tools::find_symbol(entry, p.name, p.language).await
    }

    #[tool(description = "Импорты файла (file_id) или модуля (module) в указанном репо. Возвращает JSON-массив ImportRecord.")]
    async fn get_imports(&self, Parameters(p): Parameters<ImportParams>) -> String {
        let entry = match self.resolve_repo(&p.repo) { Ok(e) => e, Err(j) => return j };
        tools::get_imports(entry, p.file_id, p.module, p.language).await
    }

    #[tool(description = "Карта файла в указанном репо: функции, классы, импорты, переменные. Возвращает JSON-объект FileSummary.")]
    async fn get_file_summary(&self, Parameters(p): Parameters<FilePathParams>) -> String {
        let entry = match self.resolve_repo(&p.repo) { Ok(e) => e, Err(j) => return j };
        tools::get_file_summary(entry, p.path).await
    }

    #[tool(description = "Статистика индекса. Если repo указан — для одного репо, иначе — массив по всем подключённым репо.")]
    async fn get_stats(&self, Parameters(p): Parameters<StatsParams>) -> String {
        tools::get_stats(self, p.repo).await
    }

    #[tool(description = "FTS поиск по текстовым файлам (md, txt, yaml, toml) в указанном репо. Возвращает JSON-массив [{path, snippet}].")]
    async fn search_text(&self, Parameters(p): Parameters<SearchParams>) -> String {
        let entry = match self.resolve_repo(&p.repo) { Ok(e) => e, Err(j) => return j };
        tools::search_text(entry, p.query, p.limit, p.language).await
    }

    #[tool(description = "Поиск по телам функций и классов в указанном репо. pattern — подстрока (LIKE), regex — регулярное выражение (REGEXP). Возвращает [{file_path, name, kind, line_start, line_end, match_lines, match_count?}].")]
    async fn grep_body(&self, Parameters(p): Parameters<GrepBodyParams>) -> String {
        let entry = match self.resolve_repo(&p.repo) { Ok(e) => e, Err(j) => return j };
        tools::grep_body(entry, p.pattern, p.regex, p.language, p.limit).await
    }

    #[tool(description = "Проверка живости MCP-сервера и демона индексации по всем подключённым репо. Возвращает JSON.")]
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
