//! Выбор репозиториев для fan-out (#32).
//!
//! Разбор селектора, общий одиночный путь и последовательное исполнение набора
//! с общими бюджетами. Селектор — список алиасов (`repos`) либо маска (`repo`):
//!
//! - зарегистрированный ТОЧНЫЙ алиас имеет приоритет над трактовкой маски;
//! - `repos` — непустой массив непустых строк, не больше [`MAX_REPOS`]
//!   элементов, повторы убираются с сохранением порядка;
//! - маска раскрывается в СОРТИРОВАННОМ порядке и тоже не больше [`MAX_REPOS`]
//!   репозиториев;
//! - `repo` и `repos` одновременно — конфликт, а не «победа одного из них»;
//! - селектор НИКОГДА не трактуется как путь или адрес;
//! - ошибка перечисляет доступные алиасы в существующем ограниченном формате
//!   (`Available: [...]`), поэтому клиент сразу видит, что было можно.
//!
//! Одиночный точный алиас — не набор: [`RepoSelection::is_single`] говорит
//! вызывающему, что такой вызов исполняется прежним одиночным путём и форма
//! ответа не меняется.
//!
//! Разбор не обращается к хранилищу; исполнение использует один снимок RepoEntry
//! и передаёт остатки бюджетов локальному или ограниченному удалённому входу.

use serde_json::Value;

/// Сколько репозиториев разрешено в одном fan-out.
pub const MAX_REPOS: usize = 8;

/// Сколько доступных алиасов перечислять в тексте ошибки.
pub const MAX_LISTED_ALIASES: usize = 20;

/// Инструменты, поддерживающие набор репозиториев. Ровно пять: остальным
/// `repos` отклоняется явно, а не игнорируется.
pub const MULTI_REPO_TOOLS: [&str; 5] = [
    "grep_code",
    "grep_text",
    "list_files",
    "find_symbol",
    "get_object_structure",
];

/// Поддерживает ли инструмент набор репозиториев.
pub fn supports_multi_repo(tool: &str) -> bool {
    MULTI_REPO_TOOLS.contains(&tool)
}

/// Разобранный селектор репозиториев.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoSelection {
    /// Алиасы в порядке исполнения.
    pub repos: Vec<String>,
    /// Селектор был маской (а не списком/алиасом).
    pub from_mask: bool,
    pub from_list: bool,
}

impl RepoSelection {
    /// Одиночный точный алиас — исполняется прежним одиночным путём.
    pub fn is_single(&self) -> bool {
        self.repos.len() == 1 && !self.from_mask && !self.from_list
    }
}

/// Отказ разбора селектора. Текст готов для клиента: он уже содержит перечень
/// доступных алиасов.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectorError(pub String);

impl std::fmt::Display for SelectorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SelectorError {}

/// Отказ с перечнем доступных алиасов (не больше [`MAX_LISTED_ALIASES`]).
fn selector_error(message: &str, registered: &[String]) -> SelectorError {
    SelectorError(format!(
        "{} Available: {}",
        message,
        super::format_repo_list(registered)
    ))
}

