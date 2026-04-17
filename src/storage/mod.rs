/// Модуль хранилища — SQLite через rusqlite (bundled)
pub mod memory;
pub mod models;
pub mod schema;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OpenFlags};
use std::path::Path;

use models::*;

/// Зарегистрировать scalar-функцию REGEXP для поддержки оператора REGEXP в SQL.
/// Использует crate `regex` — никаких внешних расширений SQLite не нужно.
/// Кеширует скомпилированный Regex через RefCell — компиляция один раз за запрос.
fn register_regexp(conn: &Connection) -> Result<()> {
    use rusqlite::functions::FunctionFlags;
    use std::cell::RefCell;

    // Кеш: (паттерн, скомпилированный Regex)
    let cache: RefCell<Option<(String, regex::Regex)>> = RefCell::new(None);

    conn.create_scalar_function(
        "regexp",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        move |ctx| {
            let pattern: String = ctx.get(0)?;
            let text: String = ctx.get(1)?;

            let mut cached = cache.borrow_mut();
            let re = match cached.as_ref() {
                Some((p, re)) if *p == pattern => re,
                _ => {
                    let new_re = regex::Regex::new(&pattern)
                        .map_err(|e| rusqlite::Error::InvalidParameterName(e.to_string()))?;
                    *cached = Some((pattern, new_re));
                    &cached.as_ref().unwrap().1
                }
            };
            Ok(re.is_match(&text))
        },
    )
    .context("Не удалось зарегистрировать REGEXP")?;
    Ok(())
}

/// Основная структура хранилища — обёртка над SQLite-соединением
pub struct Storage {
    conn: Connection,
}

impl Storage {
    // ── Конструкторы ────────────────────────────────────────────────────────

