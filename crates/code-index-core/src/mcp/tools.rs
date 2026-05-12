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

/// Soft-cap: число строк в одном `read_file` (по умолчанию).
pub(crate) const READ_FILE_SOFT_CAP_LINES: usize = 5_000;
/// Soft-cap: размер ответа `read_file` в байтах (по умолчанию).
pub(crate) const READ_FILE_SOFT_CAP_BYTES: usize = 500 * 1024;
/// Hard-cap: абсолютный максимум для `read_file`, даже с line_start/line_end.
pub(crate) const READ_FILE_HARD_CAP_BYTES: usize = 2 * 1024 * 1024;
/// Hard-cap: суммарный размер ответа grep_text/grep_body.
pub(crate) const GREP_TOTAL_BYTES_CAP: usize = 1 * 1024 * 1024;
/// Default-limit grep_text если path_glob и language не заданы.
pub(crate) const GREP_TEXT_FULL_SCAN_DEFAULT_LIMIT: usize = 100;

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

// ── Event-based invalidation helpers (Phase 2) ──────────────────────────────

/// Завернуть результат tool'а в `{result, _meta: {dependent_files: [...]}}`.
///
/// Целевой потребитель — `mcp-cache-ci`: при cache-fill он парсит payload и
/// регистрирует связи `cache_key → file_path` в `reverse_index`. По
/// последующему `POST /invalidate {file_paths: [...]}` от daemon после
/// `transaction.commit()` SQLite (этап 3) cache-ci мгновенно сносит ровно те
/// entries, что зависят от изменённых файлов — не задевая соседних.
///
/// `dependent_files` пустой → entry попадёт в кэш без file-зависимостей и будет
/// чиститься только по TTL (как раньше). Это нормально для tools без явной
/// привязки к файлам (часть BSL-инструментов).
///
/// Дубликаты в `dependent_files` дедуплицируются (HashSet → Vec, без гарантии
/// порядка — cache-ci порядок не использует).
pub(crate) fn wrap_with_meta<T: serde::Serialize>(
    result: &T,
    dependent_files: Vec<String>,
) -> String {
    use std::collections::HashSet;
    let deps: Vec<String> = dependent_files
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let result_value = match serde_json::to_value(result) {
        Ok(v) => v,
        Err(e) => return format!("{{\"error\": \"Сериализация result: {}\"}}", e),
    };
    let wrapped = serde_json::json!({
        "result": result_value,
        "_meta": { "dependent_files": deps },
    });
    serde_json::to_string_pretty(&wrapped)
        .unwrap_or_else(|e| format!("{{\"error\": \"Сериализация wrap: {}\"}}", e))
}

/// Собрать `dependent_files` из vec'а записей через extractor file_id.
/// Применяется к Vec<FunctionRecord>, Vec<ClassRecord>, Vec<CallRecord> и т.п.
/// Дубликаты не нужно дедуплицировать здесь — `wrap_with_meta` сам сделает.
pub(crate) fn collect_paths_via<R>(
    storage: &tokio::sync::MutexGuard<'_, crate::storage::Storage>,
    records: &[R],
    extract: impl Fn(&R) -> i64,
) -> Vec<String> {
    records
        .iter()
        .map(|r| lookup_path(storage, extract(r)))
        .filter(|p| !p.is_empty())
        .collect()
}

// ── Phase 1 helpers ─────────────────────────────────────────────────────────

/// Скомпилировать glob → matcher через `globset`. Применяется к результатам
/// после SQL-выборки в search_*/get_*. Использует `storage::normalize_glob`
/// для приведения `**` к `*` (см. SQLite GLOB-семантику).
pub(crate) fn build_path_matcher(glob: &str) -> Result<globset::GlobMatcher, String> {
    let normalized = crate::storage::normalize_glob(glob);
    globset::Glob::new(&normalized)
        .map(|g| g.compile_matcher())
        .map_err(|e| format!("невалидный glob '{}': {}", glob, e))
}

/// Lookup пути по file_id через storage. Любая ошибка/отсутствие → пустая строка
/// (она не пройдёт ни один matcher, так что результат честно отбросится).
/// Storage уже заблокирован вызывающей стороной (передаётся через `&MutexGuard`).
pub(crate) fn lookup_path(
    storage: &tokio::sync::MutexGuard<'_, crate::storage::Storage>,
    file_id: i64,
) -> String {
    storage
        .get_path_by_file_id(file_id)
        .ok()
        .flatten()
        .unwrap_or_default()
}

