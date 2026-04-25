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
    tools, CodeIndexServer, FilePathParams, FunctionNameParams, GrepBodyParams, ImportParams,
    NameParams, RepoEntry, SearchParams, StatsParams,
};

use super::dispatcher::federation_error;

type Server = Arc<CodeIndexServer>;

/// Собрать роутер `/federate/<tool>`. Вызывается в `serve_http` при наличии
/// `serve.toml` и ставится `merge` рядом с `/mcp`.
pub fn federate_router(server: CodeIndexServer) -> Router {
    Router::new()
        .route("/federate/search_function", post(handle_search_function))
        .route("/federate/search_class", post(handle_search_class))
        .route("/federate/get_function", post(handle_get_function))
        .route("/federate/get_class", post(handle_get_class))
        .route("/federate/get_callers", post(handle_get_callers))
        .route("/federate/get_callees", post(handle_get_callees))
        .route("/federate/find_symbol", post(handle_find_symbol))
        .route("/federate/get_imports", post(handle_get_imports))
        .route("/federate/get_file_summary", post(handle_get_file_summary))
        .route("/federate/get_stats", post(handle_get_stats))
        .route("/federate/search_text", post(handle_search_text))
        .route("/federate/grep_body", post(handle_grep_body))
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
fn resolve_local<'a>(
    server: &'a CodeIndexServer,
    repo: &str,
    tool: &str,
) -> Result<&'a RepoEntry, axum::response::Response> {
    let entry = match server.resolve_repo(repo) {
        Ok(e) => e,
        Err(j) => return Err(ok_json(j)),
    };
    if !entry.is_local {
        return Err(ok_json(federation_error(
            tool,
            &entry.ip,
            format!(
                "Конфиги разошлись: репо '{}' помечен local на удалённой стороне, \
                 но у нас он указывает на ip={}",
                repo, entry.ip
            ),
        )));
    }
    Ok(entry)
}

// ── Handlers ────────────────────────────────────────────────────────────────

async fn handle_search_function(
    State(server): State<Server>,
    Json(p): Json<SearchParams>,
) -> axum::response::Response {
    let entry = match resolve_local(&server, &p.repo, "search_function") {
        Ok(e) => e,
        Err(r) => return r,
    };
    ok_json(tools::search_function(entry, p.query, p.limit, p.language).await)
}

async fn handle_search_class(
    State(server): State<Server>,
    Json(p): Json<SearchParams>,
) -> axum::response::Response {
    let entry = match resolve_local(&server, &p.repo, "search_class") {
        Ok(e) => e,
        Err(r) => return r,
    };
    ok_json(tools::search_class(entry, p.query, p.limit, p.language).await)
}

async fn handle_get_function(
    State(server): State<Server>,
    Json(p): Json<NameParams>,
) -> axum::response::Response {
    let entry = match resolve_local(&server, &p.repo, "get_function") {
        Ok(e) => e,
        Err(r) => return r,
    };
    ok_json(tools::get_function(entry, p.name).await)
}

async fn handle_get_class(
    State(server): State<Server>,
    Json(p): Json<NameParams>,
) -> axum::response::Response {
    let entry = match resolve_local(&server, &p.repo, "get_class") {
        Ok(e) => e,
        Err(r) => return r,
    };
    ok_json(tools::get_class(entry, p.name).await)
}

async fn handle_get_callers(
    State(server): State<Server>,
    Json(p): Json<FunctionNameParams>,
) -> axum::response::Response {
    let entry = match resolve_local(&server, &p.repo, "get_callers") {
        Ok(e) => e,
        Err(r) => return r,
    };
    ok_json(tools::get_callers(entry, p.function_name, p.language).await)
}

async fn handle_get_callees(
    State(server): State<Server>,
    Json(p): Json<FunctionNameParams>,
) -> axum::response::Response {
    let entry = match resolve_local(&server, &p.repo, "get_callees") {
        Ok(e) => e,
        Err(r) => return r,
    };
    ok_json(tools::get_callees(entry, p.function_name, p.language).await)
}

async fn handle_find_symbol(
    State(server): State<Server>,
    Json(p): Json<NameParams>,
) -> axum::response::Response {
    let entry = match resolve_local(&server, &p.repo, "find_symbol") {
        Ok(e) => e,
        Err(r) => return r,
    };
    ok_json(tools::find_symbol(entry, p.name, p.language).await)
}

async fn handle_get_imports(
    State(server): State<Server>,
    Json(p): Json<ImportParams>,
) -> axum::response::Response {
    let entry = match resolve_local(&server, &p.repo, "get_imports") {
        Ok(e) => e,
        Err(r) => return r,
    };
    ok_json(tools::get_imports(entry, p.file_id, p.module, p.language).await)
}

async fn handle_get_file_summary(
    State(server): State<Server>,
    Json(p): Json<FilePathParams>,
) -> axum::response::Response {
    let entry = match resolve_local(&server, &p.repo, "get_file_summary") {
        Ok(e) => e,
        Err(r) => return r,
    };
    ok_json(tools::get_file_summary(entry, p.path).await)
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
        if let Some(entry) = server.repos.get(alias) {
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
            return ok_json(tools::get_stats(&server, Some(alias.clone())).await);
        }
        return ok_json(crate::mcp::tools::format_unavailable(
            crate::daemon_core::ipc::ToolUnavailable::NotStarted {
                message: format!(
                    "Неизвестный repo '{}'. Доступные на этой ноде: {:?}.",
                    alias,
                    server.repo_aliases()
                ),
            },
        ));
    }
    // repo=None — fan-out, но приёмная сторона ограничивает его только локальными,
    // чтобы не создавать круг (forwarded → forwarded). Делаем это
    // «вручную» через короткий цикл по local-репо.
    let mut all = Vec::new();
    for (alias, entry) in server.repos.iter() {
        if !entry.is_local {
            continue;
        }
        let body = tools::get_stats(&server, Some(alias.clone())).await;
        // body — это уже JSON-string одной записи, парсим обратно в Value.
        match serde_json::from_str::<serde_json::Value>(&body) {
            Ok(v) => all.push(v),
            Err(_) => all.push(serde_json::json!({"repo": alias, "raw": body})),
        }
    }
    let resp = serde_json::json!({ "repos": all });
    ok_json(serde_json::to_string_pretty(&resp).unwrap_or_else(|_| "{}".to_string()))
}

async fn handle_search_text(
    State(server): State<Server>,
    Json(p): Json<SearchParams>,
) -> axum::response::Response {
    let entry = match resolve_local(&server, &p.repo, "search_text") {
        Ok(e) => e,
        Err(r) => return r,
    };
    ok_json(tools::search_text(entry, p.query, p.limit, p.language).await)
}

async fn handle_grep_body(
    State(server): State<Server>,
    Json(p): Json<GrepBodyParams>,
) -> axum::response::Response {
    let entry = match resolve_local(&server, &p.repo, "grep_body") {
        Ok(e) => e,
        Err(r) => return r,
    };
    ok_json(tools::grep_body(entry, p.pattern, p.regex, p.language, p.limit).await)
}
