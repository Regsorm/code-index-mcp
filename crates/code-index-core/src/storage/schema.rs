/// SQL для создания индексов (отдельная константа, чтобы переиспользовать при rebuild)
pub const INDEXES_SQL: &str = "
CREATE INDEX IF NOT EXISTS idx_files_path         ON files(path);
CREATE INDEX IF NOT EXISTS idx_files_hash         ON files(content_hash);
CREATE INDEX IF NOT EXISTS idx_functions_name     ON functions(name);
CREATE INDEX IF NOT EXISTS idx_functions_qname    ON functions(qualified_name);
CREATE INDEX IF NOT EXISTS idx_functions_file     ON functions(file_id);
CREATE INDEX IF NOT EXISTS idx_classes_name       ON classes(name);
CREATE INDEX IF NOT EXISTS idx_classes_file       ON classes(file_id);
CREATE INDEX IF NOT EXISTS idx_imports_module     ON imports(module);
CREATE INDEX IF NOT EXISTS idx_imports_name       ON imports(name);
CREATE INDEX IF NOT EXISTS idx_imports_file       ON imports(file_id);
CREATE INDEX IF NOT EXISTS idx_calls_caller       ON calls(caller);
CREATE INDEX IF NOT EXISTS idx_calls_callee       ON calls(callee);
CREATE INDEX IF NOT EXISTS idx_calls_file         ON calls(file_id);
CREATE INDEX IF NOT EXISTS idx_variables_name     ON variables(name);
CREATE INDEX IF NOT EXISTS idx_variables_file     ON variables(file_id);
";

/// SQL для создания FTS-триггеров (отдельная константа, чтобы переиспользовать при rebuild)
pub const TRIGGERS_SQL: &str = "
-- Триггеры синхронизации FTS: functions

CREATE TRIGGER IF NOT EXISTS fts_functions_insert
AFTER INSERT ON functions BEGIN
    INSERT INTO fts_functions(rowid, name, qualified_name, docstring, body)
    VALUES (new.id, new.name, new.qualified_name, new.docstring, new.body);
END;

CREATE TRIGGER IF NOT EXISTS fts_functions_delete
AFTER DELETE ON functions BEGIN
    INSERT INTO fts_functions(fts_functions, rowid, name, qualified_name, docstring, body)
    VALUES ('delete', old.id, old.name, old.qualified_name, old.docstring, old.body);
END;

CREATE TRIGGER IF NOT EXISTS fts_functions_update
AFTER UPDATE ON functions BEGIN
    INSERT INTO fts_functions(fts_functions, rowid, name, qualified_name, docstring, body)
    VALUES ('delete', old.id, old.name, old.qualified_name, old.docstring, old.body);
    INSERT INTO fts_functions(rowid, name, qualified_name, docstring, body)
    VALUES (new.id, new.name, new.qualified_name, new.docstring, new.body);
END;

-- Триггеры синхронизации FTS: classes

CREATE TRIGGER IF NOT EXISTS fts_classes_insert
AFTER INSERT ON classes BEGIN
    INSERT INTO fts_classes(rowid, name, docstring, body)
    VALUES (new.id, new.name, new.docstring, new.body);
END;

CREATE TRIGGER IF NOT EXISTS fts_classes_delete
AFTER DELETE ON classes BEGIN
    INSERT INTO fts_classes(fts_classes, rowid, name, docstring, body)
    VALUES ('delete', old.id, old.name, old.docstring, old.body);
END;

CREATE TRIGGER IF NOT EXISTS fts_classes_update
AFTER UPDATE ON classes BEGIN
    INSERT INTO fts_classes(fts_classes, rowid, name, docstring, body)
    VALUES ('delete', old.id, old.name, old.docstring, old.body);
    INSERT INTO fts_classes(rowid, name, docstring, body)
    VALUES (new.id, new.name, new.docstring, new.body);
END;

-- Триггеры синхронизации FTS: text_files