pub(crate) fn matches_with(matcher: &globset::GlobMatcher, path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    matcher.is_match(path)
}

// ── Реализации инструментов ─────────────────────────────────────────────────

pub async fn search_function(
    entry: &RepoEntry,
    query: String,
    limit: Option<usize>,
    language: Option<String>,
    path_glob: Option<String>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    let want = limit.unwrap_or(20);
    // Если path_glob задан — берём с запасом (5×, до 500), потом фильтруем по пути,
    // потом обрезаем до want. Это компромисс между точностью и нагрузкой.
    let sql_limit = if path_glob.is_some() {
        (want.saturating_mul(5)).min(500)
    } else {
        want
    };
    match storage.search_functions(&query, sql_limit, language.as_deref()) {
        Ok(mut r) => {
            if let Some(ref g) = path_glob {
                let matcher = match build_path_matcher(g) {
                    Ok(m) => m,
                    Err(e) => return format!("{{\"error\": \"path_glob: {}\"}}", e),
                };
                r.retain(|fr| matches_with(&matcher, &lookup_path(&storage, fr.file_id)));
                r.truncate(want);
            }
            let deps = collect_paths_via(&storage, &r, |fr| fr.file_id);
            wrap_with_meta(&r, deps)
        }
        Err(e) => format!("{{\"error\": \"search_function: {}\"}}", e),
    }
}

pub async fn search_class(
    entry: &RepoEntry,
    query: String,
    limit: Option<usize>,
    language: Option<String>,
    path_glob: Option<String>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    let want = limit.unwrap_or(20);
    let sql_limit = if path_glob.is_some() {
        (want.saturating_mul(5)).min(500)
    } else {
        want
    };
    match storage.search_classes(&query, sql_limit, language.as_deref()) {
        Ok(mut r) => {
            if let Some(ref g) = path_glob {
                let matcher = match build_path_matcher(g) {
                    Ok(m) => m,
                    Err(e) => return format!("{{\"error\": \"path_glob: {}\"}}", e),
                };
                r.retain(|cr| matches_with(&matcher, &lookup_path(&storage, cr.file_id)));
                r.truncate(want);
            }
            let deps = collect_paths_via(&storage, &r, |cr| cr.file_id);
            wrap_with_meta(&r, deps)
        }
        Err(e) => format!("{{\"error\": \"search_class: {}\"}}", e),
    }
}

pub async fn get_function(
    entry: &RepoEntry,
    name: String,
    path_glob: Option<String>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    match storage.get_function_by_name(&name) {
        Ok(mut r) => {
            if let Some(ref g) = path_glob {
                let matcher = match build_path_matcher(g) {
                    Ok(m) => m,
                    Err(e) => return format!("{{\"error\": \"path_glob: {}\"}}", e),
                };
                r.retain(|fr| matches_with(&matcher, &lookup_path(&storage, fr.file_id)));
            }
            let deps = collect_paths_via(&storage, &r, |fr| fr.file_id);
            wrap_with_meta(&r, deps)
        }
        Err(e) => format!("{{\"error\": \"get_function: {}\"}}", e),
    }
}

pub async fn get_class(
    entry: &RepoEntry,
    name: String,
    path_glob: Option<String>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    match storage.get_class_by_name(&name) {
        Ok(mut r) => {
            if let Some(ref g) = path_glob {
                let matcher = match build_path_matcher(g) {
                    Ok(m) => m,
                    Err(e) => return format!("{{\"error\": \"path_glob: {}\"}}", e),
                };
                r.retain(|cr| matches_with(&matcher, &lookup_path(&storage, cr.file_id)));
            }
            let deps = collect_paths_via(&storage, &r, |cr| cr.file_id);
            wrap_with_meta(&r, deps)
        }
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
        Ok(r) => {
            let deps = collect_paths_via(&storage, &r, |cr| cr.file_id);
            wrap_with_meta(&r, deps)
        }
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
        Ok(r) => {
            let deps = collect_paths_via(&storage, &r, |cr| cr.file_id);
            wrap_with_meta(&r, deps)
        }
        Err(e) => format!("{{\"error\": \"get_callees: {}\"}}", e),
    }
}