    /// Открыть (или создать) файловую базу данных
    pub fn open_file(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("Не удалось открыть БД: {}", path.display()))?;
        schema::initialize(&conn).context("Ошибка инициализации схемы БД")?;
        register_regexp(&conn)?;
        Ok(Self { conn })
    }

    /// Открыть БД только для чтения — не пишет в БД, не блокирует.
    /// Используется CLI-командами для параллельной работы с MCP-демоном.
    pub fn open_file_readonly(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_URI,
        )
        .with_context(|| format!("Не удалось открыть БД (readonly): {}", path.display()))?;
        schema::initialize_readonly(&conn).context("Ошибка инициализации readonly-схемы")?;
        register_regexp(&conn)?;
        Ok(Self { conn })
    }

    /// Открыть базу данных в памяти (используется в тестах)
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("Не удалось создать in-memory БД")?;
        schema::initialize(&conn).context("Ошибка инициализации схемы in-memory БД")?;
        register_regexp(&conn)?;
        Ok(Self { conn })
    }

    /// Открыть хранилище с автоопределением режима (in-memory или disk).
    ///
    /// Если выбран режим InMemory и файл БД существует — данные загружаются
    /// из файла в память через SQLite Backup API. Если файл не существует —
    /// создаётся чистая in-memory БД.
    pub fn open_auto(db_path: &Path, storage_config: &memory::StorageConfig) -> Result<Self> {
        let mode = memory::determine_storage_mode(storage_config, db_path);

        match mode {
            memory::StorageMode::InMemory => {
                eprintln!("[storage] Режим: in-memory (БД загружена в RAM)");

                if db_path.exists() {
                    // Загрузить данные с диска в память через backup API
                    let disk_conn = Connection::open(db_path)
                        .with_context(|| format!("Не удалось открыть файл БД: {}", db_path.display()))?;
                    let mut memory_conn = Connection::open_in_memory()
                        .context("Не удалось создать in-memory БД")?;

                    // Копируем disk → memory (Backup::new(src, &mut dst))
                    {
                        let backup = rusqlite::backup::Backup::new(&disk_conn, &mut memory_conn)
                            .context("Не удалось инициализировать backup disk→memory")?;
                        backup
                            .run_to_completion(100, std::time::Duration::from_millis(0), None)
                            .context("Ошибка при копировании БД disk→memory")?;
                    }

                    // Миграции для существующей БД, загруженной в память
                    schema::migrate_v2(&memory_conn)
                        .context("Ошибка миграции v2 (in-memory)")?;
                    schema::migrate_v3(&memory_conn)
                        .context("Ошибка миграции v3 (in-memory)")?;
                    register_regexp(&memory_conn)?;
                    Ok(Self { conn: memory_conn })
                } else {
                    // Новая БД — чистая in-memory со схемой
                    Self::open_in_memory()
                }
            }
            memory::StorageMode::Disk => {
                eprintln!("[storage] Режим: disk (WAL)");
                Self::open_file(db_path)
            }
        }
    }

    /// Сохранить содержимое in-memory БД на диск.
    ///
    /// Используется после индексации в режиме InMemory, чтобы персистировать
    /// результаты. Безопасно вызывать и для disk-режима (создаст копию файла).
    pub fn flush_to_disk(&self, db_path: &Path) -> Result<()> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Не удалось создать директорию: {}", parent.display()))?;
        }
        // Connection::backup() открывает dst сам и не требует &mut dst
        self.conn
            .backup(rusqlite::MAIN_DB, db_path, None)
            .with_context(|| format!("Ошибка flush_to_disk: {}", db_path.display()))?;
        Ok(())
    }

    /// Принудительно выполнить checkpoint WAL с усечением файла до минимума.
    ///
    /// Используется в disk-режиме после bulk-операций (initial reindex, крупные
    /// batch'и watcher'а), где `PRAGMA wal_autocheckpoint=500` не успевает
    /// физически уменьшать WAL-файл — он только переносит страницы в основную БД,
    /// но сам файл WAL не truncate'ится.
    ///
    /// Возвращает (busy, log_pages, checkpointed_pages) — стандартный вывод
    /// SQLite `PRAGMA wal_checkpoint(TRUNCATE)`. В штатной работе интересен
    /// только busy=0 (успех); log_pages/checkpointed_pages — для диагностики.
    pub fn checkpoint_truncate(&self) -> Result<(i64, i64, i64)> {
        self.conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE);", [], |row| {
                let busy: i64 = row.get(0)?;
                let log_pages: i64 = row.get(1)?;
                let checkpointed: i64 = row.get(2)?;
                Ok((busy, log_pages, checkpointed))
            })
            .context("PRAGMA wal_checkpoint(TRUNCATE) failed")
    }

    // ── Files ────────────────────────────────────────────────────────────────

    /// Вставить или обновить запись файла; возвращает id строки
    pub fn upsert_file(&self, record: &FileRecord) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO files (path, content_hash, ast_hash, language, lines_total, indexed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(path) DO UPDATE SET
                 content_hash = excluded.content_hash,
                 ast_hash     = excluded.ast_hash,
                 language     = excluded.language,
                 lines_total  = excluded.lines_total,
                 indexed_at   = excluded.indexed_at",
            params![
                record.path,
                record.content_hash,
                record.ast_hash,
                record.language,
                record.lines_total as i64,
                record.indexed_at,
            ],
        )
        .context("upsert_file: ошибка выполнения запроса")?;

        // Получаем id — либо только что вставленной, либо существующей строки
        let id: i64 = self.conn.query_row(
            "SELECT id FROM files WHERE path = ?1",
            params![record.path],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    /// Получить запись файла по пути
    pub fn get_file_by_path(&self, path: &str) -> Result<Option<FileRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, content_hash, ast_hash, language, lines_total, indexed_at, mtime, file_size
             FROM files WHERE path = ?1",
        )?;
        let result = stmt.query_row(params![path], row_to_file);
        match result {
            Ok(r) => Ok(Some(r)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Получить все файлы в индексе
    pub fn get_all_files(&self) -> Result<Vec<FileRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, content_hash, ast_hash, language, lines_total, indexed_at, mtime, file_size
             FROM files ORDER BY path",
        )?;
        let rows = stmt.query_map([], row_to_file)?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Обновить только mtime и file_size для существующего файла (без перепарсинга)
    pub fn update_file_metadata(&self, path: &str, mtime: i64, file_size: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE files SET mtime = ?1, file_size = ?2 WHERE path = ?3",
            params![mtime, file_size, path],
        )?;
        Ok(())
    }

    /// Удалить файл и все связанные записи (каскадно через FK)
    pub fn delete_file(&self, file_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM files WHERE id = ?1", params![file_id])
            .context("delete_file: ошибка удаления")?;
        Ok(())
    }

    // ── Functions ────────────────────────────────────────────────────────────

    /// Пакетная вставка функций
    pub fn insert_functions(&self, records: &[FunctionRecord]) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "INSERT INTO functions
                 (file_id, name, qualified_name, line_start, line_end,
                  args, return_type, docstring, body, is_async, node_hash,
                  override_type, override_target)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
        )?;
        for r in records {
            stmt.execute(params![
                r.file_id,
                r.name,
                r.qualified_name,
                r.line_start as i64,
                r.line_end as i64,
                r.args,
                r.return_type,
                r.docstring,
                r.body,
                r.is_async as i32,
                r.node_hash,
                r.override_type,
                r.override_target,
            ])
            .context("insert_functions: ошибка вставки строки")?;
        }
        Ok(())
    }

    /// Удалить все функции файла
    pub fn delete_functions_by_file(&self, file_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM functions WHERE file_id = ?1", params![file_id])
            .context("delete_functions_by_file")?;
        Ok(())
    }

    // ── Classes ──────────────────────────────────────────────────────────────

    /// Пакетная вставка классов
    pub fn insert_classes(&self, records: &[ClassRecord]) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "INSERT INTO classes
                 (file_id, name, line_start, line_end, bases, docstring, body, node_hash)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        )?;
        for r in records {
            stmt.execute(params![
                r.file_id,
                r.name,
                r.line_start as i64,
                r.line_end as i64,
                r.bases,
                r.docstring,
                r.body,
                r.node_hash,
            ])
            .context("insert_classes: ошибка вставки строки")?;
        }
        Ok(())
    }

    /// Удалить все классы файла
    pub fn delete_classes_by_file(&self, file_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM classes WHERE file_id = ?1", params![file_id])
            .context("delete_classes_by_file")?;
        Ok(())
    }

    // ── Imports ──────────────────────────────────────────────────────────────

    /// Пакетная вставка импортов
    pub fn insert_imports(&self, records: &[ImportRecord]) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "INSERT INTO imports (file_id, module, name, alias, line, kind)
             VALUES (?1,?2,?3,?4,?5,?6)",
        )?;
        for r in records {
            stmt.execute(params![
                r.file_id,
                r.module,
                r.name,
                r.alias,
                r.line as i64,
                r.kind,
            ])
            .context("insert_imports: ошибка вставки строки")?;
        }
        Ok(())
    }

    /// Удалить все импорты файла
    pub fn delete_imports_by_file(&self, file_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM imports WHERE file_id = ?1", params![file_id])
            .context("delete_imports_by_file")?;
        Ok(())
    }

    // ── Calls ────────────────────────────────────────────────────────────────

    /// Пакетная вставка вызовов
    pub fn insert_calls(&self, records: &[CallRecord]) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "INSERT INTO calls (file_id, caller, callee, line) VALUES (?1,?2,?3,?4)",
        )?;
        for r in records {
            stmt.execute(params![r.file_id, r.caller, r.callee, r.line as i64])
                .context("insert_calls: ошибка вставки строки")?;
        }
        Ok(())
    }

    /// Удалить все вызовы файла
    pub fn delete_calls_by_file(&self, file_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM calls WHERE file_id = ?1", params![file_id])
            .context("delete_calls_by_file")?;
        Ok(())
    }

    // ── Variables ────────────────────────────────────────────────────────────

    /// Пакетная вставка переменных
    pub fn insert_variables(&self, records: &[VariableRecord]) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "INSERT INTO variables (file_id, name, value, line) VALUES (?1,?2,?3,?4)",
        )?;
        for r in records {
            stmt.execute(params![r.file_id, r.name, r.value, r.line as i64])
                .context("insert_variables: ошибка вставки строки")?;
        }
        Ok(())
    }

    /// Удалить все переменные файла
    pub fn delete_variables_by_file(&self, file_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM variables WHERE file_id = ?1", params![file_id])
            .context("delete_variables_by_file")?;
        Ok(())
    }

    // ── Text files ───────────────────────────────────────────────────────────

    /// Вставить запись текстового файла
    pub fn insert_text_file(&self, record: &TextFileRecord) -> Result<()> {
        self.conn.execute(
            "INSERT INTO text_files (file_id, content) VALUES (?1, ?2)",
            params![record.file_id, record.content],
        )
        .context("insert_text_file")?;
        Ok(())
    }

    /// Удалить запись текстового файла
    pub fn delete_text_file_by_file(&self, file_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM text_files WHERE file_id = ?1", params![file_id])
            .context("delete_text_file_by_file")?;
        Ok(())
    }

    // ── Поисковые запросы ────────────────────────────────────────────────────

    /// Полнотекстовый поиск функций через FTS5
    pub fn search_functions(&self, query: &str, limit: usize, language: Option<&str>) -> Result<Vec<FunctionRecord>> {
        let safe_query = sanitize_fts_query(query);
        match language {
            Some(lang) => {
                let mut stmt = self.conn.prepare(
                    "SELECT f.id, f.file_id, f.name, f.qualified_name, f.line_start, f.line_end,
                            f.args, f.return_type, f.docstring, f.body, f.is_async, f.node_hash
                     FROM fts_functions ft
                     JOIN functions f ON f.id = ft.rowid
                     JOIN files fi ON fi.id = f.file_id
                     WHERE fts_functions MATCH ?1 AND fi.language = ?2
                     ORDER BY rank
                     LIMIT ?3",
                )?;
                let rows = stmt.query_map(params![safe_query, lang, limit as i64], row_to_function)?;
                rows.map(|r| r.map_err(Into::into)).collect()
            }
            None => {
                let mut stmt = self.conn.prepare(
                    "SELECT f.id, f.file_id, f.name, f.qualified_name, f.line_start, f.line_end,
                            f.args, f.return_type, f.docstring, f.body, f.is_async, f.node_hash
                     FROM fts_functions ft
                     JOIN functions f ON f.id = ft.rowid
                     WHERE fts_functions MATCH ?1
                     ORDER BY rank
                     LIMIT ?2",
                )?;
                let rows = stmt.query_map(params![safe_query, limit as i64], row_to_function)?;
                rows.map(|r| r.map_err(Into::into)).collect()
            }
        }
    }

    /// Полнотекстовый поиск классов через FTS5
    pub fn search_classes(&self, query: &str, limit: usize, language: Option<&str>) -> Result<Vec<ClassRecord>> {
        let safe_query = sanitize_fts_query(query);
        match language {
            Some(lang) => {
                let mut stmt = self.conn.prepare(
                    "SELECT c.id, c.file_id, c.name, c.line_start, c.line_end,
                            c.bases, c.docstring, c.body, c.node_hash
                     FROM fts_classes ft
                     JOIN classes c ON c.id = ft.rowid
                     JOIN files fi ON fi.id = c.file_id
                     WHERE fts_classes MATCH ?1 AND fi.language = ?2
                     ORDER BY rank
                     LIMIT ?3",
                )?;
                let rows = stmt.query_map(params![safe_query, lang, limit as i64], row_to_class)?;
                rows.map(|r| r.map_err(Into::into)).collect()
            }
            None => {
                let mut stmt = self.conn.prepare(
                    "SELECT c.id, c.file_id, c.name, c.line_start, c.line_end,
                            c.bases, c.docstring, c.body, c.node_hash
                     FROM fts_classes ft
                     JOIN classes c ON c.id = ft.rowid
                     WHERE fts_classes MATCH ?1
                     ORDER BY rank
                     LIMIT ?2",
                )?;
                let rows = stmt.query_map(params![safe_query, limit as i64], row_to_class)?;
                rows.map(|r| r.map_err(Into::into)).collect()
            }
        }
    }

    /// Полнотекстовый поиск по текстовым файлам; возвращает (path, фрагмент контента)
    pub fn search_text(&self, query: &str, limit: usize, language: Option<&str>) -> Result<Vec<(String, String)>> {
        let safe_query = sanitize_fts_query(query);
        match language {
            Some(lang) => {
                let mut stmt = self.conn.prepare(
                    "SELECT fi.path, snippet(fts_text_files, 0, '[', ']', '...', 20)
                     FROM fts_text_files ft
                     JOIN text_files tf ON tf.id = ft.rowid
                     JOIN files fi ON fi.id = tf.file_id
                     WHERE fts_text_files MATCH ?1 AND fi.language = ?2
                     ORDER BY rank
                     LIMIT ?3",
                )?;
                let rows = stmt.query_map(params![safe_query, lang, limit as i64], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?;
                rows.map(|r| r.map_err(Into::into)).collect()
            }
            None => {
                let mut stmt = self.conn.prepare(
                    "SELECT fi.path, snippet(fts_text_files, 0, '[', ']', '...', 20)
                     FROM fts_text_files ft
                     JOIN text_files tf ON tf.id = ft.rowid
                     JOIN files fi ON fi.id = tf.file_id
                     WHERE fts_text_files MATCH ?1
                     ORDER BY rank
                     LIMIT ?2",
                )?;
                let rows = stmt.query_map(params![safe_query, limit as i64], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?;
                rows.map(|r| r.map_err(Into::into)).collect()
            }
        }
    }

    /// Поиск подстроки или regex в телах функций и классов.
    ///
    /// `pattern` — буквальная подстрока (LIKE), `regex_pattern` — регулярное выражение (REGEXP).
    /// Указать одно из двух. Возвращает список совпадений с путём, именем и строками.
    pub fn grep_body(
        &self,
        pattern: Option<&str>,
        regex_pattern: Option<&str>,
        language: Option<&str>,
        limit: usize,
    ) -> Result<Vec<GrepBodyMatch>> {
        // Определяем условие WHERE для body
        let (body_condition, body_param) = match (pattern, regex_pattern) {
            (Some(p), _) => ("body LIKE ?1".to_string(), format!("%{}%", p)),
            (_, Some(r)) => ("body REGEXP ?1".to_string(), r.to_string()),
            _ => anyhow::bail!("Необходимо указать pattern или regex"),
        };

        let sql = match language {
            Some(_) => format!(
                "SELECT fi.path, fn.name, 'function' as kind, fn.line_start, fn.line_end, fn.body
                 FROM functions fn
                 JOIN files fi ON fi.id = fn.file_id
                 WHERE fn.{cond} AND fi.language = ?2
                 UNION ALL
                 SELECT fi.path, c.name, 'class' as kind, c.line_start, c.line_end, c.body
                 FROM classes c
                 JOIN files fi ON fi.id = c.file_id
                 WHERE c.{cond} AND fi.language = ?2
                 ORDER BY 1, 4
                 LIMIT ?3",
                cond = body_condition
            ),
            None => format!(
                "SELECT fi.path, fn.name, 'function' as kind, fn.line_start, fn.line_end, fn.body
                 FROM functions fn
                 JOIN files fi ON fi.id = fn.file_id
                 WHERE fn.{cond}
                 UNION ALL
                 SELECT fi.path, c.name, 'class' as kind, c.line_start, c.line_end, c.body
                 FROM classes c
                 JOIN files fi ON fi.id = c.file_id
                 WHERE c.{cond}
                 ORDER BY 1, 4
                 LIMIT ?2",
                cond = body_condition
            ),
        };

        /// Промежуточный результат SQL-запроса grep_body (с телом для построчного поиска)
        struct GrepBodyRaw {
            file_path: String,
            name: String,
            kind: String,
            line_start: usize,
            line_end: usize,
            body: String,
        }

        let row_mapper = |row: &rusqlite::Row| -> rusqlite::Result<GrepBodyRaw> {
            Ok(GrepBodyRaw {
                file_path: row.get(0)?,
                name: row.get(1)?,
                kind: row.get(2)?,
                line_start: row.get::<_, i64>(3)? as usize,
                line_end: row.get::<_, i64>(4)? as usize,
                body: row.get(5)?,
            })
        };

        let mut stmt = self.conn.prepare(&sql)?;
        let raw_results: Vec<GrepBodyRaw> = match language {
            Some(lang) => {
                let rows = stmt.query_map(params![body_param, lang, limit as i64], row_mapper)?;
                rows.map(|r| r.map_err(Into::into)).collect::<Result<Vec<_>>>()?
            }
            None => {
                let rows = stmt.query_map(params![body_param, limit as i64], row_mapper)?;
                rows.map(|r| r.map_err(Into::into)).collect::<Result<Vec<_>>>()?
            }
        };

        // Компилируем regex один раз (если задан)
        let compiled_re = regex_pattern
            .map(|r| regex::Regex::new(r))
            .transpose()
            .context("grep_body: невалидный regex")?;

        // Построчный поиск совпадений внутри тел
        let results = raw_results
            .into_iter()
            .map(|raw| {
                let mut all_match_lines = Vec::new();
                for (i, line) in raw.body.lines().enumerate() {
                    let matched = if let Some(ref re) = compiled_re {
                        re.is_match(line)
                    } else if let Some(p) = pattern {
                        // Без учёта регистра, аналогично LIKE
                        line.to_lowercase().contains(&p.to_lowercase())
                    } else {
                        false
                    };
                    if matched {
                        all_match_lines.push(raw.line_start + i);
                    }
                }
                let total = all_match_lines.len();
                let match_lines: Vec<usize> = all_match_lines.into_iter().take(3).collect();
                let match_count = if total > 3 { Some(total) } else { None };
                GrepBodyMatch {
                    file_path: raw.file_path,
                    name: raw.name,
                    kind: raw.kind,
                    line_start: raw.line_start,
                    line_end: raw.line_end,
                    match_lines,
                    match_count,
                }
            })
            .collect();

        Ok(results)
    }

    /// Найти функции по точному имени
    pub fn get_function_by_name(&self, name: &str) -> Result<Vec<FunctionRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, file_id, name, qualified_name, line_start, line_end,
                    args, return_type, docstring, body, is_async, node_hash
             FROM functions WHERE name = ?1",
        )?;
        let rows = stmt.query_map(params![name], row_to_function)?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Найти классы по точному имени
    pub fn get_class_by_name(&self, name: &str) -> Result<Vec<ClassRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, file_id, name, line_start, line_end,
                    bases, docstring, body, node_hash
             FROM classes WHERE name = ?1",
        )?;
        let rows = stmt.query_map(params![name], row_to_class)?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Найти все вызовы, где данная функция является caller
    pub fn get_callees(&self, function_name: &str, language: Option<&str>) -> Result<Vec<CallRecord>> {
        match language {
            Some(lang) => {
                let mut stmt = self.conn.prepare(
                    "SELECT c.id, c.file_id, c.caller, c.callee, c.line
                     FROM calls c JOIN files fi ON fi.id = c.file_id
                     WHERE c.caller = ?1 AND fi.language = ?2",
                )?;
                let rows = stmt.query_map(params![function_name, lang], row_to_call)?;
                rows.map(|r| r.map_err(Into::into)).collect()
            }
            None => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, file_id, caller, callee, line FROM calls WHERE caller = ?1",
                )?;
                let rows = stmt.query_map(params![function_name], row_to_call)?;
                rows.map(|r| r.map_err(Into::into)).collect()
            }
        }
    }

    /// Найти все вызовы, где данная функция является callee
    pub fn get_callers(&self, function_name: &str, language: Option<&str>) -> Result<Vec<CallRecord>> {
        match language {
            Some(lang) => {
                let mut stmt = self.conn.prepare(
                    "SELECT c.id, c.file_id, c.caller, c.callee, c.line
                     FROM calls c JOIN files fi ON fi.id = c.file_id
                     WHERE c.callee = ?1 AND fi.language = ?2",
                )?;
                let rows = stmt.query_map(params![function_name, lang], row_to_call)?;
                rows.map(|r| r.map_err(Into::into)).collect()
            }
            None => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, file_id, caller, callee, line FROM calls WHERE callee = ?1",
                )?;
                let rows = stmt.query_map(params![function_name], row_to_call)?;
                rows.map(|r| r.map_err(Into::into)).collect()
            }
        }
    }

    /// Объединённый поиск символа по имени (функции + классы + переменные + импорты)
    pub fn find_symbol(&self, name: &str, language: Option<&str>) -> Result<SymbolSearchResult> {
        // Функции
        let functions = {
            match language {
                Some(lang) => {
                    let mut stmt = self.conn.prepare(
                        "SELECT f.id, f.file_id, f.name, f.qualified_name, f.line_start, f.line_end,
                                f.args, f.return_type, f.docstring, f.body, f.is_async, f.node_hash
                         FROM functions f JOIN files fi ON fi.id = f.file_id
                         WHERE (f.name = ?1 OR f.qualified_name = ?1) AND fi.language = ?2",
                    )?;
                    let rows = stmt.query_map(params![name, lang], row_to_function)?;
                    rows.map(|r| r.map_err(Into::into)).collect::<Result<Vec<_>>>()?
                }
                None => {
                    let mut stmt = self.conn.prepare(
                        "SELECT id, file_id, name, qualified_name, line_start, line_end,
                                args, return_type, docstring, body, is_async, node_hash
                         FROM functions WHERE name = ?1 OR qualified_name = ?1",
                    )?;
                    let rows = stmt.query_map(params![name], row_to_function)?;
                    rows.map(|r| r.map_err(Into::into)).collect::<Result<Vec<_>>>()?
                }
            }
        };
        // Классы
        let classes = {
            match language {
                Some(lang) => {
                    let mut stmt = self.conn.prepare(
                        "SELECT c.id, c.file_id, c.name, c.line_start, c.line_end,
                                c.bases, c.docstring, c.body, c.node_hash
                         FROM classes c JOIN files fi ON fi.id = c.file_id
                         WHERE c.name = ?1 AND fi.language = ?2",
                    )?;
                    let rows = stmt.query_map(params![name, lang], row_to_class)?;
                    rows.map(|r| r.map_err(Into::into)).collect::<Result<Vec<_>>>()?
                }
                None => {
                    let mut stmt = self.conn.prepare(
                        "SELECT id, file_id, name, line_start, line_end,
                                bases, docstring, body, node_hash
                         FROM classes WHERE name = ?1",
                    )?;
                    let rows = stmt.query_map(params![name], row_to_class)?;
                    rows.map(|r| r.map_err(Into::into)).collect::<Result<Vec<_>>>()?
                }
            }
        };
        // Переменные (фильтр language не применяется — variables не имеют прямой связи с language)
        let variables = {
            let mut stmt = self.conn.prepare(
                "SELECT id, file_id, name, value, line FROM variables WHERE name = ?1",
            )?;
            let rows = stmt.query_map(params![name], row_to_variable)?;
            rows.map(|r| r.map_err(Into::into))
                .collect::<Result<Vec<_>>>()?
        };
        // Импорты
        let imports = {
            match language {
                Some(lang) => {
                    let mut stmt = self.conn.prepare(
                        "SELECT i.id, i.file_id, i.module, i.name, i.alias, i.line, i.kind
                         FROM imports i JOIN files fi ON fi.id = i.file_id
                         WHERE (i.name = ?1 OR i.alias = ?1) AND fi.language = ?2",
                    )?;
                    let rows = stmt.query_map(params![name, lang], row_to_import)?;
                    rows.map(|r| r.map_err(Into::into)).collect::<Result<Vec<_>>>()?
                }
                None => {
                    let mut stmt = self.conn.prepare(
                        "SELECT id, file_id, module, name, alias, line, kind
                         FROM imports WHERE name = ?1 OR alias = ?1",
                    )?;
                    let rows = stmt.query_map(params![name], row_to_import)?;
                    rows.map(|r| r.map_err(Into::into)).collect::<Result<Vec<_>>>()?
                }
            }
        };

        Ok(SymbolSearchResult { functions, classes, variables, imports })
    }

    /// Получить все импорты файла
    pub fn get_imports_by_file(&self, file_id: i64) -> Result<Vec<ImportRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, file_id, module, name, alias, line, kind
             FROM imports WHERE file_id = ?1 ORDER BY line",
        )?;
        let rows = stmt.query_map(params![file_id], row_to_import)?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Найти все импорты указанного модуля
    pub fn get_imports_by_module(&self, module: &str, language: Option<&str>) -> Result<Vec<ImportRecord>> {
        match language {
            Some(lang) => {
                let mut stmt = self.conn.prepare(
                    "SELECT i.id, i.file_id, i.module, i.name, i.alias, i.line, i.kind
                     FROM imports i JOIN files fi ON fi.id = i.file_id
                     WHERE i.module = ?1 AND fi.language = ?2",
                )?;
                let rows = stmt.query_map(params![module, lang], row_to_import)?;
                rows.map(|r| r.map_err(Into::into)).collect()
            }
            None => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, file_id, module, name, alias, line, kind
                     FROM imports WHERE module = ?1",
                )?;
                let rows = stmt.query_map(params![module], row_to_import)?;
                rows.map(|r| r.map_err(Into::into)).collect()
            }
        }
    }

    /// Сводная информация о файле по пути
    pub fn get_file_summary(&self, path: &str) -> Result<Option<FileSummary>> {
        let file = match self.get_file_by_path(path)? {
            Some(f) => f,
            None => return Ok(None),
        };
        let file_id = file.id.unwrap();

        // Функции файла
        let functions = {
            let mut stmt = self.conn.prepare(
                "SELECT id, file_id, name, qualified_name, line_start, line_end,
                        args, return_type, docstring, body, is_async, node_hash
                 FROM functions WHERE file_id = ?1 ORDER BY line_start",
            )?;
            let rows = stmt.query_map(params![file_id], row_to_function)?;
            rows.map(|r| r.map_err(Into::into))
                .collect::<Result<Vec<_>>>()?
        };
        // Классы файла
        let classes = {
            let mut stmt = self.conn.prepare(
                "SELECT id, file_id, name, line_start, line_end,
                        bases, docstring, body, node_hash
                 FROM classes WHERE file_id = ?1 ORDER BY line_start",
            )?;
            let rows = stmt.query_map(params![file_id], row_to_class)?;
            rows.map(|r| r.map_err(Into::into))
                .collect::<Result<Vec<_>>>()?
        };
        // Импорты файла
        let imports = self.get_imports_by_file(file_id)?;
        // Переменные файла
        let variables = {
            let mut stmt = self.conn.prepare(
                "SELECT id, file_id, name, value, line
                 FROM variables WHERE file_id = ?1 ORDER BY line",
            )?;
            let rows = stmt.query_map(params![file_id], row_to_variable)?;
            rows.map(|r| r.map_err(Into::into))
                .collect::<Result<Vec<_>>>()?
        };

        Ok(Some(FileSummary { file, functions, classes, imports, variables }))
    }

    /// Статистика базы данных
    pub fn get_stats(&self) -> Result<DbStats> {
        let count = |table: &str| -> Result<usize> {
            let n: i64 = self.conn.query_row(
                &format!("SELECT COUNT(*) FROM {table}"),
                [],
                |row| row.get(0),
            )?;
            Ok(n as usize)
        };
        Ok(DbStats {
            total_files:      count("files")?,
            total_functions:  count("functions")?,
            total_classes:    count("classes")?,
            total_imports:    count("imports")?,
            total_calls:      count("calls")?,
            total_variables:  count("variables")?,
            total_text_files: count("text_files")?,
            indexing_status: None,
        })
    }

    // ── Bulk-load ────────────────────────────────────────────────────────────

    /// Инициализировать БД для массовой первичной загрузки: только таблицы, без индексов.
    ///
    /// Используется когда БД пустая и нужно загрузить большое количество файлов.
    /// Индексы и триггеры создаются позже через `finish_bulk_load`.
    pub fn initialize_for_bulk(&self) -> Result<()> {
        schema::initialize_tables_only(&self.conn)
            .context("initialize_for_bulk: ошибка создания таблиц без индексов")?;
        Ok(())
    }

    /// Подготовить БД к массовой загрузке: удалить индексы и FTS-триггеры.
    ///
    /// Вызывать перед началом bulk-load, если планируется индексация > N файлов.
    /// Без индексов и триггеров каждый INSERT выполняется значительно быстрее.
    pub fn prepare_bulk_load(&self) -> Result<()> {
        schema::drop_indexes_and_triggers(&self.conn)
            .context("prepare_bulk_load: ошибка удаления индексов и триггеров")?;
        Ok(())
    }

    /// Завершить массовую загрузку: пересоздать индексы, триггеры и перестроить FTS.
    ///
    /// Вызывать после завершения bulk-load. Пересоздание индексов одним проходом
    /// дешевле, чем инкрементальное обновление на каждый INSERT.
    pub fn finish_bulk_load(&self) -> Result<()> {
        schema::rebuild_indexes_and_triggers(&self.conn)
            .context("finish_bulk_load: ошибка пересоздания индексов и триггеров")?;
        Ok(())
    }

    // ── Транзакции ───────────────────────────────────────────────────────────

    /// Выполнить функцию внутри транзакции
    pub fn execute_in_transaction<F, T>(&mut self, f: F) -> Result<T>
    where
        F: FnOnce(&rusqlite::Transaction) -> Result<T>,
    {
        let tx = self.conn.transaction().context("Не удалось начать транзакцию")?;
        let result = f(&tx)?;
        tx.commit().context("Не удалось закоммитить транзакцию")?;
        Ok(result)
    }

    // ── Батч-транзакции ──────────────────────────────────────────────────────

    /// Начать батч-транзакцию для группового INSERT.
    ///
    /// Все последующие операции с БД будут выполняться внутри одной транзакции
    /// до вызова [`commit_batch`]. Это устраняет fsync на каждый INSERT и
    /// существенно ускоряет массовую индексацию.
    pub fn begin_batch(&self) -> Result<()> {
        self.conn
            .execute_batch("BEGIN TRANSACTION")
            .context("begin_batch: не удалось начать транзакцию")?;
        Ok(())
    }

    /// Завершить батч-транзакцию, записав все накопленные изменения на диск.
    ///
    /// Должен вызываться строго после [`begin_batch`]. Пара begin/commit
    /// гарантирует атомарную запись батча файлов.
    pub fn commit_batch(&self) -> Result<()> {
        self.conn
            .execute_batch("COMMIT")
            .context("commit_batch: не удалось закоммитить транзакцию")?;
        Ok(())
    }
}