CREATE TRIGGER IF NOT EXISTS fts_text_files_insert
AFTER INSERT ON text_files BEGIN
    INSERT INTO fts_text_files(rowid, content)
    VALUES (new.id, new.content);
END;

CREATE TRIGGER IF NOT EXISTS fts_text_files_delete
AFTER DELETE ON text_files BEGIN
    INSERT INTO fts_text_files(fts_text_files, rowid, content)
    VALUES ('delete', old.id, old.content);
END;

CREATE TRIGGER IF NOT EXISTS fts_text_files_update
AFTER UPDATE ON text_files BEGIN
    INSERT INTO fts_text_files(fts_text_files, rowid, content)
    VALUES ('delete', old.id, old.content);
    INSERT INTO fts_text_files(rowid, content)
    VALUES (new.id, new.content);
END;
";

/// Полная SQL-схема базы данных
pub const SQL_SCHEMA: &str = "
-- Основные таблицы

CREATE TABLE IF NOT EXISTS files (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    path         TEXT    NOT NULL UNIQUE,
    content_hash TEXT    NOT NULL,
    ast_hash     TEXT,
    language     TEXT    NOT NULL,
    lines_total  INTEGER NOT NULL DEFAULT 0,
    indexed_at   TEXT    NOT NULL DEFAULT (datetime('now')),
    mtime        INTEGER,
    file_size    INTEGER
);

CREATE TABLE IF NOT EXISTS functions (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    file_id        INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    name           TEXT    NOT NULL,
    qualified_name TEXT,
    line_start     INTEGER NOT NULL DEFAULT 0,
    line_end       INTEGER NOT NULL DEFAULT 0,
    args           TEXT,
    return_type    TEXT,
    docstring      TEXT,
    body           TEXT    NOT NULL DEFAULT '',
    is_async       INTEGER NOT NULL DEFAULT 0,
    node_hash      TEXT    NOT NULL DEFAULT '',
    override_type   TEXT,
    override_target TEXT
);

CREATE TABLE IF NOT EXISTS classes (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    file_id    INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    name       TEXT    NOT NULL,
    line_start INTEGER NOT NULL DEFAULT 0,
    line_end   INTEGER NOT NULL DEFAULT 0,
    bases      TEXT,
    docstring  TEXT,
    body       TEXT    NOT NULL DEFAULT '',
    node_hash  TEXT    NOT NULL DEFAULT ''
);

CREATE TABLE IF NOT EXISTS imports (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    module  TEXT,
    name    TEXT,
    alias   TEXT,
    line    INTEGER NOT NULL DEFAULT 0,
    kind    TEXT    NOT NULL DEFAULT 'import'
);