/// Разобрать селектор репозиториев из аргументов вызова.
///
/// `Ok(None)` — селектора нет вовсе (прежнее одиночное поведение с
/// `default_repo`). `Ok(Some(_))` — селектор есть; одиночный точный алиас
/// отличим по [`RepoSelection::is_single`].
pub fn resolve_repo_selector(
    args: &serde_json::Map<String, Value>,
    registered: &[String],
) -> Result<Option<RepoSelection>, SelectorError> {
    let repo_arg = args.get("repo");
    let repos_arg = args.get("repos");
    if repo_arg.is_some() && repos_arg.is_some() {
        return Err(selector_error(
            "'repo' и 'repos' одновременно — выберите один селектор.",
            registered,
        ));
    }

    if let Some(value) = repos_arg {
        let Some(items) = value.as_array() else {
            return Err(selector_error(
                "'repos' должен быть массивом строковых алиасов.",
                registered,
            ));
        };
        if items.len() > MAX_REPOS {
            return Err(selector_error(
                "в repos больше восьми элементов.",
                registered,
            ));
        }
        if items.is_empty() {
            return Err(selector_error(
                "'repos' пуст — перечислите хотя бы один алиас.",
                registered,
            ));
        }
        let mut out: Vec<String> = Vec::new();
        for item in items {
            let Some(raw) = item.as_str() else {
                return Err(selector_error(
                    "'repos' должен быть массивом строковых алиасов.",
                    registered,
                ));
            };
            let alias = raw.trim();
            if alias.is_empty() {
                return Err(selector_error(
                    "'repos' содержит пустой элемент.",
                    registered,
                ));
            }
            if !registered.iter().any(|a| a == alias) {
                return Err(selector_error(
                    &format!("неизвестный алиас '{}'.", alias),
                    registered,
                ));
            }
            // Повторение не ошибка — просто не исполняем один repo дважды.
            if !out.iter().any(|a| a == alias) {
                out.push(alias.to_string());
            }
        }
        if out.len() > MAX_REPOS {
            return Err(selector_error(
                &format!("в 'repos' больше {} репозиториев.", MAX_REPOS),
                registered,
            ));
        }
        return Ok(Some(RepoSelection {
            repos: out,
            from_mask: false,
            from_list: true,
        }));
    }

    let Some(value) = repo_arg else {
        return Ok(None);
    };
    let Some(raw) = value.as_str() else {
        return Err(selector_error(
            "'repo' должен быть строковым алиасом либо маской алиасов.",
            registered,
        ));
    };
    let mask = raw.trim();
    if mask.is_empty() {
        return Err(selector_error(
            "'repo' пуст — передайте алиас или маску.",
            registered,
        ));
    }
    // Зарегистрированный точный алиас главнее любой трактовки маски.
    if let Some(exact) = registered.iter().find(|a| a.as_str() == mask) {
        return Ok(Some(RepoSelection {
            repos: vec![exact.clone()],
            from_mask: false,
            from_list: false,
        }));
    }
    // Селектор — не путь и не адрес: такие значения не «раскрываются» никак.
    if mask.contains('/')
        || mask.contains('\\')
        || mask.contains(':')
        || mask.chars().any(char::is_whitespace)
    {
        return Err(selector_error(
            &format!(
                "'{}' не является маской алиасов: путь или адрес селектором не бывает.",
                mask
            ),
            registered,
        ));
    }
    if !mask.contains('*') && !mask.contains('?') {
        return Err(selector_error(
            &format!("неизвестный алиас '{}'.", mask),
            registered,
        ));
    }
    if mask.chars().all(|c| c == '*' || c == '?') {
        return Err(selector_error(
            "маска слишком широкая: добавьте часть алиаса.",
            registered,
        ));
    }
    if mask.len() > 256 || globset::Glob::new(mask).is_err() {
        return Err(selector_error("неправильная маска алиасов.", registered));
    }
    let mut matched: Vec<String> = registered
        .iter()
        .filter(|a| glob_match(mask, a))
        .cloned()
        .collect();
    matched.sort();
    if matched.is_empty() {
        return Err(selector_error(
            &format!("маска '{}' не покрывает ни одного репозитория.", mask),
            registered,
        ));
    }
    if matched.len() > MAX_REPOS {
        return Err(selector_error(
            &format!(
                "маска '{}' покрывает больше {} репозиториев — сузьте её.",
                mask, MAX_REPOS
            ),
            registered,
        ));
    }
    Ok(Some(RepoSelection {
        repos: matched,
        from_mask: true,
        from_list: false,
    }))
}