// ── Вспомогательные функции ───────────────────────────────────────────────────

/// Экранировать спецсимволы FTS5 в поисковом запросе.
///
/// FTS5 интерпретирует дефис как NOT, «+» и «*» как операторы.
/// Если запрос содержит такие символы внутри слова — оборачиваем всё в кавычки,
/// чтобы FTS5 искал буквальную фразу.
fn sanitize_fts_query(query: &str) -> String {
    // Проверяем наличие FTS-спецсимволов внутри токенов
    if query.contains('-') || query.contains('+') || query.contains('*') {
        format!("\"{}\"", query)
    } else {
        query.to_string()
    }
}

// ── Вспомогательные функции маппинга строк ───────────────────────────────────

fn row_to_file(row: &rusqlite::Row<'_>) -> rusqlite::Result<FileRecord> {
    Ok(FileRecord {
        id:           Some(row.get(0)?),
        path:         row.get(1)?,
        content_hash: row.get(2)?,
        ast_hash:     row.get(3)?,
        language:     row.get(4)?,
        lines_total:  row.get::<_, i64>(5)? as usize,
        indexed_at:   row.get(6)?,
        mtime:        row.get(7)?,
        file_size:    row.get(8)?,
    })
}

fn row_to_function(row: &rusqlite::Row<'_>) -> rusqlite::Result<FunctionRecord> {
    Ok(FunctionRecord {
        id:              Some(row.get(0)?),
        file_id:         row.get(1)?,
        name:            row.get(2)?,
        qualified_name:  row.get(3)?,
        line_start:      row.get::<_, i64>(4)? as usize,
        line_end:        row.get::<_, i64>(5)? as usize,
        args:            row.get(6)?,
        return_type:     row.get(7)?,
        docstring:       row.get(8)?,
        body:            row.get(9)?,
        is_async:        row.get::<_, i32>(10)? != 0,
        node_hash:       row.get(11)?,
        // Колонки 12 и 13 появились в миграции v2 — читаем через try_get,
        // чтобы не ломаться на старых индексах без этих колонок
        override_type:   row.get(12).ok(),
        override_target: row.get(13).ok(),
    })
}

