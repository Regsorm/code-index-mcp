// Чтение/запись/проверка `enrichment_signature` в `embedding_meta`.
//
// Подпись — отпечаток `<provider>:<model>` из `[enrichment]`. Нужна, чтобы
// детектировать рассинхрон: оператор поменял модель в конфиге, но в БД
// уже накоплены тысячи процедур, обогащённых старой моделью. Их выводы
// несравнимы напрямую (разные стили формулировок, разный лексикон).
//
// Поведение по карточке 261:
//   * подписей нет → пишем текущую, считаем что только что начали;
//   * подписи совпали → продолжаем как обычно;
//   * подписи разошлись → warning + рекомендация `bsl-indexer enrich --reenrich`.
//     Старт демона/индекса не валим — данные остаются полезными, просто
//     результаты search_terms могут смешивать стили.
//
// Эта логика не требует HTTP-клиента и нужна даже при `bsl-indexer index`
// (для записи начальной подписи), поэтому модуль НЕ под feature `enrichment`.
// Сами вызовы из CLI/индексации — будут добавлены позже.

use anyhow::Result;
use rusqlite::{params, Connection};

/// Ключ в `embedding_meta` под подпись текущей модели обогащения.
pub const ENRICHMENT_SIG_KEY: &str = "enrichment_signature";

/// Прочитать сохранённую подпись. None если ни разу не записывали.
pub fn read_signature(conn: &Connection) -> Result<Option<String>> {
    let v = conn
        .query_row(
            "SELECT value FROM embedding_meta WHERE key = ?",
            params![ENRICHMENT_SIG_KEY],
            |r| r.get::<_, String>(0),
        )
        .ok();
    Ok(v)
}

/// Записать новую подпись (UPSERT). updated_at заполняется триггером DEFAULT.
pub fn write_signature(conn: &Connection, sig: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO embedding_meta (key, value) VALUES (?, ?) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, \
                                         updated_at = CAST(strftime('%s','now') AS INTEGER)",
        params![ENRICHMENT_SIG_KEY, sig],
    )?;
    Ok(())
}

/// Сверить текущую подпись с сохранённой и вернуть состояние:
///   * `SignatureCheck::Fresh` — подписи нет, БД пустая для enrichment;
///   * `SignatureCheck::Match` — совпала, всё ок;
///   * `SignatureCheck::Mismatch { stored }` — разошлась, требуется решение.
///
/// Запись новой подписи остаётся на ответственности вызывающего —
/// `Fresh` намекает «можно записать», `Mismatch` намекает «не пиши,
/// сначала пусть человек решит».
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignatureCheck {
    Fresh,
    Match,
    Mismatch { stored: String },
}

pub fn check_signature(conn: &Connection, expected: &str) -> Result<SignatureCheck> {
    match read_signature(conn)? {
        None => Ok(SignatureCheck::Fresh),
        Some(s) if s == expected => Ok(SignatureCheck::Match),
        Some(s) => Ok(SignatureCheck::Mismatch { stored: s }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn fresh_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        for ddl in crate::schema::SCHEMA_EXTENSIONS {
            conn.execute_batch(ddl).unwrap();
        }
        conn
    }

    #[test]
    fn fresh_when_table_is_empty() {
        let conn = fresh_db();
        assert_eq!(check_signature(&conn, "anything").unwrap(), SignatureCheck::Fresh);
    }

    #[test]
    fn match_after_write() {
        let conn = fresh_db();
        write_signature(&conn, "openai_compatible:claude-haiku-4.5").unwrap();
        assert_eq!(
            check_signature(&conn, "openai_compatible:claude-haiku-4.5").unwrap(),
            SignatureCheck::Match
        );
    }

    #[test]
    fn mismatch_when_changed() {
        let conn = fresh_db();
        write_signature(&conn, "openai_compatible:claude-haiku-4.5").unwrap();
        match check_signature(&conn, "openai_compatible:gpt-5-mini").unwrap() {
            SignatureCheck::Mismatch { stored } => {
                assert_eq!(stored, "openai_compatible:claude-haiku-4.5");
            }
            other => panic!("ожидался Mismatch, получили {:?}", other),
        }
    }

    #[test]
    fn write_is_idempotent_upsert() {
        let conn = fresh_db();
        write_signature(&conn, "v1").unwrap();
        write_signature(&conn, "v2").unwrap();
        assert_eq!(read_signature(&conn).unwrap().as_deref(), Some("v2"));
    }
}
