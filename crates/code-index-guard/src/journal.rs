//! База событий хука (необязательная): одно решение хука — одна строка
//! в таблице `hook_events` базы SQLite.
//!
//! Включается только явно: путь к базе передаёт вызывающий (ключ `events_db`
//! конфига гарда); нет пути — события не записываются.
//! Схема таблицы совместима с общим журналом хуков Claude Code, так что гард
//! может писать в одну базу с соседними хуками.
//!
//! Хуки вызывают [`record`] ровно один раз за запуск, в точке, где решение
//! окончательно (включая воздержание). Ошибка журналирования (нет каталога,
//! база занята, битый JSON) не должна влиять на работу хука — [`record`]
//! ничего не возвращает и никогда не паникует.

use rusqlite::{params, Connection};
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

/// Схема журнала: таблица и индексы создаются при каждом открытии.
const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS hook_events (
  id INTEGER PRIMARY KEY,
  ts TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f','now','localtime')),
  hook TEXT NOT NULL, event TEXT, session_id TEXT, agent_id TEXT, entrypoint TEXT,
  cwd TEXT, tool_name TEXT, target TEXT, decision TEXT NOT NULL, reason TEXT,
  duration_ms INTEGER, details TEXT);
CREATE INDEX IF NOT EXISTS hook_events_ts ON hook_events(ts);
CREATE INDEX IF NOT EXISTS hook_events_hook ON hook_events(hook, decision);
CREATE INDEX IF NOT EXISTS hook_events_session ON hook_events(session_id);";

/// Решение одного запуска хука.
pub struct Event<'a> {
    /// Имя хука, напр. "code-index-guard"
    pub hook: &'a str,
    /// allow | deny | skip | inject | update | error
    pub decision: &'a str,
    /// JSON, пришедший хуку на stdin
    pub input: Option<&'a serde_json::Value>,
    /// путь файла / имя агента / начало команды
    pub target: Option<&'a str>,
    /// текст отказа или причина пропуска
    pub reason: Option<&'a str>,
    /// всё специфичное для хука
    pub details: Option<serde_json::Value>,
    /// начало работы хука — для duration_ms
    pub started: Option<std::time::Instant>,
}

/// Строковое поле входного JSON хука
fn text<'a>(input: Option<&'a Value>, key: &str) -> Option<&'a str> {
    input?.get(key)?.as_str()
}

/// Записать решение хука в журнал `db`. `None` — журнал выключен.
/// Ничего не возвращает, ошибки глотает.
pub fn record(db: Option<&Path>, e: Event) {
    if let Some(path) = db {
        let _ = write_row(path, e);
    }
}

fn write_row(path: &Path, e: Event) -> Option<()> {
    // Каталога нет — не создаём его, просто выходим
    if !path.parent().is_some_and(|d| d.is_dir()) {
        return None;
    }
    let conn = Connection::open(path).ok()?;
    conn.busy_timeout(Duration::from_millis(2000)).ok()?;
    let _ = conn.execute_batch("PRAGMA journal_mode=WAL;");
    conn.execute_batch(SCHEMA).ok()?;
    let details = e
        .details
        .as_ref()
        .and_then(|d| serde_json::to_string(d).ok());
    let duration_ms = e.started.map(|s| s.elapsed().as_millis() as i64);
    let entrypoint = std::env::var("CLAUDE_CODE_ENTRYPOINT")
        .ok()
        .filter(|s| !s.is_empty());
    conn.execute(
        "INSERT INTO hook_events (ts, hook, event, session_id, agent_id, entrypoint, cwd, tool_name,
                                  target, decision, reason, duration_ms, details)
         VALUES (strftime('%Y-%m-%dT%H:%M:%f','now','localtime'),
                 ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            e.hook,
            text(e.input, "hook_event_name"),
            text(e.input, "session_id"),
            text(e.input, "agent_id"),
            entrypoint,
            text(e.input, "cwd"),
            text(e.input, "tool_name"),
            e.target,
            e.decision,
            e.reason,
            duration_ms,
            details,
        ],
    )
    .ok()?;
    // Каждая тысячная запись — повод подчистить журнал старше 90 дней
    if conn.last_insert_rowid() % 1000 == 0 {
        let _ = conn.execute(
            "DELETE FROM hook_events WHERE ts < strftime('%Y-%m-%dT%H:%M:%f','now','localtime','-90 days')",
            [],
        );
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Instant;

    fn event<'a>(decision: &'a str, input: Option<&'a Value>) -> Event<'a> {
        Event {
            hook: "test-hook",
            decision,
            input,
            target: Some("C:/work/src/main.rs"),
            reason: Some("причина отказа"),
            details: Some(json!({"k": 1})),
            started: Some(Instant::now()),
        }
    }

    #[test]
    fn record_into_temp_db() {
        let unique = format!(
            "hook-events-test-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let db = std::env::temp_dir().join(unique);
        let _ = std::fs::remove_file(&db);

        let input = json!({
            "session_id": "s-1",
            "agent_id": "a-1",
            "cwd": "C:/work",
            "tool_name": "Read",
            "hook_event_name": "PreToolUse",
        });
        record(Some(&db), event("deny", Some(&input)));

        let conn = Connection::open(&db).unwrap();
        // Кортеж из одиннадцати полей — форма выборки в тесте, выносить его
        // в отдельный тип незачем
        #[allow(clippy::type_complexity)]
        let row: (
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            String,
            Option<String>,
            Option<i64>,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT hook, event, session_id, agent_id, cwd, tool_name, target, decision,
                        reason, duration_ms, details FROM hook_events",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                        r.get(9)?,
                        r.get(10)?,
                    ))
                },
            )
            .unwrap();
        drop(conn);
        assert_eq!(row.0, "test-hook");
        assert_eq!(row.1.as_deref(), Some("PreToolUse"));
        assert_eq!(row.2.as_deref(), Some("s-1"));
        assert_eq!(row.3.as_deref(), Some("a-1"));
        assert_eq!(row.4.as_deref(), Some("C:/work"));
        assert_eq!(row.5.as_deref(), Some("Read"));
        assert_eq!(row.6.as_deref(), Some("C:/work/src/main.rs"));
        assert_eq!(row.7, "deny");
        assert_eq!(row.8.as_deref(), Some("причина отказа"));
        assert!(row.9.is_some());
        assert_eq!(row.10.as_deref(), Some("{\"k\":1}"));

        let _ = std::fs::remove_file(&db);
        let _ = std::fs::remove_file(db.with_extension("db-wal"));
        let _ = std::fs::remove_file(db.with_extension("db-shm"));
    }

    #[test]
    fn missing_dir_is_skipped_silently() {
        let missing = std::env::temp_dir()
            .join("hook-events-no-such-dir")
            .join("hook-events.db");
        record(Some(&missing), event("skip", None));
        assert!(!missing.exists());
        assert!(!missing.parent().unwrap().exists());
    }

    #[test]
    fn no_path_means_no_journal() {
        // Журнал выключен: вызов ничего не делает и не паникует
        record(None, event("skip", None));
    }
}
