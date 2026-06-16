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
//
// Защита контекста: каждое направление ограничено `limit` рёбрами
// (default 100, max 1000). При превышении возвращаются первые `limit`
// рёбер, рядом — `out_total`/`out_truncated` (и `in_*`), чтобы модель
// видела полный размер и при необходимости сузила запрос (direction/depth)
// или дослала больший limit. Без этого «центральные» объекты (ЗаказКлиента,
// Номенклатура) отдавали сотни-тысячи рёбер и переполняли контекст агента.

use std::future::Future;
use std::pin::Pin;

use code_index_core::extension::{IndexTool, ToolContext};
use rusqlite::params;
use serde_json::{json, Value};

/// Потолок рёбер на одно направление по умолчанию.
const DEFAULT_LIMIT: i64 = 100;
/// Жёсткий максимум (защита от выгрузки тысяч рёбер в контекст).
const MAX_LIMIT: i64 = 1000;

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
         обобщённая ссылка (терминал, дальше не разворачивается). Каждое направление \
         ограничено параметром limit (default 100, max 1000); при превышении рядом с \
         out/in отдаются out_total/out_truncated (и in_total/in_truncated). \
         For BSL/1C repositories only."
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
                },
                "limit": {
                    "type": "integer",
                    "description": "Потолок рёбер на направление (default 100, max 1000). При превышении — первые limit + флаг *_truncated и счётчик *_total.",
                    "default": 100,
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
            let object = match crate::tools::object_value(&args) {
                Some(s) => crate::code_usages::normalize_object_ref(s).into_owned(),
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
            let limit: i64 = args
                .get("limit")
                .and_then(|v| v.as_i64())
                .unwrap_or(DEFAULT_LIMIT)
                .clamp(1, MAX_LIMIT);

            let storage = match ctx.storage.get().await {
                Ok(s) => s,
                Err(e) => {
                    return crate::tools::wrap_error(serde_json::json!({
                        "error": format!("storage pool: {}", e)
                    }));
                }
            };
            let conn = storage.conn();

            let mut result = json!({ "object": object, "depth": depth, "limit": limit });

            if direction == "out" || direction == "both" {
                match query_links(conn, &object, depth, Direction::Out, limit) {
                    Ok((v, total, truncated)) => {
                        result["out"] = Value::Array(v);
                        result["out_total"] = json!(total);
                        result["out_truncated"] = json!(truncated);
                    }
                    Err(e) => return crate::tools::wrap_error(json!({"error": format!("database error (out): {}", e)})),
                }
            }
            if direction == "in" || direction == "both" {
                match query_links(conn, &object, depth, Direction::In, limit) {
                    Ok((v, total, truncated)) => {
                        result["in"] = Value::Array(v);
                        result["in_total"] = json!(total);
                        result["in_truncated"] = json!(truncated);
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

/// Тело recursive-CTE для направления (без LIMIT) — переиспользуется и для
/// выборки рёбер (с LIMIT), и для COUNT(*) при подсчёте total.
fn walk_cte(dir: &Direction) -> &'static str {
    match dir {
        // Out: стартовая привязка по from_object, переход by to_object.
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
        // In: зеркально (start by to_object, переход by from_object).
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
    }
}

/// Собрать рёбра окрестности объекта в заданном направлении до глубины depth,
/// не более `limit` штук. Возвращает (рёбра, total, truncated):
/// total — полное число рёбер (через COUNT, только если упёрлись в limit),
/// truncated — true, если рёбер было больше limit.
/// Терминальные `*`-узлы (is_universal=1) не разворачиваются на следующий шаг (только Out).
fn query_links(
    conn: &rusqlite::Connection,
    object: &str,
    depth: i64,
    dir: Direction,
    limit: i64,
) -> rusqlite::Result<(Vec<Value>, i64, bool)> {
    let cte = walk_cte(&dir);
    // Берём limit+1, чтобы отличить «ровно limit» от «есть ещё».
    let data_sql = format!("{cte} LIMIT ?4");
    let mut stmt = conn.prepare(&data_sql)?;
    let rows = stmt.query_map(params!["default", object, depth, limit + 1], |r| {
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
    let truncated = out.len() as i64 > limit;
    if truncated {
        out.truncate(limit as usize);
    }
    // total: если не обрезано — это и есть len; иначе считаем COUNT по тому же CTE.
    let total = if truncated {
        let count_sql = format!("SELECT COUNT(*) FROM ({cte})");
        conn.query_row(&count_sql, params!["default", object, depth], |r| {
            r.get::<_, i64>(0)
        })?
    } else {
        out.len() as i64
    };
    Ok((out, total, truncated))
}
