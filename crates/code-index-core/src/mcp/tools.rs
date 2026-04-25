// Реализации MCP-инструментов (v0.5+): read-only, с проверкой статуса папки у демона.
//
// Multi-repo: каждая функция принимает `&RepoEntry` (конкретный репозиторий, выбранный
// через `resolve_repo` в mod.rs по параметру `repo`). Диагностические инструменты
// `get_stats` и `health` принимают весь `&CodeIndexServer`, чтобы собрать сводку
// по всем подключённым репо.
//
// Перед каждым data-tool функция спрашивает у демона статус `root_path` этого репо.
// Если папка не `Ready` — возвращается `ToolUnavailable` JSON, и реальный запрос
// к БД не выполняется.

use super::{CodeIndexServer, RepoEntry};
use crate::daemon_core::client;
use crate::daemon_core::ipc::{PathStatus, ToolUnavailable};

/// Сериализовать `ToolUnavailable` в JSON-строку.
pub fn format_unavailable(value: ToolUnavailable) -> String {
    match serde_json::to_string_pretty(&value) {
        Ok(s) => s,
        Err(e) => format!("{{\"status\":\"error\",\"message\":\"Сериализация: {}\"}}", e),
    }
}

/// Проверить у демона статус папки репо. `None` — папка Ready, можно продолжать.
/// `Some(json)` — нужно отдать клиенту этот ToolUnavailable-ответ вместо данных.
pub async fn check_path_status(entry: &RepoEntry) -> Option<String> {
    let root = entry.local_root();
    match client::path_status_async(root).await {
        Ok(resp) => match resp.status {
            PathStatus::Ready => None,
            PathStatus::InitialIndexing | PathStatus::ReindexingBatch => Some(format_unavailable(
                ToolUnavailable::Indexing {
                    progress: resp.progress.unwrap_or_default(),
                    message: match resp.status {
                        PathStatus::InitialIndexing => "Первичная индексация в процессе".into(),
                        _ => "Применяется батч изменений".into(),
                    },
                },
            )),
            PathStatus::NotStarted => Some(format_unavailable(ToolUnavailable::NotStarted {
                message: format!(
                    "Путь {} не отслеживается демоном. Добавьте его в daemon.toml и вызовите 'code-index daemon reload'.",
                    root.display()
                ),
            })),
            PathStatus::Error => Some(format_unavailable(ToolUnavailable::Error {
                message: resp
                    .error
                    .unwrap_or_else(|| "Неизвестная ошибка индексации".into()),
            })),
        },
        Err(e) => Some(format_unavailable(ToolUnavailable::DaemonOffline {
            message: format!(
                "Демон code-index не доступен ({}). Запустите 'code-index daemon run' или Scheduled Task / systemd user unit.",
                e
            ),
        })),
    }
}

/// Макрос-хелпер: если папка не Ready — вернуть unavailable JSON немедленно.
macro_rules! bail_if_not_ready {
    ($entry:expr) => {{
        if let Some(json) = crate::mcp::tools::check_path_status($entry).await {
            return json;
        }
    }};
}

fn to_json<T: serde::Serialize>(value: &T) -> String {
    match serde_json::to_string_pretty(value) {
        Ok(s) => s,
        Err(e) => format!("{{\"error\": \"Сериализация: {}\"}}", e),
    }
}

// ── Реализации инструментов ─────────────────────────────────────────────────

pub async fn search_function(
    entry: &RepoEntry,
    query: String,
    limit: Option<usize>,
    language: Option<String>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    match storage.search_functions(&query, limit.unwrap_or(20), language.as_deref()) {
        Ok(r) => to_json(&r),
        Err(e) => format!("{{\"error\": \"search_function: {}\"}}", e),
    }
}

pub async fn search_class(
    entry: &RepoEntry,
    query: String,
    limit: Option<usize>,
    language: Option<String>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    match storage.search_classes(&query, limit.unwrap_or(20), language.as_deref()) {
        Ok(r) => to_json(&r),
        Err(e) => format!("{{\"error\": \"search_class: {}\"}}", e),
    }
}

pub async fn get_function(entry: &RepoEntry, name: String) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    match storage.get_function_by_name(&name) {
        Ok(r) => to_json(&r),
        Err(e) => format!("{{\"error\": \"get_function: {}\"}}", e),
    }
}

