// Основная точка входа демона. Связывает вместе:
//   * захват глобального PID-lock
//   * загрузку daemon.toml
//   * DaemonState
//   * HTTP-сервер (axum)
//   * worker'ы по одному на папку
//   * обработку команд reload/stop и Ctrl-C

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;

use std::sync::Arc;

use anyhow::Result;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, Semaphore};

use super::commands::{self, DaemonCommand};
use super::config::{self, PathEntry};
use super::ipc::{ReloadResponse, RuntimeInfo, StopResponse};
use super::lock;
use super::paths;
use super::server::{build_router, AppState};
use super::state::DaemonState;
use super::worker;

/// Запустить демона в foreground-режиме. Возврат происходит только после
/// полной остановки (сигнал stop или Ctrl-C).
pub async fn run() -> Result<()> {
    let _pid_lock = lock::acquire()?;
    let started_at = std::time::Instant::now();
    let version = env!("CARGO_PKG_VERSION").to_string();
    let pid = std::process::id();

    let cfg = config::load_or_default()?;
    eprintln!(
        "[daemon] Конфиг: {} (папок: {})",
        paths::config_path()?.display(),
        cfg.paths.len()
    );

    let daemon_state = DaemonState::new();
    let (cmd_tx, mut cmd_rx) = commands::channel();

    // HTTP-сервер слушает на loopback. Порт 0 → ОС выбирает свободный.
    let host: std::net::IpAddr = cfg
        .daemon
        .http_host
        .parse()
        .unwrap_or_else(|_| "127.0.0.1".parse().unwrap());
    let listener = TcpListener::bind(SocketAddr::new(host, cfg.daemon.http_port)).await?;
    let actual_addr = listener.local_addr()?;

    write_runtime_info(&actual_addr, pid, &version)?;
    eprintln!("[daemon] HTTP health-IPC: http://{}", actual_addr);

    let app_state = AppState {
        state: daemon_state.clone(),
        commands: cmd_tx.clone(),
        version: version.clone(),
        pid,
    };
    let router = build_router(app_state);

    let server_handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, router).await {
            eprintln!("[daemon] HTTP-сервер упал: {}", e);
        }
    });

    // Глобальный shutdown-канал для workers.
    let (shutdown_tx, _) = broadcast::channel::<()>(16);

    // Семафор, ограничивающий число одновременных initial-reindex'ов.
    // `0` = без ограничений (старое поведение параллельного старта всех).
    let initial_limiter = if cfg.daemon.max_concurrent_initial == 0 {
        None
    } else {
        Some(Arc::new(Semaphore::new(cfg.daemon.max_concurrent_initial)))
    };

    // Зарегистрировать пути в state и запустить worker'ы.
    let wanted_canon: Vec<PathBuf> = cfg
        .paths
        .iter()
        .map(|p| p.path.canonicalize().unwrap_or_else(|_| p.path.clone()))
        .collect();
    daemon_state.apply_config(&wanted_canon).await;

    let mut workers: HashMap<PathBuf, tokio::task::JoinHandle<()>> = HashMap::new();
    for entry in cfg.paths.into_iter() {
        let canonical = entry
            .path
            .canonicalize()
            .unwrap_or_else(|_| entry.path.clone());
        let handle = spawn_worker(
            entry,
            daemon_state.clone(),
            shutdown_tx.subscribe(),
            initial_limiter.clone(),
        );
        workers.insert(canonical, handle);
    }

    // Основной цикл: команды + Ctrl-C
    loop {
        tokio::select! {
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    DaemonCommand::Reload { respond_to } => {
                        let resp = handle_reload(&daemon_state, &mut workers, &shutdown_tx).await;
                        let _ = respond_to.send(resp);
                    }
                    DaemonCommand::Stop { respond_to } => {
                        let _ = respond_to.send(StopResponse { stopping: true });
                        break;
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                eprintln!("[daemon] Ctrl-C — завершение");
                break;
            }
        }
    }

    eprintln!("[daemon] остановка worker'ов...");
    let _ = shutdown_tx.send(());
    for (path, handle) in workers {
        if let Err(e) = handle.await {
            eprintln!(
                "[daemon] worker {} не завершился корректно: {}",
                path.display(),
                e
            );
        }
    }
    server_handle.abort();

    remove_runtime_info();
    eprintln!(
        "[daemon] завершено, uptime {}с",
        started_at.elapsed().as_secs()
    );
    Ok(())
}