/// Простой glob по алиасу: `*` — любая последовательность, `?` — один символ.
/// Остальные символы буквальны; регистр значим (алиасы регистрозависимы).
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(sp) = star {
            pi = sp + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

use super::{
    cap, tools, CodeIndexServer, GrepCodeParams, GrepTextParams, ListFilesParams, NameParams,
    RepoEntry,
};
use crate::storage::search_support::{self, SearchBudget, SearchUsage};
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct BoundedSearchRequest {
    pub tool: String,
    pub args: Value,
    pub budget: SearchBudget,
    pub header_budget: SearchBudget,
    pub limit: usize,
    pub max_response_bytes: usize,
    pub timeout_ms: u64,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct BoundedSearchResponse {
    pub payload: Value,
    pub usage: SearchUsage,
    pub header_usage: SearchUsage,
}

fn parse<T: serde::de::DeserializeOwned>(args: Value) -> Result<T, Value> {
    serde_json::from_value(args)
        .map_err(|e| json!({"error": format!("failed to deserialize parameters: {e}")}))
}

fn decode(body: String) -> Value {
    serde_json::from_str(&body).unwrap_or_else(|_| json!({"error": "invalid search response"}))
}

pub(crate) fn search_shown(tool: &str, payload: &Value) -> usize {
    let result = payload.get("result").unwrap_or(payload);
    match tool {
        "grep_code" | "grep_text" => {
            result.get("shown").and_then(Value::as_u64).unwrap_or(0) as usize
        }
        "list_files" => result.as_array().map_or(0, Vec::len),
        "find_symbol" => ["functions", "classes", "variables", "imports"]
            .iter()
            .map(|key| {
                result
                    .get(*key)
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len)
            })
            .sum(),
        "get_object_structure" => result
            .get("results")
            .and_then(Value::as_array)
            .map_or_else(|| usize::from(result.get("full_name").is_some()), Vec::len),
        _ => 0,
    }
}

pub(crate) fn search_incomplete(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.iter().any(|(key, value)| {
            (key == "error" && !value.is_null())
                || (key == "complete" && value == &json!(false))
                || ((key == "truncated"
                    || key.ends_with("_truncated")
                    || key.ends_with("_omitted"))
                    && value == &json!(true))
                || (key == "files_unreadable" && value.as_u64().unwrap_or(0) > 0)
                || (key == "status" && value.as_str().is_some_and(|s| !matches!(s, "ok" | "ready")))
                || (key != "_meta" && search_incomplete(value))
        }),
        Value::Array(values) => values.iter().any(search_incomplete),
        _ => false,
    }
}

pub(crate) fn strip_internal(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("_meta");
            for value in map.values_mut() {
                strip_internal(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                strip_internal(value);
            }
        }
        _ => {}
    }
}

