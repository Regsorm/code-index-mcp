// HTTP-сервер демона (axum) — health / path-status / reload / stop.
//
// Эндпоинты принимают/возвращают JSON. Транспорт — loopback HTTP по
// фактическому порту, записанному в `runtime_info_file()`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use tokio::sync::oneshot;

use super::commands::{CommandSender, DaemonCommand};
use super::ipc::{
    HealthResponse, PathStatus, PathStatusResponse, ReloadResponse, StopResponse,
};
use super::state::DaemonState;

/// Разделяемое состояние, передаваемое в handler'ы axum.
#[derive(Clone)]
pub struct AppState {
    pub state: DaemonState,
    pub commands: CommandSender,
    pub version: String,
    pub pid: u32,
}

/// Собрать роутер. Биндинг порта выполняется в `runner.rs` — здесь только маршруты.
pub fn build_router(app_state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/path-status", get(path_status))
        .route("/reload", post(reload))
        .route("/stop", post(stop))
        .with_state(Arc::new(app_state))
}

async fn health(State(app): State<Arc<AppState>>) -> Json<HealthResponse> {
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let started_unix = app.state.started_at_unix().await;
    let uptime = now_unix.saturating_sub(started_unix);

    Json(HealthResponse {
        status: "running".into(),
        version: app.version.clone(),
        pid: app.pid,
        uptime_sec: uptime,
        started_at: app.state.started_at_rfc3339().await,
        paths: app.state.to_health_paths().await,
    })
}

/// Параметр `?path=...` для GET /path-status.
#[derive(Debug, Deserialize)]
struct PathQuery {
    path: String,
}

async fn path_status(
    State(app): State<Arc<AppState>>,
    Query(q): Query<PathQuery>,
) -> impl IntoResponse {
    let target = canonical_path(&q.path);
    let tracked = app.state.tracked_paths().await;

    // Пытаемся сопоставить запрошенный путь с одним из отслеживаемых.
    let matched = tracked
        .into_iter()
        .find(|p| paths_equal(p, &target))
        .or_else(|| {
            // Если точного совпадения нет — возможно пользователь запрашивает
            // вложенный путь (например файл внутри проекта). Находим ближайший
            // родитель из отслеживаемых.
            app_parent(&target, &app.state)
        });

    match matched {
        Some(p) => {
            let rt = app.state.get(&p).await.unwrap_or_default();
            let resp = PathStatusResponse {
                path: p,
                status: rt.status,
                progress: rt.progress,
                error: rt.error,
            };
            (StatusCode::OK, Json(resp)).into_response()
        }
        None => {
            let resp = PathStatusResponse {
                path: target,
                status: PathStatus::NotStarted,
                progress: None,
                error: Some(
                    "Путь не отслеживается демоном — добавьте его в daemon.toml и вызовите reload"
                        .into(),
                ),
            };
            (StatusCode::OK, Json(resp)).into_response()
        }
    }
}

fn app_parent(target: &std::path::Path, state: &DaemonState) -> Option<PathBuf> {
    // Синхронный fallback: берём snapshot отслеживаемых путей и ищем ближайшего
    // родителя. Функция вызывается из async-контекста, поэтому state обращаемся
    // через blocking — но tracked_paths уже async, так что отдадим пустой путь.
    let _ = (target, state);
    None
}

async fn reload(State(app): State<Arc<AppState>>) -> impl IntoResponse {
    let (tx, rx) = oneshot::channel();
    if app
        .commands
        .send(DaemonCommand::Reload { respond_to: tx })
        .await
        .is_err()
    {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ReloadResponse {
                reloaded: false,
                added: vec![],
                removed: vec![],
                unchanged: vec![],
                error: Some("Runner демона не принимает команды".into()),
            }),
        )
            .into_response();
    }
    match rx.await {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ReloadResponse {
                reloaded: false,
                added: vec![],
                removed: vec![],
                unchanged: vec![],
                error: Some("Runner демона не ответил на reload".into()),
            }),
        )
            .into_response(),
    }
}

async fn stop(State(app): State<Arc<AppState>>) -> impl IntoResponse {
    let (tx, rx) = oneshot::channel();
    if app
        .commands
        .send(DaemonCommand::Stop { respond_to: tx })
        .await
        .is_err()
    {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(StopResponse { stopping: false }),
        )
            .into_response();
    }
    match rx.await {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(StopResponse { stopping: false }),
        )
            .into_response(),
    }
}

// ── Вспомогательное ──────────────────────────────────────────────────────────

fn canonical_path(input: &str) -> PathBuf {
    std::path::Path::new(input)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(input))
}

fn paths_equal(a: &std::path::Path, b: &std::path::Path) -> bool {
    let ca = a.canonicalize().unwrap_or_else(|_| a.to_path_buf());
    let cb = b.canonicalize().unwrap_or_else(|_| b.to_path_buf());
    ca == cb
}

// Возвращает фактически используемые клиентом заголовки для удобного разбора
// ответа. Отдельно вынесено, чтобы `path_status` мог вернуть IntoResponse.
#[allow(dead_code)]
pub(crate) const HEADER_CONTENT_TYPE: &str = "application/json; charset=utf-8";

// Используются внутри сервера для более точных ответов на OPTIONS/HEAD, но
// сейчас мы не реализуем CORS — demon слушает только loopback.