pub async fn get_class(entry: &RepoEntry, name: String) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    match storage.get_class_by_name(&name) {
        Ok(r) => to_json(&r),
        Err(e) => format!("{{\"error\": \"get_class: {}\"}}", e),
    }
}

pub async fn get_callers(
    entry: &RepoEntry,
    function_name: String,
    language: Option<String>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    match storage.get_callers(&function_name, language.as_deref()) {
        Ok(r) => to_json(&r),
        Err(e) => format!("{{\"error\": \"get_callers: {}\"}}", e),
    }
}

pub async fn get_callees(
    entry: &RepoEntry,
    function_name: String,
    language: Option<String>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    match storage.get_callees(&function_name, language.as_deref()) {
        Ok(r) => to_json(&r),
        Err(e) => format!("{{\"error\": \"get_callees: {}\"}}", e),
    }
}

pub async fn find_symbol(
    entry: &RepoEntry,
    name: String,
    language: Option<String>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    match storage.find_symbol(&name, language.as_deref()) {
        Ok(r) => to_json(&r),
        Err(e) => format!("{{\"error\": \"find_symbol: {}\"}}", e),
    }
}

pub async fn get_imports(
    entry: &RepoEntry,
    file_id: Option<i64>,
    module: Option<String>,
    language: Option<String>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    if let Some(fid) = file_id {
        return match storage.get_imports_by_file(fid) {
            Ok(r) => to_json(&r),
            Err(e) => format!("{{\"error\": \"get_imports_by_file: {}\"}}", e),
        };
    }
    if let Some(ref m) = module {
        return match storage.get_imports_by_module(m, language.as_deref()) {
            Ok(r) => to_json(&r),
            Err(e) => format!("{{\"error\": \"get_imports_by_module: {}\"}}", e),
        };
    }
    "{\"error\": \"Укажите file_id или module\"}".to_string()
}

pub async fn get_file_summary(entry: &RepoEntry, path: String) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    match storage.get_file_summary(&path) {
        Ok(Some(s)) => to_json(&s),
        Ok(None) => format!("{{\"error\": \"Файл '{}' не найден\"}}", path),
        Err(e) => format!("{{\"error\": \"get_file_summary: {}\"}}", e),
    }
}

/// Статистика по одному репо: читает локальный SQLite. Для remote — паника
/// (диспатчер не должен сюда попадать). get_stats остаётся диагностическим:
/// возвращает данные даже если папка не Ready.
async fn local_stats(alias: &str, entry: &RepoEntry) -> serde_json::Value {
    let root = entry.local_root();
    let path_info = client::path_status_async(root).await.ok();
    let storage = entry.local_storage().lock().await;
    match storage.get_stats() {
        Ok(mut stats) => {
            stats.indexing_status = None;
            serde_json::json!({
                "repo": alias,
                "db": stats,
                "path": root.display().to_string(),
                "daemon": path_info,
            })
        }
        Err(e) => serde_json::json!({
            "repo": alias,
            "error": format!("get_stats: {}", e),
            "path": root.display().to_string(),
        }),
    }
}

/// Запрос статистики у удалённого serve через `/federate/get_stats` с таймаутом.
async fn remote_stats(
    server: &CodeIndexServer,
    alias: &str,
    entry: &RepoEntry,
) -> serde_json::Value {
    use tokio::time::{timeout, Duration};

    let fut = crate::federation::dispatcher::dispatch_remote_value(
        &server.clients,
        &entry.ip,
        "get_stats",
        serde_json::json!({ "repo": alias }),
    );
    let body = match timeout(Duration::from_secs(5), fut).await {
        Ok(b) => b,
        Err(_) => {
            return serde_json::json!({
                "repo": alias,
                "ip": entry.ip,
                "status": "unreachable",
                "error": "timeout 5s",
            });
        }
    };
    // Удалённый сервер отвечает строкой JSON (тот же формат, что local_stats).
    // Если парсинг падает — остаётся хотя бы raw для диагностики.
    serde_json::from_str::<serde_json::Value>(&body).unwrap_or_else(|_| {
        serde_json::json!({
            "repo": alias,
            "ip": entry.ip,
            "status": "parse_error",
            "raw": body,
        })
    })
}

