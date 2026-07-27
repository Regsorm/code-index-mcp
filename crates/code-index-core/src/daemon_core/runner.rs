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

use super::cache_client::CacheClient;
use super::commands::{self, DaemonCommand};
use super::config::{self, IndexerSection, PathEntry};
use super::ipc::{ReloadResponse, RuntimeInfo, StopResponse};
use super::language_detect;
use super::lock;
use super::paths;
use super::server::{build_router, AppState};
use super::state::DaemonState;
use super::worker;
use crate::extension::ProcessorRegistry;

/// Запустить демона в foreground-режиме. Возврат происходит только после
/// полной остановки (сигнал stop или Ctrl-C).
///
/// `processor_registry` — реестр `LanguageProcessor`-ов от bin'а
/// (`code-index` или `bsl-indexer`). Пробрасывается до worker'ов, чтобы
/// они могли применить `schema_extensions()` и `index_extras()` для
/// своих репо. `None` — universal-only сборка без BSL.
pub async fn run(processor_registry: Option<Arc<ProcessorRegistry>>) -> Result<()> {
    let _pid_lock = lock::acquire()?;
    let started_at = std::time::Instant::now();
    let version = env!("CARGO_PKG_VERSION").to_string();
    let pid = std::process::id();

    let mut cfg = config::load_or_default()?;
    let cfg_path = paths::config_path()?;
    eprintln!(
        "[daemon] Конфиг: {} (папок: {})",
        cfg_path.display(),
        cfg.paths.len()
    );

    // Миграция: заполнить language у тех [[paths]], где он не задан
    // (старые конфиги до этой версии не имели поля language).
    // Auto-detect → дозапись обратно в TOML через toml_edit (сохраняет
    // комментарии и форматирование). После миграции локальная копия
    // cfg обновляется in-memory, чтобы дальнейший код видел корректные
    // языки без повторного чтения с диска.
    migrate_languages(&cfg_path, &mut cfg)?;

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
    // Копии PathEntry по каноническому пути — чтобы сторож ниже мог перезапустить
    // упавший/аварийно завершившийся worker с той же конфигурацией.
    let mut worker_entries: HashMap<PathBuf, PathEntry> = HashMap::new();
    // Backoff перезапусков: (число попыток, время последней) на путь. Защита от
    // шторма перезапусков при устойчиво падающем worker'е.
    let mut respawn_tracker: HashMap<PathBuf, (u32, std::time::Instant)> = HashMap::new();
    let indexer_section = cfg.indexer.clone();

    // Event-based cache invalidation (этап 3, v0.9.1+): создаём один общий
    // CacheClient на все workers, передаём как `Option<Arc<_>>`. Пустой
    // `cache_targets` → None (внутренний путь invalidate отключён).
    let cache_target_urls: Vec<String> =
        cfg.cache_targets.iter().map(|t| t.url.clone()).collect();
    let cache_client = if cache_target_urls.is_empty() {
        None
    } else {
        let cc = Arc::new(CacheClient::new(cache_target_urls));
        eprintln!(
            "[daemon] cache invalidation: {} target(s) настроено",
            cc.target_count()
        );
        Some(cc)
    };

    for entry in cfg.paths.into_iter() {
        let canonical = entry
            .path
            .canonicalize()
            .unwrap_or_else(|_| entry.path.clone());
        worker_entries.insert(canonical.clone(), entry.clone());
        let handle = spawn_worker(
            entry,
            daemon_state.clone(),
            shutdown_tx.subscribe(),
            initial_limiter.clone(),
            indexer_section.clone(),
            processor_registry.clone(),
            cache_client.clone(),
        );
        workers.insert(canonical, handle);
    }

    // Сторож worker'ов тикает раз в 5 секунд — ищет аварийно завершившиеся потоки
    // и перезапускает их (см. supervise_workers).
    let mut watchdog = tokio::time::interval(std::time::Duration::from_secs(5));

    // Основной цикл: команды + Ctrl-C + сторож
    loop {
        tokio::select! {
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    DaemonCommand::Reload { respond_to } => {
                        let resp = handle_reload(
                            &daemon_state,
                            &mut workers,
                            &mut worker_entries,
                            &shutdown_tx,
                            processor_registry.clone(),
                        ).await;
                        let _ = respond_to.send(resp);
                    }
                    DaemonCommand::Stop { respond_to } => {
                        let _ = respond_to.send(StopResponse { stopping: true });
                        break;
                    }
                }
            }
            _ = watchdog.tick() => {
                supervise_workers(
                    &daemon_state,
                    &mut workers,
                    &worker_entries,
                    &mut respawn_tracker,
                    &shutdown_tx,
                    &initial_limiter,
                    &indexer_section,
                    &processor_registry,
                    &cache_client,
                ).await;
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
    indexer_section: IndexerSection,
    processor_registry: Option<Arc<ProcessorRegistry>>,
    cache_client: Option<Arc<CacheClient>>,
) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        worker::run_worker(
            entry,
            state,
            shutdown_rx,
            initial_limiter,
            indexer_section,
            processor_registry,
            cache_client,
        );
    })
}

