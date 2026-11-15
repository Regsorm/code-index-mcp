// Приёмная сторона федерации: HTTP-роутер `/federate/<tool>`.
//
// Принимает forwarded-вызовы от других serve-нод. Каждый handler:
//   1. парсит JSON в типизированную `*Params`-структуру;
//   2. resolve_repo + проверка `is_local` (если репо у нас не local — это
//      операционная ошибка вызывающей стороны: значит конфиги разъехались);
//   3. вызывает соответствующую функцию из `tools::*`;
//   4. возвращает строку JSON (тот же формат, что MCP tool-call).
//
// Endpoint защищён общим IP-whitelist middleware (см. `whitelist`).

use std::sync::Arc;

use axum::{
    extract::{Json, State},
    http::StatusCode,
    response::IntoResponse,
    routing::post,
    Router,
};

use crate::mcp::{
    git_tools, grep_regex_from_params, tools, CallTreeParams, CodeIndexServer, ExtensionToolParams,
    FilePathParams, FindPathParams, FunctionNameParams, GitChangedFilesParams,
    GitChangesByObjectParams, GitDiffParams, GitLogParams, GitShowFileParams, GrepBodyParams,
    GrepCodeParams, GrepTextParams, ImportParams, ListFilesParams, NameParams, ReadFileParams,
    RepoEntry, SearchParams, StatFileParams, StatsParams,
};

use super::dispatcher::federation_error;

type Server = Arc<CodeIndexServer>;

/// Собрать роутер `/federate/<tool>`. Вызывается в `serve_http` при наличии
/// `serve.toml` и ставится `merge` рядом с `/mcp`.
pub fn federate_router(server: CodeIndexServer) -> Router {
    Router::new()
        .route("/federate/bounded_search", post(handle_bounded_search))
        .route("/federate/search_function", post(handle_search_function))
        .route("/federate/search_class", post(handle_search_class))
        .route("/federate/get_function", post(handle_get_function))
        .route("/federate/get_class", post(handle_get_class))
        .route("/federate/get_callers", post(handle_get_callers))
        .route("/federate/get_callees", post(handle_get_callees))
        .route("/federate/find_path", post(handle_find_path))
        .route("/federate/get_call_tree", post(handle_get_call_tree))
        .route("/federate/find_symbol", post(handle_find_symbol))
        .route("/federate/get_imports", post(handle_get_imports))
        .route("/federate/get_file_summary", post(handle_get_file_summary))
        .route("/federate/get_stats", post(handle_get_stats))
        .route("/federate/search_text", post(handle_search_text))
        .route("/federate/grep_body", post(handle_grep_body))
        // Phase 1 (v0.7.0)
        .route("/federate/stat_file", post(handle_stat_file))
        .route("/federate/list_files", post(handle_list_files))
        .route("/federate/read_file", post(handle_read_file))
        // Git-слой: те же пять функций, что и в MCP-слое; git_config
        // проверяет исполняющая локальная нода.
        .route("/federate/git_log", post(handle_git_log))
        .route(
            "/federate/git_changed_files",
            post(handle_git_changed_files),
        )
        .route(
            "/federate/git_changes_by_object",
            post(handle_git_changes_by_object),
        )
        .route("/federate/git_diff", post(handle_git_diff))
        .route("/federate/git_show_file", post(handle_git_show_file))
        .route("/federate/grep_text", post(handle_grep_text))
        // Phase 2 (v0.8.0)
        .route("/federate/grep_code", post(handle_grep_code))
        // v0.8.1: универсальный route для extension-tools (BSL и любых
        // будущих расширений). Один route на все extension-tools, чтобы
        // не плодить per-tool маршруты при добавлении нового language-процессора.
        .route("/federate/extension", post(handle_extension_tool))
        .with_state(Arc::new(server))
}

// ── Хелперы ─────────────────────────────────────────────────────────────────