pub async fn find_symbol(
    entry: &RepoEntry,
    name: String,
    language: Option<String>,
    path_glob: Option<String>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    match storage.find_symbol(&name, language.as_deref()) {
        Ok(mut r) => {
            if let Some(ref g) = path_glob {
                let matcher = match build_path_matcher(g) {
                    Ok(m) => m,
                    Err(e) => return format!("{{\"error\": \"path_glob: {}\"}}", e),
                };
                r.functions
                    .retain(|fr| matches_with(&matcher, &lookup_path(&storage, fr.file_id)));
                r.classes
                    .retain(|cr| matches_with(&matcher, &lookup_path(&storage, cr.file_id)));
                r.variables
                    .retain(|vr| matches_with(&matcher, &lookup_path(&storage, vr.file_id)));
                r.imports
                    .retain(|ir| matches_with(&matcher, &lookup_path(&storage, ir.file_id)));
            }
            let mut deps = collect_paths_via(&storage, &r.functions, |fr| fr.file_id);
            deps.extend(collect_paths_via(&storage, &r.classes, |cr| cr.file_id));
            deps.extend(collect_paths_via(&storage, &r.variables, |vr| vr.file_id));
            deps.extend(collect_paths_via(&storage, &r.imports, |ir| ir.file_id));
            wrap_with_meta(&r, deps)
        }
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
            Ok(r) => {
                let deps = collect_paths_via(&storage, &r, |ir| ir.file_id);
                wrap_with_meta(&r, deps)
            }
            Err(e) => format!("{{\"error\": \"get_imports_by_file: {}\"}}", e),
        };
    }
    if let Some(ref m) = module {
        return match storage.get_imports_by_module(m, language.as_deref()) {
            Ok(r) => {
                let deps = collect_paths_via(&storage, &r, |ir| ir.file_id);
                wrap_with_meta(&r, deps)
            }
            Err(e) => format!("{{\"error\": \"get_imports_by_module: {}\"}}", e),
        };
    }
    "{\"error\": \"Укажите file_id или module\"}".to_string()
}

