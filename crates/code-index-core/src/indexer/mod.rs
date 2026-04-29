/// Модуль индексатора — обход директорий, определение типов файлов, хеширование
pub mod config;
pub mod file_types;
pub mod hasher;

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use rayon::prelude::*;
use walkdir::WalkDir;

use crate::parser::types::ParseResult;
use crate::parser::ParserRegistry;
use crate::parser::LanguageParser;
use crate::parser::text::TextParser;
use crate::storage::models::*;
use crate::storage::Storage;
use config::IndexConfig;
use file_types::{categorize_file, FileCategory};

/// Результат одного прохода индексации
#[derive(Debug)]
pub struct IndexResult {
    /// Сколько файлов просмотрено (не считая бинарных)
    pub files_scanned: usize,
    /// Сколько файлов реально записано в БД (новые или изменённые)
    pub files_indexed: usize,
    /// Сколько файлов пропущено (хеш не изменился)
    pub files_skipped: usize,
    /// Сколько файлов удалено из БД (больше не существуют на диске)
    pub files_deleted: usize,
    /// Список ошибок: (путь, сообщение)
    pub errors: Vec<(String, String)>,
    /// Время работы в миллисекундах
    pub elapsed_ms: u64,
}

/// Результат параллельного парсинга одного файла
pub enum ParsedFile {
    /// Файл с исходным кодом успешно распаршен
    Code {
        rel_path: String,
        content_hash: String,
        language: String,
        lines_total: usize,
        ast_hash: String,
        parse_result: ParseResult,
        mtime: i64,
        file_size: i64,
        /// Для языков с двойной индексацией (html в v0.7.1) — raw-content
        /// для дополнительной записи в text_files (FTS+regex+read_file).
        /// Для остальных языков — None.
        text_for_fts: Option<String>,
    },
    /// Текстовый файл (без AST)
    Text {
        rel_path: String,
        content_hash: String,
        lines_total: usize,
        content: String,
        mtime: i64,
        file_size: i64,
    },
    /// Ошибка парсинга
    Error {
        rel_path: String,
        error: String,
    },
}

/// Индексатор файловой системы
pub struct Indexer<'a> {
    storage: &'a mut Storage,
    /// Конфигурация индексатора
    config: IndexConfig,
}

impl<'a> Indexer<'a> {
    /// Создать индексатор с уже открытым хранилищем и конфигурацией по умолчанию
    pub fn new(storage: &'a mut Storage) -> Self {
        Self {
            storage,
            config: IndexConfig::default(),
        }
    }

    /// Создать индексатор с явно переданной конфигурацией
    pub fn with_config(storage: &'a mut Storage, config: IndexConfig) -> Self {
        Self { storage, config }
    }

    /// Полная переиндексация директории `root`.
    ///
    /// Если `force = true` — перезаписать все файлы независимо от хеша.
    /// Если `force = false` — пропустить файлы с неизменённым content_hash.
    ///
    /// При количестве файлов для индексации > `config.bulk_threshold` автоматически
    /// включается bulk-load режим: индексы и FTS-триггеры удаляются перед загрузкой
    /// и пересоздаются (с rebuild FTS) после — это значительно ускоряет INSERT.
    ///
    /// Парсинг (tree-sitter, CPU-bound) выполняется параллельно через rayon.
    /// Запись в SQLite (I/O-bound) — последовательно из основного потока.
    ///
    /// По завершении удаляет из БД записи файлов, которых больше нет на диске.
    pub fn full_reindex(&mut self, root: &Path, force: bool) -> Result<IndexResult> {
        let start = std::time::Instant::now();
        let mut result = IndexResult {
            files_scanned: 0,
            files_indexed: 0,
            files_skipped: 0,
            files_deleted: 0,
            errors: vec![],
            elapsed_ms: 0,
        };

        // ── Этап 0: загрузка состояния БД ─────────────────────────────────────
        // Тип: path → (id, content_hash, mtime, file_size)
        let existing_files: HashMap<String, (i64, String, Option<i64>, Option<i64>)> = self
            .storage
            .get_all_files()?
            .into_iter()
            .filter_map(|f| {
                f.id.map(|id| (f.path.clone(), (id, f.content_hash.clone(), f.mtime, f.file_size)))
            })
            .collect();

        // Определяем: это первичная индексация (пустая БД) или обновление
        let is_fresh_db = existing_files.is_empty();

        // ── Этап 1: сбор кандидатов (параллельный read+hash) ─────────────────
        let candidates_start = std::time::Instant::now();
        let (candidate_files, seen_paths, metadata_updates) = self.collect_candidates(root, force, &existing_files, &mut result)?;
        let candidates_ms = candidates_start.elapsed().as_millis();
        eprintln!("[timing] Сбор кандидатов: {} мс ({} файлов)", candidates_ms, candidate_files.len());

        // Включаем bulk-load если количество файлов для индексации превышает порог
        let bulk_mode = candidate_files.len() > self.config.bulk_threshold;

        if bulk_mode && is_fresh_db {
            // Первичная индексация: таблицы уже созданы через initialize(),
            // дропаем индексы которые были созданы вместе со схемой
            eprintln!(
                "[bulk] Первичная индексация {} файлов (порог {}): удаляем индексы",
                candidate_files.len(),
                self.config.bulk_threshold
            );
            self.storage.prepare_bulk_load()?;
        } else if bulk_mode {
            // Обновление существующей БД: дропаем индексы перед массовой загрузкой
            eprintln!(
                "[bulk] Обновление {} файлов (порог {}): удаляем индексы",
                candidate_files.len(),
                self.config.bulk_threshold
            );
            self.storage.prepare_bulk_load()?;
        }

        // Создаём реестр парсеров из конфигурации — один раз для всего прохода.
        // ParserRegistry содержит HashMap<String, Arc<dyn LanguageParser>>.
        // LanguageParser: Send + Sync, Arc: Send + Sync, HashMap: Send+Sync →
        // ParserRegistry: Send + Sync, что требуется для par_iter.
        let registry = ParserRegistry::from_languages(&self.config.languages);

        // ── Этап 2: параллельный парсинг (CPU-bound) ─────────────────────────
        // tree-sitter парсинг выполняется в нескольких потоках через rayon.
        // Чтение файлов уже выполнено в collect_candidates — здесь только AST.
        let parse_start = std::time::Instant::now();
        let parse_results: Vec<ParsedFile> = candidate_files
            .par_iter()
            .map(|(rel_path, content, hash, category, mtime, file_size)| {
                match category {
                    FileCategory::Code(language) => {
                        // Определяем парсер по расширению файла
                        let ext = Path::new(rel_path.as_str())
                            .extension()
                            .and_then(|e| e.to_str())
                            .unwrap_or("")
                            .to_lowercase();

                        match registry.get_parser(&ext) {
                            Some(parser) => {
                                match parser.parse(content, rel_path) {
                                    Ok(pr) => ParsedFile::Code {
                                        rel_path: rel_path.clone(),
                                        content_hash: hash.clone(),
                                        language: language.clone(),
                                        lines_total: pr.lines_total,
                                        ast_hash: pr.ast_hash.clone(),
                                        parse_result: pr,
                                        mtime: *mtime,
                                        file_size: *file_size,
                                        text_for_fts: if super::indexer::file_types::is_dual_indexed_language(language) {
                                            Some(content.clone())
                                        } else {
                                            None
                                        },
                                    },
                                    Err(e) => ParsedFile::Error {
                                        rel_path: rel_path.clone(),
                                        error: e.to_string(),
                                    },
                                }
                            }
                            None => ParsedFile::Error {
                                rel_path: rel_path.clone(),
                                error: format!("Нет парсера для расширения: {}", ext),
                            },
                        }
                    }
                    FileCategory::Text => {
                        // Проверяем: это XML-файл выгрузки 1С?
                        let ext = Path::new(rel_path.as_str())
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
                                        mtime: *mtime,
                                        file_size: *file_size,
                                        text_for_fts: None,
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
                            mtime: *mtime,
                            file_size: *file_size,
                        }
                    }
                    FileCategory::Binary => unreachable!("бинарные файлы не должны попасть сюда"),
                }
            })
            .collect();
        let parse_ms = parse_start.elapsed().as_millis();
        eprintln!("[timing] Парсинг (rayon): {} мс ({} файлов)", parse_ms, parse_results.len());

