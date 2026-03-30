/// Daemon-режим: MCP-сервер стартует мгновенно, индексация в фоне + file watcher + flush таймер
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use rmcp::ServiceExt;
use tokio::sync::Mutex;

use crate::indexer::config::IndexConfig;
use crate::indexer::file_types::{categorize_file, FileCategory};
use crate::indexer::hasher;
use crate::indexer::{collect_candidates_standalone, collect_seen_paths_standalone, Indexer, ParsedFile};
use crate::mcp::CodeIndexServer;
use crate::parser::text::TextParser;
use crate::parser::LanguageParser;
use crate::parser::ParserRegistry;
use crate::storage::memory::StorageConfig;
use crate::storage::models::IndexingStatus;
use crate::storage::Storage;

/// Конфигурация daemon
pub struct DaemonConfig {
    /// Корневая директория проекта
    pub root: PathBuf,
    /// Путь к файлу БД
    pub db_path: PathBuf,
    /// Конфигурация индексатора
    pub index_config: IndexConfig,
    /// Конфигурация хранилища
    pub storage_config: StorageConfig,
    /// Запустить без file watcher (только MCP-сервер)
    pub no_watch: bool,
    /// Интервал записи на диск в секундах
    pub flush_interval_sec: u64,
}

/// Запустить daemon: MCP стартует мгновенно, открытие БД и индексация в фоне
pub async fn run_daemon(config: DaemonConfig) -> anyhow::Result<()> {
    // 1. Создать пустой shared_storage — Storage ещё не открыт
    let shared_storage: Arc<Mutex<Option<Storage>>> = Arc::new(Mutex::new(None));

    // 2. Статус фоновой инициализации — начинаем с Initializing
    let indexing_status = Arc::new(Mutex::new(IndexingStatus::Initializing));

    // 3. Создать MCP-сервер без Storage — стартует МГНОВЕННО
    let mcp_server = CodeIndexServer::new_from_shared(
        shared_storage.clone(),
        indexing_status.clone(),
    );

    // 4. Запустить фоновую инициализацию: открытие БД + индексация
    eprintln!("[daemon] MCP-сервер запускается (Storage инициализируется в фоне)...");
    let reindex_handle = tokio::spawn(background_init(
        shared_storage.clone(),
        indexing_status.clone(),
        config.db_path.clone(),
        config.storage_config.clone(),
        config.root.clone(),
        config.index_config.clone(),
    ));

    // Startup flush убран: данные в памяти, MCP отдаёт их оттуда.
    // Периодический flush (flush_interval_sec) сохранит на диск позже.
    // Это критично для больших БД (1+ ГБ), где backup блокирует Mutex на десятки секунд.

    if config.no_watch {
        // Без watcher — только MCP-сервер + периодический flush
        eprintln!("[daemon] File watcher отключён (--no-watch)");

        // Периодический flush — сохраняет in-memory БД на диск
        let storage_for_flush = shared_storage.clone();
        let db_path_flush = config.db_path.clone();
        let flush_interval = config.flush_interval_sec;
        let flush_handle = tokio::spawn(async move {
            let interval = tokio::time::Duration::from_secs(flush_interval);
            loop {
                tokio::time::sleep(interval).await;
                let guard = storage_for_flush.lock().await;
                // Пропускаем flush если Storage ещё не инициализирован
                if let Some(ref storage) = *guard {
                    match storage.flush_to_disk(&db_path_flush) {
                        Ok(_) => eprintln!("[flush] Записано на диск"),
                        Err(e) => eprintln!("[flush] Ошибка: {}", e),
                    }
                }
            }
        });

        let service = mcp_server
            .serve(rmcp::transport::io::stdio())
            .await
            .map_err(|e| anyhow::anyhow!("MCP serve error: {}", e))?;
        service.waiting().await.map_err(|e| anyhow::anyhow!("MCP wait error: {}", e))?;
        flush_handle.abort();
        reindex_handle.abort();
        return Ok(());
    }

    // 6. Создать file watcher
    let watcher_config = crate::watcher::WatcherConfig {
        debounce_ms: config.index_config.debounce_ms,
        batch_ms: config.index_config.batch_ms,
        exclude_dirs: config.index_config.exclude_dirs.clone(),
    };
    let (_watcher, rx) = crate::watcher::create_watcher(&config.root, &watcher_config)?;
    eprintln!("[daemon] File watcher запущен: {:?}", config.root);

    // Копии значений для задач
    let root = config.root.clone();
    let db_path_for_flush = config.db_path.clone();
    let db_path_for_final = config.db_path.clone();
    let index_config = config.index_config.clone();
    let flush_interval = config.flush_interval_sec;

    let storage_for_watcher = shared_storage.clone();
    let storage_for_flush = shared_storage.clone();

    // 7. Watcher task — блокирующий поток (notify использует std::sync)
    let watcher_handle = tokio::task::spawn_blocking(move || {
        // Создаём реестр парсеров из конфигурации
        let registry = ParserRegistry::from_languages(&index_config.languages);
        let debounce_ms = index_config.debounce_ms;
        let batch_ms = index_config.batch_ms;

        loop {
            // Ждём и собираем батч событий
            let batch = crate::watcher::collect_batch(&rx, debounce_ms, batch_ms);
            if batch.is_empty() {
                // Канал закрыт — завершаем цикл
                eprintln!("[watcher] Канал закрыт, завершение");
                break;
            }

            eprintln!("[watcher] Обработка {} событий", batch.len());

            // Получаем блокировку через tokio runtime
            let rt = tokio::runtime::Handle::current();
            let mut guard = rt.block_on(storage_for_watcher.lock());

            // Пропускаем батч если Storage ещё не инициализирован
            let storage: &mut Storage = match guard.as_mut() {
                Some(s) => s,
                None => {
                    eprintln!("[watcher] Storage ещё не готов, пропускаем батч");
                    continue;
                }
            };

            // Начинаем транзакцию батча
            if let Err(e) = storage.begin_batch() {
                eprintln!("[watcher] Ошибка begin_batch: {}", e);
                continue;
            }

            for event in &batch {
                match event {
                    crate::watcher::FileEvent::Modified(path)
                    | crate::watcher::FileEvent::Created(path) => {
                        // Читаем файл и вычисляем хеш
                        let (content, hash) = match hasher::file_hash(path) {
                            Ok(pair) => pair,
                            Err(e) => {
                                eprintln!("[watcher] Ошибка чтения {:?}: {}", path, e);
                                continue;
                            }
                        };

                        // Нормализуем путь к относительному
                        let rel_path = path
                            .strip_prefix(&root)
                            .unwrap_or(path)
                            .to_string_lossy()
                            .replace('\\', "/");

                        let category = categorize_file(path);
                        match &category {
                            FileCategory::Code(language) => {
                                let ext = path
                                    .extension()
                                    .and_then(|e| e.to_str())
                                    .unwrap_or("")
                                    .to_lowercase();

                                if let Some(parser) = registry.get_parser(&ext) {
                                    match parser.parse(&content, &rel_path) {
                                        Ok(parse_result) => {
                                            // Используем Indexer::write_code_to_db
                                            let indexer = Indexer::new(storage);
                                            if let Err(e) = indexer.write_code_to_db(
                                                &rel_path,
                                                &hash,
                                                language,
                                                parse_result.lines_total,
                                                &parse_result.ast_hash,
                                                &parse_result,
                                                false, // skip_delete = false (инкрементальное обновление)
                                            ) {
                                                eprintln!(
                                                    "[watcher] Ошибка write_code_to_db {}: {}",
                                                    rel_path, e
                                                );
                                            }
                                        }
                                        Err(e) => {
                                            eprintln!(
                                                "[watcher] Ошибка парсинга {}: {}",
                                                rel_path, e
                                            );
                                        }
                                    }
                                }
                            }
                            FileCategory::Text => {
                                // Проверяем: возможно это XML 1С
                                let ext = path
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
                                            let indexer = Indexer::new(storage);
                                            let _ = indexer.write_code_to_db(
                                                &rel_path,
                                                &hash,
                                                "xml_1c",
                                                pr.lines_total,
                                                &pr.ast_hash,
                                                &pr,
                                                false,
                                            );
                                            true
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
                                    let text_result = TextParser::parse(&content);
                                    let indexer = Indexer::new(storage);
                                    if let Err(e) = indexer.write_text_to_db(
                                        &rel_path,
                                        &hash,
                                        text_result.lines_total,
                                        &text_result.content,
                                        false, // skip_delete = false
                                    ) {
                                        eprintln!(
                                            "[watcher] Ошибка write_text_to_db {}: {}",
                                            rel_path, e
                                        );
                                    }
                                }
                            }
                            FileCategory::Binary => {
                                // Бинарные файлы пропускаем
                            }
                        }
                    }
                    crate::watcher::FileEvent::Deleted(path) => {
                        let rel_path = path
                            .strip_prefix(&root)
                            .unwrap_or(path)
                            .to_string_lossy()
                            .replace('\\', "/");

                        // Ищем файл в БД и удаляем
                        match storage.get_file_by_path(&rel_path) {
                            Ok(Some(file)) => {
                                if let Some(id) = file.id {
                                    if let Err(e) = storage.delete_file(id) {
                                        eprintln!(
                                            "[watcher] Ошибка delete_file {}: {}",
                                            rel_path, e
                                        );
                                    } else {
                                        eprintln!("[watcher] Удалён из индекса: {}", rel_path);
                                    }
                                }
                            }
                            Ok(None) => {} // файла не было в индексе
                            Err(e) => {
                                eprintln!(
                                    "[watcher] Ошибка поиска файла {}: {}",
                                    rel_path, e
                                );
                            }
                        }
                    }
                }
            }

            // Коммитим транзакцию батча
            if let Err(e) = storage.commit_batch() {
                eprintln!("[watcher] Ошибка commit_batch: {}", e);
            } else {
                eprintln!("[watcher] Обработано {} событий", batch.len());
            }
        }
    });

    // 8. Flush timer task — периодическая запись на диск
    let flush_handle = tokio::spawn(async move {
        let interval = tokio::time::Duration::from_secs(flush_interval);
        loop {
            tokio::time::sleep(interval).await;
            let guard = storage_for_flush.lock().await;
            // Пропускаем flush если Storage ещё не инициализирован
            if let Some(ref storage) = *guard {
                match storage.flush_to_disk(&db_path_for_flush) {
                    Ok(_) => eprintln!("[flush] Записано на диск"),
                    Err(e) => eprintln!("[flush] Ошибка: {}", e),
                }
            }
        }
    });

    // 9. MCP-сервер — основной цикл, блокирует до завершения клиента
    let service = mcp_server
        .serve(rmcp::transport::io::stdio())
        .await
        .map_err(|e| anyhow::anyhow!("MCP serve error: {}", e))?;
    service
        .waiting()
        .await
        .map_err(|e| anyhow::anyhow!("MCP wait error: {}", e))?;

    // 10. Graceful shutdown
    eprintln!("[daemon] Завершение...");
    watcher_handle.abort();
    flush_handle.abort();
    reindex_handle.abort();

    // Финальный flush перед выходом
    let guard = shared_storage.lock().await;
    if let Some(ref storage) = *guard {
        if let Err(e) = storage.flush_to_disk(&db_path_for_final) {
            eprintln!("[daemon] Ошибка финального flush: {}", e);
        } else {
            eprintln!("[daemon] Финальный flush выполнен");
        }
    }

    Ok(())
}