pub async fn get_file_summary(entry: &RepoEntry, path: String) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    match storage.get_file_summary(&path) {
        Ok(Some(s)) => {
            // Зависимость одна и явная — путь из args, который daemon только что
            // запросил из таблицы files. Если результат пустой — entry всё равно
            // помечается как зависящий от этого path (туда придёт invalidate при
            // изменении).
            wrap_with_meta(&s, vec![path.clone()])
        }
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
        entry.port,
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
    path_glob: Option<String>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    let want = limit.unwrap_or(20);
    let sql_limit = if path_glob.is_some() {
        (want.saturating_mul(5)).min(500)
    } else {
        want
    };
    match storage.search_text(&query, sql_limit, language.as_deref()) {
        Ok(mut results) => {
            if let Some(ref g) = path_glob {
                let matcher = match build_path_matcher(g) {
                    Ok(m) => m,
                    Err(e) => return format!("{{\"error\": \"path_glob: {}\"}}", e),
                };
                results.retain(|(p, _)| matches_with(&matcher, p));
                results.truncate(want);
            }
            let deps: Vec<String> = results.iter().map(|(p, _)| p.clone()).collect();
            let items: Vec<serde_json::Value> = results
                .into_iter()
                .map(|(path, snippet)| serde_json::json!({ "path": path, "snippet": snippet }))
                .collect();
            wrap_with_meta(&items, deps)
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
    path_glob: Option<String>,
    context_lines: Option<usize>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    // Если есть либо path_glob, либо context_lines — идём через grep_body_with_options.
    // Иначе старый grep_body для обратной совместимости с CHANGELOG / тестами.
    let ctx = context_lines.unwrap_or(0);
    if path_glob.is_some() || ctx > 0 {
        match storage.grep_body_with_options(
            pattern.as_deref(),
            regex.as_deref(),
            language.as_deref(),
            path_glob.as_deref(),
            limit.unwrap_or(100),
            ctx,
            GREP_TOTAL_BYTES_CAP,
        ) {
            Ok(r) => {
                let deps: Vec<String> = r.iter().map(|m| m.file_path.clone()).collect();
                wrap_with_meta(&r, deps)
            }
            Err(e) => format!("{{\"error\": \"grep_body: {}\"}}", e),
        }
    } else {
        match storage.grep_body(
            pattern.as_deref(),
            regex.as_deref(),
            language.as_deref(),
            limit.unwrap_or(100),
        ) {
            Ok(r) => {
                let deps: Vec<String> = r.iter().map(|m| m.file_path.clone()).collect();
                wrap_with_meta(&r, deps)
            }
            Err(e) => format!("{{\"error\": \"grep_body: {}\"}}", e),
        }
    }
}

// ── Phase 1 tool-handlers ───────────────────────────────────────────────────

pub async fn stat_file(entry: &RepoEntry, path: String) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    // stat_file намеренно НЕ заворачиваем в `_meta` — он non-cacheable по
    // policy (всегда быстрая прямая выборка, к тому же быстро меняется на
    // тонких операциях типа `oversize` после реиндексации). Прокси даже не
    // увидит этот ответ в кэше.
    match storage.stat_file_meta(&path) {
        Ok(r) => to_json(&r),
        Err(e) => format!("{{\"error\": \"stat_file: {}\"}}", e),
    }
}

pub async fn list_files(
    entry: &RepoEntry,
    pattern: Option<String>,
    path_prefix: Option<String>,
    language: Option<String>,
    limit: Option<usize>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    match storage.list_files_filtered(
        pattern.as_deref(),
        path_prefix.as_deref(),
        language.as_deref(),
        limit.unwrap_or(500),
    ) {
        Ok(r) => {
            let deps: Vec<String> = r.iter().map(|lf| lf.path.clone()).collect();
            wrap_with_meta(&r, deps)
        }
        Err(e) => format!("{{\"error\": \"list_files: {}\"}}", e),
    }
}

pub async fn read_file(
    entry: &RepoEntry,
    path: String,
    line_start: Option<usize>,
    line_end: Option<usize>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    match storage.read_file_text(
        &path,
        line_start,
        line_end,
        READ_FILE_SOFT_CAP_LINES,
        READ_FILE_SOFT_CAP_BYTES,
        READ_FILE_HARD_CAP_BYTES,
        // size_limit_bytes для hint в oversize-ответе. MCP-слой не знает per-repo
        // лимит daemon'а — передаём None, hint будет короткий «файл превышает лимит».
        // file_size в ответе всё равно показывается, оператор может сравнить.
        None,
    ) {
        Ok(Some(r)) => wrap_with_meta(&r, vec![path.clone()]),
        Ok(None) => format!("{{\"error\": \"Файл '{}' не найден в индексе\"}}", path),
        Err(e) => format!("{{\"error\": \"read_file: {}\"}}", e),
    }
}

pub async fn grep_text(
    entry: &RepoEntry,
    regex: String,
    path_glob: Option<String>,
    language: Option<String>,
    limit: Option<usize>,
    context_lines: Option<usize>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    let want = limit.unwrap_or_else(|| {
        // Без path_glob и language full-scan может быть тяжёлым — занижаем default.
        if path_glob.is_none() && language.is_none() {
            GREP_TEXT_FULL_SCAN_DEFAULT_LIMIT
        } else {
            500
        }
    });
    match storage.grep_text_filtered(
        &regex,
        path_glob.as_deref(),
        language.as_deref(),
        want,
        context_lines.unwrap_or(0),
        GREP_TOTAL_BYTES_CAP,
    ) {
        Ok(r) => {
            let deps: Vec<String> = r.iter().map(|m| m.path.clone()).collect();
            wrap_with_meta(&r, deps)
        }
        Err(e) => format!("{{\"error\": \"grep_text: {}\"}}", e),
    }
}

/// grep_code (Phase 2, v0.8.0): regex-поиск по содержимому **code-файлов** через
/// `file_contents` (zstd). Закрывает слепые зоны `grep_body` (ищет только в телах
/// функций/классов): module-level код, имена символов как идентификаторы,
/// комментарии вне тел, макросы, use-импорты. Файлы с `oversize=true` пропускаются —
/// для них нет content в индексе, нужно увеличить `max_code_file_size_bytes` либо
/// читать с диска.
pub async fn grep_code(
    entry: &RepoEntry,
    regex: String,
    path_glob: Option<String>,
    language: Option<String>,
    limit: Option<usize>,
    context_lines: Option<usize>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    let want = limit.unwrap_or_else(|| {
        // Без path_glob/language full-scan по всему репо может быть тяжёлым:
        // distinct от grep_text здесь сильнее, потому что zstd-decode на каждый
        // файл — full-scan на 100K файлов реально дорогой. Занижаем default.
        if path_glob.is_none() && language.is_none() {
            GREP_TEXT_FULL_SCAN_DEFAULT_LIMIT
        } else {
            500
        }
    });
    match storage.grep_code_filtered(
        &regex,
        path_glob.as_deref(),
        language.as_deref(),
        want,
        context_lines.unwrap_or(0),
        GREP_TOTAL_BYTES_CAP,
    ) {
        Ok(r) => {
            let deps: Vec<String> = r.iter().map(|m| m.path.clone()).collect();
            wrap_with_meta(&r, deps)
        }
        Err(e) => format!("{{\"error\": \"grep_code: {}\"}}", e),
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