impl CodeIndexServer {
    pub(crate) async fn execute_single_search(
        &self,
        tool: &str,
        mut args: Value,
        entry: &RepoEntry,
        bounded: bool,
    ) -> Value {
        if !supports_multi_repo(tool) {
            return json!({"error": "unsupported search tool"});
        }
        // Типы и алиасы проверяются одинаково до локального и удалённого входа.
        let mut remote_empty_hint = None;
        match tool {
            "grep_code" => {
                let p: GrepCodeParams = match parse(args.clone()) {
                    Ok(p) => p,
                    Err(e) => return e,
                };
                let Some(regex) = super::grep_regex_from_params(
                    p.regex.clone(),
                    p.query.clone(),
                    p.pattern.clone(),
                ) else {
                    return json!({"error": "grep_code: укажите regex либо pattern"});
                };
                let glob = super::path_glob_from_params(p.path_glob.clone(), p.glob.clone());
                if entry.is_local {
                    let hint = super::grep_pattern_meta_hint(&p);
                    return decode(
                        tools::grep_code(
                            entry,
                            regex,
                            glob,
                            p.language,
                            p.limit,
                            p.context_lines,
                            hint,
                        )
                        .await,
                    );
                }
                remote_empty_hint = super::grep_pattern_meta_hint(&p);
                args["regex"] = json!(regex);
                args["path_glob"] = json!(glob);
                for key in ["query", "pattern", "glob"] {
                    args.as_object_mut().unwrap().remove(key);
                }
            }
            "grep_text" => {
                let p: GrepTextParams = match parse(args.clone()) {
                    Ok(p) => p,
                    Err(e) => return e,
                };
                let Some(regex) = super::grep_regex_from_params(
                    p.regex.clone(),
                    p.query.clone(),
                    p.pattern.clone(),
                ) else {
                    return json!({"error": "grep_text: укажите regex либо pattern"});
                };
                let glob = super::path_glob_from_params(p.path_glob.clone(), p.glob.clone());
                if entry.is_local {
                    return decode(
                        tools::grep_text(entry, regex, glob, p.language, p.limit, p.context_lines)
                            .await,
                    );
                }
                args["regex"] = json!(regex);
                args["path_glob"] = json!(glob);
                for key in ["query", "pattern", "glob"] {
                    args.as_object_mut().unwrap().remove(key);
                }
            }
            "list_files" => {
                let p: ListFilesParams = match parse(args.clone()) {
                    Ok(p) => p,
                    Err(e) => return e,
                };
                if entry.is_local {
                    return decode(
                        tools::list_files(entry, p.pattern, p.path_prefix, p.language, p.limit)
                            .await,
                    );
                }
            }
            "find_symbol" => {
                let p: NameParams = match parse::<NameParams>(args.clone()) {
                    Ok(p) => p.normalize_path_alias(),
                    Err(e) => return e,
                };
                if entry.is_local {
                    return decode(
                        tools::find_symbol(entry, p.name, p.language, p.path_glob).await,
                    );
                }
                args["name"] = json!(p.name);
                args["path_glob"] = json!(p.path_glob);
                for key in ["symbol", "query", "path"] {
                    args.as_object_mut().unwrap().remove(key);
                }
            }
            "get_object_structure" if entry.is_local => {
                if let Some(body) = tools::check_path_status(entry).await {
                    return decode(body);
                }
                if let Some(value) = tools::extras_building_response(entry.storage_pool()).await {
                    return value;
                }
                let ext = self
                    .extension_tools
                    .load()
                    .iter()
                    .find(|t| t.name() == tool)
                    .cloned();
                let Some(ext) = ext else {
                    return json!({"error": "tool not found"});
                };
                let repo = args
                    .get("repo")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                return ext
                    .execute(
                        args,
                        crate::extension::ToolContext {
                            repo: &repo,
                            root_path: entry.root_path.as_deref(),
                            language: entry.language.as_deref(),
                            storage: entry.storage_pool(),
                        },
                    )
                    .await;
            }
            _ => {}
        }
        if bounded {
            let request = BoundedSearchRequest {
                tool: tool.to_string(),
                args: args.clone(),
                budget: search_support::remaining_budget(SearchBudget::multi_repo()),
                header_budget: search_support::remaining_header_budget(),
                limit: search_support::search_limit().unwrap_or(100),
                max_response_bytes: args
                    .get("max_response_bytes")
                    .and_then(Value::as_u64)
                    .unwrap_or(cap::DEFAULT_MAX_RESPONSE_BYTES as u64)
                    as usize,
                timeout_ms: search_support::remaining_timeout().as_millis().min(30_000) as u64,
            };
            let client = match self.clients.get_or_create(&entry.ip, entry.port).await {
                Ok(client) => client,
                Err(e) => return json!({"error": e.to_string()}),
            };
            return match client
                .call_bounded_search(
                    &request,
                    request.max_response_bytes,
                    Duration::from_millis(request.timeout_ms),
                )
                .await
            {
                Ok(response) => {
                    search_support::record_usage(&response.usage);
                    search_support::record_header_usage(
                        response.header_usage.files,
                        response.header_usage.compressed_bytes,
                        response.header_usage.uncompressed_bytes,
                    );
                    decode(super::inject_empty_grep_hint(
                        response.payload.to_string(),
                        remote_empty_hint,
                    ))
                }
                Err(e) => {
                    let unsupported = e.to_string().contains("HTTP 404");
                    let not_connected = e
                        .downcast_ref::<reqwest::Error>()
                        .is_some_and(reqwest::Error::is_connect);
                    if !unsupported && !not_connected {
                        search_support::mark_remote_usage_unknown();
                    }
                    json!({"error": format!("bounded_search: {e}"),
                        "reason": if unsupported { "unsupported_remote" } else { "remote_error" }})
                }
            };
        }
        let (endpoint, params) = if tool == "get_object_structure" {
            ("extension", json!({"tool_name": tool, "args": args}))
        } else {
            (tool, args)
        };
        let body = crate::federation::dispatcher::dispatch_remote_value(
            &self.clients,
            &entry.ip,
            entry.port,
            endpoint,
            params,
        )
        .await;
        decode(super::inject_empty_grep_hint(body, remote_empty_hint))
    }