        // ── Этап 3: последовательная запись в SQLite ──────────────────────────
        // SQLite не поддерживает параллельную запись — пишем из основного потока.
        let write_start = std::time::Instant::now();
        let batch_size = self.config.batch_size;
        let mut batch_count = 0usize;

        // Открываем первую транзакцию перед началом цикла
        self.storage.begin_batch()?;

        for parsed in &parse_results {
            // Прогресс-лог каждые batch_size файлов
            let total_processed = result.files_indexed + result.errors.len();
            if total_processed > 0 && total_processed % batch_size == 0 {
                eprintln!(
                    "[{}/{}] Проиндексировано {}, пропущено {}...",
                    total_processed,
                    parse_results.len(),
                    result.files_indexed,
                    result.files_skipped
                );
            }

            match parsed {
                ParsedFile::Code {
                    rel_path,
                    content_hash,
                    language,
                    lines_total,
                    ast_hash,
                    parse_result,
                    mtime,
                    file_size,
                    text_for_fts,
                } => {
                    match self.write_code_to_db(
                        rel_path,
                        content_hash,
                        language,
                        *lines_total,
                        ast_hash,
                        parse_result,
                        is_fresh_db,
                        Some(*mtime),
                        Some(*file_size),
                        text_for_fts.as_deref(),
                    ) {
                        Ok(_) => {
                            result.files_indexed += 1;
                            batch_count += 1;
                        }
                        Err(e) => {
                            result.errors.push((rel_path.clone(), e.to_string()));
                        }
                    }
                }
                ParsedFile::Text {
                    rel_path,
                    content_hash,
                    lines_total,
                    content,
                    mtime,
                    file_size,
                } => {
                    match self.write_text_to_db(rel_path, content_hash, *lines_total, content, is_fresh_db, Some(*mtime), Some(*file_size)) {
                        Ok(_) => {
                            result.files_indexed += 1;
                            batch_count += 1;
                        }
                        Err(e) => {
                            result.errors.push((rel_path.clone(), e.to_string()));
                        }
                    }
                }
                ParsedFile::Error { rel_path, error } => {
                    result.errors.push((rel_path.clone(), error.clone()));
                }
            }

            // Коммитим накопленный батч и открываем новую транзакцию
            if batch_count >= batch_size {
                self.storage.commit_batch()?;
                self.storage.begin_batch()?;
                batch_count = 0;
            }
        }

        // Коммитим оставшиеся записи последнего неполного батча
        self.storage.commit_batch()?;
        let write_ms = write_start.elapsed().as_millis();
        eprintln!("[timing] Запись в БД: {} мс ({} файлов)", write_ms, result.files_indexed);

        // Обновляем mtime/file_size для файлов с неизменённым содержимым
        if !metadata_updates.is_empty() {
            self.storage.begin_batch()?;
            for (path, mtime, file_size) in &metadata_updates {
                let _ = self.storage.update_file_metadata(path, *mtime, *file_size);
            }
            self.storage.commit_batch()?;
        }

