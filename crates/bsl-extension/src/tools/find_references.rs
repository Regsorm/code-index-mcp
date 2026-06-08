// MCP-tool `find_references` — ВСЁ, что ссылается на объект метаданных 1С,
// одним вызовом. Объединяет три источника обратных ссылок, собранных
// index_extras:
//
//   * data_links (реверс по to_object, idx_dl_to) — кто ссылается на объект
//     через реквизиты/измерения (attr/tabular_attr/register_dim), движения
//     (recorder), владение (owner), а также состав подсистем/планов обмена и
//     определяемые типы (subsystem_content/exchange_plan_content/
//     defined_type_content) — «структурные» ссылки из метаданных-XML;
//   * metadata_code_usages (по object_ref) — обращения В КОДЕ (.bsl): менеджер
//     коллекции, тип-ссылка в литерале, путь метаданных в тексте запроса;
//   * role_rights (по object_name) — какие роли выдают права на объект.
//
// Заменяет три отдельных запроса (get_data_links direction=in + bsl_sql по
// metadata_code_usages + bsl_sql по role_rights) одним компактным ответом —
// «карта влияния» объекта: что сломается/затронется при его изменении.
//
// Счётчики (total + разбивка по видам) считаются точно; примеры (sample)
// ограничены `limit` на секцию (по умолчанию 20), чтобы ответ оставался лёгким
// для часто-ссылаемых объектов (Контрагенты, Номенклатура — тысячи ссылок).

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use code_index_core::extension::{IndexTool, ToolContext};
use rusqlite::params;
use serde_json::{json, Map, Value};

const DEFAULT_LIMIT: i64 = 20;
const MAX_LIMIT: i64 = 200;
/// Таймаут набора запросов (как в bsl_sql): по истечении sqlite3_interrupt
/// обрывает runaway COUNT/GROUP BY на центральных объектах (десятки тысяч рёбер).
const QUERY_TIMEOUT_SECS: u64 = 8;

pub struct FindReferencesTool;

impl IndexTool for FindReferencesTool {
    fn name(&self) -> &str {
        "find_references"
    }