fn row_to_class(row: &rusqlite::Row<'_>) -> rusqlite::Result<ClassRecord> {
    Ok(ClassRecord {
        id:        Some(row.get(0)?),
        file_id:   row.get(1)?,
        name:      row.get(2)?,
        line_start: row.get::<_, i64>(3)? as usize,
        line_end:   row.get::<_, i64>(4)? as usize,
        bases:     row.get(5)?,
        docstring: row.get(6)?,
        body:      row.get(7)?,
        node_hash: row.get(8)?,
    })
}

fn row_to_import(row: &rusqlite::Row<'_>) -> rusqlite::Result<ImportRecord> {
    Ok(ImportRecord {
        id:      Some(row.get(0)?),
        file_id: row.get(1)?,
        module:  row.get(2)?,
        name:    row.get(3)?,
        alias:   row.get(4)?,
        line:    row.get::<_, i64>(5)? as usize,
        kind:    row.get(6)?,
    })
}

fn row_to_call(row: &rusqlite::Row<'_>) -> rusqlite::Result<CallRecord> {
    Ok(CallRecord {
        id:      Some(row.get(0)?),
        file_id: row.get(1)?,
        caller:  row.get(2)?,
        callee:  row.get(3)?,
        line:    row.get::<_, i64>(4)? as usize,
    })
}