/// Решение backoff для перезапуска пути. Обновляет счётчик в `tracker` и
/// возвращает `true`, если перезапуск ещё разрешён (число попыток в пределах
/// окна не превысило `max`). Счётчик сбрасывается, если с последней попытки
/// прошло больше `window`. Вынесено чистой функцией ради модульного теста.
fn allow_respawn(
    tracker: &mut HashMap<PathBuf, (u32, std::time::Instant)>,
    path: &PathBuf,
    now: std::time::Instant,
    window: std::time::Duration,
    max: u32,
) -> bool {
    let slot = tracker.entry(path.clone()).or_insert((0, now));
    if now.duration_since(slot.1) > window {
        *slot = (0, now);
    }
    slot.0 += 1;
    slot.1 = now;
    slot.0 <= max
}

/// Сторож worker'ов. Перезапускает потоки, завершившиеся НЕ по общему shutdown
/// (паника внутри батча или ранний выход из-за ошибки транзакции). В штатном
/// режиме worker живёт до shutdown, поэтому `is_finished()` во время работы
/// демона означает аварию: раньше статус папки при этом навсегда застревал на
/// `ReindexingBatch` («indexing»), а самовосстановления не было.
///
/// Перезапуск ограничен backoff'ом (`allow_respawn`): устойчиво падающий путь
/// после лимита попыток оставляется в `Error`, чтобы не крутить бесконечный
/// цикл перезапусков.
#[allow(clippy::too_many_arguments)]
async fn supervise_workers(
    state: &DaemonState,
    workers: &mut HashMap<PathBuf, tokio::task::JoinHandle<()>>,
    worker_entries: &HashMap<PathBuf, PathEntry>,
    respawn_tracker: &mut HashMap<PathBuf, (u32, std::time::Instant)>,
    shutdown_tx: &broadcast::Sender<()>,
    initial_limiter: &Option<Arc<Semaphore>>,
    indexer_section: &IndexerSection,
    processor_registry: &Option<Arc<ProcessorRegistry>>,
    cache_client: &Option<Arc<CacheClient>>,
) {
    const RESPAWN_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);
    const MAX_RESPAWNS: u32 = 5;

    // Сначала собрать пути завершившихся потоков, потом мутировать `workers`.
    let finished: Vec<PathBuf> = workers
        .iter()
        .filter(|(_, h)| h.is_finished())
        .map(|(p, _)| p.clone())
        .collect();

    for path in finished {
        if let Some(handle) = workers.remove(&path) {
            match handle.await {
                Ok(()) => eprintln!(
                    "[daemon] worker {} завершился сам (не shutdown) — перезапуск",
                    path.display()
                ),
                Err(e) => {
                    eprintln!("[daemon] worker {} упал ({}) — перезапуск", path.display(), e)
                }
            }
        }

        let entry = match worker_entries.get(&path) {
            Some(e) => e.clone(),
            None => {
                eprintln!(
                    "[daemon] worker {} завершился, но PathEntry не найден — перезапуск невозможен",
                    path.display()
                );
                state
                    .set_error(
                        &path,
                        "worker завершился, перезапуск невозможен (нет конфигурации пути)",
                    )
                    .await;
                continue;
            }
        };

        if !allow_respawn(
            respawn_tracker,
            &path,
            std::time::Instant::now(),
            RESPAWN_WINDOW,
            MAX_RESPAWNS,
        ) {
            eprintln!(
                "[daemon] worker {} аварийно завершался > {} раз за {}с — прекращаю перезапуски, статус Error",
                path.display(),
                MAX_RESPAWNS,
                RESPAWN_WINDOW.as_secs()
            );
            state
                .set_error(
                    &path,
                    "worker многократно аварийно завершается — см. журнал демона",
                )
                .await;
            continue;
        }

        // Честный промежуточный статус до готовности свежего worker'а.
        state
            .set_error(&path, "worker перезапускается после аварийного завершения")
            .await;

        let handle = spawn_worker(
            entry,
            state.clone(),
            shutdown_tx.subscribe(),
            initial_limiter.clone(),
            indexer_section.clone(),
            processor_registry.clone(),
            cache_client.clone(),
        );
        workers.insert(path, handle);
    }
}

