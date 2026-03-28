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
    indexed_at   TEXT    NOT NULL DEFAULT (datetime('now'))
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
    node_hash      TEXT    NOT NULL DEFAULT ''
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

-- Индексы для ускорения поиска

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
    // Применяем DDL-схему
    conn.execute_batch(SQL_SCHEMA)?;
    Ok(())
}