/// Фоновая инициализация: открыть БД, положить в shared_storage, затем запустить индексацию.
///
/// Это единственное место, где происходит Storage::open_auto — MCP к этому моменту уже
/// принимает соединения и отвечает на запросы статусом "Initializing".
async fn background_init(
    shared_storage: Arc<Mutex<Option<Storage>>>,
    indexing_status: Arc<Mutex<IndexingStatus>>,
    db_path: PathBuf,
    storage_config: StorageConfig,
    root: PathBuf,
    config: IndexConfig,
) {
    // Фаза 0: открыть БД (может занять время для большой in-memory базы)
    eprintln!("[init] Открытие базы данных: {:?}", db_path);
    let storage = match Storage::open_auto(&db_path, &storage_config) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[init] ОШИБКА открытия БД: {}", e);
            let mut status = indexing_status.lock().await;
            *status = IndexingStatus::Failed {
                error: format!("open_auto: {}", e),
            };
            return;
        }
    };

    // Положить Storage в shared — инструменты начинают работать
    {
        let mut guard = shared_storage.lock().await;
        *guard = Some(storage);
    }
    eprintln!("[init] БД загружена, инструменты доступны");

    // Передать управление фоновой индексации
    background_reindex(shared_storage, indexing_status, root, config).await;
}