        // ── Этап 4: индексы + FTS rebuild ────────────────────────────────────
        // Завершаем bulk-load: пересоздаём индексы, триггеры, rebuild FTS
        if bulk_mode {
            let idx_start = std::time::Instant::now();
            eprintln!("[bulk] Создание B-tree индексов и перестройка FTS...");
            self.storage.finish_bulk_load()?;
            let idx_ms = idx_start.elapsed().as_millis();
            eprintln!("[timing] Индексы + FTS rebuild: {} мс", idx_ms);
        }

        // ── Этап 5: удаление устаревших записей ──────────────────────────────
        // seen_paths уже собран в Этапе 1 — повторный обход дерева не нужен
        let cleanup_start = std::time::Instant::now();

        // Удаляем из БД файлы, которых больше нет на диске — в одной транзакции
        self.storage.begin_batch()?;
        for (path, (id, _, _, _)) in &existing_files {
            if !seen_paths.contains(path) {
                self.storage.delete_file(*id)?;
                result.files_deleted += 1;
            }
        }
        self.storage.commit_batch()?;
        let cleanup_ms = cleanup_start.elapsed().as_millis();
        if result.files_deleted > 0 {
            eprintln!("[timing] Удаление устаревших: {} мс ({} файлов)", cleanup_ms, result.files_deleted);
        }

