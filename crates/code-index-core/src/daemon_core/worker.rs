// Worker одной отслеживаемой папки. Делает initial reindex + держит watcher-цикл.
//
// Работа полностью блокирующая (tree-sitter, rayon, notify), поэтому worker
// запускается из runner'а через `tokio::task::spawn_blocking`. Взаимодействие с
// tokio-миром только через `DaemonState` (асинхронный RwLock) и через
// `shutdown_rx` (broadcast).

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Semaphore;

use crate::extension::ProcessorRegistry;
use crate::indexer::config::IndexConfig;
use crate::indexer::file_types::{categorize_file, FileCategory};
use crate::indexer::hasher;
use crate::indexer::Indexer;
use crate::parser::text::TextParser;
use crate::parser::LanguageParser;
use crate::parser::ParserRegistry;
use crate::storage::memory::StorageConfig;
use crate::storage::Storage;
use crate::watcher::{create_watcher, poll_batch, FileEvent, WatcherConfig};

use super::config::{IndexerSection, PathEntry};
use super::ipc::{PathStatus, Progress};
use super::state::DaemonState;

/// Выполнить initial reindex и запустить watcher-цикл для одной папки.
///
/// Функция блокирующая. Runner вызывает её через `spawn_blocking`. По завершении
/// (включая ошибку) статус папки уже записан в `DaemonState`.
///
/// `processor_registry` — список зарегистрированных `LanguageProcessor`-ов.
/// `None` означает «universal-only сборка» (`code-index.exe` без BSL); в этом
/// случае пропускаем `apply_schema_extensions` / `index_extras`. В сборке
/// `bsl-indexer.exe` сюда приходит registry с `BslLanguageProcessor`,
/// благодаря чему создаются специфичные таблицы (metadata_objects/...).
pub fn run_worker(
    entry: PathEntry,
    state: DaemonState,
    mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
    initial_limiter: Option<Arc<Semaphore>>,
    indexer_section: IndexerSection,
    processor_registry: Option<Arc<ProcessorRegistry>>,
) {
    let path = match entry.path.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            tokio_block_on(async {
                state
                    .set_error(&entry.path, format!("Не удалось разрешить путь: {}", e))
                    .await;
            });
            return;
        }
    };

    // 1. Открыть/создать .code-index/index.db
    let db_dir = path.join(".code-index");
    if let Err(e) = std::fs::create_dir_all(&db_dir) {
        tokio_block_on(async {
            state
                .set_error(&path, format!("Создание .code-index/: {}", e))
                .await;
        });
        return;
    }
    let db_path = db_dir.join("index.db");

    // 2. Загрузить конфигурацию проекта (для exclude_dirs, debounce и т.п.)
    let mut index_config = match IndexConfig::load(&path) {
        Ok(c) => c,
        Err(e) => {
            tokio_block_on(async {
                state
                    .set_error(&path, format!("Загрузка IndexConfig: {}", e))
                    .await;
            });
            return;
        }
    };
    // Phase 2 (v0.8.0): эффективный лимит для file_contents.
    // Приоритет: per-path (`[[paths]].max_code_file_size_bytes`) →
    // глобальный `[indexer].max_code_file_size_bytes` → hardcoded 5 МБ.
    // Перетираем дефолт IndexConfig — переоформленные правила сильнее JSON-конфига проекта.
    index_config.max_code_file_size_bytes = entry.effective_max_code_file_size(&indexer_section);
    let storage_config = StorageConfig {
        mode: index_config.storage_mode.clone(),
        memory_max_percent: index_config.memory_max_percent,
    };

    // 3. Взять permit из семафора. Permit держится на всё время initial reindex,
    // включая открытие in-memory Storage — чтобы в памяти одновременно жил
    // максимум ОДИН in-memory storage (ограничено max_concurrent_initial).
    let _permit = initial_limiter.as_ref().map(|sem| {
        eprintln!("[worker:{}] ждём слота initial reindex (доступно {})", path.display(), sem.available_permits());
        let sem = sem.clone();
        tokio_block_on_value(async move { sem.acquire_owned().await.expect("semaphore closed") })
    });

    // 4. Выставить статус InitialIndexing ПОСЛЕ получения permit — иначе
    // папки-кандидаты показываются как активно индексируются, хотя на самом
    // деле ждут своей очереди.
    tokio_block_on(async {
        state.set_status(&path, PathStatus::InitialIndexing).await;
        state.set_progress(&path, Progress::new(0, 0)).await;
    });

    // 5. Открыть Storage.
    //    * Если БД уже существует — сразу disk-режим. fast-path почти ничего
    //      не пишет, нет лишнего backup memory→disk (WAL не раздувается).
    //    * Если БД новая (первый запуск на этой папке) — in-memory для
    //      скорости, потом flush на диск и reopen в disk для watcher'а.
    let db_existed_before = db_path.exists()
        && std::fs::metadata(&db_path).map(|m| m.len() > 0).unwrap_or(false);

    let mut storage = if db_existed_before {
        eprintln!("[worker:{}] БД уже существует — открываем сразу в disk", path.display());
        match Storage::open_file(&db_path) {
            Ok(s) => s,
            Err(e) => {
                tokio_block_on(async {
                    state.set_error(&path, format!("Storage::open_file: {}", e)).await;
                });
                return;
            }
        }
    } else {
        eprintln!("[worker:{}] новая БД — открываем в {}", path.display(), storage_config.mode);
        match Storage::open_auto(&db_path, &storage_config) {
            Ok(s) => s,
            Err(e) => {
                tokio_block_on(async {
                    state.set_error(&path, format!("Storage::open_auto: {}", e)).await;
                });
                return;
            }
        }
    };

    // 5a. Применить schema_extensions процессора, соответствующего этому репо.
    //     Двухступенчатый resolve: явный `language` из daemon.toml → fallback
    //     на auto-detect по маркерам корня. DDL идемпотентен (`IF NOT EXISTS`),
    //     повторный вызов на каждом старте безопасен.
    //
    //     Без этого вызова в сборке `bsl-indexer.exe` BSL-tools падают с
    //     `no such table: metadata_objects` (см. v0.8.0 регрессия —
    //     apply_schema_extensions раньше вызывался только в CLI-команде Index).
    let resolved_processor = processor_registry
        .as_ref()
        .and_then(|reg| reg.resolve(entry.language.as_deref(), &path).cloned());
    if let Some(proc) = resolved_processor.as_ref() {
        let exts = proc.schema_extensions();
        if !exts.is_empty() {
            if let Err(e) = storage.apply_schema_extensions(exts) {
                eprintln!(
                    "[worker:{}] apply_schema_extensions ('{}') упал: {}. \
                     Базовая индексация продолжится, но extension-tools могут не работать.",
                    path.display(), proc.name(), e
                );
            } else {
                eprintln!(
                    "[worker:{}] schema_extensions процессора '{}' применены ({} DDL)",
                    path.display(), proc.name(), exts.len()
                );
            }
        }
    }

    eprintln!("[worker:{}] initial reindex", path.display());

    // 6. Полная переиндексация (fast-path по mtime, если БД уже есть)
    let indexer_result = {
        let mut indexer = Indexer::with_config(&mut storage, index_config.clone());
        indexer.full_reindex(&path, false)
    };
    match indexer_result {
        Ok(result) => {
            eprintln!(
                "[worker:{}] initial reindex: {} файлов за {} мс (записано {}, пропущено {}, удалено {})",
                path.display(),
                result.files_scanned,
                result.elapsed_ms,
                result.files_indexed,
                result.files_skipped,
                result.files_deleted
            );
        }
        Err(e) => {
            tokio_block_on(async {
                state.set_error(&path, format!("full_reindex: {}", e)).await;
            });
            return;
        }
    }

    // 6a. index_extras процессора — для BSL это парсинг Configuration.xml /
    //     Forms / EventSubscriptions и заполнение metadata_*-таблиц.
    //
    //     ВАЖНО: вызывается ДО flush_to_disk. Если БД была новой и открыта
    //     in-memory — записи extras должны попасть в snapshot до сброса на
    //     диск, иначе исчезнут при reopen. Для disk-режима порядок не важен,
    //     но единый код проще.
    //
    //     Ошибка не фатальна: базовая индексация уже сохранена. Логируем и
    //     продолжаем — например, для репо без Configuration.xml (старая
    //     выгрузка обработок) парсер может ничего не найти и это нормально.
    if let Some(proc) = resolved_processor.as_ref() {
        if let Err(e) = proc.index_extras(&path, &mut storage) {
            eprintln!(
                "[worker:{}] index_extras процессора '{}' упал: {}. \
                 Базовая индексация при этом сохранена.",
                path.display(), proc.name(), e
            );
        } else {
            eprintln!(
                "[worker:{}] index_extras процессора '{}' выполнен",
                path.display(), proc.name()
            );
        }
    }

    // 7. Если БД была новой и открылась в памяти — flush + reopen в disk.
    //    Если уже был disk — ничего делать не нужно, изменения уже на диске.
    if !db_existed_before {
        if let Err(e) = storage.flush_to_disk(&db_path) {
            eprintln!("[worker:{}] предупреждение: flush_to_disk: {}", path.display(), e);
        }
        drop(storage);
        storage = match Storage::open_file(&db_path) {
            Ok(s) => s,
            Err(e) => {
                tokio_block_on(async {
                    state.set_error(&path, format!("Storage::open_file (disk reopen): {}", e)).await;
                });
                return;
            }
        };
        eprintln!("[worker:{}] переоткрыт в disk-режиме", path.display());
    }

    // Initial reindex мог накопить много страниц в WAL (особенно для больших
    // репо с 90k+ файлов). `PRAGMA wal_autocheckpoint=500` не гарантирует
    // физическое уменьшение файла — нужен явный TRUNCATE.
    match storage.checkpoint_truncate() {
        Ok((busy, log_pages, _)) if busy == 0 => {
            eprintln!(
                "[worker:{}] post-initial WAL checkpoint: {} страниц вытеснено",
                path.display(), log_pages
            );
        }
        Ok((busy, _, _)) => {
            eprintln!(
                "[worker:{}] post-initial WAL checkpoint: busy={} (частичный)",
                path.display(), busy
            );
        }
        Err(e) => {
            eprintln!("[worker:{}] post-initial checkpoint_truncate: {}", path.display(), e);
        }
    }

    // 9. Отпустить permit — следующий worker может начинать initial reindex.
    drop(_permit);

    // 10. Перевести в Ready и запустить watcher
    tokio_block_on(async {
        state.set_status(&path, PathStatus::Ready).await;
    });

    // 8. Watcher-цикл
    let debounce_ms = entry.debounce_ms.unwrap_or(index_config.debounce_ms);
    let batch_ms = entry.batch_ms.unwrap_or(index_config.batch_ms);
    let watcher_config = WatcherConfig {
        debounce_ms,
        batch_ms,
        exclude_dirs: index_config.exclude_dirs.clone(),
        exclude_file_patterns: index_config.exclude_file_patterns.clone(),
    };
    let (watcher, rx) = match create_watcher(&path, &watcher_config) {
        Ok(pair) => pair,
        Err(e) => {
            tokio_block_on(async {
                state.set_error(&path, format!("create_watcher: {}", e)).await;
            });
            return;
        }
    };
    // Держим watcher на стеке — при drop watcher остановится.
    let _watcher = watcher;

    eprintln!("[worker:{}] watcher активен (debounce={}ms, batch={}ms)",
        path.display(), debounce_ms, batch_ms);

    let registry = ParserRegistry::from_languages(&index_config.languages);
    // Эффективный лимит для file_contents — пробросим в apply_event,
    // чтобы Indexer::with_config не пересоздавался на каждое событие.
    let max_code_file_size = index_config.max_code_file_size_bytes;

    // Основной цикл обработки батчей. Idle-таймаут 500 мс даёт шанс проверить
    // shutdown-сигнал даже если файлов давно не меняли.
    const IDLE_POLL_MS: u64 = 500;
    loop {
        if shutdown_received(&mut shutdown_rx) {
            break;
        }

        let batch = match poll_batch(&rx, IDLE_POLL_MS, debounce_ms, batch_ms) {
            Ok(Some(b)) => {
                eprintln!("[worker:{}] batch: {} events", path.display(), b.len());
                b
            }
            Ok(None) => continue, // idle timeout — проверим shutdown на следующей итерации
            Err(_) => break,      // канал закрыт — watcher дропнут
        };
        if batch.is_empty() {
            continue;
        }

        tokio_block_on(async {
            state.set_status(&path, PathStatus::ReindexingBatch).await;
            state
                .set_progress(&path, Progress::new(0, batch.len()))
                .await;
        });

        if let Err(e) = storage.begin_batch() {
            eprintln!("[worker:{}] begin_batch: {}", path.display(), e);
            tokio_block_on(async {
                state.set_status(&path, PathStatus::Ready).await;
            });
            continue;
        }

        let mut done = 0usize;
        let batch_len = batch.len();
        for event in &batch {
            apply_event(&mut storage, &path, event, &registry, max_code_file_size);
            done += 1;
            if done % 50 == 0 || done == batch_len {
                tokio_block_on(async {
                    state
                        .set_progress(&path, Progress::new(done, batch_len))
                        .await;
                });
            }
        }

        if let Err(e) = storage.commit_batch() {
            eprintln!("[worker:{}] commit_batch: {}", path.display(), e);
        }
        // В disk-режиме (а worker сюда попадает всегда в disk после reopen на шаге 7)
        // flush_to_disk через Connection::backup() — бесполезное копирование БД самой
        // в себя, WAL не уменьшает. checkpoint_truncate реально схлопывает WAL.
        if let Err(e) = storage.checkpoint_truncate() {
            eprintln!("[worker:{}] checkpoint_truncate: {}", path.display(), e);
        }

        tokio_block_on(async {
            state.set_status(&path, PathStatus::Ready).await;
        });
    }

    eprintln!("[worker:{}] shutdown, финальный checkpoint", path.display());
    if let Err(e) = storage.checkpoint_truncate() {
        eprintln!("[worker:{}] финальный checkpoint_truncate: {}", path.display(), e);
    }
}