    fn description(&self) -> &str {
        "Всё, что ссылается на объект метаданных 1С, одним вызовом — «карта \
         влияния» (что затронется при изменении объекта). Объединяет три источника: \
         data_refs — структурные ссылки из метаданных (реквизиты/измерения других \
         объектов, движения, владение, состав подсистем/планов обмена, определяемые \
         типы); code_usages — обращения В КОДЕ (.bsl: менеджер коллекции, тип-ссылка, \
         путь в запросе); role_rights — какие роли выдают права на объект. У каждой \
         секции total + разбивка по видам + примеры (sample, ограничены параметром \
         limit). Объект задаётся канонически: 'Document.РеализацияТоваровУслуг', \
         'Catalog.Контрагенты', 'AccumulationRegister.ТоварыНаСкладах'. Реверс \
         data_links здесь — это direction=in у get_data_links плюс ещё code_usages и \
         role_rights в одном ответе. For BSL/1C repositories only."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo": { "type": "string", "description": "Алиас репозитория" },
                "object": {
                    "type": "string",
                    "description": "Канонический объект: 'Document.РеализацияТоваровУслуг', 'Catalog.Контрагенты', 'AccumulationRegister.ТоварыНаСкладах' и т.п."
                },
                "limit": {
                    "type": "integer",
                    "description": "Потолок примеров (sample) на секцию (default 20, max 200). На счётчики total не влияет.",
                    "minimum": 1
                }
            },
            "required": ["repo", "object"]
        })
    }

    fn applicable_languages(&self) -> Option<&'static [&'static str]> {
        Some(&["bsl"])
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        ctx: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = Value> + Send + 'a>> {
        Box::pin(async move {
            let object = match args.get("object").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => {
                    return crate::tools::wrap_error(json!({
                        "error": "missing required parameter 'object' (string)"
                    }));
                }
            };
            let limit = args
                .get("limit")
                .and_then(|v| v.as_i64())
                .unwrap_or(DEFAULT_LIMIT)
                .clamp(1, MAX_LIMIT);
            // object_ref_key для metadata_code_usages — лоуэркейс с кириллицей
            // (SQLite lower() кириллицу не берёт, поэтому считаем в Rust).
            let object_key = object.to_lowercase();

            let storage = match ctx.storage.get().await {
                Ok(s) => s,
                Err(e) => {
                    return crate::tools::wrap_error(serde_json::json!({
                        "error": format!("storage pool: {}", e)
                    }));
                }
            };
            let conn = storage.conn();

            // interrupt-таймаут против runaway COUNT/GROUP BY на больших
            // data_links / metadata_code_usages (центральные объекты — десятки
            // тысяч рёбер). Тот же паттерн, что в bsl_sql: handle живёт в отдельной
            // задаче, по истечении дёргает sqlite3_interrupt; после сбора гасим.
            let handle = conn.get_interrupt_handle();
            let timer = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(QUERY_TIMEOUT_SECS)).await;
                handle.interrupt();
            });

            let data_refs = match query_data_refs(conn, &object_key, limit) {
                Ok(v) => v,
                Err(e) => {
                    timer.abort();
                    return crate::tools::wrap_error(json!({
                        "error": format!("database error (data_refs): {}", e)
                    }));
                }
            };
            let code_usages = match query_code_usages(conn, &object_key, limit) {
                Ok(v) => v,
                Err(e) => {
                    timer.abort();
                    return crate::tools::wrap_error(json!({
                        "error": format!("database error (code_usages): {}", e)
                    }));
                }
            };
            let role_rights = match query_role_rights(conn, &object_key, limit) {
                Ok(v) => v,
                Err(e) => {
                    timer.abort();
                    return crate::tools::wrap_error(json!({
                        "error": format!("database error (role_rights): {}", e)
                    }));
                }
            };
            timer.abort();

            crate::tools::wrap_with_meta(
                json!({
                    "object": object,
                    "data_refs": data_refs,
                    "code_usages": code_usages,
                    "role_rights": role_rights,
                }),
                Vec::new(),
            )
        })
    }
}

/// Структурные ссылки на объект (реверс data_links по to_object).
fn query_data_refs(
    conn: &rusqlite::Connection,
    object_key: &str,
    limit: i64,
) -> rusqlite::Result<Value> {
    // Фильтр по to_object_key (= lower(to_object)): регистронезависимо по
    // кириллице, через индекс idx_dl_to_key.
    let mut by_kind = Map::new();
    let mut total: i64 = 0;
    {
        let mut stmt = conn.prepare(
            "SELECT link_kind, COUNT(*) FROM data_links \
             WHERE repo = ?1 AND to_object_key = ?2 GROUP BY link_kind ORDER BY link_kind",
        )?;
        let rows = stmt.query_map(params!["default", object_key], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (kind, cnt) = row?;
            total += cnt;
            by_kind.insert(kind, json!(cnt));
        }
    }
    let mut sample = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT from_object, from_path, link_kind FROM data_links \
             WHERE repo = ?1 AND to_object_key = ?2 ORDER BY link_kind, from_object LIMIT ?3",
        )?;
        let rows = stmt.query_map(params!["default", object_key, limit], |r| {
            Ok(json!({
                "from_object": r.get::<_, String>(0)?,
                "from_path": r.get::<_, String>(1)?,
                "link_kind": r.get::<_, String>(2)?,
            }))
        })?;
        for row in rows {
            sample.push(row?);
        }
    }
    Ok(json!({ "total": total, "by_link_kind": by_kind, "sample": sample }))
}