    pub(crate) async fn execute_multi_repo(
        &self,
        tool: &str,
        args: Value,
        aliases: Vec<String>,
        snapshot: Arc<BTreeMap<String, RepoEntry>>,
        bytes: usize,
    ) -> Result<Value, String> {
        let maximum = if tool == "get_object_structure" {
            50
        } else {
            500
        };
        let default = if tool == "get_object_structure" {
            50
        } else {
            100
        };
        let limit = match args.get("limit") {
            None => default,
            Some(value) => value
                .as_u64()
                .filter(|&n| n > 0 && n <= maximum)
                .ok_or_else(|| format!("limit: допустимо 1–{maximum}"))?
                as usize,
        };
        let timeout = self
            .tool_timeout
            .unwrap_or(Duration::from_secs(30))
            .min(Duration::from_secs(30));
        let deadline = Instant::now() + timeout;
        let (mut value, usage) = search_support::search_scope(SearchBudget::multi_repo(), timeout, limit, async {
            let mut groups = Vec::new();
            let mut shown = 0usize;
            let mut complete = true;
            for alias in aliases {
                let budget = search_support::remaining_budget(SearchBudget::multi_repo());
                let used_bytes = serde_json::to_vec(&groups).map_or(bytes, |v| v.len());
                let remaining_bytes = (bytes.saturating_sub(512) / 3).saturating_sub(used_bytes).saturating_sub(512);
                let reason = if Instant::now() >= deadline { Some("timeout") }
                    else if shown >= limit { Some("limit") }
                    else if remaining_bytes < 256 { Some("response_bytes") }
                    else if budget.max_files == 0 || budget.max_compressed_bytes == 0 || budget.max_uncompressed_bytes == 0 { Some("scan_budget") }
                    else { None };
                if let Some(reason) = reason {
                    groups.push(json!({"repo": alias, "status": "skipped", "reason": reason}));
                    complete = false;
                    continue;
                }
                let Some(entry) = snapshot.get(&alias) else {
                    groups.push(json!({"repo": alias, "status": "error", "error": "unknown repo"}));
                    complete = false;
                    continue;
                };
                let remaining = limit.saturating_sub(shown);
                search_support::set_search_limit(remaining);
                search_support::set_response_bytes(remaining_bytes / 2);
                let mut params = args.clone();
                params.as_object_mut().unwrap().remove("repos");
                params["repo"] = json!(alias);
                params["limit"] = json!(remaining);
                params["max_response_bytes"] = json!(remaining_bytes);
                let future = self.execute_single_search(tool, params, entry, true);
                let mut payload = match tokio::time::timeout(deadline.saturating_duration_since(Instant::now()), future).await {
                    Ok(value) => value,
                    Err(_) => json!({"error": "tool_timeout", "complete": false}),
                };
                if entry.is_local && tool != "list_files" {
                    match tokio::time::timeout(
                        deadline.saturating_duration_since(Instant::now()),
                        tools::annotate_stale_index(payload.clone(), entry.storage_pool(), entry.root_path.as_deref()),
                    ).await {
                        Ok(annotated) => payload = annotated,
                        Err(_) => {
                            payload["data_version_check_incomplete"] = json!("timeout");
                            payload["complete"] = json!(false);
                        }
                    }
                }
                let count = search_shown(tool, &payload);
                shown += count;
                let partial = search_incomplete(&payload);
                let error = payload.get("error").is_some()
                    || payload.get("result").and_then(|v| v.get("error")).is_some()
                    || payload.get("status").and_then(Value::as_str).is_some_and(|s| !matches!(s, "ok" | "ready"));
                complete &= !partial;
                groups.push(json!({"repo": alias, "status": if error { "error" } else if partial { "partial" } else { "ok" }, "payload": payload}));
            }
            json!({"result": {"repos": groups, "shown": shown, "limit": limit, "truncated": !complete, "complete": complete,
                "header_usage": search_support::current_header_usage().to_json()}, "_meta": {}})
        }).await;
        value["result"]["search_usage"] = usage.to_json();
        Ok(value)
    }
}

