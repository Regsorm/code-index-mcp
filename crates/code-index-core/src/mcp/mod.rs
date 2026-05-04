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
use crate::storage::Storage;

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
    /// SQLite-подключение (только для local). Mutex сериализует доступ к
    /// rusqlite::Connection (не Sync). БД read-only, на запись не конкурирует.
    pub storage: Option<Arc<Mutex<Storage>>>,
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

    /// Ссылка на SQLite-хранилище — для local. Panic для remote.
    pub fn local_storage(&self) -> &Arc<Mutex<Storage>> {
        self.storage.as_ref().unwrap_or_else(|| {
            panic!(
                "local_storage() вызван для remote-репо ip={} — это баг диспатчера",
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
    pub regex: String,
    /// Glob по path. Хотя бы один из {path_glob, language} желателен —
    /// иначе работает full-scan по всем text-файлам.
    pub path_glob: Option<String>,
    pub language: Option<String>,
    pub limit: Option<usize>,
    /// Сколько строк до/после совпадения возвращать в `context`. 0 — без контекста.
    pub context_lines: Option<usize>,
}

// ── Сервер ───────────────────────────────────────────────────────────────────

/// Read-only MCP-сервер индексатора. Держит N открытых репозиториев
/// (по одному на alias), диспатч по параметру `repo` в каждом tool-call.
///
/// В федеративном режиме (`from_federated`): часть репо может быть remote —
/// для них `RepoEntry.is_local=false`, tool-handler форвардит запрос
/// в `clients` по `ip`.
///
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
                let storage = Storage::open_file_readonly(db_path)?;
                RepoEntry {
                    root_path: repo.root_path,
                    storage: Some(Arc::new(Mutex::new(storage))),
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
        })
    }

    /// Удобство: собрать сервер из массива (alias, root_path, db_path),
    /// открывая все БД read-only. Все репо считаются local на 127.0.0.1
    /// (моно-режим, обратная совместимость с rc5).
    pub fn open_readonly_multi(entries: Vec<(String, PathBuf, PathBuf)>) -> anyhow::Result<Self> {
        let mut map = BTreeMap::new();
        for (alias, root_path, db_path) in entries {
            let storage = Storage::open_file_readonly(&db_path)?;
            map.insert(alias, RepoEntry {
                root_path: Some(root_path),
                storage: Some(Arc::new(Mutex::new(storage))),
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
            storage: Some(Arc::new(Mutex::new(storage))),
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
    #[tool(description = "FTS поиск функций по указанному репо: по имени, docstring, телу. path_glob — опциональный фильтр по пути (`src/**/*.py`). Возвращает JSON-массив FunctionRecord.")]
    async fn search_function(&self, Parameters(p): Parameters<SearchParams>) -> String {
        let entry = match self.resolve_repo(&p.repo) { Ok(e) => e, Err(j) => return j };
        if !entry.is_local {
            return crate::federation::dispatcher::dispatch_remote(
                &self.clients, &entry.ip, entry.port, "search_function", &p,
            ).await;
        }
        tools::search_function(entry, p.query, p.limit, p.language, p.path_glob).await
    }

    #[tool(description = "FTS поиск классов по указанному репо: по имени, docstring, телу. path_glob — опциональный фильтр по пути. Возвращает JSON-массив ClassRecord.")]
    async fn search_class(&self, Parameters(p): Parameters<SearchParams>) -> String {
        let entry = match self.resolve_repo(&p.repo) { Ok(e) => e, Err(j) => return j };
        if !entry.is_local {
            return crate::federation::dispatcher::dispatch_remote(
                &self.clients, &entry.ip, entry.port, "search_class", &p,
            ).await;
        }
        tools::search_class(entry, p.query, p.limit, p.language, p.path_glob).await
    }

    #[tool(description = "Найти функцию по точному имени в указанном репо. path_glob — опциональный фильтр по пути. Возвращает JSON-массив FunctionRecord.")]
    async fn get_function(&self, Parameters(p): Parameters<NameParams>) -> String {
        let entry = match self.resolve_repo(&p.repo) { Ok(e) => e, Err(j) => return j };
        if !entry.is_local {
            return crate::federation::dispatcher::dispatch_remote(
                &self.clients, &entry.ip, entry.port, "get_function", &p,
            ).await;
        }
        tools::get_function(entry, p.name, p.path_glob).await
    }

    #[tool(description = "Найти класс по точному имени в указанном репо. path_glob — опциональный фильтр по пути. Возвращает JSON-массив ClassRecord.")]
    async fn get_class(&self, Parameters(p): Parameters<NameParams>) -> String {
        let entry = match self.resolve_repo(&p.repo) { Ok(e) => e, Err(j) => return j };
        if !entry.is_local {
            return crate::federation::dispatcher::dispatch_remote(
                &self.clients, &entry.ip, entry.port, "get_class", &p,
            ).await;
        }
        tools::get_class(entry, p.name, p.path_glob).await
    }

    #[tool(description = "Найти вызывателей функции (callers) в указанном репо. Возвращает JSON-массив CallRecord.")]
    async fn get_callers(&self, Parameters(p): Parameters<FunctionNameParams>) -> String {
        let entry = match self.resolve_repo(&p.repo) { Ok(e) => e, Err(j) => return j };
        if !entry.is_local {
            return crate::federation::dispatcher::dispatch_remote(
                &self.clients, &entry.ip, entry.port, "get_callers", &p,
            ).await;
        }
        tools::get_callers(entry, p.function_name, p.language).await
    }

    #[tool(description = "Найти что вызывает функция (callees) в указанном репо. Возвращает JSON-массив CallRecord.")]
    async fn get_callees(&self, Parameters(p): Parameters<FunctionNameParams>) -> String {
        let entry = match self.resolve_repo(&p.repo) { Ok(e) => e, Err(j) => return j };
        if !entry.is_local {
            return crate::federation::dispatcher::dispatch_remote(
                &self.clients, &entry.ip, entry.port, "get_callees", &p,
            ).await;
        }
        tools::get_callees(entry, p.function_name, p.language).await
    }

    #[tool(description = "Универсальный поиск символа по точному имени в указанном репо. path_glob — опциональный фильтр по пути. Возвращает JSON-объект {functions, classes, variables, imports}.")]
    async fn find_symbol(&self, Parameters(p): Parameters<NameParams>) -> String {
        let entry = match self.resolve_repo(&p.repo) { Ok(e) => e, Err(j) => return j };
        if !entry.is_local {
            return crate::federation::dispatcher::dispatch_remote(
                &self.clients, &entry.ip, entry.port, "find_symbol", &p,
            ).await;
        }
        tools::find_symbol(entry, p.name, p.language, p.path_glob).await
    }

    #[tool(description = "Импорты файла (file_id) или модуля (module) в указанном репо. Возвращает JSON-массив ImportRecord.")]
    async fn get_imports(&self, Parameters(p): Parameters<ImportParams>) -> String {
        let entry = match self.resolve_repo(&p.repo) { Ok(e) => e, Err(j) => return j };
        if !entry.is_local {
            return crate::federation::dispatcher::dispatch_remote(
                &self.clients, &entry.ip, entry.port, "get_imports", &p,
            ).await;
        }
        tools::get_imports(entry, p.file_id, p.module, p.language).await
    }

    #[tool(description = "Карта файла в указанном репо: функции, классы, импорты, переменные. Возвращает JSON-объект FileSummary.")]
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

    #[tool(description = "Поиск по телам функций и классов. pattern — подстрока (LIKE), regex — регулярное выражение (REGEXP). path_glob — фильтр по пути (SQL pushdown). context_lines — N строк до/после совпадения.")]
    async fn grep_body(&self, Parameters(p): Parameters<GrepBodyParams>) -> String {
        let entry = match self.resolve_repo(&p.repo) { Ok(e) => e, Err(j) => return j };
        if !entry.is_local {
            return crate::federation::dispatcher::dispatch_remote(
                &self.clients, &entry.ip, entry.port, "grep_body", &p,
            ).await;
        }
        tools::grep_body(
            entry, p.pattern, p.regex, p.language, p.limit, p.path_glob, p.context_lines,
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

    #[tool(description = "Список файлов в индексе с фильтрами. pattern — glob по пути (`**/*.py`), path_prefix — префикс (`src/auth/`), language — язык. Возвращает JSON-массив [{path, language, lines_total, size, mtime}].")]
    async fn list_files(&self, Parameters(p): Parameters<ListFilesParams>) -> String {
        let entry = match self.resolve_repo(&p.repo) { Ok(e) => e, Err(j) => return j };
        if !entry.is_local {
            return crate::federation::dispatcher::dispatch_remote(
                &self.clients, &entry.ip, entry.port, "list_files", &p,
            ).await;
        }
        tools::list_files(entry, p.pattern, p.path_prefix, p.language, p.limit).await
    }

    #[tool(description = "Прочитать содержимое файла из индекса. Phase 1: только text-файлы (yaml/md/json/toml/xml/sh и др.). Для code-файлов вернётся category=\"code\" с пустым content. line_start/line_end — 1-based, inclusive. Soft-cap 5000 строк / 500 КБ, hard-cap 2 МБ.")]
    async fn read_file(&self, Parameters(p): Parameters<ReadFileParams>) -> String {
        let entry = match self.resolve_repo(&p.repo) { Ok(e) => e, Err(j) => return j };
        if !entry.is_local {
            return crate::federation::dispatcher::dispatch_remote(
                &self.clients, &entry.ip, entry.port, "read_file", &p,
            ).await;
        }
        tools::read_file(entry, p.path, p.line_start, p.line_end).await
    }

    #[tool(description = "Regex-поиск по содержимому text-файлов. path_glob ИЛИ language обязательно желателен (full-scan по всем text-файлам — дорого). context_lines — N строк до/после. Возвращает JSON-массив [{path, line, content, context}].")]
    async fn grep_text(&self, Parameters(p): Parameters<GrepTextParams>) -> String {
        let entry = match self.resolve_repo(&p.repo) { Ok(e) => e, Err(j) => return j };
        if !entry.is_local {
            return crate::federation::dispatcher::dispatch_remote(
                &self.clients, &entry.ip, entry.port, "grep_text", &p,
            ).await;
        }
        tools::grep_text(entry, p.regex, p.path_glob, p.language, p.limit, p.context_lines).await
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

        // Local-only: extension-tools не форвардятся в federation на этапе 1.6.
        // (При необходимости федеративный форвард можно добавить позже —
        // сейчас 1С-инструменты работают только с local SQLite.)
        if !entry.is_local {
            return Err(ErrorData::invalid_params(
                format!("extension tool '{}' currently supports only local repos", tool_name),
                None,
            ));
        }
        let storage = entry.local_storage();
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