fn spawn_worker(
    entry: PathEntry,
    state: DaemonState,
    shutdown_rx: broadcast::Receiver<()>,
    initial_limiter: Option<Arc<Semaphore>>,
) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        worker::run_worker(entry, state, shutdown_rx, initial_limiter);
    })
}

/// Обработка `POST /reload` в runner'е. Добавляем новые папки и запускаем для них
/// worker'ы. Удаление папок в MVP требует рестарта демона — это зафиксировано в
/// брифе и в поле `error` ответа.
async fn handle_reload(
    state: &DaemonState,
    workers: &mut HashMap<PathBuf, tokio::task::JoinHandle<()>>,
    shutdown_tx: &broadcast::Sender<()>,
) -> ReloadResponse {
    let cfg = match config::load_or_default() {
        Ok(c) => c,
        Err(e) => {
            return ReloadResponse {
                reloaded: false,
                added: vec![],
                removed: vec![],
                unchanged: vec![],
                error: Some(format!("Не удалось перечитать конфиг: {}", e)),
            };
        }
    };

    let wanted_canon: Vec<PathBuf> = cfg
        .paths
        .iter()
        .map(|p| p.path.canonicalize().unwrap_or_else(|_| p.path.clone()))
        .collect();
    let (added, removed, unchanged) = state.apply_config(&wanted_canon).await;

    // Запускаем worker'ы для добавленных. Семафор берём из текущего конфига —
    // предположение: limiter не меняется в рантайме, только при рестарте демона.
    let limiter = if cfg.daemon.max_concurrent_initial == 0 {
        None
    } else {
        Some(Arc::new(Semaphore::new(cfg.daemon.max_concurrent_initial)))
    };
    for entry in cfg.paths.into_iter() {
        let canonical = entry
            .path
            .canonicalize()
            .unwrap_or_else(|_| entry.path.clone());
        if added.contains(&canonical) {
            let handle = spawn_worker(
                entry,
                state.clone(),
                shutdown_tx.subscribe(),
                limiter.clone(),
            );
            workers.insert(canonical, handle);
        }
    }

    let note = if removed.is_empty() {
        None
    } else {
        Some(
            "Удаление папок применится после рестарта демона (MVP-ограничение)".into(),
        )
    };

    ReloadResponse {
        reloaded: true,
        added,
        removed,
        unchanged,
        error: note,
    }
}

fn write_runtime_info(addr: &SocketAddr, pid: u32, version: &str) -> Result<()> {
    paths::ensure_state_dir()?;
    let info = RuntimeInfo {
        pid,
        version: version.to_string(),
        http_host: addr.ip().to_string(),
        http_port: addr.port(),
        started_at: chrono::Utc::now().to_rfc3339(),
    };
    let text = serde_json::to_string_pretty(&info)?;
    std::fs::write(paths::runtime_info_file()?, text)?;
    Ok(())
}

fn remove_runtime_info() {
    if let Ok(path) = paths::runtime_info_file() {
        let _ = std::fs::remove_file(path);
    }
}

/// Попытаться прочитать runtime-info файл. Возвращает None если демон не запущен
/// либо `CODE_INDEX_HOME` не задана (значит и запускать негде).
pub fn read_runtime_info() -> Option<RuntimeInfo> {
    let path = paths::runtime_info_file().ok()?;
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<RuntimeInfo>(&text).ok()
}
