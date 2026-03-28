/// Daemon-режим: startup индексация + MCP-сервер + file watcher + flush таймер
use std::path::PathBuf;
use std::sync::Arc;

use rmcp::ServiceExt;
use tokio::sync::Mutex;

use crate::indexer::config::IndexConfig;
use crate::indexer::file_types::{categorize_file, FileCategory};
use crate::indexer::hasher;
use crate::indexer::Indexer;
use crate::mcp::CodeIndexServer;
use crate::parser::text::TextParser;
use crate::parser::LanguageParser;
use crate::parser::ParserRegistry;
use crate::storage::memory::StorageConfig;
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

/// Запустить daemon: startup scan + MCP + watcher + flush
pub async fn run_daemon(config: DaemonConfig) -> anyhow::Result<()> {
    // 1. Загрузить/создать базу данных
    let mut storage = Storage::open_auto(&config.db_path, &config.storage_config)?;

    // 2. Startup scan — полная индексация проекта
    eprintln!("[daemon] Запуск startup scan...");
    let mut indexer = Indexer::with_config(&mut storage, config.index_config.clone());
    let result = indexer.full_reindex(&config.root, false)?;
    eprintln!(
        "[daemon] Startup: {} файлов за {} мс",
        result.files_indexed, result.elapsed_ms
    );

    // Flush после startup — сохраняем результаты на диск
    storage.flush_to_disk(&config.db_path)?;
    eprintln!("[daemon] Startup flush выполнен");

    // 3. Обернуть storage в Arc<Mutex> для shared доступа
    let shared_storage = Arc::new(Mutex::new(storage));

    // 4. Создать MCP-сервер с shared хранилищем
    let mcp_server = CodeIndexServer::new_from_shared(shared_storage.clone());

    if config.no_watch {
        // Без watcher — только MCP-сервер (режим совместимости)
        eprintln!("[daemon] File watcher отключён (--no-watch)");
        let service = mcp_server
            .serve(rmcp::transport::io::stdio())
            .await
            .map_err(|e| anyhow::anyhow!("MCP serve error: {}", e))?;
        service.waiting().await.map_err(|e| anyhow::anyhow!("MCP wait error: {}", e))?;
        return Ok(());
    }

    // 5. Создать file watcher
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

    // 6. Watcher task — блокирующий поток (notify использует std::sync)
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
            let mut storage = rt.block_on(storage_for_watcher.lock());

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
                                            // Создаём временный Indexer с &mut storage
                                            let indexer = Indexer::new(&mut storage);
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
                                            let indexer = Indexer::new(&mut storage);
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
                                    let indexer = Indexer::new(&mut storage);
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

    // 7. Flush timer task — периодическая запись на диск
    let flush_handle = tokio::spawn(async move {
        let interval = tokio::time::Duration::from_secs(flush_interval);
        loop {
            tokio::time::sleep(interval).await;
            let storage = storage_for_flush.lock().await;
            match storage.flush_to_disk(&db_path_for_flush) {
                Ok(_) => eprintln!("[flush] Записано на диск"),
                Err(e) => eprintln!("[flush] Ошибка: {}", e),
            }
        }
    });

    // 8. MCP-сервер — основной цикл, блокирует до завершения клиента
    let service = mcp_server
        .serve(rmcp::transport::io::stdio())
        .await
        .map_err(|e| anyhow::anyhow!("MCP serve error: {}", e))?;
    service
        .waiting()
        .await
        .map_err(|e| anyhow::anyhow!("MCP wait error: {}", e))?;

    // 9. Graceful shutdown
    eprintln!("[daemon] Завершение...");
    watcher_handle.abort();
    flush_handle.abort();

    // Финальный flush перед выходом
    let storage = shared_storage.lock().await;
    if let Err(e) = storage.flush_to_disk(&db_path_for_final) {
        eprintln!("[daemon] Ошибка финального flush: {}", e);
    } else {
        eprintln!("[daemon] Финальный flush выполнен");
    }

    Ok(())
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
