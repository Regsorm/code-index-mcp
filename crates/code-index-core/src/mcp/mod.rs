// MCP-сервер (v0.5+) — тонкий read-only слой над SQLite-индексом.
//
// Multi-repo: один stdio-процесс держит открытыми несколько SQLite-баз
// (по одной на репозиторий), диспатч по параметру `repo` в каждом tool-call.
// Перед каждым tool-call проверяет у демона статус папки для конкретного репо.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolRequestParams, CallToolResult, Implementation, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
    },
    service::{NotificationContext, Peer, RequestContext},
    tool, tool_router, ErrorData, RoleServer, ServerHandler,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::extension::{IndexTool, ProcessorRegistry};
use crate::federation::client::RemoteClientPool;
use crate::federation::repos::FederatedRepo;
use crate::storage::{PoolConfig, Storage, StoragePool};

pub mod config_watch;
pub mod tools;

/// IP по умолчанию для legacy-конструкторов (моно-режим без serve.toml).
/// Все репо считаются local на этом IP.
pub(crate) const LEGACY_OWN_IP: &str = "127.0.0.1";

// ── Один репозиторий, обслуживаемый сервером ───────────────────────────────

/// Одна запись в репо-карте.
///
/// Для local-репо заполнены `root_path` и `storage` — tool-handler читает
/// данные из локального SQLite. Для remote — оба поля `None`, `is_local=false`,
/// и tool-handler форвардит запрос через `RemoteClientPool` по `ip`.
pub struct RepoEntry {
    /// Канонический путь к корню проекта (только для local).
    pub root_path: Option<PathBuf>,
    /// Пул read-only SQLite-соединений (только для local). Несколько соединений
    /// позволяют читать индекс одновременно — запросы к одному репо больше не
    /// сериализуются на едином мьютексе. БД read-only, на запись не конкурирует.
    pub storage: Option<Arc<StoragePool>>,
    /// IP машины, на которой лежит репо (для решения local vs remote и логов).
    pub ip: String,
    /// Порт удалённого `code-index serve` для federate-форвардинга.
    /// Для remote-репо — обязателен (default `DEFAULT_REMOTE_PORT` из
    /// `serve.toml::ServePathEntry::effective_port`). Для local-репо —
    /// заполнен тем же значением, что и у remote (информационно), но не
    /// используется: tool-handler идёт по local-ветке.
    pub port: u16,
    /// `true` если репо обслуживается этим процессом (`ip == own_ip`).
    pub is_local: bool,
    /// Преобладающий язык, под который репо классифицирован. Определяется
    /// при загрузке конфига (явно из TOML или auto-detect). `None` — пока
    /// не определён (например, для remote-репо без локального daemon.toml).
    /// Используется для conditional registration MCP-tools и для
    /// валидации совместимости в `IndexTool::execute`.
    pub language: Option<String>,
}

impl RepoEntry {
    /// Ссылка на корневой путь — для local. Panic для remote (ловит баги
    /// диспатчера: tools::* не должны вызываться для remote).
    pub fn local_root(&self) -> &Path {
        self.root_path.as_ref().unwrap_or_else(|| {
            panic!("local_root() вызван для remote-репо ip={} — это баг диспатчера", self.ip)
        })
    }