/// Обернуть строку JSON в HTTP-ответ 200 с `application/json`.
fn ok_json(body: String) -> axum::response::Response {
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

/// Найти RepoEntry с гарантией is_local=true. Если репо нет / он remote —
/// возвращаем federation-error JSON со статусом 200 (не 4xx, чтобы вызывающая
/// сторона могла прочитать тело и решить).
fn resolve_local(
    server: &CodeIndexServer,
    repo: &str,
    tool: &str,
) -> Result<RepoEntry, Box<axum::response::Response>> {
    let entry = match server.resolve_repo(repo) {
        Ok(e) => e,
        Err(j) => return Err(Box::new(ok_json(j))),
    };
    if !entry.is_local {
        return Err(Box::new(ok_json(federation_error(
            tool,
            &entry.ip,
            format!(
                "Конфиги разошлись: репо '{}' помечен local на удалённой стороне, \
                 но у нас он указывает на ip={}",
                repo, entry.ip
            ),
        ))));
    }
    Ok(entry)
}

async fn annotated_json(entry: &RepoEntry, body: String) -> axum::response::Response {
    let value = serde_json::from_str(&body)
        .unwrap_or_else(|_| serde_json::json!({"error": "invalid tool JSON"}));
    let value = tools::annotate_stale_index(value, entry.storage_pool(), None).await;
    ok_json(value.to_string())
}

async fn handle_bounded_search(
    State(server): State<Server>,
    Json(mut request): Json<crate::mcp::multi_repo::BoundedSearchRequest>,
) -> axum::response::Response {
    use crate::mcp::multi_repo::{supports_multi_repo, BoundedSearchResponse};
    use crate::storage::search_support::{self, SearchBudget};
    let fail = |message: &str| (StatusCode::BAD_REQUEST, message.to_string()).into_response();
    if !supports_multi_repo(&request.tool) {
        return fail("bounded_search: tool запрещён");
    }
    if server
        .allowed_tools
        .as_ref()
        .as_ref()
        .is_some_and(|allowed| !allowed.contains(&request.tool))
    {
        return fail("bounded_search: tool выключен whitelist");
    }
    if request.args.get("repos").is_some() || !request.args.is_object() {
        return fail("bounded_search: вложенный fan-out запрещён");
    }
    if request.args.get("full_names").is_some_and(|v| !v.is_null())
        && !server.mass_mode_tools.contains(&request.tool)
    {
        return fail("bounded_search: массовый режим выключен");
    }
    let Some(repo) = request.args.get("repo").and_then(serde_json::Value::as_str) else {
        return fail("bounded_search: repo должен быть строковым алиасом");
    };
    let snapshot = server.repos.load_full();
    let Some(entry) = snapshot.get(repo).filter(|entry| entry.is_local) else {
        return fail("bounded_search: требуется зарегистрированный локальный repo");
    };
    let ceiling = SearchBudget::multi_repo();
    let header_ceiling = SearchBudget::header_probe();
    let max_limit = if request.tool == "get_object_structure" {
        50
    } else {
        500
    };
    if request.limit == 0
        || request.limit > max_limit
        || request.budget.max_files > ceiling.max_files
        || request.budget.max_compressed_bytes > ceiling.max_compressed_bytes
        || request.budget.max_uncompressed_bytes > ceiling.max_uncompressed_bytes
        || request.header_budget.max_files > header_ceiling.max_files
        || request.header_budget.max_compressed_bytes > header_ceiling.max_compressed_bytes
        || request.header_budget.max_uncompressed_bytes > header_ceiling.max_uncompressed_bytes
        || request.max_response_bytes == 0
        || request.max_response_bytes > crate::mcp::cap::response_cap_hard()
        || request.timeout_ms == 0
        || request.timeout_ms > 30_000
    {
        return fail("bounded_search: неверный бюджет");
    }
    request.args["limit"] = serde_json::json!(request.limit);
    request.args["max_response_bytes"] = serde_json::json!(request.max_response_bytes);
    let timeout = std::time::Duration::from_millis(request.timeout_ms).min(
        server
            .tool_timeout
            .unwrap_or(std::time::Duration::from_secs(30)),
    );
    let ((payload, header_usage), mut usage) =
        search_support::search_scope(request.budget, timeout, request.limit, async {
            search_support::limit_header_budget(request.header_budget);
            search_support::set_response_bytes(request.max_response_bytes / 2);
            let run = async {
                let payload = server
                    .execute_single_search(&request.tool, request.args.clone(), entry, true)
                    .await;
                if request.tool == "list_files" {
                    payload
                } else {
                    tools::annotate_stale_index(payload, entry.storage_pool(), None).await
                }
            };
            let payload = match tokio::time::timeout(timeout, run).await {
                Ok(value) => value,
                Err(_) => serde_json::json!({"error": "tool_timeout", "complete": false}),
            };
            (payload, search_support::current_header_usage())
        })
        .await;
    // Набор не кэшируется: пути удалённого индекса не нужны origin для invalidation.
    usage.dependent_files.clear();
    let mut payload = payload;
    crate::mcp::multi_repo::strip_internal(&mut payload);
    let response = BoundedSearchResponse {
        payload,
        usage,
        header_usage,
    };
    let body = match serde_json::to_string(&response) {
        Ok(body) if body.len() <= request.max_response_bytes.saturating_add(4096) => body,
        _ => return fail("bounded_search: response_bytes"),
    };
    ok_json(body)
}

// ── Handlers ────────────────────────────────────────────────────────────────

async fn handle_search_function(
    State(server): State<Server>,
    Json(p): Json<SearchParams>,
) -> axum::response::Response {
    let entry = match resolve_local(&server, &p.repo, "search_function") {
        Ok(e) => e,
        Err(r) => return *r,
    };
    annotated_json(
        &entry,
        tools::search_function(&entry, p.query, p.limit, p.language, p.path_glob).await,
    )
    .await
}

async fn handle_search_class(
    State(server): State<Server>,
    Json(p): Json<SearchParams>,
) -> axum::response::Response {
    let entry = match resolve_local(&server, &p.repo, "search_class") {
        Ok(e) => e,
        Err(r) => return *r,
    };
    annotated_json(
        &entry,
        tools::search_class(&entry, p.query, p.limit, p.language, p.path_glob).await,
    )
    .await
}

async fn handle_get_function(
    State(server): State<Server>,
    Json(p): Json<NameParams>,
) -> axum::response::Response {
    let entry = match resolve_local(&server, &p.repo, "get_function") {
        Ok(e) => e,
        Err(r) => return *r,
    };
    annotated_json(
        &entry,
        tools::get_function(&entry, p.name, p.path_glob).await,
    )
    .await
}

async fn handle_get_class(
    State(server): State<Server>,
    Json(p): Json<NameParams>,
) -> axum::response::Response {
    let entry = match resolve_local(&server, &p.repo, "get_class") {
        Ok(e) => e,
        Err(r) => return *r,
    };
    annotated_json(&entry, tools::get_class(&entry, p.name, p.path_glob).await).await
}

async fn handle_get_callers(
    State(server): State<Server>,
    Json(p): Json<FunctionNameParams>,
) -> axum::response::Response {
    let entry = match resolve_local(&server, &p.repo, "get_callers") {
        Ok(e) => e,
        Err(r) => return *r,
    };
    annotated_json(
        &entry,
        tools::get_callers(&entry, p.function_name, p.language, p.limit, p.path_glob).await,
    )
    .await
}

async fn handle_get_callees(
    State(server): State<Server>,
    Json(p): Json<FunctionNameParams>,
) -> axum::response::Response {
    let entry = match resolve_local(&server, &p.repo, "get_callees") {
        Ok(e) => e,
        Err(r) => return *r,
    };
    annotated_json(
        &entry,
        tools::get_callees(&entry, p.function_name, p.language, p.limit, p.path_glob).await,
    )
    .await
}

async fn handle_find_path(
    State(server): State<Server>,
    Json(p): Json<FindPathParams>,
) -> axum::response::Response {
    let entry = match resolve_local(&server, &p.repo, "find_path") {
        Ok(e) => e,
        Err(r) => return *r,
    };
    annotated_json(
        &entry,
        tools::find_path(&entry, p.from, p.to, p.max_depth, p.language).await,
    )
    .await
}

async fn handle_get_call_tree(
    State(server): State<Server>,
    Json(p): Json<CallTreeParams>,
) -> axum::response::Response {
    let entry = match resolve_local(&server, &p.repo, "get_call_tree") {
        Ok(e) => e,
        Err(r) => return *r,
    };
    annotated_json(
        &entry,
        tools::get_call_tree(
            &entry,
            tools::CallTreeOptions {
                root: p.root,
                direction: p.direction,
                max_depth: p.max_depth,
                max_nodes: p.max_nodes,
                language: p.language,
                max_response_bytes: p.max_response_bytes,
            },
            &p.repo,
        )
        .await,
    )
    .await
}

async fn handle_find_symbol(
    State(server): State<Server>,
    Json(p): Json<NameParams>,
) -> axum::response::Response {
    let entry = match resolve_local(&server, &p.repo, "find_symbol") {
        Ok(e) => e,
        Err(r) => return *r,
    };
    annotated_json(
        &entry,
        tools::find_symbol(&entry, p.name, p.language, p.path_glob).await,
    )
    .await
}

async fn handle_get_imports(
    State(server): State<Server>,
    Json(p): Json<ImportParams>,
) -> axum::response::Response {
    let entry = match resolve_local(&server, &p.repo, "get_imports") {
        Ok(e) => e,
        Err(r) => return *r,
    };
    annotated_json(
        &entry,
        tools::get_imports(&entry, p.file_id, p.module, p.language, p.limit).await,
    )
    .await
}

async fn handle_get_file_summary(
    State(server): State<Server>,
    Json(p): Json<FilePathParams>,
) -> axum::response::Response {
    let entry = match resolve_local(&server, &p.repo, "get_file_summary") {
        Ok(e) => e,
        Err(r) => return *r,
    };
    annotated_json(&entry, tools::get_file_summary(&entry, p.path).await).await
}

async fn handle_get_stats(
    State(server): State<Server>,
    Json(p): Json<StatsParams>,
) -> axum::response::Response {
    // Forwarded `get_stats` всегда конкретизирован на один alias: соседи
    // дёргают «дай статистику по конкретному репо». Если вдруг прилетел
    // repo=None — приёмник честно отдаёт сводку (только по своим, без
    // рекурсивного fan-out — это исключает круг между нодами).
    if let Some(ref alias) = p.repo {
        let entry = server.repos.load().get(alias).cloned();
        if let Some(entry) = entry {
            if !entry.is_local {
                return ok_json(federation_error(
                    "get_stats",
                    &entry.ip,
                    format!(
                        "Конфиги разошлись: репо '{}' помечен local у вызывающей \
                         стороны, у нас он remote (ip={})",
                        alias, entry.ip
                    ),
                ));
            }
            // local — `tools::get_stats` сразу пойдёт по local-ветке.
            return ok_json(tools::stats_for_entry(&server, alias, &entry).await);
        }
        return ok_json(crate::mcp::tools::format_unavailable(
            crate::daemon_core::ipc::ToolUnavailable::UnknownRepo {
                message: crate::mcp::unknown_repo_message(alias, &server.repo_aliases()),
            },
        ));
    }
    // repo=None — fan-out, но приёмная сторона ограничивает его только локальными,
    // чтобы не создавать круг (forwarded → forwarded). Делаем это
    // «вручную» через короткий цикл по local-репо.
    let mut all = Vec::new();
    let local_repos: Vec<_> = server
        .repos
        .load()
        .iter()
        .filter(|(_, entry)| entry.is_local)
        .map(|(alias, entry)| (alias.clone(), entry.clone()))
        .collect();
    for (alias, entry) in local_repos {
        let body = tools::stats_for_entry(&server, &alias, &entry).await;
        // body — это уже JSON-string одной записи, парсим обратно в Value.
        match serde_json::from_str::<serde_json::Value>(&body) {
            Ok(v) => all.push(v),
            Err(_) => all.push(serde_json::json!({"repo": alias, "raw": body})),
        }
    }
    let resp = serde_json::json!({ "repos": all });
    ok_json(serde_json::to_string(&resp).unwrap_or_else(|_| "{}".to_string()))
}

async fn handle_search_text(
    State(server): State<Server>,
    Json(p): Json<SearchParams>,
) -> axum::response::Response {
    let entry = match resolve_local(&server, &p.repo, "search_text") {
        Ok(e) => e,
        Err(r) => return *r,
    };
    annotated_json(
        &entry,
        tools::search_text(&entry, p.query, p.limit, p.language, p.path_glob).await,
    )
    .await
}

async fn handle_grep_body(
    State(server): State<Server>,
    Json(p): Json<GrepBodyParams>,
) -> axum::response::Response {
    let entry = match resolve_local(&server, &p.repo, "grep_body") {
        Ok(e) => e,
        Err(r) => return *r,
    };
    // `query` — алиас для `regex` (см. GrepBodyParams).
    let regex = p.regex.clone().or_else(|| p.query.clone());
    annotated_json(
        &entry,
        tools::grep_body(
            &entry,
            p.pattern,
            regex,
            p.language,
            p.limit,
            p.path_glob,
            p.context_lines,
        )
        .await,
    )
    .await
}

// ── Phase 1 federation handlers ─────────────────────────────────────────────

async fn handle_stat_file(
    State(server): State<Server>,
    Json(p): Json<StatFileParams>,
) -> axum::response::Response {
    let entry = match resolve_local(&server, &p.repo, "stat_file") {
        Ok(e) => e,
        Err(r) => return *r,
    };
    ok_json(tools::stat_file(&entry, p.path).await)
}

async fn handle_list_files(
    State(server): State<Server>,
    Json(p): Json<ListFilesParams>,
) -> axum::response::Response {
    let entry = match resolve_local(&server, &p.repo, "list_files") {
        Ok(e) => e,
        Err(r) => return *r,
    };
    ok_json(tools::list_files(&entry, p.pattern, p.path_prefix, p.language, p.limit).await)
}

async fn handle_read_file(
    State(server): State<Server>,
    Json(p): Json<ReadFileParams>,
) -> axum::response::Response {
    let entry = match resolve_local(&server, &p.repo, "read_file") {
        Ok(e) => e,
        Err(r) => return *r,
    };
    annotated_json(
        &entry,
        tools::read_file(&entry, p.path, p.line_start, p.line_end).await,
    )
    .await
}

async fn handle_git_log(
    State(server): State<Server>,
    Json(p): Json<GitLogParams>,
) -> axum::response::Response {
    let entry = match resolve_local(&server, &p.repo, "git_log") {
        Ok(e) => e,
        Err(r) => return *r,
    };
    ok_json(git_tools::git_log(&entry, p).await)
}

async fn handle_git_changed_files(
    State(server): State<Server>,
    Json(p): Json<GitChangedFilesParams>,
) -> axum::response::Response {
    let entry = match resolve_local(&server, &p.repo, "git_changed_files") {
        Ok(e) => e,
        Err(r) => return *r,
    };
    ok_json(git_tools::git_changed_files(&entry, p).await)
}

async fn handle_git_changes_by_object(
    State(server): State<Server>,
    Json(p): Json<GitChangesByObjectParams>,
) -> axum::response::Response {
    let entry = match resolve_local(&server, &p.repo, "git_changes_by_object") {
        Ok(e) => e,
        Err(r) => return *r,
    };
    ok_json(git_tools::git_changes_by_object(&entry, p).await)
}

async fn handle_git_diff(
    State(server): State<Server>,
    Json(p): Json<GitDiffParams>,
) -> axum::response::Response {
    let entry = match resolve_local(&server, &p.repo, "git_diff") {
        Ok(e) => e,
        Err(r) => return *r,
    };
    ok_json(git_tools::git_diff(&entry, p).await)
}

async fn handle_git_show_file(
    State(server): State<Server>,
    Json(p): Json<GitShowFileParams>,
) -> axum::response::Response {
    let entry = match resolve_local(&server, &p.repo, "git_show_file") {
        Ok(e) => e,
        Err(r) => return *r,
    };
    ok_json(git_tools::git_show_file(&entry, p).await)
}

async fn handle_grep_text(
    State(server): State<Server>,
    Json(p): Json<GrepTextParams>,
) -> axum::response::Response {
    let entry = match resolve_local(&server, &p.repo, "grep_text") {
        Ok(e) => e,
        Err(r) => return *r,
    };
    // `query` — алиас для `regex`, `pattern` — буквальная подстрока
    // (см. GrepTextParams).
    let regex = match grep_regex_from_params(p.regex.clone(), p.query.clone(), p.pattern.clone()) {
        Some(r) => r,
        None => {
            return ok_json(
                "{\"error\": \"grep_text: укажите regex= (синтаксис crate regex) либо pattern= (буквальная подстрока).\"}"
                    .to_string(),
            )
        }
    };
    annotated_json(
        &entry,
        tools::grep_text(
            &entry,
            regex,
            p.path_glob,
            p.language,
            p.limit,
            p.context_lines,
        )
        .await,
    )
    .await
}

async fn handle_grep_code(
    State(server): State<Server>,
    Json(p): Json<GrepCodeParams>,
) -> axum::response::Response {
    let entry = match resolve_local(&server, &p.repo, "grep_code") {
        Ok(e) => e,
        Err(r) => return *r,
    };
    // `query` — алиас для `regex`, `pattern` — буквальная подстрока
    // (см. GrepCodeParams).
    let regex = match grep_regex_from_params(p.regex.clone(), p.query.clone(), p.pattern.clone()) {
        Some(r) => r,
        None => {
            return ok_json(
                "{\"error\": \"grep_code: укажите regex= (синтаксис crate regex) либо pattern= (буквальная подстрока).\"}"
                    .to_string(),
            )
        }
    };
    annotated_json(
        &entry,
        tools::grep_code(
            &entry,
            regex,
            p.path_glob,
            p.language,
            p.limit,
            p.context_lines,
            // Узел-приёмник не знает, каким ключом звал вызывающий: свою
            // подсказку про `pattern=` строит сторона-источник.
            None,
        )
        .await,
    )
    .await
}

/// Универсальный handler для extension-tools (v0.8.1).
///
/// Принимает `{tool_name, args}`. `repo` извлекается из `args` (как и в
/// штатном MCP call_tool). Находит tool в `extension_tools` снимке сервера,
/// строит `ToolContext` для local repo и вызывает `IndexTool::execute`.
///
/// Если на этой ноде такого tool нет (например, target-узел не bsl-indexer
/// сборка) — возвращаем federation_error с понятным текстом, чтобы caller
/// мог отличить «tool не найден на target» от «target недоступен».
async fn handle_extension_tool(
    State(server): State<Server>,
    Json(p): Json<ExtensionToolParams>,
) -> axum::response::Response {
    // Извлечь repo из args — стандартный контракт: у extension-tools repo
    // обязателен.
    let repo = match p.args.get("repo").and_then(|v| v.as_str()) {
        Some(r) => r.to_string(),
        None => {
            return ok_json(federation_error(
                &p.tool_name,
                &server.own_ip,
                "extension-tool вызван без обязательного 'repo' в args".to_string(),
            ));
        }
    };

    let entry = match resolve_local(&server, &repo, &p.tool_name) {
        Ok(e) => e,
        Err(r) => return *r,
    };

    // Найти tool в snapshot — extension_tools меняется на reload, но мы
    // работаем со стабильным snapshot текущего вызова.
    let snapshot = server.extension_tools.load();
    let ext = match snapshot.iter().find(|t| t.name() == p.tool_name) {
        Some(t) => t.clone(),
        None => {
            return ok_json(federation_error(
                &p.tool_name,
                &server.own_ip,
                format!(
                    "extension-tool '{}' не зарегистрирован на этой ноде \
                     (возможно, сборка собрана без bsl-extension)",
                    p.tool_name
                ),
            ));
        }
    };
    drop(snapshot);

    let storage = entry.storage_pool();
    // Тот же гейт, что и в штатном `call_tool`: пока надстройка репо
    // досчитывается, extension-tool отдаёт структурированное «слой в работе»,
    // а не неполные метаданные/термы/граф. Без него федеративный вход обходил
    // прогрессивную готовность.
    if let Some(value) = crate::mcp::tools::extras_building_response(storage).await {
        let value = tools::annotate_stale_index(value, storage, None).await;
        let body = serde_json::to_string(&value).unwrap_or_else(|e| {
            federation_error(&p.tool_name, &server.own_ip, format!("serialize: {}", e))
        });
        return ok_json(body);
    }
    let root_path: Option<&std::path::Path> = entry.root_path.as_deref();
    let language: Option<&str> = entry.language.as_deref();
    let ctx = crate::extension::ToolContext {
        repo: &repo,
        root_path,
        language,
        storage,
    };

    let value = ext.execute(p.args, ctx).await;
    // Федерация — второй вход к тем же extension-tools, и подсказка об
    // устаревшей базе нужна здесь так же, как в штатном `call_tool`: вызывающая
    // сторона отдаёт наш ответ клиенту как есть, ничего к нему не добавляя.
    // Решение принимается по базе ЭТОЙ ноды — она и собрана своим сборщиком.
    let value = crate::mcp::tools::annotate_stale_index(value, storage, None).await;
    // Сериализуем результат — он уже валидный JSON.
    let body = serde_json::to_string(&value).unwrap_or_else(|e| {
        federation_error(&p.tool_name, &server.own_ip, format!("serialize: {}", e))
    });
    ok_json(body)
}