/// Диспатч одного запроса по `repo` (с учётом is_local). Используется и через
/// MCP-tool, и через `/federate/get_stats` для конкретного алиаса.
pub async fn one_stats(
    server: &CodeIndexServer,
    alias: &str,
    entry: &RepoEntry,
) -> serde_json::Value {
    if entry.is_local {
        local_stats(alias, entry).await
    } else {
        remote_stats(server, alias, entry).await
    }
}

/// Полная сводка: для одного `repo` или fan-out по всем подключённым.
pub async fn get_stats(server: &CodeIndexServer, repo: Option<String>) -> String {
    if let Some(alias) = repo {
        return match server.repos.get(&alias) {
            Some(entry) => to_json(&one_stats(server, &alias, entry).await),
            None => format_unavailable(ToolUnavailable::NotStarted {
                message: format!(
                    "Неизвестный repo '{}'. Доступные: {:?}.",
                    alias,
                    server.repo_aliases()
                ),
            }),
        };
    }

    // Fan-out по всем репо. Параллельно через JoinSet, удалённые с таймаутом 5с.
    let mut set = tokio::task::JoinSet::new();
    for alias in server.repos.keys().cloned().collect::<Vec<_>>() {
        let server_clone = server.clone();
        set.spawn(async move {
            let entry = server_clone
                .repos
                .get(&alias)
                .expect("alias только что взят из repos.keys()");
            one_stats(&server_clone, &alias, entry).await
        });
    }

    let mut all = Vec::new();
    while let Some(res) = set.join_next().await {
        match res {
            Ok(v) => all.push(v),
            Err(e) => all.push(serde_json::json!({
                "status": "join_error",
                "error": e.to_string(),
            })),
        }
    }
    // JoinSet не сохраняет порядок — сортируем по `repo` для стабильности вывода.
    all.sort_by(|a, b| {
        let ka = a.get("repo").and_then(|v| v.as_str()).unwrap_or("");
        let kb = b.get("repo").and_then(|v| v.as_str()).unwrap_or("");
        ka.cmp(kb)
    });
    to_json(&serde_json::json!({ "repos": all }))
}

pub async fn search_text(
    entry: &RepoEntry,
    query: String,
    limit: Option<usize>,
    language: Option<String>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    match storage.search_text(&query, limit.unwrap_or(20), language.as_deref()) {
        Ok(results) => {
            let items: Vec<serde_json::Value> = results
                .into_iter()
                .map(|(path, snippet)| serde_json::json!({ "path": path, "snippet": snippet }))
                .collect();
            to_json(&items)
        }
        Err(e) => format!("{{\"error\": \"search_text: {}\"}}", e),
    }
}

pub async fn grep_body(
    entry: &RepoEntry,
    pattern: Option<String>,
    regex: Option<String>,
    language: Option<String>,
    limit: Option<usize>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    match storage.grep_body(
        pattern.as_deref(),
        regex.as_deref(),
        language.as_deref(),
        limit.unwrap_or(100),
    ) {
        Ok(r) => to_json(&r),
        Err(e) => format!("{{\"error\": \"grep_body: {}\"}}", e),
    }
}

/// Живость MCP + демон по каждому репо.
pub async fn health(server: &CodeIndexServer) -> String {
    let daemon_info = client::runtime_info();

    // Сводка по репо: для local — статус пути у демона; для remote —
    // короткая запись без HTTP-ping (ping вне rc6).
    let mut repos = Vec::new();
    for (alias, entry) in server.repos.iter() {
        if !entry.is_local {
            repos.push(serde_json::json!({
                "repo": alias,
                "ip": entry.ip,
                "kind": "remote",
            }));
            continue;
        }
        let root = entry.local_root();
        let path_status = match client::path_status_async(root).await {
            Ok(s) => serde_json::to_value(s).unwrap_or(serde_json::Value::Null),
            Err(e) => serde_json::json!({ "error": e.to_string() }),
        };
        repos.push(serde_json::json!({
            "repo": alias,
            "root_path": root.display().to_string(),
            "path_status": path_status,
        }));
    }

    let daemon_health = match daemon_info {
        Some(_) => serde_json::json!({ "status": "online" }),
        None => serde_json::json!({
            "status": "offline",
            "message": "Демон не запущен (runtime-info отсутствует)",
        }),
    };

    let obj = serde_json::json!({
        "mcp": {
            "status": "ok",
            "version": env!("CARGO_PKG_VERSION"),
            "repos": server.repo_aliases(),
        },
        "daemon": daemon_health,
        "repos": repos,
    });
    to_json(&obj)
}