/// Обработка `POST /reload` в runner'е. Добавляем новые папки и запускаем для них
/// worker'ы. Удаление папок в MVP требует рестарта демона — это зафиксировано в
/// брифе и в поле `error` ответа.
async fn handle_reload(
    state: &DaemonState,
    workers: &mut HashMap<PathBuf, tokio::task::JoinHandle<()>>,
    worker_entries: &mut HashMap<PathBuf, PathEntry>,
    shutdown_tx: &broadcast::Sender<()>,
    processor_registry: Option<Arc<ProcessorRegistry>>,
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
    let indexer_section = cfg.indexer.clone();

    // На reload пересоздаём CacheClient — оператор мог добавить/удалить
    // cache_targets в daemon.toml. Существующие workers продолжают
    // использовать свой (захваченный при старте) client, новые получают
    // обновлённый. После полного рестарта demon все workers будут на
    // одной актуальной версии.
    let cache_target_urls: Vec<String> =
        cfg.cache_targets.iter().map(|t| t.url.clone()).collect();
    let reload_cache_client = if cache_target_urls.is_empty() {
        None
    } else {
        Some(Arc::new(CacheClient::new(cache_target_urls)))
    };

    for entry in cfg.paths.into_iter() {
        let canonical = entry
            .path
            .canonicalize()
            .unwrap_or_else(|_| entry.path.clone());
        if added.contains(&canonical) {
            worker_entries.insert(canonical.clone(), entry.clone());
            let handle = spawn_worker(
                entry,
                state.clone(),
                shutdown_tx.subscribe(),
                limiter.clone(),
                indexer_section.clone(),
                processor_registry.clone(),
                reload_cache_client.clone(),
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

/// Миграция конфига: для каждой `[[paths]]` без `language` определить
/// язык эвристикой `language_detect::detect_language` и дописать
/// результат обратно в `daemon.toml` через `toml_edit`. После записи
/// `cfg.paths[i].language` обновляется в памяти.
///
/// Вызывается один раз при старте демона. Если файл `daemon.toml` не
/// существует (демон стартовал без конфига), функция ничего не делает —
/// записи без `language` всё равно нечем сохранить.
///
/// Не возвращает ошибку при сбое отдельной записи — пишет warning и
/// идёт дальше; одна нераспознанная папка не должна валить весь демон.
fn migrate_languages(cfg_path: &std::path::Path, cfg: &mut config::DaemonFileConfig) -> Result<()> {
    if !cfg_path.exists() {
        // Конфига нет — нет и записей для миграции. Норма для первого запуска.
        return Ok(());
    }
    for entry in cfg.paths.iter_mut() {
        if entry.language.is_some() {
            continue;
        }
        // Корень репо для эвристик. Канонизация может упасть на не существующем
        // пути — тогда работаем с тем что в конфиге; detect_language вернёт None
        // и мы пропустим запись.
        let root = entry
            .path
            .canonicalize()
            .unwrap_or_else(|_| entry.path.clone());

        match language_detect::detect_language(&root) {
            Some(lang) => match language_detect::write_language_back(cfg_path, &entry.path, lang) {
                Ok(true) => {
                    tracing::info!(
                        "[migrate_languages] {} → language=\"{}\" (записано в {})",
                        entry.path.display(),
                        lang,
                        cfg_path.display()
                    );
                    entry.language = Some(lang.to_string());
                }
                Ok(false) => {
                    tracing::warn!(
                        "[migrate_languages] язык определён ({}) для {}, \
                         но запись в TOML не нашла такой [[paths]] — \
                         возможно canonical-путь и путь в конфиге расходятся",
                        lang,
                        entry.path.display()
                    );
                    // В памяти всё равно проставляем — на случай, если
                    // active set строится отсюда.
                    entry.language = Some(lang.to_string());
                }
                Err(e) => {
                    tracing::warn!(
                        "[migrate_languages] не удалось записать language для {}: {}",
                        entry.path.display(),
                        e
                    );
                    entry.language = Some(lang.to_string());
                }
            },
            None => {
                tracing::warn!(
                    "[migrate_languages] не удалось определить язык для {}: \
                     добавьте `language = \"...\"` вручную в {}",
                    entry.path.display(),
                    cfg_path.display()
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod migrate_tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn allow_respawn_разрешает_до_лимита_и_сбрасывается_после_окна() {
        let mut tracker = HashMap::new();
        let path = PathBuf::from("/tmp/repo");
        let window = std::time::Duration::from_secs(60);
        let t0 = std::time::Instant::now();
        // Первые 5 попыток в пределах одного окна разрешены.
        for i in 1..=5 {
            assert!(
                allow_respawn(&mut tracker, &path, t0, window, 5),
                "попытка {i} должна быть разрешена"
            );
        }
        // 6-я попытка в том же окне — запрещена (сработал backoff).
        assert!(!allow_respawn(&mut tracker, &path, t0, window, 5));
        // По прошествии окна счётчик сбрасывается — снова разрешено.
        let later = t0 + std::time::Duration::from_secs(61);
        assert!(allow_respawn(&mut tracker, &path, later, window, 5));
    }

    fn make_repo_with_marker(tmp: &TempDir, name: &str, marker: &str) -> std::path::PathBuf {
        let dir = tmp.path().join(name);
        std::fs::create_dir(&dir).unwrap();
        std::fs::File::create(dir.join(marker)).unwrap();
        dir
    }

    /// Пишет path в TOML как literal-string (одинарные кавычки) — там
    /// не действуют escape-sequences, что важно для Windows-путей с
    /// backslashes (`\\?\C:\...`).
    fn toml_literal_path(p: &std::path::Path) -> String {
        format!("'{}'", p.display())
    }

    #[test]
    fn fills_language_for_entries_without_it() {
        let tmp = TempDir::new().unwrap();

        // Два «репо» — Rust по Cargo.toml, BSL по Configuration.xml.
        let rust_repo = make_repo_with_marker(&tmp, "rust_repo", "Cargo.toml");
        let bsl_repo = make_repo_with_marker(&tmp, "bsl_repo", "Configuration.xml");

        // daemon.toml без language. Используем canonical-путь, т.к.
        // migrate_languages canonicalize'ит entry.path и сравнивает.
        // В TOML literal-strings (одинарные кавычки) backslashes не
        // экранируются — Windows-пути сохраняются как есть.
        let toml_path = tmp.path().join("daemon.toml");
        let rust_canon = rust_repo.canonicalize().unwrap();
        let bsl_canon = bsl_repo.canonicalize().unwrap();
        let original = format!(
            "# user comment\n\
             [[paths]]\n\
             path = {}\n\
             \n\
             [[paths]]\n\
             path = {}\n",
            toml_literal_path(&rust_canon),
            toml_literal_path(&bsl_canon),
        );
        std::fs::File::create(&toml_path)
            .unwrap()
            .write_all(original.as_bytes())
            .unwrap();

        let mut cfg = config::parse_str(&original).unwrap();
        migrate_languages(&toml_path, &mut cfg).unwrap();

        // В памяти заполнено.
        assert_eq!(cfg.paths[0].language.as_deref(), Some("rust"));
        assert_eq!(cfg.paths[1].language.as_deref(), Some("bsl"));

        // На диске тоже — комментарий сохранён.
        let new_text = std::fs::read_to_string(&toml_path).unwrap();
        assert!(new_text.contains("# user comment"));
        assert!(new_text.contains(r#"language = "rust""#));
        assert!(new_text.contains(r#"language = "bsl""#));
    }

    #[test]
    fn keeps_explicit_language_unchanged() {
        let tmp = TempDir::new().unwrap();
        let repo = make_repo_with_marker(&tmp, "repo", "Cargo.toml");

        let toml_path = tmp.path().join("daemon.toml");
        let canon = repo.canonicalize().unwrap();
        let original = format!(
            "[[paths]]\n\
             path = {}\n\
             language = \"python\"\n",
            toml_literal_path(&canon),
        );
        std::fs::File::create(&toml_path)
            .unwrap()
            .write_all(original.as_bytes())
            .unwrap();

        let mut cfg = config::parse_str(&original).unwrap();
        migrate_languages(&toml_path, &mut cfg).unwrap();

        // Явное `python` не должно быть перезаписано на rust, даже
        // несмотря на Cargo.toml. Оператор знает лучше.
        assert_eq!(cfg.paths[0].language.as_deref(), Some("python"));
    }

    #[test]
    fn warns_but_doesnt_fail_when_language_undetectable() {
        let tmp = TempDir::new().unwrap();
        // Пустая директория — детектор не сможет определить язык.
        let empty_repo = tmp.path().join("empty_repo");
        std::fs::create_dir(&empty_repo).unwrap();

        let toml_path = tmp.path().join("daemon.toml");
        let canon = empty_repo.canonicalize().unwrap();
        let original = format!(
            "[[paths]]\n\
             path = {}\n",
            toml_literal_path(&canon),
        );
        std::fs::File::create(&toml_path)
            .unwrap()
            .write_all(original.as_bytes())
            .unwrap();

        let mut cfg = config::parse_str(&original).unwrap();
        // Не должно паниковать или возвращать Err.
        migrate_languages(&toml_path, &mut cfg).unwrap();

        // language остался None — ждём от оператора ручного указания.
        assert_eq!(cfg.paths[0].language, None);
    }
}
