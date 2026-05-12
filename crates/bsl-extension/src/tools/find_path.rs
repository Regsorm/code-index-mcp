// MCP-tool `find_path` — находит путь от одной процедуры до другой
// в графе вызовов через recursive CTE по `proc_call_graph`.
//
// Запрос:
//   from = "ОбщегоНазначенияСервер.Старт"
//   to   = "Логирование.Записать"
//   max_depth = 3
//
// Ответ — список рёбер первого найденного пути (BFS), либо пустой
// массив если не нашли. Используется агентами 1С для анализа
// «как процедура A может в итоге вызвать процедуру B».

use std::future::Future;
use std::pin::Pin;

use code_index_core::extension::{IndexTool, ToolContext};
use rusqlite::params;
use serde_json::{json, Value};

pub struct FindPathTool;

impl IndexTool for FindPathTool {
    fn name(&self) -> &str {
        "find_path"
    }

    fn description(&self) -> &str {
        "Ищет путь в графе вызовов от процедуры 'from' до процедуры 'to' \
         через таблицу proc_call_graph. Возвращает первый найденный путь \
         (BFS) длиной до max_depth (по умолчанию 3) — массив рёбер с \
         caller/callee/call_type. Пустой массив, если пути нет. \
         For BSL/1C repositories only."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo": {
                    "type": "string",
                    "description": "Алиас репозитория"
                },
                "from": {
                    "type": "string",
                    "description": "caller_proc_key начальной точки, например 'ОбщегоНазначенияСервер.Старт'"
                },
                "to": {
                    "type": "string",
                    "description": "callee_proc_name конечной точки, например 'Логирование.Записать'"
                },
                "max_depth": {
                    "type": "integer",
                    "description": "Максимальная длина пути (число рёбер). По умолчанию 3.",
                    "default": 3,
                    "minimum": 1,
                    "maximum": 10
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
                .unwrap_or(3)
                .clamp(1, 10);

            let storage = ctx.storage.lock().await;
            let conn = storage.conn();

            // Recursive CTE по proc_call_graph: ищем кратчайший путь
            // через обычный BFS (LIMIT 1 на нужной глубине).
            //
            // path_json — массив рёбер в порядке обхода. Глубина
            // (`depth`) ограничена max_depth для защиты от
            // экспоненциального взрыва на густых графах.
            let sql = "
                WITH RECURSIVE walk(cur_callee, depth, path_json) AS (
                    SELECT
                        callee_proc_name,
                        1,
                        json_array(json_object(
                            'caller', caller_proc_key,
                            'callee', callee_proc_name,
                            'call_type', call_type
                        ))
                    FROM proc_call_graph
                    WHERE repo = ?1 AND caller_proc_key = ?2
                    UNION ALL
                    SELECT
                        pcg.callee_proc_name,
                        w.depth + 1,
                        json_insert(
                            w.path_json,
                            '$[#]',
                            json_object(
                                'caller', pcg.caller_proc_key,
                                'callee', pcg.callee_proc_name,
                                'call_type', pcg.call_type
                            )
                        )
                    FROM walk w
                    JOIN proc_call_graph pcg
                      ON pcg.repo = ?1
                     AND pcg.caller_proc_key = w.cur_callee
                    WHERE w.depth < ?3
                )
                SELECT path_json FROM walk
                WHERE cur_callee = ?4
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
                    json!({
                        "from": from,
                        "to": to,
                        "found": true,
                        "path": path,
                    })
                }
                Err(rusqlite::Error::QueryReturnedNoRows) => json!({
                    "from": from,
                    "to": to,
                    "found": false,
                    "path": [],
                    "max_depth": max_depth,
                }),
                Err(e) => json!({"error": format!("database error: {}", e)}),
            };
            crate::tools::wrap_with_meta(result_value, Vec::new())
        })
    }
}