pub(crate) fn resolve_response_bytes(
    args: &serde_json::Map<String, Value>,
) -> Result<usize, String> {
    let requested = match args.get("max_response_bytes") {
        None => None,
        Some(value) => Some(
            value
                .as_u64()
                .filter(|&n| n > 0)
                .ok_or("max_response_bytes должен быть положительным целым")? as usize,
        ),
    };
    let budget = cap::resolve_request_budget(requested);
    Ok(effective_response_bytes(budget.applied))
}

fn effective_response_bytes(applied: usize) -> usize {
    if applied == 0 {
        cap::DEFAULT_MAX_RESPONSE_BYTES
    } else {
        applied
    }
}

pub(crate) fn fit_multi_response(
    response: &mut Value,
    tool: &str,
    bytes: usize,
) -> Result<(), String> {
    *response = tools::map_response_payload(response.take(), |mut value| {
        strip_internal(&mut value);
        update_multi_shown(&mut value, tool);
        value
    });
    while serde_json::to_vec(response)
        .map_err(|e| e.to_string())?
        .len()
        > bytes
    {
        let mut reduced = false;
        *response = tools::map_response_payload(response.take(), |mut value| {
            reduced = trim_multi_response(&mut value, tool);
            update_multi_shown(&mut value, tool);
            value
        });
        if !reduced {
            return Err(
                "max_response_bytes: превышен бюджет диагностики неизвестных параметров или других обязательных предупреждений и групп; увеличьте бюджет".into(),
            );
        }
    }
    Ok(())
}

fn trim_multi_response(value: &mut Value, tool: &str) -> bool {
    let Some(groups) = value["result"]["repos"].as_array_mut() else {
        return false;
    };
    let Some(group) = groups.iter_mut().rev().find(|g| g.get("payload").is_some()) else {
        return false;
    };
    let mut payload = group["payload"].take();
    let omitted = search_shown(tool, &payload);
    // Удаляем находки целиком; диагностические строки и причины не сокращаем.
    if let Some(obj) = payload.as_object_mut() {
        let result = obj.remove("result").unwrap_or(Value::Null);
        let mut summary = serde_json::Map::new();
        if let Some(result) = result.as_object() {
            for (key, value) in result {
                if matches!(
                    key.as_str(),
                    "error"
                        | "files_unreadable"
                        | "search_usage"
                        | "header_usage"
                        | "complete"
                        | "truncated"
                        | "other_category"
                        | "unknown_params"
                        | "unknown_params_note"
                        | "stale_index"
                        | "data_version"
                ) || key.contains("hint")
                    || key.contains("warning")
                    || key.contains("reason")
                    || key.contains("incomplete")
                    || key.starts_with("file_annotations")
                {
                    summary.insert(key.clone(), value.clone());
                }
            }
        }
        summary.insert("omitted".into(), json!(true));
        if let Some(reason) = summary.remove("reason") {
            summary.insert("search_reason".into(), reason);
        }
        summary.insert("reason".into(), json!("response_bytes"));
        summary.insert("shown_before_cap".into(), json!(omitted));
        summary.insert("complete".into(), json!(false));
        summary.insert("truncated".into(), json!(true));
        obj.insert("result".into(), Value::Object(summary));
    }
    if let Some(obj) = group.as_object_mut() {
        obj.remove("payload");
        if let Some(reason) = obj.remove("reason") {
            obj.insert("search_reason".into(), reason);
        }
        obj.insert("status".into(), json!("truncated"));
        obj.insert("reason".into(), json!("response_bytes"));
        obj.insert("warnings".into(), payload);
    }
    value["result"]["complete"] = json!(false);
    value["result"]["truncated"] = json!(true);
    true
}