    /// Ссылка на пул соединений — для local. Panic для remote.
    pub fn storage_pool(&self) -> &Arc<StoragePool> {
        self.storage.as_ref().unwrap_or_else(|| {
            panic!(
                "storage_pool() вызван для remote-репо ip={} — это баг диспатчера",
                self.ip
            )
        })
    }
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
    /// Glob по path для сужения поиска (Phase 1, post-filter в MCP-слое).
    /// Например `src/**/*.py` или `Documents/**`.
    pub path_glob: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct NameParams {
    /// Алиас репозитория (из --path alias=dir при запуске сервера).
    pub repo: String,
    pub name: String,
    pub language: Option<String>,
    /// Glob по path (Phase 1, post-filter).
    pub path_glob: Option<String>,
}

/// Параметры для get_function/get_class с поддержкой МАССОВОГО режима:
/// одиночное `name` ИЛИ список `names` (структуры нескольких символов одним
/// вызовом → {results:[...]}). find_symbol на это не переводим — у него свой
/// одиночный NameParams.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct MultiNameParams {
    /// Алиас репозитория (из --path alias=dir при запуске сервера).
    pub repo: String,
    /// Имя ОДНОГО символа. Для нескольких — `names`.
    pub name: Option<String>,
    /// Список имён для МАССОВОГО запроса. Применяй ТОЛЬКО когда заведомо нужны все из набора (см. описание инструмента); отбираешь релевантные — по одному 'name'.
    /// Ответ — {results:[...]} в порядке запроса. Передавайте либо `name`, либо `names`.
    pub names: Option<Vec<String>>,
    pub language: Option<String>,
    /// Glob по path (Phase 1, post-filter).
    pub path_glob: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct FunctionNameParams {
    /// Алиас репозитория (из --path alias=dir при запуске сервера).
    pub repo: String,
    pub function_name: String,
    pub language: Option<String>,
    /// Cap на число рёбер графа вызовов (default 200). При обрезке в ответе —
    /// {truncated, total, limit}. Защищает от мегабайтных ответов на «горячих» функциях.
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct FindPathParams {
    /// Алиас репозитория (из --path alias=dir при запуске сервера).
    pub repo: String,
    /// Имя функции-источника (caller) — начало пути.
    pub from: String,
    /// Имя функции-цели (callee) — конец пути.
    pub to: String,
    /// Максимальная длина пути (число рёбер), [1..10]. По умолчанию 5.
    pub max_depth: Option<i64>,
    /// Опциональный фильтр по языку файла-источника ребра (rust/python/bsl/…).
    pub language: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct CallTreeParams {
    /// Алиас репозитория (из --path alias=dir при запуске сервера).
    pub repo: String,
    /// Имя функции-корня дерева.
    pub root: String,
    /// Направление: "callees"/"down" (что вызывает root, вглубь — по умолчанию)
    /// или "callers"/"up" (кто вызывает root).
    pub direction: Option<String>,
    /// Глубина дерева (число уровней), [1..10]. По умолчанию 3.
    pub max_depth: Option<i64>,
    /// Cap на число рёбер, [1..5000]. По умолчанию 200; при обрезке — truncated=true.
    pub max_nodes: Option<i64>,
    /// Опциональный фильтр по языку файла-источника ребра.
    pub language: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ImportParams {
    /// Алиас репозитория (из --path alias=dir при запуске сервера).
    pub repo: String,
    pub file_id: Option<i64>,
    pub module: Option<String>,
    pub language: Option<String>,
    /// Cap на число импортов в ответе (default 200). При обрезке — {truncated, total, limit}.
    pub limit: Option<usize>,
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
    /// Подстрока (LIKE).
    pub pattern: Option<String>,
    /// Регулярное выражение (REGEXP).
    pub regex: Option<String>,
    /// Алиас для `regex` (частая путаница: модели передают `query`).
    /// Если ни `pattern`, ни `regex` не заданы — используется `query` как regex.
    pub query: Option<String>,
    pub language: Option<String>,
    /// Максимум находок в ответе. Default 100. При обрезке — `truncated=true`.
    pub limit: Option<usize>,
    /// Glob по path для сужения поиска. SQL-pushdown.
    pub path_glob: Option<String>,
    /// Сколько строк до/после совпадения возвращать в `context`.
    /// 0 (по умолчанию) — без контекста, как раньше.
    pub context_lines: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct StatsParams {
    /// Алиас репозитория. Если не указан — возвращается статистика по всем подключённым репо.
    pub repo: Option<String>,
}

// ── Phase 1 параметры (v0.7.0) ──

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct StatFileParams {
    pub repo: String,
    pub path: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ListFilesParams {
    pub repo: String,
    /// Glob по path (`**/*.py`, `Documents/**/*.bsl`). Опционально.
    pub pattern: Option<String>,
    /// Префикс по path (`src/auth/`). Опционально.
    pub path_prefix: Option<String>,
    pub language: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ReadFileParams {
    pub repo: String,
    pub path: String,
    /// 1-based, inclusive. None — с начала.
    pub line_start: Option<usize>,
    /// 1-based, inclusive. None — до конца.
    pub line_end: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GrepTextParams {
    pub repo: String,
    /// Регулярное выражение (синтаксис crate `regex`).
    pub regex: Option<String>,
    /// Алиас для `regex` (частая путаница: модели передают `query`).
    /// Если `regex` не задан — используется `query` как regex.
    pub query: Option<String>,
    /// Glob по path. Хотя бы один из {path_glob, language} желателен —
    /// иначе работает full-scan по всем text-файлам.
    pub path_glob: Option<String>,
    pub language: Option<String>,
    /// Максимум находок в ответе (default 100; при заданном path_glob/language — 500). При обрезке — `truncated=true`.
    pub limit: Option<usize>,
    /// Сколько строк до/после совпадения возвращать в `context`. 0 — без контекста.
    pub context_lines: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GrepCodeParams {
    pub repo: String,
    /// Регулярное выражение (синтаксис crate `regex`).
    pub regex: Option<String>,
    /// Алиас для `regex` (частая путаница: модели передают `query`).
    /// Если `regex` не задан — используется `query` как regex.
    pub query: Option<String>,
    /// Glob по path. Хотя бы один из {path_glob, language} желателен —
    /// иначе full-scan по всем code-файлам репо (zstd-decode каждого) дорогой.
    pub path_glob: Option<String>,
    pub language: Option<String>,
    /// Максимум совпадений в ответе. Default 100. При достижении — `truncated=true`.
    pub limit: Option<usize>,
    /// Сколько строк до/после совпадения возвращать в `context`. 0 — без контекста.
    pub context_lines: Option<usize>,
}

/// Универсальные параметры для federation-форварда extension-tools
/// (`get_object_structure`, `get_form_handlers`, `find_path_bsl` и т.д.).
///
/// Введён в v0.8.1 как замена per-tool routes: extension-tools меняются
/// чаще, чем core, и заводить отдельный route на каждый — лишний шум.
/// `tool_name` — имя tool (например, `"get_object_structure"`),
/// `args` — оригинальные arguments вызова MCP (включая `repo`).
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ExtensionToolParams {
    /// Имя extension-tool как он зарегистрирован в `IndexTool::name()`.
    pub tool_name: String,
    /// Полный набор arguments в формате MCP (произвольный JSON-объект).
    /// `repo` извлекается из этого поля на target-стороне.
    pub args: serde_json::Value,
}

// ── Сервер ───────────────────────────────────────────────────────────────────

/// Read-only MCP-сервер индексатора. Держит N открытых репозиториев
/// (по одному на alias), диспатч по параметру `repo` в каждом tool-call.
///
/// В федеративном режиме (`from_federated`): часть репо может быть remote —
/// для них `RepoEntry.is_local=false`, tool-handler форвардит запрос
/// в `clients` по `ip`.
///
/// Инструменты с массовым режимом и их plural-параметр (`tool`, `plural`).
/// Управляется белым списком `daemon.toml [mcp].mass_mode_tools`: инструмент
/// **в списке** публикует plural-параметр (модель видит/может батчить);
/// **не в списке** — `list_tools` убирает параметр из схемы, `call_tool`
/// отбивает вызов с ним. Пустой список (дефолт v0.28.0) → массовый режим
/// выключен у всех. Причина дефолта-выкл — см. [`crate::daemon_core::config::McpSection`].
pub(crate) const MASS_MODE_PARAMS: &[(&str, &str)] = &[
    ("get_function", "names"),
    ("get_class", "names"),
    ("get_object_structure", "full_names"),
];

/// Поверх жёстко прописанных core-tools (макрос `#[tool_router]`) сервер
/// держит набор «extension-tools» — MCP-инструментов, поставляемых
/// активными `LanguageProcessor`-ами. Их подбор зависит от того, какие
/// языки реально используются репозиториями (`active_languages`):
/// например, BSL-tools (`get_object_structure` и т.д.) попадают в
/// `tools/list` только если хотя бы один репо имеет `language = "bsl"`.
/// Сама интеграция в MCP-протокол (override `list_tools`/`call_tool`)
/// сделана на этапе 1.6.
#[derive(Clone)]
pub struct CodeIndexServer {
    /// Карта alias → RepoEntry. BTreeMap для детерминированного порядка в логах и /health.
    pub repos: Arc<BTreeMap<String, RepoEntry>>,
    /// Собственный IP машины (из `serve.toml [me].ip`) — для логов и диагностики.
    pub own_ip: Arc<String>,
    /// Пул HTTP-клиентов к удалённым serve-нодам (lazy init).
    pub clients: Arc<RemoteClientPool>,
    /// Роутер MCP-инструментов (генерируется макросом).
    tool_router: ToolRouter<Self>,
    /// Множество активных языков репозиториев. Обёрнуто в `ArcSwap`,
    /// чтобы file-watch на `daemon.toml` (этап 1.7) мог атомарно
    /// заменить содержимое без блокировок чтения. Тип внутри — `Arc`
    /// для дешёвого клонирования при чтении.
    pub active_languages: Arc<ArcSwap<BTreeSet<String>>>,
    /// Tool-инструменты от активных `LanguageProcessor`-ов. Тоже `ArcSwap`,
    /// так как пересобирается одновременно с `active_languages`.
    pub extension_tools: Arc<ArcSwap<Vec<Arc<dyn IndexTool>>>>,
    /// Реестр процессоров. Хранится отдельно, чтобы `reload_extensions`
    /// мог пересобрать `extension_tools` после изменения `active_languages`.
    /// `None` — legacy-сценарий без registry.
    pub registry: Arc<Option<ProcessorRegistry>>,
    /// Peer клиента для отправки `notifications/tools/list_changed`.
    /// Заполняется в `on_initialized`, очищается при разрыве сессии
    /// (rmcp дёргает `on_initialized` для каждой сессии). Mutex поверх
    /// `Option<Peer>` нужен, потому что `Peer` не Sync без обёртки.
    pub peer: Arc<Mutex<Option<Peer<RoleServer>>>>,
    /// Опциональный whitelist MCP-инструментов из `daemon.toml [tools].enabled`.
    /// `None` — фильтр не применяется (все зарегистрированные tools доступны),
    /// `Some(set)` — в `tools/list` уходят только tools с именами из множества,
    /// а `tools/call` для остальных возвращает `-32602 Invalid params`.
    /// Двойная защита нужна потому, что модель может вызвать tool вне
    /// `tools/list` (из системного промпта/CLAUDE.md) — фильтр в `list_tools`
    /// её не остановит, без проверки в `call_tool` это уйдёт в router.
    pub allowed_tools: Arc<Option<BTreeSet<String>>>,
    /// Белый список инструментов, публикующих массовый режим (`names[]`/
    /// `full_names[]`). Пустой (дефолт с v0.28.0) → массовый режим выключен у
    /// всех: `list_tools` убирает plural-параметр из схемы, `call_tool`
    /// отбивает вызов с ним. Управляется `daemon.toml [mcp].mass_mode_tools`.
    /// Перечень массовых инструментов — [`MASS_MODE_PARAMS`].
    pub mass_mode_tools: Arc<BTreeSet<String>>,
}

impl CodeIndexServer {
    /// Создать сервер из уже собранной карты репо. own_ip и clients задаются
    /// дефолтами для legacy-сценария (моно-режим, локальный пул).
    /// Активные языки и extension-tools вычисляются по `RepoEntry.language`
    /// (если у каких-то записей оно заполнено), но без `ProcessorRegistry`
    /// extension-tools остаётся пустым.
    pub fn with_repos(repos: BTreeMap<String, RepoEntry>) -> Self {
        let active_languages = collect_active_languages(&repos);
        Self {
            repos: Arc::new(repos),
            own_ip: Arc::new(LEGACY_OWN_IP.to_string()),
            clients: Arc::new(RemoteClientPool::with_defaults()),
            tool_router: Self::tool_router(),
            active_languages: Arc::new(ArcSwap::from_pointee(active_languages)),
            extension_tools: Arc::new(ArcSwap::from_pointee(Vec::new())),
            registry: Arc::new(None),
            peer: Arc::new(Mutex::new(None)),
            allowed_tools: Arc::new(None),
            mass_mode_tools: Arc::new(BTreeSet::new()),
        }
    }

    /// Создать сервер из карты репо и реестра процессоров.
    /// Активные языки берутся из `RepoEntry.language`; extension-tools
    /// собираются из `additional_tools()` каждого зарегистрированного
    /// процессора, чьё имя входит в множество активных языков.
    pub fn with_repos_and_registry(
        repos: BTreeMap<String, RepoEntry>,
        registry: ProcessorRegistry,
    ) -> Self {
        let active_languages = collect_active_languages(&repos);
        let extension_tools = collect_extension_tools(&active_languages, &registry);
        Self {
            repos: Arc::new(repos),
            own_ip: Arc::new(LEGACY_OWN_IP.to_string()),
            clients: Arc::new(RemoteClientPool::with_defaults()),
            tool_router: Self::tool_router(),
            active_languages: Arc::new(ArcSwap::from_pointee(active_languages)),
            extension_tools: Arc::new(ArcSwap::from_pointee(extension_tools)),
            registry: Arc::new(Some(registry)),
            peer: Arc::new(Mutex::new(None)),
            allowed_tools: Arc::new(None),
            mass_mode_tools: Arc::new(BTreeSet::new()),
        }
    }

    /// Федеративный конструктор: принимает реестр из `federation::repos::merge`,
    /// собственный IP, опциональный реестр процессоров и мапу local-aliases →
    /// language (из daemon.toml). Для local-записей открывает SQLite read-only
    /// и проставляет `RepoEntry.language` из `local_languages`, чтобы
    /// `collect_active_languages` нашёл нужные языки и conditional registration
    /// зарегистрировал extension-tools (`get_object_structure` и др.) в
    /// `tools/list`. Для remote-записей storage/root_path/language=None —
    /// они приходят через federation, активный язык по ним неизвестен.
    pub fn from_federated(
        repos: Vec<FederatedRepo>,
        own_ip: String,
        registry: Option<ProcessorRegistry>,
        local_languages: BTreeMap<String, String>,
        pool_cfg: PoolConfig,
    ) -> anyhow::Result<Self> {
        let mut map = BTreeMap::new();
        for repo in repos {
            let entry = if repo.is_local {
                let db_path = repo.db_path.as_ref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "Локальный репо '{}' (ip={}) без db_path — баг merge.",
                        repo.alias,
                        repo.ip
                    )
                })?;
                let storage = StoragePool::open_file_readonly(db_path, pool_cfg)?;
                RepoEntry {
                    root_path: repo.root_path,
                    storage: Some(storage),
                    ip: repo.ip,
                    port: repo.port,
                    is_local: true,
                    language: local_languages.get(&repo.alias).cloned(),
                }
            } else {
                RepoEntry {
                    root_path: None,
                    storage: None,
                    ip: repo.ip,
                    port: repo.port,
                    is_local: false,
                    language: None,
                }
            };
            map.insert(repo.alias, entry);
        }
        let active_languages = collect_active_languages(&map);
        let extension_tools = match registry.as_ref() {
            Some(reg) => collect_extension_tools(&active_languages, reg),
            None => Vec::new(),
        };
        Ok(Self {
            repos: Arc::new(map),
            own_ip: Arc::new(own_ip),
            clients: Arc::new(RemoteClientPool::with_defaults()),
            tool_router: Self::tool_router(),
            active_languages: Arc::new(ArcSwap::from_pointee(active_languages)),
            extension_tools: Arc::new(ArcSwap::from_pointee(extension_tools)),
            registry: Arc::new(registry),
            peer: Arc::new(Mutex::new(None)),
            allowed_tools: Arc::new(None),
            mass_mode_tools: Arc::new(BTreeSet::new()),
        })
    }

    /// Удобство: собрать сервер из массива (alias, root_path, db_path),
    /// открывая все БД read-only. Все репо считаются local на 127.0.0.1
    /// (моно-режим, обратная совместимость с rc5).
    pub fn open_readonly_multi(entries: Vec<(String, PathBuf, PathBuf)>) -> anyhow::Result<Self> {
        let mut map = BTreeMap::new();
        for (alias, root_path, db_path) in entries {
            let storage = StoragePool::open_file_readonly(&db_path, PoolConfig::default())?;
            map.insert(alias, RepoEntry {
                root_path: Some(root_path),
                storage: Some(storage),
                ip: LEGACY_OWN_IP.to_string(),
                port: crate::federation::client::DEFAULT_REMOTE_PORT,
                is_local: true,
                language: None,
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
            root_path: Some(root_path),
            storage: Some(StoragePool::single(storage)),
            ip: LEGACY_OWN_IP.to_string(),
            port: crate::federation::client::DEFAULT_REMOTE_PORT,
            is_local: true,
            language: None,
        });
        Self::with_repos(map)
    }

    /// Список алиасов для описаний и диагностики.
    pub fn repo_aliases(&self) -> Vec<String> {
        self.repos.keys().cloned().collect()
    }

    /// Builder для опционального whitelist'а MCP-инструментов
    /// (`daemon.toml [tools].enabled`).
    ///
    /// - `None` (или метод не вызван) → фильтр выключен, все зарегистрированные
    ///   tools отдаются в `tools/list` и доступны через `tools/call`.
    /// - `Some(set)` → в `tools/list` уходят только tools с именами из множества;
    ///   `tools/call` для tools вне набора возвращает ошибку
    ///   `-32602 Invalid params` с пояснением «tool 'X' is disabled by
    ///   `[tools].enabled` whitelist in daemon.toml».
    ///
    /// Пустое множество (`Some(BTreeSet::new())`) трактуется буквально —
    /// **ни один tool не доступен**. Если намерение — «снять фильтр», передавайте
    /// `None`, не пустой `Some`. На уровне `daemon.toml` пустой `enabled = []`
    /// в `cli.rs` маппится в `None` (см. конвертацию в `Commands::Serve`),
    /// поэтому пользователь не может случайно отключить вообще всё через
    /// пустой массив.
    pub fn with_allowed_tools(mut self, allowed: Option<BTreeSet<String>>) -> Self {
        self.allowed_tools = Arc::new(allowed);
        self
    }

    /// Применить whitelist tools из списка имён (типично из
    /// `daemon.toml [tools].enabled`).
    ///
    /// Это convenience-обёртка над [`Self::validate_whitelist`] +
    /// [`Self::with_allowed_tools`]: преобразует список в множество,
    /// логирует количество известных/неизвестных tools, при опечатках
    /// печатает warning, и устанавливает whitelist. Используется обоими
    /// ветками `code-index serve` (federation и моно) — это единственная
    /// точка интеграции с `daemon.toml [tools]`.
    ///
    /// Поведение:
    /// - Пустой `enabled` → фильтр выключен (`allowed_tools = None`),
    ///   все tools доступны. Лог: `[tools].enabled пуст — whitelist выключен`.
    /// - Непустой → фильтр применяется. Лог: `whitelist активен: N известных
    ///   tools разрешены (M в списке)`. Если в списке есть имена, не
    ///   соответствующие ни одному зарегистрированному tool, печатается
    ///   warning (но сервер запускается — неизвестные имена просто не
    ///   повлияют ни на что).
    ///
    /// Метод **потребляет self и возвращает Self** — удобно для chain'а
    /// после конструктора:
    /// ```ignore
    /// let server = CodeIndexServer::from_federated(...)?
    ///     .apply_tools_whitelist(&daemon_cfg.tools.enabled);
    /// ```
    pub fn apply_tools_whitelist(self, enabled: &[String]) -> Self {
        if enabled.is_empty() {
            tracing::info!(
                "[tools].enabled пуст — whitelist выключен, все tools доступны"
            );
            return self;
        }
        let allowed: BTreeSet<String> = enabled.iter().cloned().collect();
        let unknown = self.validate_whitelist(&allowed);
        if !unknown.is_empty() {
            tracing::warn!(
                "[tools].enabled содержит неизвестные имена tools (опечатка?): {:?}. \
                 Сервер запущен, но эти имена ни на что не повлияют — реальных tools \
                 с такими именами не зарегистрировано.",
                unknown
            );
        }
        let known_count = allowed.len().saturating_sub(unknown.len());
        tracing::info!(
            "[tools].enabled whitelist активен: {} известных tools разрешены ({} в списке)",
            known_count,
            allowed.len(),
        );
        self.with_allowed_tools(Some(allowed))
    }

    /// Builder: белый список инструментов с массовым режимом
    /// (`names[]`/`full_names[]`). Пустой набор (дефолт) → массовый режим
    /// выключен у всех. См. [`MASS_MODE_PARAMS`] и [`Self::apply_mass_mode_tools`].
    pub fn with_mass_mode_tools(mut self, tools: BTreeSet<String>) -> Self {
        self.mass_mode_tools = Arc::new(tools);
        self
    }

    /// Применить белый список массового режима из `daemon.toml [mcp].mass_mode_tools`.
    /// Пустой список → массовый режим выключен у всех (дефолт). Имена вне
    /// набора массовых инструментов ([`MASS_MODE_PARAMS`]) игнорируются с
    /// warning. Единственная точка интеграции с `[mcp]` для обеих веток serve.
    pub fn apply_mass_mode_tools(self, names: &[String]) -> Self {
        if names.is_empty() {
            tracing::info!(
                "[mcp].mass_mode_tools пуст — массовый режим выключен у всех инструментов"
            );
            return self.with_mass_mode_tools(BTreeSet::new());
        }
        let known: BTreeSet<&str> = MASS_MODE_PARAMS.iter().map(|(n, _)| *n).collect();
        let unknown: Vec<&String> = names.iter().filter(|n| !known.contains(n.as_str())).collect();
        if !unknown.is_empty() {
            tracing::warn!(
                "[mcp].mass_mode_tools содержит имена без массового режима (опечатка?): {:?}. \
                 Массовый режим есть только у: {:?}",
                unknown,
                known
            );
        }
        let set: BTreeSet<String> = names.iter().cloned().collect();
        tracing::info!(
            "[mcp].mass_mode_tools активен: массовый режим включён для {:?}",
            set
        );
        self.with_mass_mode_tools(set)
    }

    /// Проверить, какие имена из whitelist'а НЕ соответствуют ни одному
    /// зарегистрированному tool (опечатки, удалённые tools).
    ///
    /// Возвращает отсортированный список «неизвестных» имён. Сервер
    /// использует это для warning при старте; неизвестные имена не
    /// блокируют запуск, потому что в обычной работе они просто ни на
    /// что не повлияют (никакой реальный tool с таким именем не
    /// зарегистрирован → фильтрация для него тривиально проходит).
    pub fn validate_whitelist(&self, whitelist: &BTreeSet<String>) -> Vec<String> {
        let mut known: BTreeSet<String> = self
            .tool_router
            .list_all()
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        let snapshot = self.extension_tools.load();
        for ext in snapshot.iter() {
            known.insert(ext.name().to_string());
        }
        let mut unknown: Vec<String> = whitelist
            .iter()
            .filter(|name| !known.contains(name.as_str()))
            .cloned()
            .collect();
        unknown.sort();
        unknown
    }

    /// Имена активных языков. Возвращает копию (через клонирование строк),
    /// так как `ArcSwap::load()` отдаёт guard, а не статический срез.
    pub fn active_language_names(&self) -> Vec<String> {
        self.active_languages.load().iter().cloned().collect()
    }

    /// Сколько extension-tools поставлено активными процессорами.
    /// Удобно для тестов и для логирования.
    pub fn extension_tools_count(&self) -> usize {
        self.extension_tools.load().len()
    }

    /// Пересобрать active_languages и extension_tools и атомарно
    /// подменить через `ArcSwap`. После подмены, если состав активных
    /// языков изменился, отправляется `notifications/tools/list_changed`
    /// по сохранённому peer (если он есть).
    ///
    /// `new_active_languages` приходит снаружи: file-watch на `daemon.toml`
    /// читает обновлённый конфиг и собирает множество явных или
    /// auto-detected языков по всем `[[paths]]`. Сервер сам не парсит
    /// конфиг — он только реагирует на готовые данные.
    pub async fn reload_extensions(&self, new_active_languages: BTreeSet<String>) {
        let registry_opt = self.registry.as_ref().as_ref();
        let new_tools = match registry_opt {
            Some(reg) => collect_extension_tools(&new_active_languages, reg),
            None => Vec::new(),
        };

        let prev_languages = self.active_languages.load_full();
        let changed = (*prev_languages) != new_active_languages;

        self.active_languages
            .store(Arc::new(new_active_languages));
        self.extension_tools.store(Arc::new(new_tools));

        if changed {
            tracing::info!(
                "Состав активных языков изменился: {:?} → {:?}. Отправляю tools/list_changed.",
                prev_languages.iter().collect::<Vec<_>>(),
                self.active_languages
                    .load()
                    .iter()
                    .collect::<Vec<_>>()
            );
            self.notify_tools_changed_if_peer().await;
        }
    }

    /// Отправить `notifications/tools/list_changed` по сохранённому peer.
    /// Если peer не сохранён (клиент ещё не подключился или сессия
    /// уже завершилась) — просто пишем info в лог. Ошибки отправки
    /// логируем как warning, но не пробрасываем — это «информирующее»
    /// уведомление, его потеря не должна валить операцию rebuild.
    pub async fn notify_tools_changed_if_peer(&self) {
        let peer_guard = self.peer.lock().await;
        match peer_guard.as_ref() {
            Some(peer) => {
                if let Err(e) = peer.notify_tool_list_changed().await {
                    tracing::warn!("notify_tool_list_changed: {}", e);
                }
            }
            None => {
                tracing::info!(
                    "tools/list_changed: peer не сохранён (клиент не подключён или ещё не initialized)"
                );
            }
        }
    }

    /// Получить RepoEntry по alias или вернуть ToolUnavailable::NotStarted JSON.
    pub(crate) fn resolve_repo(&self, alias: &str) -> Result<&RepoEntry, String> {
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

// ── Conditional registration helpers ──────────────────────────────────────
//
// Эти функции собирают «активные» языки и tools из репо-карты и реестра
// процессоров. Активный = есть хотя бы одно репо с `language = X`.
// extension-tools — сумма `additional_tools()` всех активных процессоров.
//
// В реестре могут быть процессоры, чьи языки сейчас не используются
// (например, BSL-процессор в `bsl-indexer`, но `daemon.toml` сейчас
// содержит только Python-репо). Их tools не попадают в `extension_tools`
// — клиент не должен видеть невалидных вариантов.

fn collect_active_languages(repos: &BTreeMap<String, RepoEntry>) -> BTreeSet<String> {
    repos
        .values()
        .filter_map(|e| e.language.clone())
        .collect()
}

fn collect_extension_tools(
    active_languages: &BTreeSet<String>,
    registry: &ProcessorRegistry,
) -> Vec<Arc<dyn IndexTool>> {
    let mut out = Vec::new();
    for proc in registry.iter() {
        if active_languages.contains(proc.name()) {
            for t in proc.additional_tools() {
                out.push(t);
            }
        }
    }
    out
}

/// Сборка ответа массового режима get_function/get_class из результатов
/// `tools::mass_map`: порядок элементов = порядок имён в запросе, ошибка
/// отдельного элемента — `{error}` на его позиции, не валит весь батч.
fn mass_rows_to_results(rows: Vec<Result<String, String>>) -> String {
    let results: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|row| match row {
            Ok(one) => {
                let mut v = serde_json::from_str::<serde_json::Value>(&one)
                    .unwrap_or(serde_json::Value::String(one));
                // tools::* оборачивают ответ в {_meta, result(, hint)}. В массив кладём
                // элемент без служебного _meta (иначе N копий мусора в ответе модели).
                if let Some(o) = v.as_object_mut() {
                    o.remove("_meta");
                }
                v
            }
            Err(e) => serde_json::json!({ "error": e }),
        })
        .collect();
    serde_json::json!({ "results": results }).to_string()
}

// ── MCP tools ──────────────────────────────────────────────────────────────
//
// В каждом data-handler:
//   1. `resolve_repo` — найти RepoEntry по alias или вернуть JSON-ошибку.
//   2. Если `entry.is_local == false` — форвард через `federation::dispatcher`
//      на удалённый serve по `entry.ip` (порт 8011, тот же endpoint /federate/<tool>).
//   3. Иначе — позвать `tools::*`, которая читает локальный SQLite.
//
// `health` не форвардится — это диагностика локального процесса.

#[tool_router]
impl CodeIndexServer {
    #[tool(description = "Нечёткий FTS-поиск функций по СЛОВАМ (bm25, OR между словами, префиксные термы): имя важнее qualified_name/docstring. Принимает и точное имя, и описание из слов ('расчёт цены продажи реализация'). Выдача БЕЗ тел — только локации (имя/путь/строки/сигнатура/обрезанный docstring). Тело конкретной функции — get_function; локации по ТОЧНОМУ имени — find_symbol; regex по коду — grep_code. path_glob — фильтр по пути. При 0 совпадений — hint.")]
    async fn search_function(&self, Parameters(p): Parameters<SearchParams>) -> String {
        let entry = match self.resolve_repo(&p.repo) { Ok(e) => e, Err(j) => return j };
        if !entry.is_local {
            return crate::federation::dispatcher::dispatch_remote(
                &self.clients, &entry.ip, entry.port, "search_function", &p,
            ).await;
        }
        tools::search_function(entry, p.query, p.limit, p.language, p.path_glob).await
    }

    #[tool(description = "Нечёткий FTS-поиск классов/структур по СЛОВАМ (bm25): имя важнее docstring. Выдача БЕЗ тел — только локации (имя/путь/строки/bases). Тело конкретного класса — get_class; локации по ТОЧНОМУ имени — find_symbol. path_glob — фильтр по пути. При 0 совпадений — hint.")]
    async fn search_class(&self, Parameters(p): Parameters<SearchParams>) -> String {
        let entry = match self.resolve_repo(&p.repo) { Ok(e) => e, Err(j) => return j };
        if !entry.is_local {
            return crate::federation::dispatcher::dispatch_remote(
                &self.clients, &entry.ip, entry.port, "search_class", &p,
            ).await;
        }
        tools::search_class(entry, p.query, p.limit, p.language, p.path_glob).await
    }

    #[tool(description = "Тело функции по ТОЧНОМУ имени (с исходником). Уникальное имя → одно тело. НЕуникальное (совпадений > порога) → тела опускаются, возвращаются локации + hint: уточните path_glob к нужному файлу. Навигация «где символ» без тел — find_symbol; поиск по словам — search_function. Возвращает JSON-массив FunctionRecord (или облегчённые локации при множестве). МАССОВЫЙ РЕЖИМ ('names'): батчи список ТОЛЬКО когда точно нужны тела ВСЕХ этих функций и результат одной не отменит надобность в остальных (например, правишь их все). Если ОТБИРАЕШЬ, какие из кандидатов релевантны, — НЕ батчи, бери по одному с остановкой по ходу. Сомневаешься — по одному. Ответ на батч — {results:[...]} в порядке запроса.")]
    async fn get_function(&self, Parameters(p): Parameters<MultiNameParams>) -> String {
        let entry = match self.resolve_repo(&p.repo) { Ok(e) => e, Err(j) => return j };
        if !entry.is_local {
            return crate::federation::dispatcher::dispatch_remote(
                &self.clients, &entry.ip, entry.port, "get_function", &p,
            ).await;
        }
        if let Some(names) = p.names {
            // Массовый режим — конкуррентно: каждый элемент берёт своё соединение
            // из пула и исполняется в spawn_blocking (tools::mass_map). Статус
            // папки проверяется один раз на весь батч.
            if let Some(json) = tools::check_path_status(entry).await {
                return json;
            }
            if names.is_empty() {
                return serde_json::json!({ "results": [] }).to_string();
            }
            let pool = entry.storage_pool().clone();
            let glob = p.path_glob.clone();
            let rows = tools::mass_map(&pool, names, move |st, nm| {
                tools::get_function_with(st, nm, glob.clone())
            })
            .await;
            return mass_rows_to_results(rows);
        }
        match p.name {
            Some(nm) => tools::get_function(entry, nm, p.path_glob).await,
            None => serde_json::json!({
                "error": "missing parameter: передайте 'name' — точное имя символа (строка)"
            })
            .to_string(),
        }
    }

    #[tool(description = "Тело класса/структуры по ТОЧНОМУ имени (с исходником). НЕуникальное имя (совпадений > порога) → тела опускаются, локации + hint, уточните path_glob. Навигация без тел — find_symbol; поиск по словам — search_class. Возвращает JSON-массив ClassRecord (или локации при множестве). МАССОВЫЙ РЕЖИМ ('names'): батчи список ТОЛЬКО когда точно нужны тела ВСЕХ этих классов и результат одного не отменит надобность в остальных (например, правишь их все). Если ОТБИРАЕШЬ, какие из кандидатов релевантны, — НЕ батчи, бери по одному с остановкой по ходу. Сомневаешься — по одному. Ответ на батч — {results:[...]} в порядке запроса.")]
    async fn get_class(&self, Parameters(p): Parameters<MultiNameParams>) -> String {
        let entry = match self.resolve_repo(&p.repo) { Ok(e) => e, Err(j) => return j };
        if !entry.is_local {
            return crate::federation::dispatcher::dispatch_remote(
                &self.clients, &entry.ip, entry.port, "get_class", &p,
            ).await;
        }
        if let Some(names) = p.names {
            // Массовый режим — конкуррентно, зеркало get_function (см. выше).
            if let Some(json) = tools::check_path_status(entry).await {
                return json;
            }
            if names.is_empty() {
                return serde_json::json!({ "results": [] }).to_string();
            }
            let pool = entry.storage_pool().clone();
            let glob = p.path_glob.clone();
            let rows = tools::mass_map(&pool, names, move |st, nm| {
                tools::get_class_with(st, nm, glob.clone())
            })
            .await;
            return mass_rows_to_results(rows);
        }
        match p.name {
            Some(nm) => tools::get_class(entry, nm, p.path_glob).await,
            None => serde_json::json!({
                "error": "missing parameter: передайте 'name' — точное имя символа (строка)"
            })
            .to_string(),
        }
    }

    #[tool(description = "Найти вызывателей функции (callers) в указанном репо. limit — cap (default 200); на «горячих» функциях ответ обрезается с {truncated,total,limit}. Возвращает JSON-массив CallRecord.")]
    async fn get_callers(&self, Parameters(p): Parameters<FunctionNameParams>) -> String {
        let entry = match self.resolve_repo(&p.repo) { Ok(e) => e, Err(j) => return j };
        if !entry.is_local {
            return crate::federation::dispatcher::dispatch_remote(
                &self.clients, &entry.ip, entry.port, "get_callers", &p,
            ).await;
        }
        tools::get_callers(entry, p.function_name, p.language, p.limit).await
    }

    #[tool(description = "Найти что вызывает функция (callees) в указанном репо. limit — cap (default 200); при обрезке {truncated,total,limit}. Возвращает JSON-массив CallRecord.")]
    async fn get_callees(&self, Parameters(p): Parameters<FunctionNameParams>) -> String {
        let entry = match self.resolve_repo(&p.repo) { Ok(e) => e, Err(j) => return j };
        if !entry.is_local {
            return crate::federation::dispatcher::dispatch_remote(
                &self.clients, &entry.ip, entry.port, "get_callees", &p,
            ).await;
        }
        tools::get_callees(entry, p.function_name, p.language, p.limit).await
    }

    #[tool(description = "Кратчайший путь в графе вызовов от функции 'from' до 'to' через таблицу calls (рекурсивный CTE, BFS, max_depth по умолчанию 5, [1..10]). Универсальный, любой язык. Возвращает {from,to,found,path:[{caller,callee,line}]}. Для BSL с call_type — find_path_bsl.")]
    async fn find_path(&self, Parameters(p): Parameters<FindPathParams>) -> String {
        let entry = match self.resolve_repo(&p.repo) { Ok(e) => e, Err(j) => return j };
        if !entry.is_local {
            return crate::federation::dispatcher::dispatch_remote(
                &self.clients, &entry.ip, entry.port, "find_path", &p,
            ).await;
        }
        tools::find_path(entry, p.from, p.to, p.max_depth, p.language).await
    }

    #[tool(description = "Дерево вызовов от функции 'root' на глубину max_depth (по умолчанию 3, [1..10]) через таблицу calls. direction: callees/down (что вызывает root вглубь, по умолчанию) или callers/up (кто вызывает root). max_nodes cap (default 200). Универсальный, любой язык. Возвращает {root,direction,edges:[{caller,callee,line,depth}],tree:{name,children}}.")]
    async fn get_call_tree(&self, Parameters(p): Parameters<CallTreeParams>) -> String {
        let entry = match self.resolve_repo(&p.repo) { Ok(e) => e, Err(j) => return j };
        if !entry.is_local {
            return crate::federation::dispatcher::dispatch_remote(
                &self.clients, &entry.ip, entry.port, "get_call_tree", &p,
            ).await;
        }
        tools::get_call_tree(entry, p.root, p.direction, p.max_depth, p.max_nodes, p.language).await
    }

    #[tool(description = "Навигация: ГДЕ определён символ по ТОЧНОМУ имени — локации функций/классов/переменных/импортов БЕЗ тел (как search_*). Тело конкретного — get_function/get_class. path_glob — фильтр по пути. Возвращает {functions, classes, variables, imports} (облегчённые: имя/путь/строки/сигнатура).")]
    async fn find_symbol(&self, Parameters(p): Parameters<NameParams>) -> String {
        let entry = match self.resolve_repo(&p.repo) { Ok(e) => e, Err(j) => return j };
        if !entry.is_local {
            return crate::federation::dispatcher::dispatch_remote(
                &self.clients, &entry.ip, entry.port, "find_symbol", &p,
            ).await;
        }
        tools::find_symbol(entry, p.name, p.language, p.path_glob).await
    }

    #[tool(description = "Импорты файла (file_id) или модуля (module) в указанном репо. limit — cap (default 200); при обрезке {truncated,total,limit}. Возвращает JSON-массив ImportRecord.")]
    async fn get_imports(&self, Parameters(p): Parameters<ImportParams>) -> String {
        let entry = match self.resolve_repo(&p.repo) { Ok(e) => e, Err(j) => return j };
        if !entry.is_local {
            return crate::federation::dispatcher::dispatch_remote(
                &self.clients, &entry.ip, entry.port, "get_imports", &p,
            ).await;
        }
        tools::get_imports(entry, p.file_id, p.module, p.language, p.limit).await
    }

    #[tool(description = "Карта/оглавление файла БЕЗ тел функций/классов (безопасно на больших модулях): имена, сигнатуры (args/return_type), диапазоны строк, обрезанные docstring, импорты, переменные + functions_total/classes_total. Тело конкретной функции — get_function(name) или read_file(line_start,line_end). Возвращает JSON-объект.")]
    async fn get_file_summary(&self, Parameters(p): Parameters<FilePathParams>) -> String {
        let entry = match self.resolve_repo(&p.repo) { Ok(e) => e, Err(j) => return j };
        if !entry.is_local {
            return crate::federation::dispatcher::dispatch_remote(
                &self.clients, &entry.ip, entry.port, "get_file_summary", &p,
            ).await;
        }
        tools::get_file_summary(entry, p.path).await
    }

    #[tool(description = "Статистика индекса. Если repo указан — для одного репо, иначе — массив по всем подключённым репо.")]
    async fn get_stats(&self, Parameters(p): Parameters<StatsParams>) -> String {
        // Если запрос адресован конкретному remote-репо — форвардим как обычно.
        if let Some(ref alias) = p.repo {
            if let Some(entry) = self.repos.get(alias) {
                if !entry.is_local {
                    return crate::federation::dispatcher::dispatch_remote(
                        &self.clients, &entry.ip, entry.port, "get_stats", &p,
                    ).await;
                }
            }
        }
        // Без repo — fan-out по всем (включая удалённые) реализуется в этапе 5.
        tools::get_stats(self, p.repo).await
    }

    #[tool(description = "FTS поиск по текстовым файлам (md, txt, yaml, toml) в указанном репо. path_glob — опциональный фильтр по пути. Возвращает JSON-массив [{path, snippet}].")]
    async fn search_text(&self, Parameters(p): Parameters<SearchParams>) -> String {
        let entry = match self.resolve_repo(&p.repo) { Ok(e) => e, Err(j) => return j };
        if !entry.is_local {
            return crate::federation::dispatcher::dispatch_remote(
                &self.clients, &entry.ip, entry.port, "search_text", &p,
            ).await;
        }
        tools::search_text(entry, p.query, p.limit, p.language, p.path_glob).await
    }

    #[tool(description = "Поиск по телам функций и классов. pattern — подстрока (LIKE), regex — регулярное выражение (REGEXP); query — алиас regex. path_glob — фильтр по пути (SQL pushdown; альтернативы `{a,b}` поддерживаются). context_lines — N строк до/после совпадения. limit — число находок (default 100); при обрезке truncated=true. Возвращает JSON-объект {files: {\"<path>\": [{name, kind, line_start, line_end, match_lines, match_count?, context?}]}, shown, limit, truncated} — находки сгруппированы по файлу, путь не дублируется в каждой строке.")]
    async fn grep_body(&self, Parameters(p): Parameters<GrepBodyParams>) -> String {
        let entry = match self.resolve_repo(&p.repo) { Ok(e) => e, Err(j) => return j };
        if !entry.is_local {
            return crate::federation::dispatcher::dispatch_remote(
                &self.clients, &entry.ip, entry.port, "grep_body", &p,
            ).await;
        }
        // `query` — алиас для `regex` (частая путаница моделей).
        let regex = p.regex.clone().or_else(|| p.query.clone());
        if p.pattern.is_none() && regex.is_none() {
            return "{\"error\": \"grep_body: укажите pattern= (подстрока) или regex= (regexp). Для кода вне тел функций — grep_code(regex=…); по xml/md/yaml — grep_text(regex=…).\"}".to_string();
        }
        tools::grep_body(
            entry, p.pattern, regex, p.language, p.limit, p.path_glob, p.context_lines,
        )
        .await
    }

    #[tool(description = "Метаданные файла из индекса: existence, размер, mtime, lines_total, language, content_hash, indexed_at, category. Чистая выборка из таблицы files (быстро).")]
    async fn stat_file(&self, Parameters(p): Parameters<StatFileParams>) -> String {
        let entry = match self.resolve_repo(&p.repo) { Ok(e) => e, Err(j) => return j };
        if !entry.is_local {
            return crate::federation::dispatcher::dispatch_remote(
                &self.clients, &entry.ip, entry.port, "stat_file", &p,
            ).await;
        }
        tools::stat_file(entry, p.path).await
    }

    #[tool(description = "Список файлов в индексе с фильтрами. pattern — glob по пути (`**/*.py`; альтернативы `{a,b}`: `**/*.{rs,toml}`), path_prefix — префикс (`src/auth/`), language — язык. Возвращает JSON-массив [{path, language, lines_total, size, mtime}].")]
    async fn list_files(&self, Parameters(p): Parameters<ListFilesParams>) -> String {
        let entry = match self.resolve_repo(&p.repo) { Ok(e) => e, Err(j) => return j };
        if !entry.is_local {
            return crate::federation::dispatcher::dispatch_remote(
                &self.clients, &entry.ip, entry.port, "list_files", &p,
            ).await;
        }
        tools::list_files(entry, p.pattern, p.path_prefix, p.language, p.limit).await
    }

    #[tool(description = "Прочитать содержимое файла из индекса. Отдаёт реальный content и для text-файлов (yaml/md/json/toml/xml/sh и др.), и для code-файлов (zstd-decode из file_contents, Phase 2 v0.8.0+); поле category в ответе — \"text\" или \"code\". Oversize code-файлы (> max_code_file_size_bytes) возвращают oversize=true и пустой content (их читать через get_function/grep_body/grep_code). line_start/line_end — 1-based, inclusive. Soft-cap 5000 строк / 500 КБ (truncated=true при обрезке), hard-cap 2 МБ.")]
    async fn read_file(&self, Parameters(p): Parameters<ReadFileParams>) -> String {
        let entry = match self.resolve_repo(&p.repo) { Ok(e) => e, Err(j) => return j };
        if !entry.is_local {
            return crate::federation::dispatcher::dispatch_remote(
                &self.clients, &entry.ip, entry.port, "read_file", &p,
            ).await;
        }
        tools::read_file(entry, p.path, p.line_start, p.line_end).await
    }

    #[tool(description = "Regex-поиск по содержимому text-файлов (параметр regex=, синоним query=). path_glob ИЛИ language обязательно желателен (full-scan по всем text-файлам — дорого); альтернативы `{a,b}` в path_glob поддерживаются. context_lines — N строк до/после. limit — число находок; при обрезке truncated=true. Возвращает JSON-объект {files: {\"<path>\": [{line, content, context?}]}, shown, limit, truncated} — находки сгруппированы по файлу, путь не дублируется в каждой строке.")]
    async fn grep_text(&self, Parameters(p): Parameters<GrepTextParams>) -> String {
        let entry = match self.resolve_repo(&p.repo) { Ok(e) => e, Err(j) => return j };
        if !entry.is_local {
            return crate::federation::dispatcher::dispatch_remote(
                &self.clients, &entry.ip, entry.port, "grep_text", &p,
            ).await;
        }
        // `query` — алиас для `regex` (частая путаница моделей).
        let regex = match p.regex.clone().or_else(|| p.query.clone()) {
            Some(r) if !r.trim().is_empty() => r,
            _ => return "{\"error\": \"grep_text: укажите regex= (синтаксис crate regex), не query=. Для кода .bsl/.py/.rs — grep_code(regex=…) или grep_body.\"}".to_string(),
        };
        tools::grep_text(entry, regex, p.path_glob, p.language, p.limit, p.context_lines).await
    }

    #[tool(description = "Regex-поиск по содержимому **code-файлов** (Phase 2, v0.8.0; параметр regex=, синоним query=): module-level код, идентификаторы, комментарии вне тел, макросы, use-импорты — всё что не ловит grep_body. Источник — таблица file_contents (zstd). path_glob ИЛИ language обязательно желателен (full-scan дорогой из-за zstd-decode каждого файла); альтернативы `{a,b}` в path_glob поддерживаются. Файлы oversize=true пропускаются. limit — число совпадений (default 100); при обрезке результат помечается truncated=true (дошлите больший limit, если нужно больше). Возвращает JSON-объект {files: {\"<path>\": [{line, content, context}]}, shown, limit, truncated} — находки сгруппированы по файлу, путь не дублируется в каждой строке.")]
    async fn grep_code(&self, Parameters(p): Parameters<GrepCodeParams>) -> String {
        let entry = match self.resolve_repo(&p.repo) { Ok(e) => e, Err(j) => return j };
        if !entry.is_local {
            return crate::federation::dispatcher::dispatch_remote(
                &self.clients, &entry.ip, entry.port, "grep_code", &p,
            ).await;
        }
        // `query` — алиас для `regex` (частая путаница моделей).
        let regex = match p.regex.clone().or_else(|| p.query.clone()) {
            Some(r) if !r.trim().is_empty() => r,
            _ => return "{\"error\": \"grep_code: укажите regex= (синтаксис crate regex), не query=. Для тел функций/классов — grep_body; для xml/md/yaml — grep_text(regex=…).\"}".to_string(),
        };
        tools::grep_code(entry, regex, p.path_glob, p.language, p.limit, p.context_lines).await
    }

    #[tool(description = "Проверка живости MCP-сервера и демона индексации по всем подключённым репо. Возвращает JSON.")]
    async fn health(&self) -> String {
        tools::health(self).await
    }
}

// Реализация ServerHandler без `#[tool_handler]`-макроса. Макрос
// собирал `list_tools`/`call_tool`/`get_tool` строго через `tool_router`,
// а нам нужно ещё подмешать extension-tools от активных
// `LanguageProcessor`-ов. Поэтому пишем три метода руками, делегируя
// core-tools в `tool_router`, а extension — в свой Vec.

impl ServerHandler for CodeIndexServer {
    fn get_info(&self) -> ServerInfo {
        // enable_tool_list_changed: даём клиенту знать, что мы способны
        // отправлять `notifications/tools/list_changed`. Сама отправка
        // подключится на этапе 1.7 (file-watch на daemon.toml вызовет
        // rebuild active set и notify_tool_list_changed через peer).
        let caps = ServerCapabilities::builder()
            .enable_tools()
            .enable_tool_list_changed()
            .build();
        ServerInfo::new(caps).with_server_info(Implementation::new(
            "code-index-mcp",
            env!("CARGO_PKG_VERSION"),
        ))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let mut tools = self.tool_router.list_all();
        let extension_snapshot = self.extension_tools.load();
        for ext in extension_snapshot.iter() {
            tools.push(extension_tool_to_rmcp(ext.as_ref()));
        }
        // Если задан whitelist через `daemon.toml [tools].enabled` —
        // оставляем только перечисленные tools. Модель видит ровно тот
        // набор, который оператор разрешил; без whitelist (`None`) фильтр
        // не применяется (поведение до v0.10.x).
        if let Some(allowed) = self.allowed_tools.as_ref().as_ref() {
            tools.retain(|t| allowed.contains(t.name.as_ref()));
        }
        // Массовый режим: инструмент не в `[mcp].mass_mode_tools` → убираем его
        // plural-параметр (`names`/`full_names`) из схемы, чтобы модель не
        // видела опцию пачки. Дефолт — пустой список → выключено у всех.
        for (name, plural) in MASS_MODE_PARAMS {
            if !self.mass_mode_tools.contains(*name) {
                if let Some(tool) = tools.iter_mut().find(|t| t.name.as_ref() == *name) {
                    strip_mass_mode_param(tool, plural);
                }
            }
        }
        // Стабильный порядок (как у tool_router::list_all): по имени.
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(ListToolsResult {
            tools,
            meta: None,
            next_cursor: None,
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        // 0. Whitelist-проверка ДО router-диспетча. Модель может вызвать
        // tool, отсутствующий в `tools/list` (из системного промпта, из
        // памяти обучения, из CLAUDE.md проекта) — без этой проверки
        // вызов уйдёт в router и выполнится. Дублирует фильтр в
        // `list_tools` намеренно (двойная защита).
        if let Some(allowed) = self.allowed_tools.as_ref().as_ref() {
            if !allowed.contains(request.name.as_ref()) {
                return Err(ErrorData::invalid_params(
                    format!(
                        "tool '{}' is disabled by [tools].enabled whitelist in daemon.toml",
                        request.name
                    ),
                    None,
                ));
            }
        }
        // 0b. Массовый режим выключен для этого инструмента? Отбиваем вызов с
        // plural-параметром (`names`/`full_names`). Дублирует стрип схемы в
        // `list_tools` (двойная защита: модель может прислать параметр из
        // памяти/системного промпта, минуя `tools/list`). Одиночный
        // `name`/`full_name` всегда проходит.
        if let Some((_, plural)) = MASS_MODE_PARAMS
            .iter()
            .find(|(n, _)| *n == request.name.as_ref())
        {
            if !self.mass_mode_tools.contains(request.name.as_ref()) {
                let has_plural = request
                    .arguments
                    .as_ref()
                    .and_then(|m| m.get(*plural))
                    .map(|v| !v.is_null())
                    .unwrap_or(false);
                if has_plural {
                    return Err(ErrorData::invalid_params(
                        format!(
                            "массовый режим ('{}') для tool '{}' выключен — добавьте его в \
                             [mcp].mass_mode_tools (daemon.toml) либо передайте одиночный параметр",
                            plural, request.name
                        ),
                        None,
                    ));
                }
            }
        }
        // 1. Сначала core-tools — они есть всегда.
        if self.tool_router.has_route(request.name.as_ref()) {
            let tcc = rmcp::handler::server::tool::ToolCallContext::new(
                self,
                request,
                context,
            );
            return self.tool_router.call(tcc).await;
        }
        // 2. Иначе — extension-tools. Ищем по имени.
        let tool_name = request.name.as_ref();
        let extension_snapshot = self.extension_tools.load();
        let ext = extension_snapshot
            .iter()
            .find(|t| t.name() == tool_name)
            .ok_or_else(|| ErrorData::invalid_params("tool not found", None))?
            .clone();

        // Извлечь параметры. У extension-tool `args` — это `serde_json::Value`,
        // который мы передаём в `IndexTool::execute` как есть. Если клиент
        // не передал arguments — подставляем пустой объект.
        let args = request
            .arguments
            .map(serde_json::Value::Object)
            .unwrap_or_else(|| serde_json::Value::Object(Default::default()));

        // Параметр `repo` обязателен у всех tools (см. ТЗ). Извлекаем его
        // из аргументов, чтобы построить ToolContext с правильным RepoEntry.
        let repo = args
            .get("repo")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                ErrorData::invalid_params(
                    "tool requires 'repo' parameter (string)",
                    None,
                )
            })?
            .to_string();

        let entry = self.repos.get(&repo).ok_or_else(|| {
            ErrorData::invalid_params(
                format!("unknown repo '{}'. Available: {:?}", repo, self.repo_aliases()),
                None,
            )
        })?;

        // v0.8.1: federation для extension-tools.
        //
        // Если репо remote — форвардим вызов на удалённую ноду через
        // универсальный route /federate/extension. Удалённый сервер
        // получит {tool_name, args} и сам найдёт tool в своём
        // extension_tools snapshot, выполнит и вернёт результат.
        //
        // Раньше (до 0.8.1) здесь стоял жёсткий Err «supports only local
        // repos» — это делало BSL-tools (`get_object_structure` и т.д.)
        // нерабочими для federation-репо (UT/BP_SS/BP_TDK/ZUP на VM rag).
        if !entry.is_local {
            let payload = serde_json::json!({
                "tool_name": tool_name,
                "args": args,
            });
            let body = crate::federation::dispatcher::dispatch_remote_value(
                &self.clients,
                &entry.ip,
                entry.port,
                "extension",
                payload,
            )
            .await;
            let value: serde_json::Value =
                serde_json::from_str(&body).unwrap_or_else(|_| serde_json::json!({"raw": body}));
            return Ok(CallToolResult::structured(value));
        }
        let storage = entry.storage_pool();
        let root_path: Option<&Path> = entry.root_path.as_deref();
        let language: Option<&str> = entry.language.as_deref();

        let ctx = crate::extension::ToolContext {
            repo: &repo,
            root_path,
            language,
            storage,
        };

        // Прогон через `IndexTool::execute` и обёртка результата.
        let value = ext.execute(args, ctx).await;
        Ok(CallToolResult::structured(value))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        if let Some(t) = self.tool_router.get(name) {
            return Some(t.clone());
        }
        self.extension_tools
            .load()
            .iter()
            .find(|t| t.name() == name)
            .map(|t| extension_tool_to_rmcp(t.as_ref()))
    }

    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        // Сохраняем peer этой сессии в self.peer, чтобы потом из
        // `notify_tools_changed_if_peer()` можно было послать уведомление
        // (например, после реактивного rebuild на file-watch). Если peer
        // от предыдущей сессии уже сохранён — заменяем; rmcp гарантирует,
        // что `on_initialized` приходит на каждую сессию.
        {
            let mut guard = self.peer.lock().await;
            *guard = Some(context.peer.clone());
        }
        tracing::info!("client initialized");
    }
}

/// Убрать из инструмента plural-параметр массового режима (`names`/`full_names`)
/// и обрезать фразу про массовый режим из описания — когда инструмента нет в
/// белом списке `[mcp].mass_mode_tools`. После этого модель не видит опцию
/// пачки в `tools/list`. Plural-параметры массовых инструментов опциональны,
/// поэтому из `required` они не удаляются (их там и нет), но на всякий случай
/// фильтруем и `required`.
fn strip_mass_mode_param(tool: &mut Tool, plural: &str) {
    let mut schema = tool.input_schema.as_ref().clone();
    if let Some(serde_json::Value::Object(props)) = schema.get_mut("properties") {
        props.remove(plural);
    }
    if let Some(serde_json::Value::Array(req)) = schema.get_mut("required") {
        req.retain(|v| v.as_str() != Some(plural));
    }
    tool.input_schema = Arc::new(schema);
    // Фраза «МАССОВ…» всегда в конце описаний массовых инструментов —
    // обрезаем хвост, чтобы описание не обещало недоступную опцию.
    if let Some(desc) = tool.description.as_ref() {
        if let Some(idx) = desc.find("МАССОВ") {
            let trimmed = desc[..idx].trim_end().to_string();
            tool.description = Some(std::borrow::Cow::Owned(trimmed));
        }
    }
}

/// Конвертация `IndexTool` (наш trait) в `rmcp::model::Tool` (формат для
/// `tools/list`). `input_schema` ожидаем как JSON-объект; если пришло не
/// объект — оборачиваем в пустой объект, чтобы не сломать клиент.
///
/// `Tool` помечен `#[non_exhaustive]`, поэтому используем `Tool::default()`
/// + мутацию полей вместо struct-expression.
fn extension_tool_to_rmcp(t: &dyn IndexTool) -> Tool {
    use std::borrow::Cow;
    let schema = t.input_schema();
    let schema_obj = match schema {
        serde_json::Value::Object(map) => map,
        _ => Default::default(),
    };
    let mut tool = Tool::default();
    tool.name = Cow::Owned(t.name().to_string());
    tool.description = Some(Cow::Owned(t.description().to_string()));
    tool.input_schema = Arc::new(schema_obj);
    tool
}

// ── Тесты массового режима ([mcp].mass_mode_tools, v0.28.0) ────────────────

#[cfg(test)]
mod mass_mode_tests {
    use super::*;
    use std::borrow::Cow;

    fn tool_with_plural(name: &str, plural: &str, desc: &str) -> Tool {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "repo": {"type": "string"},
                "name": {"type": "string"},
                plural: {"type": "array", "items": {"type": "string"}}
            },
            "required": ["repo"]
        });
        let map = match schema {
            serde_json::Value::Object(m) => m,
            _ => unreachable!(),
        };
        let mut t = Tool::default();
        t.name = Cow::Owned(name.to_string());
        t.description = Some(Cow::Owned(desc.to_string()));
        t.input_schema = Arc::new(map);
        t
    }

    #[test]
    fn strip_removes_plural_keeps_single_and_trims_desc() {
        let mut t = tool_with_plural(
            "get_function",
            "names",
            "Тело функции по точному имени. МАССОВЫЙ РЕЖИМ: передайте список в 'names'.",
        );
        strip_mass_mode_param(&mut t, "names");
        let props = t
            .input_schema
            .get("properties")
            .and_then(|v| v.as_object())
            .unwrap();
        assert!(!props.contains_key("names"), "plural удалён");
        assert!(props.contains_key("name"), "одиночный параметр цел");
        let desc = t.description.as_ref().unwrap();
        assert!(!desc.contains("МАССОВ"), "фраза обрезана: {desc}");
        assert!(desc.contains("Тело функции"), "полезная часть описания цела");
    }

    #[test]
    fn strip_full_names_for_object_structure() {
        let mut t = tool_with_plural(
            "get_object_structure",
            "full_names",
            "Структура объекта. МАССОВЫЙ РЕЖИМ: передайте 'full_names'.",
        );
        strip_mass_mode_param(&mut t, "full_names");
        let props = t
            .input_schema
            .get("properties")
            .and_then(|v| v.as_object())
            .unwrap();
        assert!(!props.contains_key("full_names"));
    }

    #[test]
    fn apply_empty_disables_all() {
        let server = CodeIndexServer::with_repos(Default::default()).apply_mass_mode_tools(&[]);
        assert!(
            server.mass_mode_tools.is_empty(),
            "пустой список → массовый режим выключен у всех"
        );
    }

    #[test]
    fn apply_allowlist_sets_membership() {
        let server = CodeIndexServer::with_repos(Default::default())
            .apply_mass_mode_tools(&["get_object_structure".to_string()]);
        assert!(server.mass_mode_tools.contains("get_object_structure"));
        assert!(!server.mass_mode_tools.contains("get_function"));
    }

    #[test]
    fn default_server_has_mass_mode_off() {
        // Конструктор по умолчанию — массовый режим выключен (дефолт v0.28.0).
        let server = CodeIndexServer::with_repos(Default::default());
        assert!(server.mass_mode_tools.is_empty());
    }

    #[test]
    fn mass_mode_params_cover_three_tools() {
        let names: BTreeSet<&str> = MASS_MODE_PARAMS.iter().map(|(n, _)| *n).collect();
        assert!(names.contains("get_function"));
        assert!(names.contains("get_class"));
        assert!(names.contains("get_object_structure"));
        assert_eq!(MASS_MODE_PARAMS.len(), 3);
    }
}

// ── Тесты conditional registration ────────────────────────────────────────

#[cfg(test)]
mod conditional_registration_tests {
    use super::*;
    use crate::extension::{LanguageProcessor, ToolContext};
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc as StdArc;

    /// Минимальный фейк-tool — только то, что нужно реестру и сборщику
    /// `collect_extension_tools`. `execute` возвращает пустой JSON.
    struct FakeBslTool;
    impl IndexTool for FakeBslTool {
        fn name(&self) -> &str {
            "fake_bsl_tool"
        }
        fn description(&self) -> &str {
            "test"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn applicable_languages(&self) -> Option<&'static [&'static str]> {
            Some(&["bsl"])
        }
        fn execute<'a>(
            &'a self,
            _args: serde_json::Value,
            _ctx: ToolContext<'a>,
        ) -> Pin<Box<dyn Future<Output = serde_json::Value> + Send + 'a>> {
            Box::pin(async { serde_json::json!({}) })
        }
    }

    /// Фейковый процессор языка `bsl`, отдающий один фиктивный tool.
    struct FakeBslProcessor;
    impl LanguageProcessor for FakeBslProcessor {
        fn name(&self) -> &str {
            "bsl"
        }
        fn additional_tools(&self) -> Vec<StdArc<dyn IndexTool>> {
            vec![StdArc::new(FakeBslTool)]
        }
    }

    fn dummy_repo(language: Option<&str>) -> RepoEntry {
        RepoEntry {
            root_path: None,
            storage: None,
            ip: LEGACY_OWN_IP.to_string(),
            port: crate::federation::client::DEFAULT_REMOTE_PORT,
            is_local: false,
            language: language.map(String::from),
        }
    }

    #[test]
    fn no_active_languages_means_no_extension_tools() {
        let mut repos = BTreeMap::new();
        repos.insert("a".to_string(), dummy_repo(Some("python")));

        let mut reg = ProcessorRegistry::new();
        reg.register(StdArc::new(FakeBslProcessor));

        let server = CodeIndexServer::with_repos_and_registry(repos, reg);
        // bsl не активен (есть только python-репо), tools нет.
        assert!(server.active_language_names().contains(&"python".to_string()));
        assert_eq!(server.extension_tools_count(), 0);
    }

    #[test]
    fn bsl_repo_activates_bsl_extension_tools() {
        let mut repos = BTreeMap::new();
        repos.insert("ut".to_string(), dummy_repo(Some("bsl")));
        repos.insert("py".to_string(), dummy_repo(Some("python")));

        let mut reg = ProcessorRegistry::new();
        reg.register(StdArc::new(FakeBslProcessor));

        let server = CodeIndexServer::with_repos_and_registry(repos, reg);
        // Активен и bsl, и python.
        let names = server.active_language_names();
        assert!(names.contains(&"bsl".to_string()));
        assert!(names.contains(&"python".to_string()));
        // BSL-процессор отдал один tool, python-процессора в реестре нет.
        assert_eq!(server.extension_tools_count(), 1);
        let snapshot = server.extension_tools.load();
        assert_eq!(snapshot[0].name(), "fake_bsl_tool");
    }

    #[test]
    fn legacy_constructor_has_no_extension_tools() {
        let mut repos = BTreeMap::new();
        // language=None — старый путь до auto-detect.
        repos.insert("ut".to_string(), dummy_repo(None));
        let server = CodeIndexServer::with_repos(repos);
        assert!(server.active_language_names().is_empty());
        assert_eq!(server.extension_tools_count(), 0);
    }

    #[test]
    fn extension_tool_to_rmcp_carries_name_and_schema() {
        let tool = FakeBslTool;
        let rmcp_tool = extension_tool_to_rmcp(&tool);
        assert_eq!(rmcp_tool.name, "fake_bsl_tool");
        assert_eq!(
            rmcp_tool.description.as_deref(),
            Some("test"),
            "description должен быть проброшен"
        );
    }

    #[test]
    fn get_tool_finds_extension_by_name() {
        // Серверу даётся фейковый bsl-процессор; его tool должен быть
        // доступен через `get_tool` наравне с core-tools.
        let mut repos = BTreeMap::new();
        repos.insert("ut".to_string(), dummy_repo(Some("bsl")));

        let mut reg = ProcessorRegistry::new();
        reg.register(StdArc::new(FakeBslProcessor));

        let server = CodeIndexServer::with_repos_and_registry(repos, reg);
        let tool = server.get_tool("fake_bsl_tool");
        assert!(tool.is_some(), "extension-tool должен находиться по имени");

        // Несуществующее имя — None.
        assert!(server.get_tool("does_not_exist").is_none());

        // Core-tool тоже должен находиться через тот же API
        // (один из жёстко прописанных core-tools).
        assert!(
            server.get_tool("search_function").is_some(),
            "core-tool search_function должен оставаться доступным"
        );
    }

    #[tokio::test]
    async fn reload_extensions_swaps_active_languages_and_tools() {
        // Старт: bsl-репо не объявлен (только python).
        let mut repos = BTreeMap::new();
        repos.insert("py".to_string(), dummy_repo(Some("python")));

        let mut reg = ProcessorRegistry::new();
        reg.register(StdArc::new(FakeBslProcessor));

        let server = CodeIndexServer::with_repos_and_registry(repos, reg);
        assert_eq!(server.extension_tools_count(), 0);

        // Имитация file-watch'а: пришёл новый набор активных языков,
        // включая bsl.
        let mut new_set = BTreeSet::new();
        new_set.insert("python".to_string());
        new_set.insert("bsl".to_string());
        server.reload_extensions(new_set).await;

        assert!(server
            .active_language_names()
            .contains(&"bsl".to_string()));
        assert_eq!(
            server.extension_tools_count(),
            1,
            "после rebuild bsl-tool должен появиться"
        );

        // Возврат к узкому набору — bsl-tool должен исчезнуть.
        let mut shrunk = BTreeSet::new();
        shrunk.insert("python".to_string());
        server.reload_extensions(shrunk).await;
        assert_eq!(server.extension_tools_count(), 0);
    }
}
