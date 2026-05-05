/// Модуль хранилища — SQLite через rusqlite (bundled)
pub mod memory;
pub mod models;
pub mod schema;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
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

    /// Применить дополнительный DDL (CREATE TABLE/INDEX/...) поверх базовой
    /// схемы. Используется расширениями (например, `bsl_extension`) для
    /// добавления специфичных таблиц при первом открытии БД репо с
    /// `language = "bsl"`.
    ///
    /// `extensions` — массив SQL-batch'ей, выполняется последовательно.
    /// Каждый batch может содержать несколько statement'ов через `;`.
    /// Все DDL должны быть идемпотентными (`IF NOT EXISTS`) — функция
    /// безопасно вызывается на каждый open, даже если таблицы уже есть.
    pub fn apply_schema_extensions(&self, extensions: &[&str]) -> Result<()> {
        for ddl in extensions {
            self.conn
                .execute_batch(ddl)
                .with_context(|| format!("DDL-расширение схемы упало: {}", ddl))?;
        }
        Ok(())
    }

    /// Прямой доступ к низкоуровневому SQLite-соединению для расширений.
    /// Используется реализациями `LanguageProcessor::index_extras` —
    /// каждое расширение работает со своими таблицами и пишет напрямую
    /// (BEGIN/COMMIT, prepared statement'ы), а не через хелперы Storage.
    /// Прямые INSERT/UPDATE/DELETE — на ответственности расширения.
    pub fn conn(&self) -> &Connection {
        &self.conn
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

    /// Получить только путь файла по id. Используется в post-filter
    /// (mcp/tools.rs) для применения path_glob к результатам search_*/get_*.
    pub fn get_path_by_file_id(&self, id: i64) -> Result<Option<String>> {
        let r: Option<String> = self
            .conn
            .query_row(
                "SELECT path FROM files WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()
            .context("get_path_by_file_id")?;
        Ok(r)
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

    // ── file_contents (Phase 2): хранение content code-файлов с zstd-сжатием ──
    //
    // Для text-файлов content живёт в `text_files.content` (без сжатия — нужен
    // для FTS5). Для code-файлов (.py/.bsl/.rs/...) — в `file_contents.content_blob`
    // с zstd-сжатием. Файлы крупнее лимита получают `oversize=1`, `content_blob=NULL`.
    //
    // Различие "записи нет" vs "запись с oversize=1" важно:
    //   * нет записи → backfill ещё не дошёл (переходное состояние v0.7.x → 0.8.0)
    //   * запись с oversize=1 → файл намеренно пропущен из-за лимита

    /// zstd-уровень сжатия для file_contents. 3 — стандартный баланс
    /// скорости и коэффициента (~5× для текстового кода).
    const FILE_CONTENTS_ZSTD_LEVEL: i32 = 3;

    /// Максимальный размер разжатого buffer'а из `file_contents.content_blob`.
    /// Защита от zstd-bomb: вредоносный или повреждённый blob не сможет
    /// аллоцировать произвольно много RAM. 256 МБ — заведомо больше любого
    /// валидного code-файла (lim_max = 5 МБ default × 5× компрессия = 25 МБ;
    /// даже если оператор поднял `max_code_file_size_bytes` до 50 МБ,
    /// 256 МБ остаётся комфортным запасом).
    const FILE_CONTENTS_MAX_DECOMPRESSED_BYTES: usize = 256 * 1024 * 1024;

    /// Безопасный zstd-decode с лимитом на размер выходного буфера.
    /// Использует stream-decoder с `io::Read::take(limit)` — если разжатый
    /// размер превысит лимит, `read_to_end` остановится; затем мы сверяем
    /// фактический размер с лимитом и возвращаем ошибку, если совпало
    /// (значит decode был усечён — потенциальная zstd-bomb).
    fn decode_zstd_safe(blob: &[u8]) -> Result<Vec<u8>> {
        use std::io::Read;
        let mut decoder = zstd::stream::read::Decoder::new(blob)
            .context("decode_zstd_safe: открыть zstd-decoder")?;
        let mut out = Vec::new();
        let limit = Self::FILE_CONTENTS_MAX_DECOMPRESSED_BYTES as u64;
        // take(limit + 1) — читаем на 1 байт больше, чтобы отличить
        // «разжалось ровно в limit» (валидно) от «разжалось больше limit»
        // (zstd-bomb: данные ещё были, но мы остановились).
        let read = (&mut decoder).take(limit + 1).read_to_end(&mut out)
            .context("decode_zstd_safe: чтение разжатого потока")?;
        if read as u64 > limit {
            anyhow::bail!(
                "decode_zstd_safe: разжатый размер превысил лимит {} байт (zstd-bomb?)",
                Self::FILE_CONTENTS_MAX_DECOMPRESSED_BYTES
            );
        }
        Ok(out)
    }

    /// Сохранить content code-файла в `file_contents` с zstd-сжатием.
    /// Если `content.len() > max_size_bytes` — записывается «oversize»-запись
    /// с `content_blob=NULL`. Idempotent через `INSERT OR REPLACE`.
    pub fn upsert_file_content(
        &self,
        file_id: i64,
        content: &str,
        max_size_bytes: usize,
    ) -> Result<()> {
        if content.len() > max_size_bytes {
            self.conn
                .execute(
                    "INSERT OR REPLACE INTO file_contents (file_id, content_blob, oversize)
                     VALUES (?1, NULL, 1)",
                    params![file_id],
                )
                .context("upsert_file_content: INSERT oversize")?;
            return Ok(());
        }
        let blob = zstd::encode_all(content.as_bytes(), Self::FILE_CONTENTS_ZSTD_LEVEL)
            .context("upsert_file_content: zstd encode")?;
        self.conn
            .execute(
                "INSERT OR REPLACE INTO file_contents (file_id, content_blob, oversize)
                 VALUES (?1, ?2, 0)",
                params![file_id, blob],
            )
            .context("upsert_file_content: INSERT blob")?;
        Ok(())
    }

    /// Получить только id файла по пути. Лёгкая альтернатива `get_file_by_path`,
    /// когда нужен только PK (например, в backfill `file_contents`).
    pub fn get_file_id_by_path(&self, path: &str) -> Result<Option<i64>> {
        self.conn
            .query_row(
                "SELECT id FROM files WHERE path = ?1",
                params![path],
                |r| r.get::<_, i64>(0),
            )
            .optional()
            .context("get_file_id_by_path")
    }

    /// Список code-файлов, у которых нет записи в `file_contents` И нет записи
    /// в `text_files` (т.е. это code-файлы, которым нужен backfill после
    /// миграции v0.7.x → v0.8.0). Возвращает `(file_id, path)`-пары, отсортированные
    /// по пути. Используется отдельной фазой backfill в `full_reindex` —
    /// независимо от того, изменился ли mtime файла.
    pub fn list_code_files_without_content(&self) -> Result<Vec<(i64, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT fi.id, fi.path
             FROM files fi
             WHERE fi.id NOT IN (SELECT file_id FROM file_contents)
               AND fi.id NOT IN (SELECT file_id FROM text_files)
             ORDER BY fi.path",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Есть ли запись в `text_files` для file_id. Используется backfill'ом
    /// `file_contents`, чтобы пропускать text-файлы (их content уже в text_files).
    pub fn has_text_file(&self, file_id: i64) -> Result<bool> {
        let n: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM text_files WHERE file_id = ?1",
                params![file_id],
                |r| r.get(0),
            )
            .context("has_text_file")?;
        Ok(n > 0)
    }

    /// Удалить запись `file_contents` (на случай удаления файла без CASCADE).
    /// При штатной работе срабатывает `ON DELETE CASCADE` — этот метод нужен
    /// только для явного контроля.
    pub fn delete_file_content(&self, file_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM file_contents WHERE file_id = ?1", params![file_id])
            .context("delete_file_content")?;
        Ok(())
    }

    /// Есть ли запись в `file_contents` для file_id (любого вида — содержательная
    /// или oversize). Используется backfill'ом, чтобы пропускать уже обработанные.
    pub fn has_file_content(&self, file_id: i64) -> Result<bool> {
        let n: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM file_contents WHERE file_id = ?1",
                params![file_id],
                |r| r.get(0),
            )
            .context("has_file_content")?;
        Ok(n > 0)
    }

    /// Прочитать content code-файла и oversize-флаг.
    /// Возвращает:
    ///   * `None` — записи нет (backfill ещё не дошёл, переходное состояние).
    ///   * `Some((Some(content), false))` — нормальная запись, content разжат из zstd.
    ///   * `Some((None, true))` — файл oversize, content намеренно не сохранён.
    pub fn read_file_content(&self, file_id: i64) -> Result<Option<(Option<String>, bool)>> {
        let row: Option<(Option<Vec<u8>>, i64)> = self
            .conn
            .query_row(
                "SELECT content_blob, oversize FROM file_contents WHERE file_id = ?1",
                params![file_id],
                |r| Ok((r.get::<_, Option<Vec<u8>>>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()
            .context("read_file_content: SELECT")?;
        let Some((blob_opt, oversize_int)) = row else {
            return Ok(None);
        };
        let oversize = oversize_int != 0;
        let content_opt = match blob_opt {
            None => None, // oversize-запись или повреждённая (oversize=0, blob=NULL — не должно случаться)
            Some(blob) => {
                let bytes = Self::decode_zstd_safe(&blob)
                    .context("read_file_content: zstd decode")?;
                let text = String::from_utf8(bytes)
                    .context("read_file_content: UTF-8 из zstd-blob")?;
                Some(text)
            }
        };
        Ok(Some((content_opt, oversize)))
    }

    /// grep_code: regex-поиск по содержимому **code-файлов** через `file_contents`.
    /// Содержимое хранится сжатым zstd, поэтому SQL делает только pre-filter
    /// по path_glob/language; сам regex применяется к разжатому тексту в Rust.
    /// Файлы oversize=1 (без content) пропускаются.
    ///
    /// Параметры идентичны `grep_text_filtered` для совместимости в MCP-слое.
    pub fn grep_code_filtered(
        &self,
        regex_pattern: &str,
        path_glob: Option<&str>,
        language: Option<&str>,
        limit: usize,
        context_lines: usize,
        max_total_bytes: usize,
    ) -> Result<Vec<GrepTextMatch>> {
        let compiled = regex::Regex::new(regex_pattern)
            .context("grep_code: невалидный regex")?;

        let mut conds: Vec<String> = vec![
            "fc.oversize = 0".to_string(),
            "fc.content_blob IS NOT NULL".to_string(),
        ];
        let mut params_dyn: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(g) = path_glob {
            conds.push("fi.path GLOB ?".to_string());
            params_dyn.push(Box::new(normalize_glob(g)));
        }
        if let Some(l) = language {
            conds.push("fi.language = ?".to_string());
            params_dyn.push(Box::new(l.to_string()));
        }
        let sql = format!(
            "SELECT fi.path, fc.content_blob
             FROM file_contents fc
             JOIN files fi ON fi.id = fc.file_id
             WHERE {}
             ORDER BY fi.path",
            conds.join(" AND ")
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let params_refs: Vec<&dyn rusqlite::ToSql> =
            params_dyn.iter().map(|b| &**b as &dyn rusqlite::ToSql).collect();

        let candidate_rows: Vec<(String, Vec<u8>)> = stmt
            .query_map(params_refs.as_slice(), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut results: Vec<GrepTextMatch> = Vec::new();
        let mut total_bytes: usize = 0;
        for (path, blob) in candidate_rows {
            // Безопасный decode zstd с лимитом размера (защита от zstd-bomb).
            // Битые blob'ы или превышение лимита пропускаем — не валим весь поиск.
            let bytes = match Self::decode_zstd_safe(&blob) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let content = match String::from_utf8(bytes) {
                Ok(s) => s,
                Err(_) => continue,
            };
            // Быстрый отказ: если в файле нет ни одного совпадения — не
            // тратим время на построчный обход.
            if !compiled.is_match(&content) {
                continue;
            }
            let lines: Vec<&str> = content.lines().collect();
            for (i, line) in lines.iter().enumerate() {
                if !compiled.is_match(line) {
                    continue;
                }
                let line_no = i + 1;
                let context = if context_lines > 0 {
                    let from = i.saturating_sub(context_lines);
                    let to = (i + context_lines + 1).min(lines.len());
                    (from..to)
                        .map(|j| ContextLine {
                            line: j + 1,
                            content: lines[j].to_string(),
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                let row_bytes = line.len()
                    + context.iter().map(|c| c.content.len()).sum::<usize>()
                    + path.len();
                total_bytes = total_bytes.saturating_add(row_bytes);
                if total_bytes > max_total_bytes {
                    return Ok(results);
                }
                results.push(GrepTextMatch {
                    path: path.clone(),
                    line: line_no,
                    content: line.to_string(),
                    context,
                });
                if results.len() >= limit {
                    return Ok(results);
                }
            }
        }
        Ok(results)
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
                    context: Vec::new(),
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

    // ── Phase 1: file listing, stat, read, grep_text ────────────────────────
    //
    // Новые read-only инструменты, добавленные в v0.7.0. Все работают только
    // с тем, что уже есть в индексе:
    //   * `stat_file` / `list_files`        — таблица `files`.
    //   * `read_file_text` / `grep_text`    — таблица `text_files` (FTS-индексируемые
    //                                          расширения: yaml, md, json, toml, xml,
    //                                          shell-скрипты и т.д.).
    //
    // Чтение содержимого code-файлов (.py/.bsl/.rs/...) откладывается до Phase 2
    // (миграция v4 с таблицей `file_contents`).

    /// stat_file: метаданные одного файла из таблицы `files`.
    /// Возвращает `exists=false` если файл не индексирован.
    pub fn stat_file_meta(&self, path: &str) -> Result<StatFileResult> {
        let row: Option<(String, String, i64, String, Option<i64>, Option<i64>, String)> = self
            .conn
            .query_row(
                "SELECT language, content_hash, lines_total, indexed_at, mtime, file_size, path
                 FROM files WHERE path = ?1",
                params![path],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,        // language
                        r.get::<_, String>(1)?,        // content_hash
                        r.get::<_, i64>(2)?,           // lines_total
                        r.get::<_, String>(3)?,        // indexed_at
                        r.get::<_, Option<i64>>(4)?,   // mtime
                        r.get::<_, Option<i64>>(5)?,   // file_size
                        r.get::<_, String>(6)?,        // path
                    ))
                },
            )
            .optional()
            .context("stat_file_meta: ошибка SELECT files")?;

        match row {
            None => Ok(StatFileResult {
                exists: false,
                path: path.to_string(),
                language: None,
                size: None,
                mtime: None,
                lines_total: None,
                content_hash: None,
                indexed_at: None,
                category: None,
                oversize: None,
            }),
            Some((language, hash, lines_total, indexed_at, mtime, size, path_db)) => {
                // Категория: text — content есть в text_files, code — content в file_contents
                // (Phase 2). Для code дополнительно вытаскиваем oversize-флаг из file_contents.
                let has_text: i64 = self
                    .conn
                    .query_row(
                        "SELECT COUNT(*) FROM text_files tf
                         JOIN files fi ON fi.id = tf.file_id
                         WHERE fi.path = ?1",
                        params![path_db],
                        |r| r.get(0),
                    )
                    .context("stat_file_meta: проверка text_files")?;
                let category = if has_text > 0 { "text" } else { "code" };

                // oversize актуален только для code-файлов (для text всегда None).
                let oversize_opt = if category == "code" {
                    let oversize_int: Option<i64> = self
                        .conn
                        .query_row(
                            "SELECT oversize FROM file_contents fc
                             JOIN files fi ON fi.id = fc.file_id
                             WHERE fi.path = ?1",
                            params![path_db],
                            |r| r.get::<_, i64>(0),
                        )
                        .optional()
                        .context("stat_file_meta: проверка file_contents")?;
                    oversize_int.map(|i| i != 0)
                } else {
                    None
                };

                Ok(StatFileResult {
                    exists: true,
                    path: path_db,
                    language: Some(language),
                    size,
                    mtime,
                    lines_total: Some(lines_total as usize),
                    content_hash: Some(hash),
                    indexed_at: Some(indexed_at),
                    category: Some(category.to_string()),
                    oversize: oversize_opt,
                })
            }
        }
    }

    /// list_files: список файлов с опциональными фильтрами (glob по пути,
    /// префикс пути, язык). Возвращает `Vec<ListedFile>` с метаданными.
    ///
    /// `pattern` использует SQLite GLOB (`*` матчит любой символ, включая `/`,
    /// поэтому `*.py` рекурсивно). `**` нормализуется в `*` для совместимости
    /// с привычным glob-синтаксисом.
    pub fn list_files_filtered(
        &self,
        pattern: Option<&str>,
        path_prefix: Option<&str>,
        language: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ListedFile>> {
        let mut conds: Vec<String> = Vec::new();
        let mut params_dyn: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(g) = pattern {
            conds.push("path GLOB ?".to_string());
            params_dyn.push(Box::new(normalize_glob(g)));
        }
        if let Some(p) = path_prefix {
            conds.push("path LIKE ?".to_string());
            // Экранируем спецсимволы LIKE (%, _) — пользователь может передать `path/with_underscore`.
            let escaped = p.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
            params_dyn.push(Box::new(format!("{}%", escaped)));
        }
        if let Some(l) = language {
            conds.push("language = ?".to_string());
            params_dyn.push(Box::new(l.to_string()));
        }
        let where_clause = if conds.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conds.join(" AND "))
        };
        let sql = format!(
            "SELECT path, language, lines_total, file_size, mtime
             FROM files {} ORDER BY path LIMIT ?",
            where_clause
        );
        params_dyn.push(Box::new(limit as i64));
        let params_refs: Vec<&dyn rusqlite::ToSql> =
            params_dyn.iter().map(|b| &**b as &dyn rusqlite::ToSql).collect();

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_refs.as_slice(), |row| {
            Ok(ListedFile {
                path: row.get::<_, String>(0)?,
                language: row.get::<_, String>(1)?,
                lines_total: row.get::<_, i64>(2)? as usize,
                size: row.get::<_, Option<i64>>(3)?,
                mtime: row.get::<_, Option<i64>>(4)?,
            })
        })?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// read_file_text: прочитать содержимое text-файла.
    /// Для code-файлов (Phase 1) — content в БД нет, возвращается результат
    /// с `category="code"` и пустой строкой; вызывающая сторона должна
    /// сообщить пользователю, что для code-файлов нужно дождаться Phase 2.
    ///
    /// `line_start`/`line_end` — 1-based, оба inclusive. Если оба None —
    /// возвращается весь файл (с применением soft-cap).
    pub fn read_file_text(
        &self,
        path: &str,
        line_start: Option<usize>,
        line_end: Option<usize>,
        soft_cap_lines: usize,
        soft_cap_bytes: usize,
        hard_cap_bytes: usize,
        // Эффективный лимит размера code-файла для этого репо (per-path > [indexer] > 5 МБ).
        // Используется только для заполнения `size_limit` и `hint` в oversize-ответе.
        // None — поля останутся пустыми (например, в тестах или для text-файлов).
        size_limit_bytes: Option<i64>,
    ) -> Result<Option<ReadFileResult>> {
        // Сначала ищем файл в files (берём id, lines_total, indexed_at, file_size)
        let meta: Option<(i64, i64, String, Option<i64>)> = self
            .conn
            .query_row(
                "SELECT id, lines_total, indexed_at, file_size FROM files WHERE path = ?1",
                params![path],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<i64>>(3)?,
                    ))
                },
            )
            .optional()
            .context("read_file_text: ошибка SELECT files")?;

        let Some((file_id, lines_total_i, indexed_at, file_size)) = meta else {
            return Ok(None);
        };
        let lines_total = lines_total_i as usize;

        // Пытаемся прочитать содержимое из text_files (для yaml/md/xml/sh и т.п.)
        let content_opt: Option<String> = self
            .conn
            .query_row(
                "SELECT content FROM text_files WHERE file_id = ?1",
                params![file_id],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .context("read_file_text: ошибка SELECT text_files")?;

        if let Some(content) = content_opt {
            let (sliced, lines_returned, truncated) = slice_with_caps(
                &content,
                line_start,
                line_end,
                soft_cap_lines,
                soft_cap_bytes,
                hard_cap_bytes,
            )?;
            return Ok(Some(ReadFileResult {
                content: sliced,
                lines_returned,
                lines_total,
                truncated,
                indexed_at,
                category: "text".to_string(),
                oversize: false,
                file_size,
                size_limit: None,
                hint: None,
            }));
        }

        // Code-файл (Phase 2): пробуем `file_contents`. Три ветки:
        //   1. Нет записи           → переходное состояние, backfill ещё не дошёл.
        //   2. Запись oversize=1    → файл крупнее лимита, content не сохранён.
        //   3. Запись с blob        → decode zstd, slice, отдать.
        let fc_row = self.read_file_content(file_id)?;
        match fc_row {
            None => {
                // Переходное состояние — пустой content + hint, что backfill ещё в работе.
                Ok(Some(ReadFileResult {
                    content: String::new(),
                    lines_returned: 0,
                    lines_total,
                    truncated: false,
                    indexed_at,
                    category: "code".to_string(),
                    oversize: false,
                    file_size,
                    size_limit: None,
                    hint: Some(
                        "Content code-файла ещё не наполнен (backfill в процессе после v0.8.0). \
                         Перезапустите запрос через несколько секунд или используйте \
                         get_function/get_class/grep_body для целевого чтения."
                            .to_string(),
                    ),
                }))
            }
            Some((None, true)) => {
                // Файл крупнее лимита — намеренно без content.
                let hint = match (file_size, size_limit_bytes) {
                    (Some(fs), Some(lim)) => format!(
                        "Файл превышает лимит сохранения content ({} байт > {} байт). \
                         Используйте get_function/get_class/grep_body для целевого чтения, \
                         либо увеличьте `[indexer].max_code_file_size_bytes` или \
                         `[[paths]].max_code_file_size_bytes` в daemon.toml.",
                        fs, lim
                    ),
                    _ => "Файл oversize: content не сохранён в индексе. \
                          Используйте get_function/get_class/grep_body."
                        .to_string(),
                };
                Ok(Some(ReadFileResult {
                    content: String::new(),
                    lines_returned: 0,
                    lines_total,
                    truncated: false,
                    indexed_at,
                    category: "code".to_string(),
                    oversize: true,
                    file_size,
                    size_limit: size_limit_bytes,
                    hint: Some(hint),
                }))
            }
            Some((Some(content), _)) => {
                // Нормальный case: разжатый content code-файла.
                let (sliced, lines_returned, truncated) = slice_with_caps(
                    &content,
                    line_start,
                    line_end,
                    soft_cap_lines,
                    soft_cap_bytes,
                    hard_cap_bytes,
                )?;
                Ok(Some(ReadFileResult {
                    content: sliced,
                    lines_returned,
                    lines_total,
                    truncated,
                    indexed_at,
                    category: "code".to_string(),
                    oversize: false,
                    file_size,
                    size_limit: None,
                    hint: None,
                }))
            }
            Some((None, false)) => {
                // Невалидное состояние: blob=NULL, oversize=0. По логике записи
                // такого быть не должно — только oversize-запись имеет blob=NULL.
                // Трактуем как переходное состояние (как будто записи нет вовсе).
                Ok(Some(ReadFileResult {
                    content: String::new(),
                    lines_returned: 0,
                    lines_total,
                    truncated: false,
                    indexed_at,
                    category: "code".to_string(),
                    oversize: false,
                    file_size,
                    size_limit: None,
                    hint: Some(
                        "Битая запись file_contents (blob=NULL без oversize). \
                         Перезапустите индексацию репо."
                            .to_string(),
                    ),
                }))
            }
        }
    }

    /// grep_text: regex-поиск по содержимому text-файлов.
    /// Pre-filter через REGEXP в SQL, post-process на номера строк и контекст.
    pub fn grep_text_filtered(
        &self,
        regex_pattern: &str,
        path_glob: Option<&str>,
        language: Option<&str>,
        limit: usize,
        context_lines: usize,
        max_total_bytes: usize,
    ) -> Result<Vec<GrepTextMatch>> {
        let compiled = regex::Regex::new(regex_pattern)
            .context("grep_text: невалидный regex")?;

        // SQL: pre-filter по REGEXP (ускоряет, отсекает файлы без матча).
        let mut conds: Vec<String> = vec!["tf.content REGEXP ?".to_string()];
        let mut params_dyn: Vec<Box<dyn rusqlite::ToSql>> =
            vec![Box::new(regex_pattern.to_string())];
        if let Some(g) = path_glob {
            conds.push("fi.path GLOB ?".to_string());
            params_dyn.push(Box::new(normalize_glob(g)));
        }
        if let Some(l) = language {
            conds.push("fi.language = ?".to_string());
            params_dyn.push(Box::new(l.to_string()));
        }
        let sql = format!(
            "SELECT fi.path, tf.content
             FROM text_files tf JOIN files fi ON fi.id = tf.file_id
             WHERE {}
             ORDER BY fi.path",
            conds.join(" AND ")
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let params_refs: Vec<&dyn rusqlite::ToSql> =
            params_dyn.iter().map(|b| &**b as &dyn rusqlite::ToSql).collect();
        let candidate_rows: Vec<(String, String)> = stmt
            .query_map(params_refs.as_slice(), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        // Post-process: построчный поиск, контекст, ограничение по limit и байтам
        let mut results: Vec<GrepTextMatch> = Vec::new();
        let mut total_bytes: usize = 0;
        for (path, content) in candidate_rows {
            let lines: Vec<&str> = content.lines().collect();
            for (i, line) in lines.iter().enumerate() {
                if !compiled.is_match(line) {
                    continue;
                }
                let line_no = i + 1;
                let context = if context_lines > 0 {
                    let from = i.saturating_sub(context_lines);
                    let to = (i + context_lines + 1).min(lines.len());
                    (from..to)
                        .map(|j| ContextLine {
                            line: j + 1,
                            content: lines[j].to_string(),
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                let row_bytes = line.len()
                    + context.iter().map(|c| c.content.len()).sum::<usize>()
                    + path.len();
                total_bytes = total_bytes.saturating_add(row_bytes);
                if total_bytes > max_total_bytes {
                    return Ok(results);
                }
                results.push(GrepTextMatch {
                    path: path.clone(),
                    line: line_no,
                    content: line.to_string(),
                    context,
                });
                if results.len() >= limit {
                    return Ok(results);
                }
            }
        }
        Ok(results)
    }

    /// grep_body с поддержкой context_lines. Существующий `grep_body` без
    /// контекста остаётся для обратной совместимости (вызовы из cli.rs/тестов).
    /// Этот метод дополнительно набирает строки контекста вокруг каждой
    /// первой партии совпадений (до 3, как у `match_lines`).
    pub fn grep_body_with_options(
        &self,
        pattern: Option<&str>,
        regex_pattern: Option<&str>,
        language: Option<&str>,
        path_glob: Option<&str>,
        limit: usize,
        context_lines: usize,
        max_total_bytes: usize,
    ) -> Result<Vec<GrepBodyMatch>> {
        // Базовое условие body
        let (body_condition, body_param) = match (pattern, regex_pattern) {
            (Some(p), _) => ("body LIKE ?".to_string(), format!("%{}%", p)),
            (_, Some(r)) => ("body REGEXP ?".to_string(), r.to_string()),
            _ => anyhow::bail!("Необходимо указать pattern или regex"),
        };

        // Доп. условия для общей секции (применяются и к functions, и к classes)
        let mut extra_conds: Vec<String> = Vec::new();
        if language.is_some() {
            extra_conds.push("fi.language = ?".to_string());
        }
        if path_glob.is_some() {
            extra_conds.push("fi.path GLOB ?".to_string());
        }
        let extra_clause_str = if extra_conds.is_empty() {
            String::new()
        } else {
            format!(" AND {}", extra_conds.join(" AND "))
        };

        // Параметры дублируются на functions и classes секции UNION'а.
        // Собираем сразу финальный вектор без хитрых клонов Box<dyn ToSql>.
        let lang_norm: Option<String> = language.map(|s| s.to_string());
        let glob_norm: Option<String> = path_glob.map(normalize_glob);
        let mut full_params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        for _ in 0..2 {
            full_params.push(Box::new(body_param.clone()));
            if let Some(ref l) = lang_norm {
                full_params.push(Box::new(l.clone()));
            }
            if let Some(ref g) = glob_norm {
                full_params.push(Box::new(g.clone()));
            }
        }
        full_params.push(Box::new(limit as i64));

        let sql = format!(
            "SELECT fi.path, fn.name, 'function' as kind, fn.line_start, fn.line_end, fn.body
             FROM functions fn JOIN files fi ON fi.id = fn.file_id
             WHERE fn.{cond}{extra}
             UNION ALL
             SELECT fi.path, c.name, 'class' as kind, c.line_start, c.line_end, c.body
             FROM classes c JOIN files fi ON fi.id = c.file_id
             WHERE c.{cond}{extra}
             ORDER BY 1, 4
             LIMIT ?",
            cond = body_condition,
            extra = extra_clause_str
        );

        let mut stmt = self.conn.prepare(&sql)?;
        let params_refs: Vec<&dyn rusqlite::ToSql> =
            full_params.iter().map(|b| &**b as &dyn rusqlite::ToSql).collect();
        let raw: Vec<(String, String, String, i64, i64, String)> = stmt
            .query_map(params_refs.as_slice(), |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let compiled_re = regex_pattern
            .map(regex::Regex::new)
            .transpose()
            .context("grep_body_with_options: невалидный regex")?;

        let mut total_bytes: usize = 0;
        let mut out: Vec<GrepBodyMatch> = Vec::new();
        for (file_path, name, kind, ls, le, body) in raw.into_iter() {
            let line_start = ls as usize;
            let line_end = le as usize;
            let body_lines: Vec<&str> = body.lines().collect();
            let mut all_matches: Vec<usize> = Vec::new(); // индексы строк в body (0-based)
            for (i, line) in body_lines.iter().enumerate() {
                let matched = if let Some(ref re) = compiled_re {
                    re.is_match(line)
                } else if let Some(p) = pattern {
                    line.to_lowercase().contains(&p.to_lowercase())
                } else {
                    false
                };
                if matched {
                    all_matches.push(i);
                }
            }
            let total = all_matches.len();
            let match_lines: Vec<usize> = all_matches
                .iter()
                .take(3)
                .map(|i| line_start + i)
                .collect();
            let match_count = if total > 3 { Some(total) } else { None };
            // Контекст: первые до 3 матчей, по context_lines строк до/после;
            // строки склеиваются в общий список без дублей.
            let context = if context_lines > 0 {
                let mut included: std::collections::BTreeSet<usize> = Default::default();
                for &mi in all_matches.iter().take(3) {
                    let from = mi.saturating_sub(context_lines);
                    let to = (mi + context_lines + 1).min(body_lines.len());
                    for j in from..to {
                        included.insert(j);
                    }
                }
                included
                    .into_iter()
                    .map(|j| ContextLine {
                        line: line_start + j,
                        content: body_lines[j].to_string(),
                    })
                    .collect()
            } else {
                Vec::new()
            };

            let row_bytes = file_path.len()
                + name.len()
                + kind.len()
                + match_lines.len() * 8
                + context.iter().map(|c| c.content.len()).sum::<usize>();
            total_bytes = total_bytes.saturating_add(row_bytes);
            if total_bytes > max_total_bytes {
                return Ok(out);
            }
            out.push(GrepBodyMatch {
                file_path,
                name,
                kind,
                line_start,
                line_end,
                match_lines,
                match_count,
                context,
            });
            if out.len() >= limit {
                return Ok(out);
            }
        }
        Ok(out)
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

/// Нормализация glob-паттерна для SQLite GLOB.
///
/// SQLite GLOB интерпретирует `*` как «любая последовательность символов»,
/// включая `/`. Поэтому `*.py` уже работает рекурсивно.
/// `**` (привычный из shell-glob и .gitignore) — синоним `*` в данном движке,
/// поэтому просто схлопываем все вхождения `**` в `*` для совместимости
/// с привычным синтаксисом, не меняя семантику.
pub(crate) fn normalize_glob(pattern: &str) -> String {
    // Многократная замена для последовательностей `***` и т.п.
    let mut s = pattern.to_string();
    while s.contains("**") {
        s = s.replace("**", "*");
    }
    s
}

/// Слайс контента по диапазону строк (1-based, inclusive) + применение
/// soft-cap по числу строк / байтам и hard-cap (отказ).
///
/// * `line_start`/`line_end` — `Some(_)` — диапазон, `None` — весь файл
/// * `soft_cap_lines` — если итог > этого, обрезается и `truncated=true`
/// * `soft_cap_bytes` — то же по байтам
/// * `hard_cap_bytes` — если результирующий контент превысит этот предел
///   ДАЖЕ после диапазона — функция возвращает Err.
///
/// Возвращает `(content, lines_returned, truncated)`.
pub(crate) fn slice_with_caps(
    content: &str,
    line_start: Option<usize>,
    line_end: Option<usize>,
    soft_cap_lines: usize,
    soft_cap_bytes: usize,
    hard_cap_bytes: usize,
) -> Result<(String, usize, bool)> {
    // `lines()` теряет трейлинг-newline; для read_file семантически достаточно
    // вернуть строки через '\n' — UI-различие незначимо.
    let all_lines: Vec<&str> = content.lines().collect();
    let total = all_lines.len();
    let (start_idx, end_idx) = match (line_start, line_end) {
        (None, None) => (0, total),
        (Some(s), None) => (s.saturating_sub(1).min(total), total),
        (None, Some(e)) => (0, e.min(total)),
        (Some(s), Some(e)) => (
            s.saturating_sub(1).min(total),
            e.min(total),
        ),
    };
    if start_idx > end_idx {
        // Пустой диапазон — возвращаем пусто без ошибки.
        return Ok((String::new(), 0, false));
    }
    let slice_len = end_idx - start_idx;

    // Hard-cap: если запрошенный диапазон по байтам превышает hard_cap — отказ.
    // Считаем байты конкатенированных строк + переносы.
    let est_bytes: usize = all_lines[start_idx..end_idx]
        .iter()
        .map(|l| l.len() + 1)
        .sum();
    if est_bytes > hard_cap_bytes {
        anyhow::bail!(
            "read_file: запрошенный диапазон ~{} байт превышает hard-cap {} байт. \
             Уточните line_start/line_end.",
            est_bytes,
            hard_cap_bytes
        );
    }

    // Soft-cap: применяем меньшее из двух (по строкам / по байтам).
    let mut take_n = slice_len.min(soft_cap_lines);
    // По байтам: укорачиваем до тех пор, пока не вписываемся.
    let mut acc_bytes: usize = 0;
    let mut byte_take_n: usize = 0;
    for line in all_lines[start_idx..start_idx + take_n].iter() {
        let next = acc_bytes + line.len() + 1;
        if next > soft_cap_bytes {
            break;
        }
        acc_bytes = next;
        byte_take_n += 1;
    }
    if byte_take_n < take_n {
        take_n = byte_take_n;
    }
    let truncated = take_n < slice_len;

    let body: String = all_lines[start_idx..start_idx + take_n].join("\n");
    Ok((body, take_n, truncated))
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

    // ── Phase 1 (v0.7.0) тесты ─────────────────────────────────────────────

    /// Создать FileRecord с произвольным путём и языком (mtime/file_size заполнены).
    fn make_file_full(path: &str, language: &str, lines: usize) -> FileRecord {
        FileRecord {
            id: None,
            path: path.to_string(),
            content_hash: format!("hash_{}", path),
            ast_hash: None,
            language: language.to_string(),
            lines_total: lines,
            indexed_at: "2026-04-28T12:00:00".to_string(),
            mtime: Some(1714305600),
            file_size: Some((lines * 50) as i64),
        }
    }

    #[test]
    fn test_normalize_glob_replaces_double_star() {
        assert_eq!(normalize_glob("**/*.py"), "*/*.py");
        assert_eq!(normalize_glob("src/**/file.rs"), "src/*/file.rs");
        assert_eq!(normalize_glob("***/foo"), "*/foo");
        assert_eq!(normalize_glob("*.py"), "*.py");
    }

    #[test]
    fn test_slice_with_caps_full_file() {
        let content = "line1\nline2\nline3\nline4\nline5";
        let (body, n, truncated) = slice_with_caps(content, None, None, 100, 1000, 10_000).unwrap();
        assert_eq!(n, 5);
        assert!(!truncated);
        assert_eq!(body, "line1\nline2\nline3\nline4\nline5");
    }

    #[test]
    fn test_slice_with_caps_range() {
        let content = "a\nb\nc\nd\ne";
        let (body, n, truncated) = slice_with_caps(content, Some(2), Some(4), 100, 1000, 10_000).unwrap();
        assert_eq!(n, 3);
        assert!(!truncated);
        assert_eq!(body, "b\nc\nd");
    }

    #[test]
    fn test_slice_with_caps_soft_cap_lines() {
        let content = (1..=10).map(|i| format!("line{}", i)).collect::<Vec<_>>().join("\n");
        let (body, n, truncated) = slice_with_caps(&content, None, None, 3, 1000, 10_000).unwrap();
        assert_eq!(n, 3);
        assert!(truncated);
        assert_eq!(body, "line1\nline2\nline3");
    }

    #[test]
    fn test_slice_with_caps_hard_cap() {
        let content: String = "x".repeat(1000);
        let res = slice_with_caps(&content, None, None, 10_000, 100_000, 100);
        assert!(res.is_err(), "превышение hard-cap должно дать Err");
    }

    #[test]
    fn test_stat_file_meta_existing_text() {
        let storage = Storage::open_in_memory().unwrap();
        let id = storage.upsert_file(&make_file_full("/cfg.yaml", "yaml", 50)).unwrap();
        // upsert_file пока не пишет mtime/file_size — это делает отдельный update_file_metadata.
        storage.update_file_metadata("/cfg.yaml", 1714305600, 2500).unwrap();
        // Помечаем как text-файл (есть запись в text_files).
        storage.insert_text_file(&TextFileRecord {
            id: None,
            file_id: id,
            content: "key: value\n".repeat(50),
        }).unwrap();

        let r = storage.stat_file_meta("/cfg.yaml").unwrap();
        assert!(r.exists);
        assert_eq!(r.path, "/cfg.yaml");
        assert_eq!(r.language.as_deref(), Some("yaml"));
        assert_eq!(r.lines_total, Some(50));
        assert_eq!(r.mtime, Some(1714305600));
        assert_eq!(r.size, Some(2500));
        assert_eq!(r.category.as_deref(), Some("text"));
    }

    #[test]
    fn test_stat_file_meta_existing_code() {
        let storage = Storage::open_in_memory().unwrap();
        storage.upsert_file(&make_file_full("/lib.py", "python", 30)).unwrap();
        // Без insert_text_file — это code-файл
        let r = storage.stat_file_meta("/lib.py").unwrap();
        assert!(r.exists);
        assert_eq!(r.category.as_deref(), Some("code"));
    }

    #[test]
    fn test_stat_file_meta_missing() {
        let storage = Storage::open_in_memory().unwrap();
        let r = storage.stat_file_meta("/nonexistent").unwrap();
        assert!(!r.exists);
        assert!(r.language.is_none());
    }

    #[test]
    fn test_list_files_pattern_glob() {
        let storage = Storage::open_in_memory().unwrap();
        storage.upsert_file(&make_file_full("/src/auth/login.py", "python", 10)).unwrap();
        storage.upsert_file(&make_file_full("/src/utils/helpers.py", "python", 20)).unwrap();
        storage.upsert_file(&make_file_full("/docs/readme.md", "markdown", 30)).unwrap();

        let py = storage.list_files_filtered(Some("**/*.py"), None, None, 100).unwrap();
        assert_eq!(py.len(), 2);
        for f in &py { assert!(f.path.ends_with(".py")); }

        let auth = storage.list_files_filtered(Some("/src/auth/*"), None, None, 100).unwrap();
        assert_eq!(auth.len(), 1);
        assert_eq!(auth[0].path, "/src/auth/login.py");
    }

    #[test]
    fn test_list_files_path_prefix() {
        let storage = Storage::open_in_memory().unwrap();
        storage.upsert_file(&make_file_full("/src/a.py", "python", 1)).unwrap();
        storage.upsert_file(&make_file_full("/src/b.py", "python", 1)).unwrap();
        storage.upsert_file(&make_file_full("/test/c.py", "python", 1)).unwrap();

        let r = storage.list_files_filtered(None, Some("/src/"), None, 100).unwrap();
        assert_eq!(r.len(), 2);
        for f in &r { assert!(f.path.starts_with("/src/")); }
    }

    #[test]
    fn test_list_files_language_filter() {
        let storage = Storage::open_in_memory().unwrap();
        storage.upsert_file(&make_file_full("/a.py", "python", 1)).unwrap();
        storage.upsert_file(&make_file_full("/b.rs", "rust", 1)).unwrap();
        storage.upsert_file(&make_file_full("/c.py", "python", 1)).unwrap();

        let r = storage.list_files_filtered(None, None, Some("rust"), 100).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].language, "rust");
    }

    #[test]
    fn test_read_file_text_full() {
        let storage = Storage::open_in_memory().unwrap();
        let id = storage.upsert_file(&make_file_full("/r.txt", "text", 3)).unwrap();
        storage.insert_text_file(&TextFileRecord {
            id: None,
            file_id: id,
            content: "alpha\nbeta\ngamma".to_string(),
        }).unwrap();
        let r = storage.read_file_text("/r.txt", None, None, 100, 10_000, 100_000, None).unwrap().unwrap();
        assert_eq!(r.category, "text");
        assert_eq!(r.lines_returned, 3);
        assert_eq!(r.lines_total, 3);
        assert!(!r.truncated);
        assert_eq!(r.content, "alpha\nbeta\ngamma");
    }

    #[test]
    fn test_read_file_text_range() {
        let storage = Storage::open_in_memory().unwrap();
        let id = storage.upsert_file(&make_file_full("/r.txt", "text", 5)).unwrap();
        storage.insert_text_file(&TextFileRecord {
            id: None,
            file_id: id,
            content: "1\n2\n3\n4\n5".to_string(),
        }).unwrap();
        let r = storage.read_file_text("/r.txt", Some(2), Some(4), 100, 10_000, 100_000, None)
            .unwrap().unwrap();
        assert_eq!(r.lines_returned, 3);
        assert_eq!(r.content, "2\n3\n4");
    }

    #[test]
    fn test_read_file_text_code_returns_empty_category_code() {
        let storage = Storage::open_in_memory().unwrap();
        storage.upsert_file(&make_file_full("/lib.py", "python", 10)).unwrap();
        // text_files не заполнен — это code-файл
        let r = storage.read_file_text("/lib.py", None, None, 100, 10_000, 100_000, None).unwrap().unwrap();
        assert_eq!(r.category, "code");
        assert!(r.content.is_empty());
    }

    #[test]
    fn test_read_file_text_missing() {
        let storage = Storage::open_in_memory().unwrap();
        let r = storage.read_file_text("/nope", None, None, 100, 10_000, 100_000, None).unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn test_grep_text_basic_match() {
        let storage = Storage::open_in_memory().unwrap();
        let id = storage.upsert_file(&make_file_full("/cfg.yaml", "yaml", 5)).unwrap();
        storage.insert_text_file(&TextFileRecord {
            id: None,
            file_id: id,
            content: "host: 10.0.0.1\nport: 8080\nname: example\n".to_string(),
        }).unwrap();
        let m = storage.grep_text_filtered(r"port:\s*\d+", None, None, 100, 0, 1_000_000).unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].path, "/cfg.yaml");
        assert_eq!(m[0].line, 2);
        assert!(m[0].content.contains("port: 8080"));
        assert!(m[0].context.is_empty());
    }

    #[test]
    fn test_grep_text_with_context() {
        let storage = Storage::open_in_memory().unwrap();
        let id = storage.upsert_file(&make_file_full("/log.txt", "text", 5)).unwrap();
        storage.insert_text_file(&TextFileRecord {
            id: None,
            file_id: id,
            content: "a\nb\nFOUND\nd\ne".to_string(),
        }).unwrap();
        let m = storage.grep_text_filtered(r"FOUND", None, None, 100, 1, 1_000_000).unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].context.len(), 3); // строки 2, 3, 4
        assert_eq!(m[0].context[0].line, 2);
        assert_eq!(m[0].context[0].content, "b");
        assert_eq!(m[0].context[1].line, 3);
        assert_eq!(m[0].context[2].line, 4);
    }

    #[test]
    fn test_grep_text_path_glob_filters() {
        let storage = Storage::open_in_memory().unwrap();
        let id1 = storage.upsert_file(&make_file_full("/a.yaml", "yaml", 1)).unwrap();
        let id2 = storage.upsert_file(&make_file_full("/b.json", "json", 1)).unwrap();
        storage.insert_text_file(&TextFileRecord { id: None, file_id: id1, content: "key: 42".into() }).unwrap();
        storage.insert_text_file(&TextFileRecord { id: None, file_id: id2, content: "{\"key\": 42}".into() }).unwrap();
        let m = storage.grep_text_filtered(r"42", Some("*.yaml"), None, 100, 0, 1_000_000).unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].path, "/a.yaml");
    }

    #[test]
    fn test_grep_body_with_options_context() {
        let storage = Storage::open_in_memory().unwrap();
        let file_id = storage.upsert_file(&make_file_full("/code.py", "python", 30)).unwrap();
        let mut fr = make_function(file_id, "do_thing");
        fr.line_start = 10;
        fr.line_end = 14;
        fr.body = "def do_thing():\n    target = 1\n    other = 2\n    return target".to_string();
        storage.insert_functions(&[fr]).unwrap();

        let m = storage.grep_body_with_options(
            Some("target"), None, None, None, 50, 1, 1_000_000,
        ).unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].name, "do_thing");
        assert!(!m[0].match_lines.is_empty());
        assert!(!m[0].context.is_empty(), "context_lines=1 должен дать контекст");
    }

    #[test]
    fn test_get_path_by_file_id() {
        let storage = Storage::open_in_memory().unwrap();
        let id = storage.upsert_file(&make_file_full("/some/path.py", "python", 1)).unwrap();
        let p = storage.get_path_by_file_id(id).unwrap();
        assert_eq!(p, Some("/some/path.py".to_string()));
        let none = storage.get_path_by_file_id(99999).unwrap();
        assert_eq!(none, None);
    }

    // ── Phase 2 (v0.8.0) тесты: file_contents + zstd ──────────────────────────

    // ── upsert / read / has ────────────────────────────────────────────────────

    /// Базовый round-trip: запись → zstd-сжатие → чтение → распаковка.
    /// Содержимое должно вернуться без изменений.
    #[test]
    fn test_upsert_file_content_round_trip() {
        let storage = Storage::open_in_memory().unwrap();
        let file_id = storage.upsert_file(&make_file_full("/src/app.py", "python", 5)).unwrap();

        storage.upsert_file_content(file_id, "hello world", 1024).unwrap();

        let result = storage.read_file_content(file_id).unwrap();
        assert_eq!(
            result,
            Some((Some("hello world".to_string()), false)),
            "ожидается нормальная запись с разжатым content"
        );
        assert!(
            storage.has_file_content(file_id).unwrap(),
            "has_file_content должен вернуть true"
        );
    }

    /// Если content длиннее max_size_bytes — должна создаться oversize-запись.
    #[test]
    fn test_upsert_file_content_oversize() {
        let storage = Storage::open_in_memory().unwrap();
        let file_id = storage.upsert_file(&make_file_full("/big.py", "python", 1000)).unwrap();

        // 100 байт > 50 байт лимита → oversize
        let big_content: String = "x".repeat(100);
        storage.upsert_file_content(file_id, &big_content, 50).unwrap();

        let result = storage.read_file_content(file_id).unwrap();
        assert_eq!(
            result,
            Some((None, true)),
            "ожидается oversize-запись: (None, true)"
        );
        assert!(
            storage.has_file_content(file_id).unwrap(),
            "has_file_content должен быть true даже для oversize"
        );
    }

    /// Повторный upsert на тот же file_id должен заменить предыдущую запись
    /// (INSERT OR REPLACE). read_file_content отдаёт второй content.
    #[test]
    fn test_upsert_file_content_idempotent_replace() {
        let storage = Storage::open_in_memory().unwrap();
        let file_id = storage.upsert_file(&make_file_full("/mod.py", "python", 10)).unwrap();

        storage.upsert_file_content(file_id, "first content", 4096).unwrap();
        storage.upsert_file_content(file_id, "second content", 4096).unwrap();

        let result = storage.read_file_content(file_id).unwrap();
        assert_eq!(
            result,
            Some((Some("second content".to_string()), false)),
            "второй upsert должен заменить первый"
        );
    }

    /// Для file_id, у которого нет записи в file_contents, read_file_content
    /// должен вернуть None (переходное состояние — backfill ещё не дошёл).
    #[test]
    fn test_read_file_content_missing_returns_none() {
        let storage = Storage::open_in_memory().unwrap();
        let file_id = storage.upsert_file(&make_file_full("/norecord.py", "python", 5)).unwrap();

        // Запись в files есть, но file_contents — нет
        let result = storage.read_file_content(file_id).unwrap();
        assert!(result.is_none(), "нет записи в file_contents → None");

        assert!(
            !storage.has_file_content(file_id).unwrap(),
            "has_file_content должен быть false"
        );
    }

    // ── delete ─────────────────────────────────────────────────────────────────

    /// После delete_file_content запись исчезает — read возвращает None, has = false.
    #[test]
    fn test_delete_file_content_removes_entry() {
        let storage = Storage::open_in_memory().unwrap();
        let file_id = storage.upsert_file(&make_file_full("/del.py", "python", 3)).unwrap();
        storage.upsert_file_content(file_id, "some code", 4096).unwrap();
        assert!(storage.has_file_content(file_id).unwrap());

        storage.delete_file_content(file_id).unwrap();

        assert!(
            storage.read_file_content(file_id).unwrap().is_none(),
            "после delete read_file_content должен вернуть None"
        );
        assert!(
            !storage.has_file_content(file_id).unwrap(),
            "после delete has_file_content должен быть false"
        );
    }

    // ── get_file_id_by_path ────────────────────────────────────────────────────

    /// get_file_id_by_path: путь есть → Some(id), нет → None.
    #[test]
    fn test_get_file_id_by_path_found_and_missing() {
        let storage = Storage::open_in_memory().unwrap();
        let id = storage.upsert_file(&make_file_full("/exists.py", "python", 1)).unwrap();

        let found = storage.get_file_id_by_path("/exists.py").unwrap();
        assert_eq!(found, Some(id), "путь есть — должен вернуть правильный id");

        let missing = storage.get_file_id_by_path("/missing.py").unwrap();
        assert!(missing.is_none(), "пути нет — должен вернуть None");
    }

    // ── has_text_file ─────────────────────────────────────────────────────────

    /// has_text_file: true для файлов с записью в text_files, false без неё.
    #[test]
    fn test_has_text_file_true_for_text_files() {
        let storage = Storage::open_in_memory().unwrap();
        let text_id = storage.upsert_file(&make_file_full("/readme.md", "markdown", 10)).unwrap();
        let code_id = storage.upsert_file(&make_file_full("/lib.rs", "rust", 20)).unwrap();

        storage.insert_text_file(&TextFileRecord {
            id: None,
            file_id: text_id,
            content: "# README\n".to_string(),
        }).unwrap();

        assert!(
            storage.has_text_file(text_id).unwrap(),
            "text-файл с записью в text_files → true"
        );
        assert!(
            !storage.has_text_file(code_id).unwrap(),
            "code-файл без записи в text_files → false"
        );
    }

    // ── read_file_text для code-файлов (Phase 2) ───────────────────────────────

    /// Нормальный case: code-файл с записью в file_contents → category="code",
    /// content правильно разжат, oversize=false.
    #[test]
    fn test_read_file_text_for_code_returns_decoded() {
        let storage = Storage::open_in_memory().unwrap();
        let file_id = storage
            .upsert_file(&make_file_full("/src/utils.py", "python", 3))
            .unwrap();
        let source = "def hello():\n    pass\n# конец";
        storage.upsert_file_content(file_id, source, 4096).unwrap();

        let r = storage
            .read_file_text("/src/utils.py", None, None, 1000, 1_000_000, 10_000_000, None)
            .unwrap()
            .expect("файл должен существовать");

        assert_eq!(r.category, "code");
        assert_eq!(r.content, source);
        assert!(!r.oversize, "нормальная запись → oversize=false");
        assert!(r.hint.is_none(), "нормальная запись → hint отсутствует");
    }

    /// Oversize case: уведомление через oversize=true и заполненный hint.
    /// При указанном size_limit_bytes hint содержит числовые размеры.
    #[test]
    fn test_read_file_text_for_code_oversize_returns_hint() {
        let storage = Storage::open_in_memory().unwrap();
        let file_id = storage
            .upsert_file(&make_file_full("/huge.bsl", "bsl", 500))
            .unwrap();
        // Устанавливаем file_size = 200 через update_file_metadata, чтобы hint мог показать размер
        storage.update_file_metadata("/huge.bsl", 1714305600, 200).unwrap();

        // content 100 байт > лимит 50
        let big: String = "a".repeat(100);
        storage.upsert_file_content(file_id, &big, 50).unwrap();

        // С явным size_limit_bytes — hint должен содержать оба числа
        let r = storage
            .read_file_text("/huge.bsl", None, None, 1000, 1_000_000, 10_000_000, Some(50))
            .unwrap()
            .expect("файл должен существовать");

        assert_eq!(r.category, "code");
        assert!(r.content.is_empty(), "oversize — content пустой");
        assert!(r.oversize, "oversize → true");
        let hint = r.hint.expect("hint должен быть заполнен");
        assert!(
            hint.contains("200") && hint.contains("50"),
            "hint должен содержать file_size=200 и size_limit=50, получили: {hint}"
        );

        // Без size_limit_bytes — hint всё равно Some (общая формулировка)
        let r2 = storage
            .read_file_text("/huge.bsl", None, None, 1000, 1_000_000, 10_000_000, None)
            .unwrap()
            .expect("файл должен существовать");
        assert!(r2.oversize);
        assert!(r2.hint.is_some(), "hint должен быть и без size_limit_bytes");
    }

    /// Переходное состояние v0.7.x→v0.8.0: файл в files есть, но в file_contents
    /// записи нет (backfill ещё не дошёл). read_file_text → category="code",
    /// content пустой, oversize=false, hint содержит слово "backfill".
    #[test]
    fn test_read_file_text_for_code_no_record_returns_transitional_hint() {
        let storage = Storage::open_in_memory().unwrap();
        // Файл только в files — file_contents пустой
        storage
            .upsert_file(&make_file_full("/old.py", "python", 20))
            .unwrap();

        let r = storage
            .read_file_text("/old.py", None, None, 1000, 1_000_000, 10_000_000, None)
            .unwrap()
            .expect("файл должен существовать");

        assert_eq!(r.category, "code");
        assert!(r.content.is_empty(), "нет записи → content пустой");
        assert!(!r.oversize, "нет записи ≠ oversize");
        let hint = r.hint.expect("переходное состояние должно давать hint");
        assert!(
            hint.to_lowercase().contains("backfill"),
            "hint должен упоминать backfill, получили: {hint}"
        );
    }

    // ── stat_file_meta с oversize (Phase 2) ───────────────────────────────────

    /// stat_file для code-файла с oversize → category="code", oversize=Some(true).
    #[test]
    fn test_stat_file_for_code_with_oversize() {
        let storage = Storage::open_in_memory().unwrap();
        let file_id = storage
            .upsert_file(&make_file_full("/heavy.rs", "rust", 200))
            .unwrap();
        // Запись oversize: content 100 байт > лимит 10
        let big: String = "r".repeat(100);
        storage.upsert_file_content(file_id, &big, 10).unwrap();

        let r = storage.stat_file_meta("/heavy.rs").unwrap();
        assert!(r.exists);
        assert_eq!(r.category.as_deref(), Some("code"));
        assert_eq!(r.oversize, Some(true), "oversize-запись → oversize=Some(true)");
    }

    /// stat_file для обычного code-файла (нормальная запись) → oversize=Some(false).
    #[test]
    fn test_stat_file_for_code_normal_oversize_false() {
        let storage = Storage::open_in_memory().unwrap();
        let file_id = storage
            .upsert_file(&make_file_full("/small.rs", "rust", 10))
            .unwrap();
        storage.upsert_file_content(file_id, "fn main() {}", 4096).unwrap();

        let r = storage.stat_file_meta("/small.rs").unwrap();
        assert!(r.exists);
        assert_eq!(r.category.as_deref(), Some("code"));
        assert_eq!(r.oversize, Some(false), "нормальная запись → oversize=Some(false)");
    }

    /// stat_file для text-файла → category="text", oversize=None (поле не заполняется).
    #[test]
    fn test_stat_file_for_text_no_oversize() {
        let storage = Storage::open_in_memory().unwrap();
        let file_id = storage
            .upsert_file(&make_file_full("/config.yaml", "yaml", 20))
            .unwrap();
        storage.insert_text_file(&TextFileRecord {
            id: None,
            file_id,
            content: "key: value\n".to_string(),
        }).unwrap();

        let r = storage.stat_file_meta("/config.yaml").unwrap();
        assert!(r.exists);
        assert_eq!(r.category.as_deref(), Some("text"));
        assert!(
            r.oversize.is_none(),
            "для text-файлов oversize не заполняется: {:?}",
            r.oversize
        );
    }

    // ── grep_code_filtered ────────────────────────────────────────────────────

    /// Базовый поиск: паттерн есть в одном из двух файлов → один матч.
    #[test]
    fn test_grep_code_finds_pattern() {
        let storage = Storage::open_in_memory().unwrap();
        let id1 = storage
            .upsert_file(&make_file_full("/a.py", "python", 3))
            .unwrap();
        let id2 = storage
            .upsert_file(&make_file_full("/b.py", "python", 3))
            .unwrap();

        storage
            .upsert_file_content(id1, "def foo():\n    specific_word\n", 4096)
            .unwrap();
        storage
            .upsert_file_content(id2, "def bar():\n    nothing_here\n", 4096)
            .unwrap();

        let m = storage
            .grep_code_filtered("specific_word", None, None, 100, 0, 1_000_000)
            .unwrap();
        assert_eq!(m.len(), 1, "только один файл должен совпасть");
        assert_eq!(m[0].path, "/a.py");
    }

    /// Oversize-файлы должны пропускаться — content у них не сохранён.
    #[test]
    fn test_grep_code_skips_oversize() {
        let storage = Storage::open_in_memory().unwrap();
        let id1 = storage
            .upsert_file(&make_file_full("/normal.py", "python", 3))
            .unwrap();
        let id2 = storage
            .upsert_file(&make_file_full("/giant.py", "python", 10000))
            .unwrap();

        // Нормальный файл содержит паттерн
        storage
            .upsert_file_content(id1, "TARGET_PATTERN in normal file", 4096)
            .unwrap();
        // Oversize-запись: content не сохранён (лимит 1 байт)
        storage
            .upsert_file_content(id2, "TARGET_PATTERN in oversize", 1)
            .unwrap();

        let m = storage
            .grep_code_filtered("TARGET_PATTERN", None, None, 100, 0, 1_000_000)
            .unwrap();
        assert_eq!(m.len(), 1, "oversize-файл должен быть пропущен");
        assert_eq!(m[0].path, "/normal.py");
    }

    /// path_glob сужает поиск до подходящих путей.
    #[test]
    fn test_grep_code_path_glob_filter() {
        let storage = Storage::open_in_memory().unwrap();
        let id1 = storage
            .upsert_file(&make_file_full("/src/match.py", "python", 2))
            .unwrap();
        let id2 = storage
            .upsert_file(&make_file_full("/test/no_match.py", "python", 2))
            .unwrap();

        storage
            .upsert_file_content(id1, "NEEDLE found here", 4096)
            .unwrap();
        storage
            .upsert_file_content(id2, "NEEDLE found here too", 4096)
            .unwrap();

        // Ищем только в /src/
        let m = storage
            .grep_code_filtered("NEEDLE", Some("/src/*"), None, 100, 0, 1_000_000)
            .unwrap();
        assert_eq!(m.len(), 1, "glob должен ограничить поиск до /src/");
        assert_eq!(m[0].path, "/src/match.py");
    }

    /// context_lines возвращает строки до и после совпадения.
    #[test]
    fn test_grep_code_context_lines() {
        let storage = Storage::open_in_memory().unwrap();
        let file_id = storage
            .upsert_file(&make_file_full("/ctx.py", "python", 5))
            .unwrap();
        // Совпадение на 3-й строке из 5
        storage
            .upsert_file_content(file_id, "before1\nbefore2\nMATCH\nafter1\nafter2", 4096)
            .unwrap();

        let m = storage
            .grep_code_filtered("MATCH", None, None, 100, 1, 1_000_000)
            .unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].line, 3, "MATCH на строке 3");
        // context_lines=1: строки 2, 3, 4
        assert_eq!(m[0].context.len(), 3, "должно быть 3 строки контекста (до, матч, после)");
        let lines: Vec<usize> = m[0].context.iter().map(|c| c.line).collect();
        assert!(lines.contains(&2), "должна быть строка 2");
        assert!(lines.contains(&3), "должна быть строка 3");
        assert!(lines.contains(&4), "должна быть строка 4");
    }

    /// limit ограничивает количество возвращаемых результатов.
    #[test]
    fn test_grep_code_respects_limit() {
        let storage = Storage::open_in_memory().unwrap();
        // Создаём 5 файлов с паттерном
        for i in 0..5 {
            let path = format!("/file{}.py", i);
            let file_id = storage
                .upsert_file(&make_file_full(&path, "python", 1))
                .unwrap();
            storage
                .upsert_file_content(file_id, &format!("COMMON_PATTERN in file {}", i), 4096)
                .unwrap();
        }

        let m = storage
            .grep_code_filtered("COMMON_PATTERN", None, None, 2, 0, 1_000_000)
            .unwrap();
        assert_eq!(m.len(), 2, "limit=2 должен вернуть ровно 2 результата");
    }

    // ── migrate_v4 идемпотентность ────────────────────────────────────────────

    /// migrate_v4 должна быть идемпотентной: второй вызов не должен давать ошибку,
    /// и в таблицу можно писать после любого количества вызовов.
    #[test]
    fn test_migrate_v4_idempotent() {
        use crate::storage::schema;
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();

        // Создаём полную схему (включает migrate_v4 внутри)
        schema::initialize(&conn).unwrap();

        // Повторный вызов не должен ломаться (CREATE TABLE IF NOT EXISTS)
        schema::migrate_v4(&conn).unwrap();

        // Убеждаемся что таблица рабочая после повторного вызова
        conn.execute(
            "INSERT INTO files (path, content_hash, language) VALUES ('/t.py', 'h', 'python')",
            [],
        ).unwrap();
        let file_id: i64 = conn
            .query_row("SELECT id FROM files WHERE path = '/t.py'", [], |r| r.get(0))
            .unwrap();
        conn.execute(
            "INSERT INTO file_contents (file_id, content_blob, oversize) VALUES (?1, NULL, 1)",
            rusqlite::params![file_id],
        ).unwrap();

        let oversize: i64 = conn
            .query_row(
                "SELECT oversize FROM file_contents WHERE file_id = ?1",
                rusqlite::params![file_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(oversize, 1, "после идемпотентных вызовов таблица должна работать");
    }
}