fn update_multi_shown(value: &mut Value, tool: &str) {
    let shown = value["result"]["repos"].as_array().map_or(0, |groups| {
        groups
            .iter()
            .filter_map(|g| g.get("payload"))
            .map(|p| search_shown(tool, p))
            .sum::<usize>()
    });
    value["result"]["shown"] = json!(shown);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn multi_envelope(payload: Value) -> Value {
        serde_json::to_value(super::super::CallToolResult::structured(payload)).unwrap()
    }

    fn assert_equal_payload_forms(response: &Value) {
        let text: Value =
            serde_json::from_str(response["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text, response["structuredContent"]);
    }

    #[test]
    fn final_multi_cap_preserves_repositories_identifiers_and_warnings() {
        let identifier = "/α/\"quoted\"\\full-path.rs | rust | 1 lines | 1B";
        let warning = "Полное предупреждение: \"не редактировать\" \\ исходник";
        let mut response = multi_envelope(json!({
            "result": {
                "repos": [
                    {"repo": "first", "status": "ok", "payload": {"result": [identifier]}},
                    {"repo": "second", "status": "partial", "payload": {
                        "result": ["Я\"\\".repeat(10000)],
                        "reason": "scan_budget", "complete": false,
                        "file_annotations_hint": warning,
                        "unknown_params": ["ordinary_unknown"]
                    }}
                ],
                "shown": 2, "complete": false, "truncated": true
            },
            "unknown_params": ["outer_unknown"],
            "unknown_params_note": warning
        }));
        fit_multi_response(&mut response, "list_files", 5000).unwrap();
        assert!(serde_json::to_vec(&response).unwrap().len() <= 5000);
        assert_equal_payload_forms(&response);
        let payload = &response["structuredContent"];
        let groups = &payload["result"]["repos"];
        assert_eq!(groups.as_array().unwrap().len(), 2);
        assert_eq!(groups[0]["repo"], "first");
        assert_eq!(groups[1]["repo"], "second");
        assert_eq!(groups[0]["payload"]["result"][0], identifier);
        assert_eq!(groups[1]["warnings"]["file_annotations_hint"], warning);
        assert_eq!(groups[1]["warnings"]["reason"], "scan_budget");
        assert_eq!(
            groups[1]["warnings"]["unknown_params"],
            json!(["ordinary_unknown"])
        );
        assert_eq!(payload["unknown_params_note"], warning);
        assert_eq!(payload["unknown_params"], json!(["outer_unknown"]));
        assert_eq!(payload["result"]["shown"], 1);
        assert_eq!(payload["result"]["complete"], false);
        assert_eq!(payload["result"]["truncated"], true);
    }

    #[test]
    fn final_multi_cap_measures_unicode_and_json_escaping_in_both_forms() {
        let identifier = format!("/{}file.rs", "Ж\"\\".repeat(600));
        let payload = json!({"result": {
            "repos": [{"repo": "repo", "status": "ok", "payload": {"result": [identifier]}}],
            "shown": 1, "complete": true, "truncated": false
        }});
        let mut exact = multi_envelope(payload.clone());
        let bytes = serde_json::to_vec(&exact).unwrap().len();
        assert!(bytes > payload.to_string().len() * 2);
        fit_multi_response(&mut exact, "list_files", bytes).unwrap();
        assert_eq!(exact["structuredContent"]["result"]["shown"], 1);
        assert_equal_payload_forms(&exact);
        let mut clipped = multi_envelope(payload);
        fit_multi_response(&mut clipped, "list_files", bytes - 1).unwrap();
        assert!(serde_json::to_vec(&clipped).unwrap().len() < bytes);
        assert_eq!(clipped["structuredContent"]["result"]["shown"], 0);
        assert_eq!(clipped["structuredContent"]["result"]["complete"], false);
        assert_equal_payload_forms(&clipped);
    }

    #[test]
    fn final_multi_cap_rejects_minimum_diagnostics_without_deleting_keys() {
        let huge = "x".repeat(100000);
        let mut response = multi_envelope(json!({
            "result": {"repos": [{"repo": "repo", "status": "skipped", "reason": "limit"}], "shown": 0},
            "unknown_params": [huge], "unknown_params_note": "полное предупреждение"
        }));
        let error = fit_multi_response(&mut response, "list_files", 48000).unwrap_err();
        assert!(error.contains("диагностики неизвестных параметров"));
        assert!(error.len() < 1000);
        assert_eq!(response["structuredContent"]["unknown_params"][0], huge);
        assert_equal_payload_forms(&response);
        let mut tiny = multi_envelope(json!({"result": {"repos": [{"repo": "repo"}]}}));
        assert!(fit_multi_response(&mut tiny, "list_files", 1).is_err());
    }

    #[test]
    fn disabled_server_cap_keeps_finite_multi_budget() {
        let bytes = effective_response_bytes(0);
        assert_eq!(bytes, cap::DEFAULT_MAX_RESPONSE_BYTES);
        assert!(bytes > 0);
        let mut response = multi_envelope(json!({"result": {
            "repos": [{"repo": "repo", "payload": {"result": ["x".repeat(bytes * 2)]}}],
            "complete": true, "shown": 1
        }}));
        fit_multi_response(&mut response, "list_files", bytes).unwrap();
        assert!(serde_json::to_vec(&response).unwrap().len() <= bytes);
        assert_eq!(response["structuredContent"]["result"]["complete"], false);
    }

    #[tokio::test]
    async fn multi_budget_validation_rejects_invalid_limits_and_byte_values() {
        for value in [json!(0), json!(-1), json!("48000"), json!(1.5)] {
            assert!(resolve_response_bytes(&args(json!({"max_response_bytes": value}))).is_err());
        }
        let server = CodeIndexServer::with_repos(BTreeMap::new());
        for limit in [json!(0), json!(501), json!(-1), json!("1")] {
            assert!(server
                .execute_multi_repo(
                    "list_files",
                    json!({"limit": limit}),
                    vec!["repo".into()],
                    Arc::new(BTreeMap::new()),
                    48000,
                )
                .await
                .is_err());
        }
    }

    fn aliases() -> Vec<String> {
        vec![
            "bp".to_string(),
            "ut".to_string(),
            "ut-test".to_string(),
            "zup".to_string(),
        ]
    }

    fn args(value: Value) -> serde_json::Map<String, Value> {
        match value {
            Value::Object(m) => m,
            _ => panic!("ожидался объект"),
        }
    }

    #[test]
    fn нет_селектора_значит_прежнее_поведение() {
        assert_eq!(
            resolve_repo_selector(&args(json!({})), &aliases()),
            Ok(None)
        );
    }

    #[test]
    fn точный_алиас_главнее_маски() {
        let sel = resolve_repo_selector(&args(json!({"repo": "ut-test"})), &aliases())
            .unwrap()
            .unwrap();
        assert_eq!(sel.repos, vec!["ut-test".to_string()]);
        assert!(sel.is_single());
    }

    #[test]
    fn маска_раскрывается_сортированной() {
        let sel = resolve_repo_selector(&args(json!({"repo": "ut*"})), &aliases())
            .unwrap()
            .unwrap();
        assert_eq!(sel.repos, vec!["ut".to_string(), "ut-test".to_string()]);
        assert!(sel.from_mask);
    }

    #[test]
    fn список_убирает_повторы_и_держит_порядок() {
        let sel = resolve_repo_selector(&args(json!({"repos": ["zup", "bp", "zup"]})), &aliases())
            .unwrap()
            .unwrap();
        assert_eq!(sel.repos, vec!["zup".to_string(), "bp".to_string()]);
    }

    #[test]
    fn конфликт_и_ошибки_селектора() {
        for bad in [
            json!({"repo": "ut", "repos": ["ut"]}),
            json!({"repos": []}),
            json!({"repos": [""]}),
            json!({"repos": [1]}),
            json!({"repos": ["нет-такого"]}),
            json!({"repo": 7}),
            json!({"repo": ""}),
            json!({"repo": "нет-такого"}),
            json!({"repo": "*"}),
            json!({"repo": "src/ut"}),
            json!({"repo": "198.51.100.10:8080"}),
            json!({"repo": "zz*"}),
        ] {
            assert!(
                resolve_repo_selector(&args(bad.clone()), &aliases()).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn девять_репозиториев_отклоняются() {
        let many: Vec<String> = (0..9).map(|i| format!("r{i}")).collect();
        let list: Vec<Value> = many.iter().map(|a| json!(a)).collect();
        assert!(resolve_repo_selector(&args(json!({ "repos": list })), &many).is_err());
        let eight: Vec<Value> = many[..8].iter().map(|a| json!(a)).collect();
        assert!(resolve_repo_selector(&args(json!({ "repos": eight })), &many).is_ok());
    }
}