CREATE TABLE IF NOT EXISTS calls (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    caller  TEXT    NOT NULL,
    callee  TEXT    NOT NULL,
    line    INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS variables (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    name    TEXT    NOT NULL,
    value   TEXT,
    line    INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS text_files (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    content TEXT    NOT NULL DEFAULT ''
);

-- FTS5 виртуальные таблицы для полнотекстового поиска

CREATE VIRTUAL TABLE IF NOT EXISTS fts_functions USING fts5(
    name,
    qualified_name,
    docstring,
    body,
    content='functions',
    content_rowid='id'
);

CREATE VIRTUAL TABLE IF NOT EXISTS fts_classes USING fts5(
    name,
    docstring,
    body,
    content='classes',
    content_rowid='id'
);

CREATE VIRTUAL TABLE IF NOT EXISTS fts_text_files USING fts5(
    content,
    content='text_files',
    content_rowid='id'
);
";

/// Создать только таблицы и FTS-виртуальные таблицы БЕЗ индексов и триггеров.
///
/// Используется при массовой первичной загрузке — индексы создаются после INSERT,
/// что значительно ускоряет процесс (один проход вместо инкрементальных обновлений).
pub fn initialize_tables_only(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.execute_batch("
        PRAGMA journal_mode=WAL;
        PRAGMA synchronous=NORMAL;
        PRAGMA foreign_keys=ON;
        PRAGMA cache_size=-64000;
        PRAGMA mmap_size=268435456;
    ")?;
    // Только таблицы + FTS-виртуальные таблицы — без INDEXES_SQL и TRIGGERS_SQL
    conn.execute_batch(SQL_SCHEMA)?;
    migrate_v2(conn)?;
    migrate_v3(conn)?;
    migrate_v4(conn)?;
    Ok(())
}

/// Инициализирует базу данных: применяет PRAGMA и создаёт схему
pub fn initialize(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    // Включаем WAL для параллельного чтения/записи
    conn.execute_batch("PRAGMA journal_mode=WAL;")?;
    // Снижаем нагрузку на диск — допускаем задержку fsync
    conn.execute_batch("PRAGMA synchronous=NORMAL;")?;
    // Принудительно включаем поддержку внешних ключей
    conn.execute_batch("PRAGMA foreign_keys=ON;")?;
    // Кеш ~64 МБ (отрицательное значение — в кибибайтах)
    conn.execute_batch("PRAGMA cache_size=-64000;")?;
    // Memory-mapped I/O: 256 МБ — снижает количество read/write syscall на диске
    conn.execute_batch("PRAGMA mmap_size=268435456;")?;
    // Агрессивный auto-checkpoint WAL: каждые 500 страниц (~2 МБ). Без этого
    // при длинных транзакциях (update metadata на 93К файлов) WAL растёт до
    // многогигабайтных размеров и забивает диск.
    conn.execute_batch("PRAGMA wal_autocheckpoint=500;")?;
    // Предельный размер WAL — после checkpoint файл truncate'ится до 64 МБ
    conn.execute_batch("PRAGMA journal_size_limit=67108864;")?;
    // Применяем DDL-схему: таблицы и FTS-виртуальные таблицы
    conn.execute_batch(SQL_SCHEMA)?;
    migrate_v2(conn)?;
    migrate_v3(conn)?;
    migrate_v4(conn)?;
    // Создаём триггеры
    conn.execute_batch(TRIGGERS_SQL)?;
    // Создаём индексы
    conn.execute_batch(INDEXES_SQL)?;
    Ok(())
}

/// Инициализация для режима только чтения — без записи в БД.
/// Устанавливает только read-safe PRAGMA, не создаёт таблиц и индексов.
pub fn initialize_readonly(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.execute_batch("PRAGMA foreign_keys=ON;")?;
    conn.execute_batch("PRAGMA cache_size=-64000;")?;
    conn.execute_batch("PRAGMA query_only=ON;")?;
    Ok(())
}

/// Миграция v2: добавить колонки override_type/override_target для BSL-расширений.
/// Безопасно вызывать повторно — проверяет наличие колонки перед ALTER.
pub fn migrate_v2(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    let has_col = conn
        .prepare("SELECT override_type FROM functions LIMIT 0")
        .is_ok();
    if !has_col {
        conn.execute_batch(
            "ALTER TABLE functions ADD COLUMN override_type TEXT;
             ALTER TABLE functions ADD COLUMN override_target TEXT;",
        )?;
    }
    Ok(())
}

/// Миграция v3: добавить колонки mtime/file_size в таблицу files для mtime pre-filter.
/// Безопасно вызывать повторно — проверяет наличие колонки перед ALTER.
pub fn migrate_v3(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    let has_col = conn
        .prepare("SELECT mtime FROM files LIMIT 0")
        .is_ok();
    if !has_col {
        conn.execute_batch(
            "ALTER TABLE files ADD COLUMN mtime INTEGER;
             ALTER TABLE files ADD COLUMN file_size INTEGER;",
        )?;
    }
    Ok(())
}

/// Миграция v4 (Phase 2): таблица `file_contents` для хранения сжатого content
/// code-файлов (.py/.bsl/.rs/.ts и т.п.). Text-файлы продолжают жить в `text_files`.
///
/// Структура:
///   * `file_id`      — PK, FK на `files(id)` с каскадным удалением.
///   * `content_blob` — содержимое, сжатое zstd. NULL для oversize-файлов.
///   * `oversize`     — 0/1 флаг: файл превышает `max_code_file_size_bytes` и не сохранён.
///
/// Зачем oversize-флаг отдельно: позволяет отличить «backfill ещё не дошёл до этого
/// файла» (записи нет) от «файл больше лимита, content намеренно не сохранён»
/// (запись есть, oversize=1, blob=NULL).
pub fn migrate_v4(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS file_contents (
            file_id      INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE,
            content_blob BLOB,
            oversize     INTEGER NOT NULL DEFAULT 0
         );",
    )?;
    Ok(())
}

/// Удалить все обычные индексы и FTS-триггеры (перед bulk-load).
///
/// Вызывается перед массовой загрузкой данных, чтобы ускорить INSERT:
/// без индексов и триггеров вставка работает значительно быстрее.
pub fn drop_indexes_and_triggers(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.execute_batch("
        -- Удаляем индексы на таблице files
        DROP INDEX IF EXISTS idx_files_path;
        DROP INDEX IF EXISTS idx_files_hash;

        -- Удаляем индексы на таблице functions
        DROP INDEX IF EXISTS idx_functions_name;
        DROP INDEX IF EXISTS idx_functions_qname;
        DROP INDEX IF EXISTS idx_functions_file;

        -- Удаляем индексы на таблице classes
        DROP INDEX IF EXISTS idx_classes_name;
        DROP INDEX IF EXISTS idx_classes_file;

        -- Удаляем индексы на таблице imports
        DROP INDEX IF EXISTS idx_imports_module;
        DROP INDEX IF EXISTS idx_imports_name;
        DROP INDEX IF EXISTS idx_imports_file;

        -- Удаляем индексы на таблице calls
        DROP INDEX IF EXISTS idx_calls_caller;
        DROP INDEX IF EXISTS idx_calls_callee;
        DROP INDEX IF EXISTS idx_calls_file;

        -- Удаляем индексы на таблице variables
        DROP INDEX IF EXISTS idx_variables_name;
        DROP INDEX IF EXISTS idx_variables_file;

        -- Удаляем FTS-триггеры functions
        DROP TRIGGER IF EXISTS fts_functions_insert;
        DROP TRIGGER IF EXISTS fts_functions_delete;
        DROP TRIGGER IF EXISTS fts_functions_update;

        -- Удаляем FTS-триггеры classes
        DROP TRIGGER IF EXISTS fts_classes_insert;
        DROP TRIGGER IF EXISTS fts_classes_delete;
        DROP TRIGGER IF EXISTS fts_classes_update;

        -- Удаляем FTS-триггеры text_files
        DROP TRIGGER IF EXISTS fts_text_files_insert;
        DROP TRIGGER IF EXISTS fts_text_files_delete;
        DROP TRIGGER IF EXISTS fts_text_files_update;
    ")?;
    Ok(())
}

/// Пересоздать индексы и FTS-триггеры, затем перестроить FTS-индексы (после bulk-load).
///
/// Последовательность:
/// 1. Пересоздаём обычные индексы (один проход по данным — дешевле инкрементальных обновлений).
/// 2. Пересоздаём FTS-триггеры для будущих изменений.
/// 3. Rebuild FTS-индексов из уже загруженных данных (команда 'rebuild').
pub fn rebuild_indexes_and_triggers(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    // Пересоздаём обычные индексы
    conn.execute_batch(INDEXES_SQL)?;

    // Пересоздаём FTS-триггеры
    conn.execute_batch(TRIGGERS_SQL)?;

    // Перестраиваем FTS-индексы из данных основных таблиц
    conn.execute_batch("
        INSERT INTO fts_functions(fts_functions) VALUES('rebuild');
        INSERT INTO fts_classes(fts_classes) VALUES('rebuild');
        INSERT INTO fts_text_files(fts_text_files) VALUES('rebuild');
    ")?;

    Ok(())
}