/// Фоновая индексация с пофазным захватом Mutex.
///
/// Фазы 1-2 (сбор файлов + парсинг) не трогают Storage → mutex свободен → tools отвечают.
/// Фаза 3 (запись) захватывает mutex батчами → между батчами tools отвечают.
/// Вызывается из background_init после того как Storage уже открыт и помещён в shared.
async fn background_reindex(
    shared_storage: Arc<Mutex<Option<Storage>>>,
    indexing_status: Arc<Mutex<IndexingStatus>>,
    root: PathBuf,
    config: IndexConfig,
) {
    let start = std::time::Instant::now();

    // ── Phase 0: прочитать existing files из БД (короткий lock) ──────────
    {
        let mut status = indexing_status.lock().await;
        *status = IndexingStatus::Indexing {
            phase: "collecting".to_string(),
            files_done: 0,
            files_total: 0,
        };
    }

    let existing_files: HashMap<String, (i64, String)> = {
        // Storage гарантированно есть — background_init уже открыл БД
        let guard = shared_storage.lock().await;
        let storage = guard.as_ref().unwrap();
        match storage.get_all_files() {
            Ok(files) => files
                .into_iter()
                .filter_map(|f| f.id.map(|id| (f.path.clone(), (id, f.content_hash.clone()))))
                .collect(),
            Err(e) => {
                eprintln!("[reindex] Ошибка get_all_files: {}", e);
                let mut status = indexing_status.lock().await;
                *status = IndexingStatus::Failed {
                    error: e.to_string(),
                };
                return;
            }
        }
    };
    // Mutex освобождён

    let is_fresh_db = existing_files.is_empty();

    // ── Phase 1: сбор кандидатов (walkdir + hash, БЕЗ lock) ─────────────
    let config_clone = config.clone();
    let root_clone = root.clone();
    let existing_clone = existing_files.clone();
    let candidates_result = tokio::task::spawn_blocking(move || {
        let cand_start = std::time::Instant::now();
        let result = collect_candidates_standalone(&root_clone, &config_clone, false, &existing_clone);
        let cand_ms = cand_start.elapsed().as_millis();
        eprintln!("[reindex] Сбор кандидатов: {} мс", cand_ms);
        result
    })
    .await;

    let (candidate_files, files_scanned, files_skipped, collect_errors) = match candidates_result {
        Ok(Ok(result)) => result,
        Ok(Err(e)) => {
            eprintln!("[reindex] Ошибка сбора кандидатов: {}", e);
            let mut status = indexing_status.lock().await;
            *status = IndexingStatus::Failed {
                error: e.to_string(),
            };
            return;
        }
        Err(e) => {
            eprintln!("[reindex] JoinError при сборе кандидатов: {}", e);
            let mut status = indexing_status.lock().await;
            *status = IndexingStatus::Failed {
                error: e.to_string(),
            };
            return;
        }
    };

    eprintln!(
        "[reindex] Кандидатов: {}, просмотрено: {}, пропущено: {}, ошибок: {}",
        candidate_files.len(),
        files_scanned,
        files_skipped,
        collect_errors.len()
    );

    if candidate_files.is_empty() {
        eprintln!("[reindex] Нечего индексировать, данные актуальны");
        let mut status = indexing_status.lock().await;
        *status = IndexingStatus::Completed {
            files_indexed: 0,
            elapsed_ms: start.elapsed().as_millis() as u64,
        };
        return;
    }

    // ── Phase 2: параллельный парсинг (rayon, CPU-bound, БЕЗ lock) ──────
    {
        let mut status = indexing_status.lock().await;
        *status = IndexingStatus::Indexing {
            phase: "parsing".to_string(),
            files_done: 0,
            files_total: candidate_files.len(),
        };
    }

    let languages = config.languages.clone();
    let parse_results = tokio::task::spawn_blocking(move || {
        use rayon::prelude::*;
        let registry = ParserRegistry::from_languages(&languages);
        let parse_start = std::time::Instant::now();

        let results: Vec<ParsedFile> = candidate_files
            .par_iter()
            .map(|(rel_path, content, hash, category)| {
                match category {
                    FileCategory::Code(language) => {
                        let ext = std::path::Path::new(rel_path.as_str())
                            .extension()
                            .and_then(|e| e.to_str())
                            .unwrap_or("")
                            .to_lowercase();

                        match registry.get_parser(&ext) {
                            Some(parser) => match parser.parse(content, rel_path) {
                                Ok(pr) => ParsedFile::Code {
                                    rel_path: rel_path.clone(),
                                    content_hash: hash.clone(),
                                    language: language.clone(),
                                    lines_total: pr.lines_total,
                                    ast_hash: pr.ast_hash.clone(),
                                    parse_result: pr,
                                },
                                Err(e) => ParsedFile::Error {
                                    rel_path: rel_path.clone(),
                                    error: e.to_string(),
                                },
                            },
                            None => ParsedFile::Error {
                                rel_path: rel_path.clone(),
                                error: format!("Нет парсера для расширения: {}", ext),
                            },
                        }
                    }
                    FileCategory::Text => {
                        // XML 1С — проверяем, есть ли в нём код
                        let ext = std::path::Path::new(rel_path.as_str())
                            .extension()
                            .and_then(|e| e.to_str())
                            .unwrap_or("");
                        if ext == "xml" {
                            let xml_parser = crate::parser::xml_1c::Xml1CParser;
                            if let Ok(pr) = xml_parser.parse(content, rel_path) {
                                if !pr.functions.is_empty()
                                    || !pr.classes.is_empty()
                                    || !pr.variables.is_empty()
                                {
                                    return ParsedFile::Code {
                                        rel_path: rel_path.clone(),
                                        content_hash: hash.clone(),
                                        language: "xml_1c".to_string(),
                                        lines_total: pr.lines_total,
                                        ast_hash: pr.ast_hash.clone(),
                                        parse_result: pr,
                                    };
                                }
                            }
                        }
                        // Fallback: текстовая индексация
                        let text_result = TextParser::parse(content);
                        ParsedFile::Text {
                            rel_path: rel_path.clone(),
                            content_hash: hash.clone(),
                            lines_total: text_result.lines_total,
                            content: text_result.content,
                        }
                    }
                    FileCategory::Binary => unreachable!("бинарные файлы не попадают в кандидаты"),
                }
            })
            .collect();

        let parse_ms = parse_start.elapsed().as_millis();
        eprintln!("[reindex] Парсинг (rayon): {} мс ({} файлов)", parse_ms, results.len());
        results
    })
    .await
    .unwrap_or_else(|e| {
        eprintln!("[reindex] JoinError при парсинге: {}", e);
        vec![]
    });

    if parse_results.is_empty() {
        let mut status = indexing_status.lock().await;
        *status = IndexingStatus::Failed {
            error: "Парсинг не вернул результатов".to_string(),
        };
        return;
    }

    // ── Phase 3: запись в БД батчами (lock per batch) ────────────────────
    {
        let mut status = indexing_status.lock().await;
        *status = IndexingStatus::Indexing {
            phase: "writing".to_string(),
            files_done: 0,
            files_total: parse_results.len(),
        };
    }

    let batch_size = config.batch_size;
    let bulk_mode = parse_results.len() > config.bulk_threshold;

    // Bulk-load: удалить индексы перед массовой записью (короткий lock)
    if bulk_mode {
        eprintln!(
            "[reindex] Bulk-load: {} файлов (порог {}), удаляем индексы",
            parse_results.len(),
            config.bulk_threshold
        );
        let guard = shared_storage.lock().await;
        let storage = guard.as_ref().unwrap();
        if let Err(e) = storage.prepare_bulk_load() {
            eprintln!("[reindex] Ошибка prepare_bulk_load: {}", e);
            // Продолжаем без bulk — будет медленнее, но работоспособно
        }
    }

    let write_start = std::time::Instant::now();
    let mut files_indexed = 0usize;
    let mut errors = collect_errors;

    // Пишем батчами, освобождая mutex между ними
    for chunk in parse_results.chunks(batch_size) {
        let mut guard = shared_storage.lock().await;
        let storage = guard.as_mut().unwrap();
        if let Err(e) = storage.begin_batch() {
            eprintln!("[reindex] Ошибка begin_batch: {}", e);
            continue;
        }

        for parsed in chunk {
            match parsed {
                ParsedFile::Code {
                    rel_path,
                    content_hash,
                    language,
                    lines_total,
                    ast_hash,
                    parse_result,
                } => {
                    let indexer = Indexer::new(storage);
                    match indexer.write_code_to_db(
                        rel_path,
                        content_hash,
                        language,
                        *lines_total,
                        ast_hash,
                        parse_result,
                        is_fresh_db,
                    ) {
                        Ok(_) => files_indexed += 1,
                        Err(e) => errors.push((rel_path.clone(), e.to_string())),
                    }
                }
                ParsedFile::Text {
                    rel_path,
                    content_hash,
                    lines_total,
                    content,
                } => {
                    let indexer = Indexer::new(storage);
                    match indexer.write_text_to_db(rel_path, content_hash, *lines_total, content, is_fresh_db) {
                        Ok(_) => files_indexed += 1,
                        Err(e) => errors.push((rel_path.clone(), e.to_string())),
                    }
                }
                ParsedFile::Error { rel_path, error } => {
                    errors.push((rel_path.clone(), error.clone()));
                }
            }
        }

        if let Err(e) = storage.commit_batch() {
            eprintln!("[reindex] Ошибка commit_batch: {}", e);
        }
        // Mutex освобождается здесь — tools могут отвечать между батчами

        // Обновляем прогресс
        let mut status = indexing_status.lock().await;
        *status = IndexingStatus::Indexing {
            phase: "writing".to_string(),
            files_done: files_indexed,
            files_total: parse_results.len(),
        };
    }

    let write_ms = write_start.elapsed().as_millis();
    eprintln!("[reindex] Запись в БД: {} мс ({} файлов)", write_ms, files_indexed);

    // ── Phase 4: rebuild индексов (короткий lock) ────────────────────────
    if bulk_mode {
        let idx_start = std::time::Instant::now();
        eprintln!("[reindex] Создание B-tree индексов и перестройка FTS...");
        let guard = shared_storage.lock().await;
        let storage = guard.as_ref().unwrap();
        if let Err(e) = storage.finish_bulk_load() {
            eprintln!("[reindex] Ошибка finish_bulk_load: {}", e);
        }
        let idx_ms = idx_start.elapsed().as_millis();
        eprintln!("[reindex] Индексы + FTS rebuild: {} мс", idx_ms);
    }

    // ── Phase 5: удаление устаревших файлов (lock) ──────────────────────
    let config_for_cleanup = config.clone();
    let root_for_cleanup = root.clone();
    let seen_paths = tokio::task::spawn_blocking(move || {
        collect_seen_paths_standalone(&root_for_cleanup, &config_for_cleanup)
    })
    .await
    .unwrap_or_default();

    let mut files_deleted = 0usize;
    {
        let guard = shared_storage.lock().await;
        let storage = guard.as_ref().unwrap();
        if let Err(e) = storage.begin_batch() {
            eprintln!("[reindex] Ошибка begin_batch (cleanup): {}", e);
        } else {
            for (path, (id, _)) in &existing_files {
                if !seen_paths.contains(path) {
                    if let Err(e) = storage.delete_file(*id) {
                        eprintln!("[reindex] Ошибка delete_file {}: {}", path, e);
                    } else {
                        files_deleted += 1;
                    }
                }
            }
            if let Err(e) = storage.commit_batch() {
                eprintln!("[reindex] Ошибка commit_batch (cleanup): {}", e);
            }
        }
    }

    if files_deleted > 0 {
        eprintln!("[reindex] Удалено устаревших файлов: {}", files_deleted);
    }

    // ── Готово ───────────────────────────────────────────────────────────
    let elapsed_ms = start.elapsed().as_millis() as u64;
    eprintln!(
        "[reindex] Завершено: {} файлов за {} мс (удалено: {}, ошибок: {})",
        files_indexed, elapsed_ms, files_deleted, errors.len()
    );

    let mut status = indexing_status.lock().await;
    *status = IndexingStatus::Completed {
        files_indexed,
        elapsed_ms,
    };
}

/// Вспомогательная функция: обработать изменённый код-файл (для тестов)
pub fn process_code_update(
    storage: &mut Storage,
    path: &std::path::Path,
    root: &std::path::Path,
    registry: &ParserRegistry,
) -> anyhow::Result<()> {
    let (content, hash) = hasher::file_hash(path)?;
    let rel_path = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");

    let category = categorize_file(path);
    let indexer = Indexer::new(storage);

    match &category {
        FileCategory::Code(language) => {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();
            if let Some(parser) = registry.get_parser(&ext) {
                let parse_result = parser.parse(&content, &rel_path)?;
                indexer.write_code_to_db(
                    &rel_path,
                    &hash,
                    language,
                    parse_result.lines_total,
                    &parse_result.ast_hash,
                    &parse_result,
                    false,
                )?;
            }
        }
        FileCategory::Text => {
            let text_result = TextParser::parse(&content);
            indexer.write_text_to_db(&rel_path, &hash, text_result.lines_total, &text_result.content, false)?;
        }
        FileCategory::Binary => {}
    }
    Ok(())
}