/// Обращения к объекту в коде (metadata_code_usages по точному object_ref).
fn query_code_usages(
    conn: &rusqlite::Connection,
    object_key: &str,
    limit: i64,
) -> rusqlite::Result<Value> {
    // Фильтр по object_ref_key (= lower(object_ref)): регистронезависимо по
    // кириллице И задействует индекс idx_mcu_ref(repo, object_ref_key) вместо
    // полного скана по неиндексированному object_ref.
    let mut by_kind = Map::new();
    let mut total: i64 = 0;
    {
        let mut stmt = conn.prepare(
            "SELECT usage_kind, COUNT(*) FROM metadata_code_usages \
             WHERE repo = ?1 AND object_ref_key = ?2 GROUP BY usage_kind ORDER BY usage_kind",
        )?;
        let rows = stmt.query_map(params!["default", object_key], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (kind, cnt) = row?;
            total += cnt;
            by_kind.insert(kind, json!(cnt));
        }
    }
    let mut sample = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT file_path, line, usage_kind, member_path FROM metadata_code_usages \
             WHERE repo = ?1 AND object_ref_key = ?2 ORDER BY usage_kind, file_path, line LIMIT ?3",
        )?;
        let rows = stmt.query_map(params!["default", object_key, limit], |r| {
            Ok(json!({
                "file_path": r.get::<_, String>(0)?,
                "line": r.get::<_, i64>(1)?,
                "usage_kind": r.get::<_, String>(2)?,
                "member_path": r.get::<_, Option<String>>(3)?,
            }))
        })?;
        for row in rows {
            sample.push(row?);
        }
    }
    Ok(json!({ "total": total, "by_usage_kind": by_kind, "sample": sample }))
}

/// Права ролей на объект (role_rights по object_name).
fn query_role_rights(
    conn: &rusqlite::Connection,
    object_key: &str,
    limit: i64,
) -> rusqlite::Result<Value> {
    // Фильтр по object_name_key (= lower(object_name)): регистронезависимо, idx_rr_object_key.
    let (total, roles): (i64, i64) = conn.query_row(
        "SELECT COUNT(*), COUNT(DISTINCT role_name) FROM role_rights \
         WHERE repo = ?1 AND object_name_key = ?2",
        params!["default", object_key],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let mut sample = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT role_name, right_name FROM role_rights \
             WHERE repo = ?1 AND object_name_key = ?2 ORDER BY role_name, right_name LIMIT ?3",
        )?;
        let rows = stmt.query_map(params!["default", object_key, limit], |r| {
            Ok(json!({
                "role_name": r.get::<_, String>(0)?,
                "right_name": r.get::<_, String>(1)?,
            }))
        })?;
        for row in rows {
            sample.push(row?);
        }
    }
    Ok(json!({ "total": total, "roles": roles, "sample": sample }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        for ddl in crate::schema::SCHEMA_EXTENSIONS {
            conn.execute_batch(ddl).unwrap();
        }
        conn
    }

    /// Регистронезависимость по кириллице: lowercase-ввод находит данные через
    /// *_key-колонки (раньше canonical-only-фильтр давал тихий total=0).
    #[test]
    fn data_refs_and_role_rights_case_insensitive_by_key() {
        let conn = mem();
        conn.execute(
            "INSERT INTO data_links \
             (repo, from_object, from_path, to_object, link_kind, to_object_key) \
             VALUES ('default','AccumulationRegister.Продажи','Заказ', \
                     'Document.ЗаказКлиента','attr','document.заказклиента')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO role_rights \
             (repo, role_name, object_name, right_name, object_name_key) \
             VALUES ('default','Менеджер','Document.ЗаказКлиента','Read','document.заказклиента')",
            [],
        )
        .unwrap();

        // Вызов с lowercase-ключом (как делает execute: object.to_lowercase()).
        let dr = query_data_refs(&conn, "document.заказклиента", 20).unwrap();
        assert_eq!(dr["total"], json!(1), "data_refs должен найти по lower-ключу");
        let rr = query_role_rights(&conn, "document.заказклиента", 20).unwrap();
        assert_eq!(rr["total"], json!(1), "role_rights должен найти по lower-ключу");

        // Несуществующий ключ — пусто (sanity).
        let none = query_data_refs(&conn, "document.нетакого", 20).unwrap();
        assert_eq!(none["total"], json!(0));
    }
}