fn row_to_variable(row: &rusqlite::Row<'_>) -> rusqlite::Result<VariableRecord> {
    Ok(VariableRecord {
        id:      Some(row.get(0)?),
        file_id: row.get(1)?,
        name:    row.get(2)?,
        value:   row.get(3)?,
        line:    row.get::<_, i64>(4)? as usize,
    })
}

// ── Тесты ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Вспомогательный FileRecord для тестов
    fn make_file(path: &str) -> FileRecord {
        FileRecord {
            id: None,
            path: path.to_string(),
            content_hash: "abc123".to_string(),
            ast_hash: None,
            language: "python".to_string(),
            lines_total: 100,
            indexed_at: "2026-01-01T00:00:00".to_string(),
            mtime: None,
            file_size: None,
        }
    }

    /// Вспомогательный FunctionRecord для тестов
    fn make_function(file_id: i64, name: &str) -> FunctionRecord {
        FunctionRecord {
            id: None,
            file_id,
            name: name.to_string(),
            qualified_name: Some(format!("module.{name}")),
            line_start: 1,
            line_end: 10,
            args: Some("(x, y)".to_string()),
            return_type: Some("int".to_string()),
            docstring: Some(format!("Вычисляет {name}")),
            body: format!("def {name}(x, y):\n    return x + y"),
            is_async: false,
            node_hash: "hash123".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn test_create_and_query_file() {
        let storage = Storage::open_in_memory().expect("Ошибка создания in-memory БД");

        let rec = make_file("/src/main.py");
        let id = storage.upsert_file(&rec).expect("upsert_file");
        assert!(id > 0, "id должен быть положительным");

        let found = storage.get_file_by_path("/src/main.py")
            .expect("get_file_by_path")
            .expect("файл должен существовать");
        assert_eq!(found.path, "/src/main.py");
        assert_eq!(found.language, "python");
        assert_eq!(found.lines_total, 100);
    }

    #[test]
    fn test_upsert_updates_existing() {
        let storage = Storage::open_in_memory().expect("Ошибка создания in-memory БД");

        let rec = make_file("/src/utils.py");
        let id1 = storage.upsert_file(&rec).expect("первый upsert");

        // Обновляем hash
        let mut rec2 = rec.clone();
        rec2.content_hash = "newHash".to_string();
        rec2.lines_total = 200;
        let id2 = storage.upsert_file(&rec2).expect("второй upsert");

        assert_eq!(id1, id2, "id не должен меняться при обновлении");
        let found = storage.get_file_by_path("/src/utils.py")
            .unwrap().unwrap();
        assert_eq!(found.content_hash, "newHash");
        assert_eq!(found.lines_total, 200);
    }

    #[test]
    fn test_functions_crud() {
        let storage = Storage::open_in_memory().expect("Ошибка создания БД");

        let file_id = storage.upsert_file(&make_file("/src/funcs.py")).unwrap();
        let funcs = vec![
            make_function(file_id, "add"),
            make_function(file_id, "subtract"),
        ];
        storage.insert_functions(&funcs).expect("insert_functions");

        // Поиск по точному имени
        let found = storage.get_function_by_name("add").expect("get_function_by_name");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "add");

        // Удаление
        storage.delete_functions_by_file(file_id).expect("delete_functions_by_file");
        let empty = storage.get_function_by_name("add").unwrap();
        assert!(empty.is_empty(), "после удаления функций не должно быть");
    }

    #[test]
    fn test_fts_search() {
        let storage = Storage::open_in_memory().expect("Ошибка создания БД");

        let file_id = storage.upsert_file(&make_file("/src/algo.py")).unwrap();
        let funcs = vec![
            FunctionRecord {
                id: None,
                file_id,
                name: "binary_search".to_string(),
                qualified_name: None,
                line_start: 1,
                line_end: 20,
                args: Some("(arr, target)".to_string()),
                return_type: Some("int".to_string()),
                docstring: Some("Бинарный поиск в отсортированном массиве".to_string()),
                body: "def binary_search(arr, target):\n    pass".to_string(),
                is_async: false,
                node_hash: "hs1".to_string(),
                ..Default::default()
            },
            FunctionRecord {
                id: None,
                file_id,
                name: "linear_scan".to_string(),
                qualified_name: None,
                line_start: 22,
                line_end: 30,
                args: None,
                return_type: None,
                docstring: Some("Линейный обход списка".to_string()),
                body: "def linear_scan():\n    pass".to_string(),
                is_async: false,
                node_hash: "hs2".to_string(),
                ..Default::default()
            },
        ];
        storage.insert_functions(&funcs).unwrap();

        // FTS-поиск по слову в имени
        let results = storage.search_functions("binary_search", 10, None).expect("search_functions");
        assert_eq!(results.len(), 1, "должна найтись ровно одна функция");
        assert_eq!(results[0].name, "binary_search");
    }

    #[test]
    fn test_cascade_delete() {
        let storage = Storage::open_in_memory().expect("Ошибка создания БД");

        let file_id = storage.upsert_file(&make_file("/src/cascade.py")).unwrap();
        storage.insert_functions(&[make_function(file_id, "foo")]).unwrap();
        storage.insert_classes(&[ClassRecord {
            id: None, file_id, name: "Bar".into(),
            line_start: 1, line_end: 5, bases: None, docstring: None,
            body: "class Bar: pass".into(), node_hash: "h".into(),
        }]).unwrap();

        // Удаляем файл — ожидаем каскадное удаление
        storage.delete_file(file_id).unwrap();

        let funcs = storage.get_function_by_name("foo").unwrap();
        assert!(funcs.is_empty(), "функции должны быть удалены каскадно");

        let classes = storage.get_class_by_name("Bar").unwrap();
        assert!(classes.is_empty(), "классы должны быть удалены каскадно");
    }

    #[test]
    fn test_find_symbol() {
        let storage = Storage::open_in_memory().expect("Ошибка создания БД");

        let file_id = storage.upsert_file(&make_file("/src/symbols.py")).unwrap();
        storage.insert_functions(&[make_function(file_id, "compute")]).unwrap();
        storage.insert_variables(&[VariableRecord {
            id: None, file_id, name: "compute".into(),
            value: Some("42".into()), line: 5,
        }]).unwrap();

        let result = storage.find_symbol("compute", None).expect("find_symbol");
        assert_eq!(result.functions.len(), 1, "должна найтись 1 функция");
        assert_eq!(result.variables.len(), 1, "должна найтись 1 переменная");
        assert!(result.classes.is_empty());
        assert!(result.imports.is_empty());
    }

    #[test]
    fn test_stats() {
        let storage = Storage::open_in_memory().expect("Ошибка создания БД");

        // Пустая база
        let stats = storage.get_stats().expect("get_stats");
        assert_eq!(stats.total_files, 0);

        let file_id = storage.upsert_file(&make_file("/src/stats.py")).unwrap();
        storage.insert_functions(&[
            make_function(file_id, "f1"),
            make_function(file_id, "f2"),
        ]).unwrap();
        storage.insert_calls(&[CallRecord {
            id: None, file_id, caller: "f1".into(), callee: "f2".into(), line: 5,
        }]).unwrap();

        let stats = storage.get_stats().expect("get_stats после вставки");
        assert_eq!(stats.total_files, 1);
        assert_eq!(stats.total_functions, 2);
        assert_eq!(stats.total_calls, 1);
    }

    #[test]
    fn test_language_filter() {
        let storage = Storage::open_in_memory().expect("Ошибка создания БД");

        // Python-файл
        let py_id = storage.upsert_file(&make_file("/src/algo.py")).unwrap();
        // Rust-файл
        let rs_rec = FileRecord {
            id: None,
            path: "/src/main.rs".to_string(),
            content_hash: "rustHash".to_string(),
            ast_hash: None,
            language: "rust".to_string(),
            lines_total: 50,
            indexed_at: "2026-01-01T00:00:00".to_string(),
            mtime: None,
            file_size: None,
        };
        let rs_id = storage.upsert_file(&rs_rec).unwrap();

        // Вставляем функции в оба файла
        storage.insert_functions(&[make_function(py_id, "py_func")]).unwrap();
        storage.insert_functions(&[make_function(rs_id, "rs_func")]).unwrap();

        // Без фильтра — обе функции
        let all = storage.search_functions("func", 10, None).expect("поиск без фильтра");
        assert_eq!(all.len(), 2, "без фильтра должны найтись обе функции");

        // Только Python
        let py_only = storage.search_functions("func", 10, Some("python")).expect("поиск python");
        assert_eq!(py_only.len(), 1, "с фильтром python — только одна функция");
        assert_eq!(py_only[0].name, "py_func");

        // Только Rust
        let rs_only = storage.search_functions("func", 10, Some("rust")).expect("поиск rust");
        assert_eq!(rs_only.len(), 1, "с фильтром rust — только одна функция");
        assert_eq!(rs_only[0].name, "rs_func");
    }

    #[test]
    fn test_fts_with_dashes() {
        let storage = Storage::open_in_memory().expect("Ошибка создания БД");

        let file_id = storage.upsert_file(&make_file("/src/deps.py")).unwrap();
        let func = FunctionRecord {
            id: None,
            file_id,
            name: "use_tree_sitter".to_string(),
            qualified_name: None,
            line_start: 1,
            line_end: 5,
            args: None,
            return_type: None,
            docstring: Some("Использует tree-sitter-python для разбора".to_string()),
            body: "def use_tree_sitter(): pass".to_string(),
            is_async: false,
            node_hash: "h_ts".to_string(),
            ..Default::default()
        };
        storage.insert_functions(&[func]).unwrap();

        // Поиск с дефисом не должен вернуть ошибку FTS5
        let results = storage.search_functions("tree-sitter-python", 10, None)
            .expect("поиск с дефисом не должен падать");
        assert_eq!(results.len(), 1, "должна найтись функция с дефисом в docstring");
    }

    #[test]
    fn test_flush_to_disk() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("test.db");

        // Создать in-memory БД и записать данные
        let storage = Storage::open_in_memory().unwrap();
        let rec = FileRecord {
            id: None,
            path: "test.py".to_string(),
            content_hash: "abc".to_string(),
            ast_hash: None,
            language: "python".to_string(),
            lines_total: 10,
            indexed_at: "2026-01-01".to_string(),
            mtime: None,
            file_size: None,
        };
        storage.upsert_file(&rec).unwrap();

        // Flush на диск
        storage.flush_to_disk(&db_path).unwrap();
        assert!(db_path.exists(), "файл БД должен появиться на диске");

        // Открыть с диска и проверить данные
        let storage2 = Storage::open_file(&db_path).unwrap();
        let file = storage2.get_file_by_path("test.py").unwrap();
        assert!(file.is_some(), "файл должен быть найден в дисковой копии");
        assert_eq!(file.unwrap().content_hash, "abc");
    }

    #[test]
    fn test_open_auto_in_memory_for_new_db() {
        // Новая БД (файл не существует) — должен выбрать in-memory
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("index.db");
        let config = memory::StorageConfig {
            mode: "auto".to_string(),
            memory_max_percent: 25,
        };

        let storage = Storage::open_auto(&db_path, &config)
            .expect("open_auto должен работать для новой БД");

        // Проверяем что БД работает — вставляем файл
        storage.upsert_file(&make_file("/hello.py")).unwrap();
        let found = storage.get_file_by_path("/hello.py").unwrap();
        assert!(found.is_some());
    }

    #[test]
    fn test_open_auto_disk_mode() {
        // Явный режим disk — должен открыть файл
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("index.db");
        let config = memory::StorageConfig {
            mode: "disk".to_string(),
            memory_max_percent: 25,
        };

        let storage = Storage::open_auto(&db_path, &config)
            .expect("open_auto disk режим");
        storage.upsert_file(&make_file("/hello.rs")).unwrap();
        assert!(db_path.exists(), "файл БД должен существовать в disk-режиме");
    }

    #[test]
    fn test_open_auto_loads_existing_db() {
        // Сначала создаём файл БД, потом открываем через open_auto memory
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("index.db");

        // Создать файловую БД с данными
        {
            let s = Storage::open_file(&db_path).unwrap();
            s.upsert_file(&make_file("/existing.py")).unwrap();
        }

        // Открыть через open_auto в режиме memory — данные должны загрузиться
        let config = memory::StorageConfig {
            mode: "memory".to_string(),
            memory_max_percent: 25,
        };
        let storage = Storage::open_auto(&db_path, &config).unwrap();
        let found = storage.get_file_by_path("/existing.py").unwrap();
        assert!(found.is_some(), "данные из файла должны быть доступны в in-memory БД");
    }
}
