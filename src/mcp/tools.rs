// Реализации MCP-инструментов (v0.5+): read-only, с проверкой статуса папки у демона.
//
// Каждая функция сначала спрашивает у демона статус отслеживания `root_path`.
// Если папка не `Ready` — возвращается `ToolUnavailable` JSON и реальный
// запрос к БД не выполняется.

use super::CodeIndexServer;
use crate::daemon_core::client;
use crate::daemon_core::ipc::{PathStatus, ToolUnavailable};

/// Сериализовать `ToolUnavailable` в JSON-строку. Отдельная функция, чтобы макрос
/// мог её вызвать по полному пути.
pub fn format_unavailable(value: ToolUnavailable) -> String {
    match serde_json::to_string_pretty(&value) {
        Ok(s) => s,
        Err(e) => format!("{{\"status\":\"error\",\"message\":\"Сериализация: {}\"}}", e),
    }
}

/// Проверяет у демона статус папки сервера. Возвращает `Some(json)` — если нужно
/// отдать unavailable вместо реального результата. Возвращает `None` — если папка
/// `Ready` и можно продолжать.
pub async fn check_path_status(server: &CodeIndexServer) -> Option<String> {
    match client::path_status_async(&server.root_path).await {
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
                    server.root_path.display()
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
    ($server:expr) => {{
        if let Some(json) = crate::mcp::tools::check_path_status($server).await {
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
    server: &CodeIndexServer,
    query: String,
    limit: Option<usize>,
    language: Option<String>,
) -> String {
    bail_if_not_ready!(server);
    let storage = server.storage.lock().await;
    match storage.search_functions(&query, limit.unwrap_or(20), language.as_deref()) {
        Ok(r) => to_json(&r),
        Err(e) => format!("{{\"error\": \"search_function: {}\"}}", e),
    }
}

pub async fn search_class(
    server: &CodeIndexServer,
    query: String,
    limit: Option<usize>,
    language: Option<String>,
) -> String {
    bail_if_not_ready!(server);
    let storage = server.storage.lock().await;
    match storage.search_classes(&query, limit.unwrap_or(20), language.as_deref()) {
        Ok(r) => to_json(&r),
        Err(e) => format!("{{\"error\": \"search_class: {}\"}}", e),
    }
}

pub async fn get_function(server: &CodeIndexServer, name: String) -> String {
    bail_if_not_ready!(server);
    let storage = server.storage.lock().await;
    match storage.get_function_by_name(&name) {
        Ok(r) => to_json(&r),
        Err(e) => format!("{{\"error\": \"get_function: {}\"}}", e),
    }
}

pub async fn get_class(server: &CodeIndexServer, name: String) -> String {
    bail_if_not_ready!(server);
    let storage = server.storage.lock().await;
    match storage.get_class_by_name(&name) {
        Ok(r) => to_json(&r),
        Err(e) => format!("{{\"error\": \"get_class: {}\"}}", e),
    }
}

pub async fn get_callers(
    server: &CodeIndexServer,
    function_name: String,
    language: Option<String>,
) -> String {
    bail_if_not_ready!(server);
    let storage = server.storage.lock().await;
    match storage.get_callers(&function_name, language.as_deref()) {
        Ok(r) => to_json(&r),
        Err(e) => format!("{{\"error\": \"get_callers: {}\"}}", e),
    }
}

pub async fn get_callees(
    server: &CodeIndexServer,
    function_name: String,
    language: Option<String>,
) -> String {
    bail_if_not_ready!(server);
    let storage = server.storage.lock().await;
    match storage.get_callees(&function_name, language.as_deref()) {
        Ok(r) => to_json(&r),
        Err(e) => format!("{{\"error\": \"get_callees: {}\"}}", e),
    }
}

pub async fn find_symbol(
    server: &CodeIndexServer,
    name: String,
    language: Option<String>,
) -> String {
    bail_if_not_ready!(server);
    let storage = server.storage.lock().await;
    match storage.find_symbol(&name, language.as_deref()) {
        Ok(r) => to_json(&r),
        Err(e) => format!("{{\"error\": \"find_symbol: {}\"}}", e),
    }
}

pub async fn get_imports(
    server: &CodeIndexServer,
    file_id: Option<i64>,
    module: Option<String>,
    language: Option<String>,
) -> String {
    bail_if_not_ready!(server);
    let storage = server.storage.lock().await;
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

pub async fn get_file_summary(server: &CodeIndexServer, path: String) -> String {
    bail_if_not_ready!(server);
    let storage = server.storage.lock().await;
    match storage.get_file_summary(&path) {
        Ok(Some(s)) => to_json(&s),
        Ok(None) => format!("{{\"error\": \"Файл '{}' не найден\"}}", path),
        Err(e) => format!("{{\"error\": \"get_file_summary: {}\"}}", e),
    }
}

pub async fn get_stats(server: &CodeIndexServer) -> String {
    // `get_stats` мы возвращаем даже если папка не Ready — это диагностический
    // инструмент, агенту полезно знать состояние индекса во время индексации.
    let path_info = client::path_status_async(&server.root_path).await.ok();
    let storage = server.storage.lock().await;
    match storage.get_stats() {
        Ok(mut stats) => {
            // Старое поле indexing_status переиспользуем: кладём туда snake_case
            // статус от демона, чтобы ответ был совместим с прошлым форматом.
            stats.indexing_status = None; // поле Option, в read-only MCP не заполняем
            let enriched = serde_json::json!({
                "db": stats,
                "path": server.root_path.display().to_string(),
                "daemon": path_info,
            });
            to_json(&enriched)
        }
        Err(e) => format!("{{\"error\": \"get_stats: {}\"}}", e),
    }
}

pub async fn search_text(
    server: &CodeIndexServer,
    query: String,
    limit: Option<usize>,
    language: Option<String>,
) -> String {
    bail_if_not_ready!(server);
    let storage = server.storage.lock().await;
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
    server: &CodeIndexServer,
    pattern: Option<String>,
    regex: Option<String>,
    language: Option<String>,
    limit: Option<usize>,
) -> String {
    bail_if_not_ready!(server);
    let storage = server.storage.lock().await;
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

/// Проверка живости: показывает состояние самого MCP-сервера и дополнительно
/// проксирует health демона. Использует `cn_ping`/`dbg_health`-подобную логику.
pub async fn health(server: &CodeIndexServer) -> String {
    let daemon_info = client::runtime_info();
    let daemon_health = match daemon_info {
        Some(_) => match client::path_status_async(&server.root_path).await {
            Ok(path_status) => serde_json::json!({
                "status": "online",
                "path_status": path_status,
            }),
            Err(e) => serde_json::json!({
                "status": "online_but_path_unknown",
                "error": e.to_string(),
            }),
        },
        None => serde_json::json!({
            "status": "offline",
            "message": "Демон не запущен (runtime-info отсутствует)",
        }),
    };

    let obj = serde_json::json!({
        "mcp": {
            "status": "ok",
            "version": env!("CARGO_PKG_VERSION"),
            "root_path": server.root_path.display().to_string(),
        },
        "daemon": daemon_health,
    });
    to_json(&obj)
}