        result.elapsed_ms = start.elapsed().as_millis() as u64;
        eprintln!("[timing] Итого: {} мс", result.elapsed_ms);
        Ok(result)
    }

    /// Записать код-файл в БД: метаданные + символы (функции, классы, импорты и т.д.)
    /// skip_delete: при первичной индексации пропускать DELETE (БД пуста, удалять нечего)
    pub fn write_code_to_db(
        &self,
        rel_path: &str,
        content_hash: &str,
        language: &str,
        lines_total: usize,
        ast_hash: &str,
        parse_result: &ParseResult,
        skip_delete: bool,
        mtime: Option<i64>,
        file_size: Option<i64>,
        // Для языков с двойной индексацией (html в v0.7.1) — raw-content,
        // который дополнительно записывается в text_files. Для остальных — None.
        text_for_fts: Option<&str>,
    ) -> Result<()> {
        // Сохраняем запись о файле
        let file_record = FileRecord {
            id: None,
            path: rel_path.to_string(),
            content_hash: content_hash.to_string(),
            ast_hash: Some(ast_hash.to_string()),
            language: language.to_string(),
            lines_total,
            indexed_at: chrono::Utc::now()
                .format("%Y-%m-%d %H:%M:%S")
                .to_string(),
            mtime,
            file_size,
        };
        let file_id = self.storage.upsert_file(&file_record)?;

        // Удаляем старые данные перед вставкой новых
        // При первичной индексации (skip_delete) — пропускаем, БД пуста
        if !skip_delete {
            self.storage.delete_functions_by_file(file_id)?;
            self.storage.delete_classes_by_file(file_id)?;
            self.storage.delete_imports_by_file(file_id)?;
            self.storage.delete_calls_by_file(file_id)?;
            self.storage.delete_variables_by_file(file_id)?;
            // Для языков с двойной индексацией убираем старую запись text_files,
            // чтобы не дублировать при upsert.
            if text_for_fts.is_some() {
                self.storage.delete_text_file_by_file(file_id)?;
            }
        }

        // Конвертируем и сохраняем функции
        let functions: Vec<FunctionRecord> = parse_result
            .functions
            .iter()
            .map(|f| FunctionRecord {
                id: None,
                file_id,
                name: f.name.clone(),
                qualified_name: f.qualified_name.clone(),
                line_start: f.line_start,
                line_end: f.line_end,
                args: f.args.clone(),
                return_type: f.return_type.clone(),
                docstring: f.docstring.clone(),
                body: f.body.clone(),
                is_async: f.is_async,
                node_hash: f.node_hash.clone(),
                // Поля переопределения BSL-расширения (для других языков = None)
                override_type: f.override_type.clone(),
                override_target: f.override_target.clone(),
            })
            .collect();
        self.storage.insert_functions(&functions)?;

        // Конвертируем и сохраняем классы
        let classes: Vec<ClassRecord> = parse_result
            .classes
            .iter()
            .map(|c| ClassRecord {
                id: None,
                file_id,
                name: c.name.clone(),
                line_start: c.line_start,
                line_end: c.line_end,
                bases: c.bases.clone(),
                docstring: c.docstring.clone(),
                body: c.body.clone(),
                node_hash: c.node_hash.clone(),
            })
            .collect();
        self.storage.insert_classes(&classes)?;

        // Конвертируем и сохраняем импорты
        let imports: Vec<ImportRecord> = parse_result
            .imports
            .iter()
            .map(|i| ImportRecord {
                id: None,
                file_id,
                module: i.module.clone(),
                name: i.name.clone(),
                alias: i.alias.clone(),
                line: i.line,
                kind: i.kind.clone(),
            })
            .collect();
        self.storage.insert_imports(&imports)?;

        // Конвертируем и сохраняем вызовы функций
        let calls: Vec<CallRecord> = parse_result
            .calls
            .iter()
            .map(|c| CallRecord {
                id: None,
                file_id,
                caller: c.caller.clone(),
                callee: c.callee.clone(),
                line: c.line,
            })
            .collect();
        self.storage.insert_calls(&calls)?;

        // Конвертируем и сохраняем переменные
        let variables: Vec<VariableRecord> = parse_result
            .variables
            .iter()
            .map(|v| VariableRecord {
                id: None,
                file_id,
                name: v.name.clone(),
                value: v.value.clone(),
                line: v.line,
            })
            .collect();
        self.storage.insert_variables(&variables)?;

        // Двойная индексация: для html (и других языков из is_dual_indexed_language)
        // дополнительно сохраняем сырой контент в text_files, чтобы продолжали
        // работать search_text/grep_text/read_file как раньше.
        if let Some(content) = text_for_fts {
            self.storage.insert_text_file(&crate::storage::models::TextFileRecord {
                id: None,
                file_id,
                content: content.to_string(),
            })?;
        }

        Ok(())
    }

    /// Записать текстовый файл в БД: метаданные + полное содержимое для FTS
    pub fn write_text_to_db(
        &self,
        rel_path: &str,
        content_hash: &str,
        lines_total: usize,
        content: &str,
        skip_delete: bool,
        mtime: Option<i64>,
        file_size: Option<i64>,
    ) -> Result<()> {
        let file_record = FileRecord {
            id: None,
            path: rel_path.to_string(),
            content_hash: content_hash.to_string(),
            ast_hash: None,
            language: "text".to_string(),
            lines_total,
            indexed_at: chrono::Utc::now()
                .format("%Y-%m-%d %H:%M:%S")
                .to_string(),
            mtime,
            file_size,
        };
        let file_id = self.storage.upsert_file(&file_record)?;

        // Удаляем старую запись текстового файла и вставляем новую
        if !skip_delete {
            self.storage.delete_text_file_by_file(file_id)?;
        }

        let text_record = TextFileRecord {
            id: None,
            file_id,
            content: content.to_string(),
        };
        self.storage.insert_text_file(&text_record)?;

        Ok(())
    }

    /// Первый проход: обойти директорию, собрать список файлов для индексации.
    ///
    /// Трёхфазный сбор:
    /// 1a. WalkDir — быстрый обход, собрать пути + metadata (mtime/size) без чтения содержимого
    /// 1b. mtime/size pre-filter — пропустить файлы, где mtime+size совпадают с БД
    /// 1c. rayon par_iter — параллельное чтение + SHA-256 хеш только изменённых файлов
    /// 1d. hash comparison — пропустить файлы с неизменённым хешем, собрать metadata_updates
    ///
    /// Возвращает (candidates, seen_paths, metadata_updates).
    /// seen_paths используется для очистки удалённых файлов без повторного обхода дерева.
    /// metadata_updates содержит файлы, у которых хеш не изменился, но mtime/size обновились.
    fn collect_candidates(
        &self,
        root: &Path,
        force: bool,
        existing_files: &HashMap<String, (i64, String, Option<i64>, Option<i64>)>,
        result: &mut IndexResult,
    ) -> Result<(Vec<(String, String, String, FileCategory, i64, i64)>, HashSet<String>, Vec<(String, i64, i64)>)> {
        let config_for_filter = self.config.clone();
        let file_matcher = self.config.build_file_exclude_matcher();

        // ── Фаза 1a: WalkDir — собрать пути + metadata (без чтения содержимого) ──
        let walker = WalkDir::new(root).into_iter().filter_entry(move |e| {
            if e.file_type().is_dir() {
                if let Some(name) = e.file_name().to_str() {
                    return !config_for_filter.is_excluded_dir(name);
                }
            }
            true
        });

        struct FileEntry {
            abs_path: std::path::PathBuf,
            rel_path: String,
            category: FileCategory,
            mtime: i64,
            file_size: i64,
        }
        let mut entries: Vec<FileEntry> = Vec::new();
        let mut seen_paths: HashSet<String> = HashSet::new();

        for entry in walker.filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() {
                continue;
            }

            // Проверяем лимит количества файлов (0 = без лимита)
            if self.config.max_files > 0 && result.files_scanned >= self.config.max_files {
                break;
            }

            let path = entry.path();

            // Проверяем exclude_file_patterns по имени файла
            if let Some(fname) = path.file_name().and_then(|f| f.to_str()) {
                if file_matcher.is_match(fname) {
                    continue;
                }
            }

            let category = categorize_file(path);

            if matches!(category, FileCategory::Binary) {
                continue;
            }

            // Получаем метаданные для всех файлов
            let meta = entry.metadata().ok();

            // Лимит размера — только для текстовых файлов, код индексируем всегда
            if !matches!(category, FileCategory::Code(_)) {
                if let Some(ref m) = meta {
                    if m.len() as usize > self.config.max_file_size {
                        result.files_skipped += 1;
                        continue;
                    }
                }
            }

            // mtime и file_size для быстрой проверки изменений
            let mtime = meta.as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let file_size_val = meta.as_ref().map(|m| m.len() as i64).unwrap_or(0);

            let rel_path = path
                .strip_prefix(root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");

            result.files_scanned += 1;
            seen_paths.insert(rel_path.clone());
            entries.push(FileEntry {
                abs_path: path.to_path_buf(),
                rel_path,
                category,
                mtime,
                file_size: file_size_val,
            });
        }

        // ── Фаза 1b: быстрая фильтрация по mtime+size (без чтения файлов) ──
        let (entries_to_read, mtime_skipped): (Vec<&FileEntry>, usize) = if force {
            (entries.iter().collect(), 0)
        } else {
            let mut to_read = Vec::new();
            let mut skipped = 0usize;
            for entry in &entries {
                match existing_files.get(&entry.rel_path) {
                    Some((_, _, Some(stored_mtime), Some(stored_size)))
                        if *stored_mtime == entry.mtime && *stored_size == entry.file_size =>
                    {
                        skipped += 1;
                    }
                    _ => to_read.push(entry),
                }
            }
            (to_read, skipped)
        };
        result.files_skipped += mtime_skipped;

        // ── Фаза 1c: параллельное чтение + хеш изменённых файлов (rayon) ────
        let read_results: Vec<_> = entries_to_read
            .par_iter()
            .map(|entry| {
                match hasher::file_hash(&entry.abs_path) {
                    Ok((content, hash)) => Ok((entry.rel_path.clone(), content, hash, entry.category.clone(), entry.mtime, entry.file_size)),
                    Err(e) => Err((entry.rel_path.clone(), e.to_string())),
                }
            })
            .collect();

        // ── Фаза 1d: фильтрация по hash + metadata-only updates ────────────
        let mut candidates = Vec::new();
        let mut metadata_updates: Vec<(String, i64, i64)> = Vec::new();
        for item in read_results {
            match item {
                Ok((rel_path, content, hash, category, mtime, file_size)) => {
                    if !force {
                        if let Some((_, existing_hash, _, _)) = existing_files.get(&rel_path) {
                            if *existing_hash == hash {
                                // Содержимое не изменилось, но mtime/size мог — обновим метаданные
                                metadata_updates.push((rel_path, mtime, file_size));
                                result.files_skipped += 1;
                                continue;
                            }
                        }
                    }
                    candidates.push((rel_path, content, hash, category, mtime, file_size));
                }
                Err((rel_path, error)) => {
                    result.errors.push((rel_path, error));
                }
            }
        }

        Ok((candidates, seen_paths, metadata_updates))
    }
}

