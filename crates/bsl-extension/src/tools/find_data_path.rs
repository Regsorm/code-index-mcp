// MCP-tool `find_data_path` — путь между двумя объектами в графе связей данных.
//
// Аналог `find_path` (граф вызовов), но по таблице `data_links`: ищет цепочку
// ссылочных связей от объекта `from` до объекта `to`. Отвечает на вопрос
// «как связаны эти две сущности по данным» — например, путь от
// Document.РеализацияТоваровУслуг до Catalog.Контрагенты.
//
// Возвращает первый найденный путь (BFS) длиной до max_depth — массив рёбер.
// Терминальные `*`-узлы (is_universal) не разворачиваются дальше.

use std::future::Future;
use std::pin::Pin;

use code_index_core::extension::{IndexTool, ToolContext};
use rusqlite::params;
use serde_json::{json, Value};

pub struct FindDataPathTool;

impl IndexTool for FindDataPathTool {
    fn name(&self) -> &str {
        "find_data_path"
    }

    fn description(&self) -> &str {
        "Ищет путь в графе связей данных (data_links) от объекта 'from' до \
         объекта 'to' по ссылочным реквизитам/измерениям. Возвращает первый \
         найденный путь (BFS) длиной до max_depth (по умолчанию 4) — массив \
         рёбер from_object/from_path/to_object. Пустой путь, если связи нет. \
         For BSL/1C repositories only."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo": { "type": "string", "description": "Алиас репозитория" },
                "from": {
                    "type": "string",
                    "description": "Объект-источник, например 'Document.РеализацияТоваровУслуг'"
                },
                "to": {
                    "type": "string",
                    "description": "Объект-цель, например 'Catalog.Контрагенты'"
                },
                "max_depth": {
                    "type": "integer",
                    "description": "Максимальная длина пути (число рёбер). По умолчанию 4.",
                    "default": 4,
                    "minimum": 1,
                    "maximum": 8
                }
            },
            "required": ["repo", "from", "to"]
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
            let from = match args.get("from").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => {
                    return crate::tools::wrap_error(json!({
                        "error": "missing required parameter 'from' (string)"
                    }));
                }
            };
            let to = match args.get("to").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => {
                    return crate::tools::wrap_error(json!({
                        "error": "missing required parameter 'to' (string)"
                    }));
                }
            };
            let max_depth: i64 = args
                .get("max_depth")
                .and_then(|v| v.as_i64())
                .unwrap_or(4)
                .clamp(1, 8);

            let storage = ctx.storage.lock().await;
            let conn = storage.conn();

            // BFS по data_links. Не разворачиваем терминальные *-узлы
            // (w.is_universal=0 на шаге рекурсии).
            let sql = "
                WITH RECURSIVE walk(cur_obj, depth, path_json) AS (
                    SELECT
                        to_object, 1,
                        json_array(json_object(
                            'from_object', from_object,
                            'from_path', from_path,
                            'to_object', to_object,
                            'link_kind', link_kind
                        ))
                    FROM data_links
                    WHERE repo = ?1 AND from_object = ?2
                    UNION ALL
                    SELECT
                        dl.to_object, w.depth + 1,
                        json_insert(w.path_json, '$[#]', json_object(
                            'from_object', dl.from_object,
                            'from_path', dl.from_path,
                            'to_object', dl.to_object,
                            'link_kind', dl.link_kind
                        ))
                    FROM walk w
                    JOIN data_links dl ON dl.repo = ?1 AND dl.from_object = w.cur_obj
                    WHERE w.depth < ?3
                )
                SELECT path_json FROM walk
                WHERE cur_obj = ?4
                ORDER BY depth ASC
                LIMIT 1
            ";

            let row = conn.query_row(
                sql,
                params!["default", &from, max_depth, &to],
                |r| r.get::<_, String>(0),
            );

            let result_value = match row {
                Ok(path_json) => {
                    let path: Value = serde_json::from_str(&path_json)
                        .unwrap_or_else(|_| Value::Array(Vec::new()));
                    json!({ "from": from, "to": to, "found": true, "path": path })
                }
                Err(rusqlite::Error::QueryReturnedNoRows) => json!({
                    "from": from, "to": to, "found": false, "path": [], "max_depth": max_depth,
                }),
                Err(e) => json!({"error": format!("database error: {}", e)}),
            };
            crate::tools::wrap_with_meta(result_value, Vec::new())
        })
    }
}