fn shutdown_received(rx: &mut tokio::sync::broadcast::Receiver<()>) -> bool {
    matches!(rx.try_recv(), Ok(()))
}

fn tokio_block_on<F: std::future::Future<Output = ()>>(fut: F) {
    tokio_block_on_value::<(), F>(fut);
}

fn tokio_block_on_value<T, F: std::future::Future<Output = T>>(fut: F) -> T {
    // Worker запускается внутри spawn_blocking, поэтому tokio runtime существует
    // и мы можем получить текущий handle.
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.block_on(fut)
    } else {
        // На случай запуска вне tokio (тесты) — собираем одноразовый runtime.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("create fallback tokio runtime");
        rt.block_on(fut)
    }
}

/// Обработать одно событие файловой системы: пересчитать хеш, записать/удалить в БД.
fn apply_event(
    storage: &mut Storage,
    root: &PathBuf,
    event: &FileEvent,
    registry: &ParserRegistry,
    max_code_file_size: usize,
) {
    match event {
        FileEvent::Modified(abs) | FileEvent::Created(abs) => {
            let (content, hash) = match hasher::file_hash(abs) {
                Ok(pair) => pair,
                Err(e) => {
                    // Частый случай: atomic-save через .tmp → rename. Watcher увидел
                    // событие на .tmp, но к моменту хэширования файл уже переименован.
                    // NotFound — не ошибка, тихо игнорируем.
                    if let Some(io_err) = e.downcast_ref::<std::io::Error>() {
                        if io_err.kind() == std::io::ErrorKind::NotFound {
                            return;
                        }
                    }
                    eprintln!("[worker:{}] file_hash {}: {}", root.display(), abs.display(), e);
                    return;
                }
            };

            let meta = std::fs::metadata(abs).ok();
            let mtime = meta.as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64);
            let file_size = meta.as_ref().map(|m| m.len() as i64);

            let rel_path = abs
                .strip_prefix(root)
                .unwrap_or(abs)
                .to_string_lossy()
                .replace('\\', "/");

            let category = categorize_file(abs);
            match category {
                FileCategory::Code(language) => {
                    let ext = abs
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("")
                        .to_lowercase();
                    if let Some(parser) = registry.get_parser(&ext) {
                        match parser.parse(&content, &rel_path) {
                            Ok(pr) => {
                                let indexer = Indexer::with_config(
                                    storage,
                                    IndexConfig {
                                        max_code_file_size_bytes: max_code_file_size,
                                        ..IndexConfig::default()
                                    },
                                );
                                // v0.7.1: для html (и других dual-indexed языков) дополнительно пишем
                                // raw-content в text_files — чтобы search_text/grep_text/read_file
                                // продолжали работать как для обычного text-файла.
                                let text_for_fts = if crate::indexer::file_types::is_dual_indexed_language(&language) {
                                    Some(content.as_str())
                                } else {
                                    None
                                };
                                if let Err(e) = indexer.write_code_to_db(
                                    &rel_path,
                                    &hash,
                                    &language,
                                    pr.lines_total,
                                    &pr.ast_hash,
                                    &pr,
                                    false,
                                    mtime,
                                    file_size,
                                    text_for_fts,
                                    Some(content.as_str()),
                                ) {
                                    eprintln!("[worker:{}] write_code {}: {}",
                                        root.display(), rel_path, e);
                                }
                            }
                            Err(e) => eprintln!("[worker:{}] parse {}: {}",
                                root.display(), rel_path, e),
                        }
                    }
                }
                FileCategory::Text => {
                    // Попробуем XML 1С — если есть BSL-блоки, пишем как код.
                    let ext = abs
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("");
                    let indexed_as_code = if ext == "xml" {
                        let xml_parser = crate::parser::xml_1c::Xml1CParser;
                        if let Ok(pr) = xml_parser.parse(&content, &rel_path) {
                            if !pr.functions.is_empty()
                                || !pr.classes.is_empty()
                                || !pr.variables.is_empty()
                            {
                                let indexer = Indexer::with_config(
                                    storage,
                                    IndexConfig {
                                        max_code_file_size_bytes: max_code_file_size,
                                        ..IndexConfig::default()
                                    },
                                );
                                indexer
                                    .write_code_to_db(
                                        &rel_path,
                                        &hash,
                                        "xml_1c",
                                        pr.lines_total,
                                        &pr.ast_hash,
                                        &pr,
                                        false,
                                        mtime,
                                        file_size,
                                        None,
                                        Some(content.as_str()),
                                    )
                                    .is_ok()
                            } else {
                                false
                            }
                        } else {
                            false
                        }
                    } else {
                        false
                    };
                    if !indexed_as_code {
                        let tr = TextParser::parse(&content);
                        let indexer = Indexer::new(storage);
                        if let Err(e) = indexer.write_text_to_db(
                            &rel_path,
                            &hash,
                            tr.lines_total,
                            &tr.content,
                            false,
                            mtime,
                            file_size,
                        ) {
                            eprintln!("[worker:{}] write_text {}: {}",
                                root.display(), rel_path, e);
                        }
                    }
                }
                FileCategory::Binary => {}
            }
        }
        FileEvent::Deleted(abs) => {
            let rel_path = abs
                .strip_prefix(root)
                .unwrap_or(abs)
                .to_string_lossy()
                .replace('\\', "/");
            if let Ok(Some(file)) = storage.get_file_by_path(&rel_path) {
                if let Some(id) = file.id {
                    let _ = storage.delete_file(id);
                }
            }
        }
    }
}