/// Standalone-версия collect_candidates — не требует Storage.
/// Трёхфазный сбор: WalkDir (пути+metadata) → mtime/size pre-filter → rayon (read+hash).
/// Возвращает (candidates, files_scanned, files_skipped, errors, seen_paths, metadata_updates).
pub fn collect_candidates_standalone(
    root: &Path,
    config: &IndexConfig,
    force: bool,
    existing_files: &HashMap<String, (i64, String, Option<i64>, Option<i64>)>,
) -> Result<(Vec<(String, String, String, FileCategory, i64, i64)>, usize, usize, Vec<(String, String)>, HashSet<String>, Vec<(String, i64, i64)>)> {
    let mut files_scanned = 0usize;
    let mut files_skipped = 0usize;
    let config_for_filter = config.clone();
    let file_matcher = config.build_file_exclude_matcher();

    // ── Фаза 1a: WalkDir — собрать пути + metadata (без чтения содержимого) ──
    struct FileEntry {
        abs_path: std::path::PathBuf,
        rel_path: String,
        category: FileCategory,
        mtime: i64,
        file_size: i64,
    }
    let mut entries: Vec<FileEntry> = Vec::new();
    let mut seen_paths: HashSet<String> = HashSet::new();

    let walker = WalkDir::new(root).into_iter().filter_entry(move |e| {
        if e.file_type().is_dir() {
            if let Some(name) = e.file_name().to_str() {
                return !config_for_filter.is_excluded_dir(name);
            }
        }
        true
    });

    for entry in walker.filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }

        if config.max_files > 0 && files_scanned >= config.max_files {
            break;
        }

        let path = entry.path();

        // Проверяем exclude_file_patterns по имени файла
        if let Some(fname) = path.file_name().and_then(|f| f.to_str()) {
            if file_matcher.is_match(fname) {
                continue;
            }
        }

        let category = categorize_file(path);

        if matches!(category, FileCategory::Binary) {
            continue;
        }

        // Получаем метаданные для всех файлов
        let meta = entry.metadata().ok();

        // Лимит размера — только для текстовых файлов, код индексируем всегда
        if !matches!(category, FileCategory::Code(_)) {
            if let Some(ref m) = meta {
                if m.len() as usize > config.max_file_size {
                    files_skipped += 1;
                    continue;
                }
            }
        }

        // mtime и file_size для быстрой проверки изменений
        let mtime = meta.as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let file_size_val = meta.as_ref().map(|m| m.len() as i64).unwrap_or(0);

        let rel_path = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");

        files_scanned += 1;
        seen_paths.insert(rel_path.clone());
        entries.push(FileEntry {
            abs_path: path.to_path_buf(),
            rel_path,
            category,
            mtime,
            file_size: file_size_val,
        });
    }

    // ── Фаза 1b: быстрая фильтрация по mtime+size (без чтения файлов) ──
    let (entries_to_read, mtime_skipped): (Vec<&FileEntry>, usize) = if force {
        (entries.iter().collect(), 0)
    } else {
        let mut to_read = Vec::new();
        let mut skipped = 0usize;
        for entry in &entries {
            match existing_files.get(&entry.rel_path) {
                Some((_, _, Some(stored_mtime), Some(stored_size)))
                    if *stored_mtime == entry.mtime && *stored_size == entry.file_size =>
                {
                    skipped += 1;
                }
                _ => to_read.push(entry),
            }
        }
        (to_read, skipped)
    };
    files_skipped += mtime_skipped;

    // ── Фаза 1c: параллельное чтение + хеш изменённых файлов (rayon) ────
    let read_results: Vec<_> = entries_to_read
        .par_iter()
        .map(|entry| {
            match hasher::file_hash(&entry.abs_path) {
                Ok((content, hash)) => Ok((entry.rel_path.clone(), content, hash, entry.category.clone(), entry.mtime, entry.file_size)),
                Err(e) => Err((entry.rel_path.clone(), e.to_string())),
            }
        })
        .collect();

    // ── Фаза 1d: фильтрация по hash + metadata-only updates ────────────
    let mut candidates = Vec::new();
    let mut errors = Vec::new();
    let mut metadata_updates: Vec<(String, i64, i64)> = Vec::new();
    for item in read_results {
        match item {
            Ok((rel_path, content, hash, category, mtime, file_size)) => {
                if !force {
                    if let Some((_, existing_hash, _, _)) = existing_files.get(&rel_path) {
                        if *existing_hash == hash {
                            // Содержимое не изменилось, но mtime/size мог — обновим метаданные
                            metadata_updates.push((rel_path, mtime, file_size));
                            files_skipped += 1;
                            continue;
                        }
                    }
                }
                candidates.push((rel_path, content, hash, category, mtime, file_size));
            }
            Err((rel_path, error)) => {
                errors.push((rel_path, error));
            }
        }
    }

    Ok((candidates, files_scanned, files_skipped, errors, seen_paths, metadata_updates))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_categorize_file() {
        assert_eq!(
            file_types::categorize_file(Path::new("test.py")),
            FileCategory::Code("python".to_string())
        );
        assert_eq!(
            file_types::categorize_file(Path::new("readme.md")),
            FileCategory::Text
        );
        assert_eq!(
            file_types::categorize_file(Path::new("image.png")),
            FileCategory::Binary
        );
    }

    #[test]
    fn test_full_reindex() {
        let tmp = TempDir::new().unwrap();

        // Создаём Python-файл с функцией и классом
        fs::write(
            tmp.path().join("main.py"),
            r#"
def hello():
    """Приветствие."""
    print("Hello!")

class App:
    def run(self):
        pass
"#,
        )
        .unwrap();

        // Создаём текстовый файл
        fs::write(tmp.path().join("readme.md"), "# Project\nDescription").unwrap();

        let mut storage = Storage::open_in_memory().unwrap();
        let mut indexer = Indexer::new(&mut storage);
        let result = indexer.full_reindex(tmp.path(), false).unwrap();

        assert_eq!(result.files_indexed, 2, "оба файла должны быть проиндексированы");
        assert_eq!(result.files_skipped, 0, "пропущенных файлов быть не должно");
        assert_eq!(result.errors.len(), 0, "ошибок быть не должно");

        // Проверяем, что данные сохранились в БД
        let stats = storage.get_stats().unwrap();
        assert!(stats.total_functions >= 2, "минимум 2 функции: hello + run");
        assert!(stats.total_classes >= 1, "минимум 1 класс: App");
        assert!(stats.total_text_files >= 1, "минимум 1 текстовый файл: readme.md");
    }

    #[test]
    fn test_reindex_skips_unchanged() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("test.py"), "def foo():\n    pass\n").unwrap();

        let mut storage = Storage::open_in_memory().unwrap();

        // Первая индексация
        {
            let mut indexer = Indexer::new(&mut storage);
            let r1 = indexer.full_reindex(tmp.path(), false).unwrap();
            assert_eq!(r1.files_indexed, 1, "первый проход должен проиндексировать файл");
        }

        // Второй проход без изменений — файл должен быть пропущен
        {
            let mut indexer = Indexer::new(&mut storage);
            let r2 = indexer.full_reindex(tmp.path(), false).unwrap();
            assert_eq!(r2.files_indexed, 0, "повторная индексация не должна записывать файл");
            assert_eq!(r2.files_skipped, 1, "файл должен быть пропущен как неизменённый");
        }
    }

    #[test]
    fn test_reindex_force_reindexes() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("test.py"), "def foo():\n    pass\n").unwrap();

        let mut storage = Storage::open_in_memory().unwrap();

        {
            let mut indexer = Indexer::new(&mut storage);
            indexer.full_reindex(tmp.path(), false).unwrap();
        }

        // Force-режим — файл должен быть переиндексирован, даже если не изменился
        {
            let mut indexer = Indexer::new(&mut storage);
            let r = indexer.full_reindex(tmp.path(), true).unwrap();
            assert_eq!(r.files_indexed, 1, "force=true должен переиндексировать файл");
            assert_eq!(r.files_skipped, 0, "при force=true пропущенных быть не должно");
        }
    }

    #[test]
    fn test_deleted_files_removed_from_db() {
        let tmp = TempDir::new().unwrap();
        let py_path = tmp.path().join("temp.py");
        fs::write(&py_path, "def bar():\n    pass\n").unwrap();

        let mut storage = Storage::open_in_memory().unwrap();

        // Индексируем файл
        {
            let mut indexer = Indexer::new(&mut storage);
            let r = indexer.full_reindex(tmp.path(), false).unwrap();
            assert_eq!(r.files_indexed, 1);
        }

        // Удаляем файл с диска
        fs::remove_file(&py_path).unwrap();

        // Повторная индексация — запись должна исчезнуть из БД
        {
            let mut indexer = Indexer::new(&mut storage);
            let r = indexer.full_reindex(tmp.path(), false).unwrap();
            assert_eq!(r.files_deleted, 1, "удалённый файл должен быть убран из БД");
        }

        let stats = storage.get_stats().unwrap();
        assert_eq!(stats.total_files, 0, "БД должна быть пуста после удаления файла");
    }

    #[test]
    fn test_excludes_binary_files() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("test.py"), "x = 1\n").unwrap();
        // Бинарный файл — не должен попасть в индекс
        fs::write(tmp.path().join("image.png"), b"\x89PNG\r\n\x1a\n").unwrap();

        let mut storage = Storage::open_in_memory().unwrap();
        let mut indexer = Indexer::new(&mut storage);
        let r = indexer.full_reindex(tmp.path(), false).unwrap();

        // Только Python-файл проиндексирован, PNG пропущен (бинарный)
        assert_eq!(r.files_scanned, 1, "бинарные файлы не должны попасть в files_scanned");
        assert_eq!(r.files_indexed, 1);
    }

    #[test]
    fn test_excludes_target_dir() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("target")).unwrap();
        fs::write(tmp.path().join("target").join("debug.py"), "x = 1\n").unwrap();
        fs::write(tmp.path().join("main.py"), "y = 2\n").unwrap();

        let mut storage = Storage::open_in_memory().unwrap();
        let mut indexer = Indexer::new(&mut storage);
        let r = indexer.full_reindex(tmp.path(), false).unwrap();

        // Файл в target/ должен быть исключён
        assert_eq!(r.files_indexed, 1, "только main.py должен быть проиндексирован");
    }

    #[test]
    fn test_hasher_deterministic() {
        let hash1 = hasher::content_hash(b"hello world");
        let hash2 = hasher::content_hash(b"hello world");
        assert_eq!(hash1, hash2, "хеш должен быть детерминированным");

        let hash3 = hasher::content_hash(b"different content");
        assert_ne!(hash1, hash3, "разные данные дают разные хеши");
    }

    #[test]
    fn test_with_config_custom_exclude() {
        let tmp = TempDir::new().unwrap();
        // Создаём директорию vendor с файлом
        fs::create_dir(tmp.path().join("vendor")).unwrap();
        fs::write(tmp.path().join("vendor").join("lib.py"), "x = 1\n").unwrap();
        // Основной файл проекта
        fs::write(tmp.path().join("app.py"), "y = 2\n").unwrap();

        let mut storage = Storage::open_in_memory().unwrap();
        let config = IndexConfig {
            exclude_dirs: vec!["vendor".to_string()],
            ..Default::default()
        };
        let mut indexer = Indexer::with_config(&mut storage, config);
        let r = indexer.full_reindex(tmp.path(), false).unwrap();

        // vendor/ исключён через конфиг — только app.py
        assert_eq!(r.files_indexed, 1, "vendor должен быть исключён через конфиг");
    }

    #[test]
    fn test_bulk_load_mode() {
        let tmp = TempDir::new().unwrap();

        // Создаём 15 Python-файлов с уникальными функциями
        for i in 0..15 {
            fs::write(
                tmp.path().join(format!("module_{i}.py")),
                format!(
                    "def func_{i}(x):\n    \"\"\"Функция номер {i}.\"\"\"\n    return x + {i}\n"
                ),
            )
            .unwrap();
        }

        let mut storage = Storage::open_in_memory().unwrap();

        // Устанавливаем порог 10 — при 15 файлах должен включиться bulk-load
        let config = IndexConfig {
            bulk_threshold: 10,
            ..Default::default()
        };

        // Первый проход: индексируем все 15 файлов в bulk-load режиме
        {
            let mut indexer = Indexer::with_config(&mut storage, config.clone());
            let result = indexer.full_reindex(tmp.path(), false).unwrap();
            assert_eq!(result.files_indexed, 15, "все 15 файлов должны быть проиндексированы");
            assert_eq!(result.files_skipped, 0, "пропущенных файлов быть не должно");
            assert_eq!(result.errors.len(), 0, "ошибок быть не должно");
        }

        // Проверяем статистику в БД (indexer уже дропнут)
        let stats = storage.get_stats().unwrap();
        assert_eq!(stats.total_files, 15, "в БД должно быть 15 файлов");
        assert_eq!(stats.total_functions, 15, "по одной функции на файл");

        // Проверяем, что FTS работает после rebuild
        let found = storage.search_functions("func_0", 10, None).unwrap();
        assert!(!found.is_empty(), "FTS должен находить func_0 после bulk-load rebuild");

        let found_5 = storage.search_functions("func_5", 10, None).unwrap();
        assert!(!found_5.is_empty(), "FTS должен находить func_5 после bulk-load rebuild");

        // Второй проход: повторная индексация — все файлы должны быть пропущены
        {
            let mut indexer = Indexer::with_config(&mut storage, config);
            let result2 = indexer.full_reindex(tmp.path(), false).unwrap();
            assert_eq!(result2.files_skipped, 15, "при повторной индексации все файлы неизменны");
            assert_eq!(result2.files_indexed, 0, "ни одного файла не должно быть переиндексировано");
        }
    }

    #[test]
    fn test_with_config_max_file_size() {
        let tmp = TempDir::new().unwrap();
        // Маленький текстовый файл — пройдёт
        fs::write(tmp.path().join("small.txt"), "x = 1\n").unwrap();
        // Большой текстовый файл — пропустим (лимит 10 байт)
        // Лимит max_file_size действует только на Text-файлы, код индексируется всегда
        fs::write(tmp.path().join("big.txt"), "y = 'a very long string that exceeds limit'\n").unwrap();
        // Большой код-файл — НЕ пропускается (код индексируется независимо от размера)
        fs::write(tmp.path().join("big.py"), "y = 'a very long string that exceeds limit'\n").unwrap();

        let mut storage = Storage::open_in_memory().unwrap();
        let config = IndexConfig {
            max_file_size: 10, // 10 байт
            ..Default::default()
        };
        let mut indexer = Indexer::with_config(&mut storage, config);
        let r = indexer.full_reindex(tmp.path(), false).unwrap();

        // big.txt пропущен из-за лимита размера, big.py — нет (код не ограничен)
        assert_eq!(r.files_indexed, 2, "small.txt + big.py (код не ограничен размером)");
        assert_eq!(r.files_skipped, 1, "big.txt пропущен по размеру");
    }

    #[test]
    fn test_batch_transactions() {
        let tmp = TempDir::new().unwrap();

        // Создаём 20 Python-файлов с уникальными функциями
        for i in 0..20 {
            fs::write(
                tmp.path().join(format!("module_{i}.py")),
                format!(
                    "def batch_func_{i}(x):\n    \"\"\"Функция батча {i}.\"\"\"\n    return x * {i}\n"
                ),
            )
            .unwrap();
        }

        let mut storage = Storage::open_in_memory().unwrap();

        // Устанавливаем маленький batch_size = 5, чтобы проверить несколько коммитов
        let config = IndexConfig {
            batch_size: 5,
            bulk_threshold: 100, // отключаем bulk-mode, чтобы проверять именно батч-транзакции
            ..Default::default()
        };

        let result = {
            let mut indexer = Indexer::with_config(&mut storage, config);
            indexer.full_reindex(tmp.path(), false).unwrap()
        };

        // Все 20 файлов должны быть успешно проиндексированы
        assert_eq!(result.files_indexed, 20, "все 20 файлов должны быть проиндексированы");
        assert_eq!(result.files_skipped, 0, "пропущенных файлов быть не должно");
        assert_eq!(result.errors.len(), 0, "ошибок быть не должно");

        // Данные реально записаны в БД — проверяем через get_stats
        let stats = storage.get_stats().unwrap();
        assert_eq!(stats.total_files, 20, "в БД должно быть 20 файлов");
        assert_eq!(stats.total_functions, 20, "по одной функции на файл");

        // FTS должен находить функции
        let found = storage.search_functions("batch_func_0", 10, None).unwrap();
        assert!(!found.is_empty(), "FTS должен находить batch_func_0");

        let found_19 = storage.search_functions("batch_func_19", 10, None).unwrap();
        assert!(!found_19.is_empty(), "FTS должен находить batch_func_19 (последний батч)");
    }

    #[test]
    fn test_parallel_reindex() {
        let tmp = TempDir::new().unwrap();

        // Создаём 30 Python-файлов с разными функциями
        for i in 0..30 {
            fs::write(
                tmp.path().join(format!("parallel_{i}.py")),
                format!(
                    "def parallel_func_{i}(a, b):\n    \"\"\"Параллельная функция {i}.\"\"\"\n    return a + b + {i}\n\ndef helper_{i}(x):\n    return x * {i}\n"
                ),
            )
            .unwrap();
        }

        let mut storage = Storage::open_in_memory().unwrap();
        let mut indexer = Indexer::new(&mut storage);
        let result = indexer.full_reindex(tmp.path(), false).unwrap();

        // Все 30 файлов проиндексированы
        assert_eq!(result.files_indexed, 30, "все 30 файлов должны быть проиндексированы");
        assert_eq!(result.files_skipped, 0, "пропущенных файлов быть не должно");
        assert_eq!(result.errors.len(), 0, "ошибок при параллельном парсинге быть не должно");

        // Проверяем что все функции на месте (по 2 на файл = 60 итого)
        let stats = storage.get_stats().unwrap();
        assert_eq!(stats.total_files, 30, "в БД должно быть 30 файлов");
        assert_eq!(stats.total_functions, 60, "по 2 функции на файл = 60 итого");

        // FTS находит функции из разных файлов (порядок парсинга не важен)
        let found_0 = storage.search_functions("parallel_func_0", 10, None).unwrap();
        assert!(!found_0.is_empty(), "FTS должен находить parallel_func_0");

        let found_15 = storage.search_functions("parallel_func_15", 10, None).unwrap();
        assert!(!found_15.is_empty(), "FTS должен находить parallel_func_15");

        let found_29 = storage.search_functions("parallel_func_29", 10, None).unwrap();
        assert!(!found_29.is_empty(), "FTS должен находить parallel_func_29");

        // helper-функции тоже проиндексированы
        let found_helper = storage.search_functions("helper_0", 10, None).unwrap();
        assert!(!found_helper.is_empty(), "FTS должен находить helper_0");
    }

    /// Тест: первичная индексация пустой БД в bulk-режиме.
    ///
    /// Проверяет, что при is_fresh_db=true + bulk_mode=true:
    /// - все файлы проиндексированы корректно
    /// - FTS-поиск работает после rebuild индексов
    /// - повторная индексация пропускает все неизменённые файлы
    #[test]
    fn test_bulk_fresh_db() {
        let tmp = TempDir::new().unwrap();

        // Создаём 20 Python-файлов с уникальными функциями
        for i in 0..20 {
            fs::write(
                tmp.path().join(format!("fresh_{i}.py")),
                format!(
                    "def fresh_func_{i}(x):\n    \"\"\"Свежая функция {i}.\"\"\"\n    return x + {i}\n"
                ),
            )
            .unwrap();
        }

        // Порог bulk_threshold=5 — при 20 файлах гарантированно активируется bulk-режим
        let config = IndexConfig {
            bulk_threshold: 5,
            ..Default::default()
        };

        let mut storage = Storage::open_in_memory().unwrap();

        // Первичная индексация пустой БД (is_fresh_db = true)
        let result = {
            let mut indexer = Indexer::with_config(&mut storage, config.clone());
            indexer.full_reindex(tmp.path(), false).unwrap()
        };

        assert_eq!(result.files_indexed, 20, "все 20 файлов должны быть проиндексированы");
        assert_eq!(result.files_skipped, 0, "пропущенных файлов быть не должно");
        assert_eq!(result.errors.len(), 0, "ошибок быть не должно");

        // Проверяем статистику
        let stats = storage.get_stats().unwrap();
        assert_eq!(stats.total_files, 20, "в БД должно быть 20 файлов");
        assert_eq!(stats.total_functions, 20, "по одной функции на файл");

        // Проверяем FTS-поиск после bulk rebuild
        let found_0 = storage.search_functions("fresh_func_0", 10, None).unwrap();
        assert!(!found_0.is_empty(), "FTS должен находить fresh_func_0 после bulk-load rebuild");

        let found_19 = storage.search_functions("fresh_func_19", 10, None).unwrap();
        assert!(!found_19.is_empty(), "FTS должен находить fresh_func_19 после bulk-load rebuild");

        // Повторная индексация (is_fresh_db = false) — все файлы должны быть пропущены
        let result2 = {
            let mut indexer = Indexer::with_config(&mut storage, config);
            indexer.full_reindex(tmp.path(), false).unwrap()
        };

        assert_eq!(result2.files_skipped, 20, "при повторной индексации все 20 файлов неизменны");
        assert_eq!(result2.files_indexed, 0, "ни одного файла не должно быть переиндексировано");

        // FTS по-прежнему работает после повторного прохода
        let found_after = storage.search_functions("fresh_func_10", 10, None).unwrap();
        assert!(!found_after.is_empty(), "FTS должен работать и после повторной индексации");
    }
}
