// MCP-tool `get_data_links` — окрестность объекта 1С в графе связей данных.
//
// Отвечает на вопросы «на что ссылается объект» (direction=out) и
// «кто ссылается на объект» (direction=in) по таблице `data_links`,
// собирая рёбра до глубины `depth` через recursive CTE.
//
// Закрывает паттерн «блуждания по структуре»: вместо N последовательных
// get_metadata_structure модель одним вызовом получает кластер связей
// вокруг объекта (например, AccumulationRegister.ТоварыНаСкладах →
// измерения Номенклатура/Склад/... → их типы).
//
// Терминальные `*`-узлы (is_universal: *CatalogRef / *AnyRef /
// *DefinedType.X) не разворачиваются дальше — у них нет исходящих рёбер,
// обход на них естественно останавливается (защита от fan-out и шума).

use std::future::Future;
use std::pin::Pin;

use code_index_core::extension::{IndexTool, ToolContext};
use rusqlite::params;
use serde_json::{json, Value};

pub struct GetDataLinksTool;

impl IndexTool for GetDataLinksTool {
    fn name(&self) -> &str {
        "get_data_links"
    }

    fn description(&self) -> &str {
        "Возвращает связи данных объекта конфигурации 1С по таблице data_links: \
         'out' — на какие объекты ссылается (реквизиты/измерения ссылочного \
         типа), 'in' — какие объекты ссылаются на него. Обходит граф до глубины \
         depth (по умолчанию 1, максимум 4). Заменяет серию get_metadata_structure \
         при анализе связей. Цель вида '*CatalogRef'/'*AnyRef'/'*DefinedType.X' — \
         обобщённая ссылка (терминал, дальше не разворачивается). For BSL/1C repositories only."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo": { "type": "string", "description": "Алиас репозитория" },
                "object": {
                    "type": "string",
                    "description": "Канонический объект, например 'Document.РеализацияТоваровУслуг' или 'AccumulationRegister.ТоварыНаСкладах'"
                },
                "direction": {
                    "type": "string",
                    "enum": ["out", "in", "both"],
                    "description": "out — на что ссылается; in — кто ссылается; both — оба. По умолчанию both.",
                    "default": "both"
                },
                "depth": {
                    "type": "integer",
                    "description": "Глубина обхода (число шагов). По умолчанию 1, максимум 4.",
                    "default": 1,
                    "minimum": 1,
                    "maximum": 4
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
            let direction = args
                .get("direction")
                .and_then(|v| v.as_str())
                .unwrap_or("both");
            let depth: i64 = args
                .get("depth")
                .and_then(|v| v.as_i64())
                .unwrap_or(1)
                .clamp(1, 4);

            let storage = ctx.storage.lock().await;
            let conn = storage.conn();

            let mut result = json!({ "object": object, "depth": depth });

            if direction == "out" || direction == "both" {
                match query_links(conn, &object, depth, Direction::Out) {
                    Ok(v) => {
                        result["out"] = Value::Array(v);
                    }
                    Err(e) => return crate::tools::wrap_error(json!({"error": format!("database error (out): {}", e)})),
                }
            }
            if direction == "in" || direction == "both" {
                match query_links(conn, &object, depth, Direction::In) {
                    Ok(v) => {
                        result["in"] = Value::Array(v);
                    }
                    Err(e) => return crate::tools::wrap_error(json!({"error": format!("database error (in): {}", e)})),
                }
            }

            crate::tools::wrap_with_meta(result, Vec::new())
        })
    }
}

enum Direction {
    Out,
    In,
}

/// Собрать рёбра окрестности объекта в заданном направлении до глубины depth.
/// Out: идём по from_object → to_object (на что ссылается).
/// In:  идём по to_object → from_object (кто ссылается).
/// Терминальные `*`-узлы (is_universal=1) не разворачиваются на следующий шаг.
fn query_links(
    conn: &rusqlite::Connection,
    object: &str,
    depth: i64,
    dir: Direction,
) -> rusqlite::Result<Vec<Value>> {
    // Для out стартовая привязка по from_object, переход by to_object.
    // Для in — зеркально (start by to_object, переход by from_object).
    let sql = match dir {
        Direction::Out => "
            WITH RECURSIVE walk(from_object, from_path, to_object, link_kind, is_composite, is_universal, depth) AS (
                SELECT from_object, from_path, to_object, link_kind, is_composite, is_universal, 1
                FROM data_links WHERE repo = ?1 AND from_object = ?2
                UNION ALL
                SELECT dl.from_object, dl.from_path, dl.to_object, dl.link_kind, dl.is_composite, dl.is_universal, w.depth + 1
                FROM walk w
                JOIN data_links dl ON dl.repo = ?1 AND dl.from_object = w.to_object
                WHERE w.depth < ?3 AND w.is_universal = 0
            )
            SELECT DISTINCT from_object, from_path, to_object, link_kind, is_composite, is_universal, depth
            FROM walk ORDER BY depth, from_object, from_path
        ",
        Direction::In => "
            WITH RECURSIVE walk(from_object, from_path, to_object, link_kind, is_composite, is_universal, depth) AS (
                SELECT from_object, from_path, to_object, link_kind, is_composite, is_universal, 1
                FROM data_links WHERE repo = ?1 AND to_object = ?2
                UNION ALL
                SELECT dl.from_object, dl.from_path, dl.to_object, dl.link_kind, dl.is_composite, dl.is_universal, w.depth + 1
                FROM walk w
                JOIN data_links dl ON dl.repo = ?1 AND dl.to_object = w.from_object
                WHERE w.depth < ?3
            )
            SELECT DISTINCT from_object, from_path, to_object, link_kind, is_composite, is_universal, depth
            FROM walk ORDER BY depth, from_object, from_path
        ",
    };

    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params!["default", object, depth], |r| {
        Ok(json!({
            "from_object": r.get::<_, String>(0)?,
            "from_path": r.get::<_, String>(1)?,
            "to_object": r.get::<_, String>(2)?,
            "link_kind": r.get::<_, String>(3)?,
            "is_composite": r.get::<_, i64>(4)? != 0,
            "is_universal": r.get::<_, i64>(5)? != 0,
            "depth": r.get::<_, i64>(6)?,
        }))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}
