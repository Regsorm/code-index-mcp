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
use std::collections::{HashMap, HashSet, VecDeque};

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
                Some(s) => crate::code_usages::normalize_object_ref(s).into_owned(),
                None => {
                    return crate::tools::wrap_error(json!({
                        "error": "missing required parameter 'from' (string)"
                    }));
                }
            };
            let to = match args.get("to").and_then(|v| v.as_str()) {
                Some(s) => crate::code_usages::normalize_object_ref(s).into_owned(),
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

            let storage = match ctx.storage.get().await {
                Ok(s) => s,
                Err(e) => {
                    return crate::tools::wrap_error(serde_json::json!({
                        "error": format!("storage pool: {}", e)
                    }));
                }
            };
            let conn = storage.conn();

            // BFS с visited-set: каждый узел разворачивается ровно один
            // раз, поэтому обход ограничен достижимым подграфом (тысячи
            // узлов), а не числом путей (на плотном циклическом графе связей
            // 1С их миллионы). Возвращаем кратчайший по числу рёбер путь
            // from -> to. Терминальные *-узлы (is_universal) исходящих рёбер
            // не имеют, поэтому не разворачиваются естественным образом.
            // Seek по (repo, from_object) на каждом шаге обеспечивает ANALYZE
            // (см. run_index_extras).
            struct Edge {
                from_object: String,
                from_path: String,
                to_object: String,
                link_kind: String,
            }

            let mut stmt = match conn.prepare(
                "SELECT from_object, from_path, to_object, link_kind \
                 FROM data_links WHERE repo = ?1 AND from_object = ?2",
            ) {
                Ok(s) => s,
                Err(e) => {
                    return crate::tools::wrap_error(json!({ "error": format!("database error: {}", e) }));
                }
            };

            let mut visited: HashSet<String> = HashSet::new();
            let mut parent: HashMap<String, Edge> = HashMap::new();
            let mut queue: VecDeque<(String, i64)> = VecDeque::new();
            visited.insert(from.clone());
            queue.push_back((from.clone(), 0));
            let mut reached = false;
            let mut db_err: Option<String> = None;

            'bfs: while let Some((node, depth)) = queue.pop_front() {
                if depth >= max_depth {
                    continue;
                }
                let rows = stmt.query_map(params!["default", &node], |r| {
                    Ok(Edge {
                        from_object: r.get(0)?,
                        from_path: r.get(1)?,
                        to_object: r.get(2)?,
                        link_kind: r.get(3)?,
                    })
                });
                let rows = match rows {
                    Ok(r) => r,
                    Err(err) => {
                        db_err = Some(format!("{}", err));
                        break 'bfs;
                    }
                };
                for edge in rows {
                    let edge = match edge {
                        Ok(ed) => ed,
                        Err(err) => {
                            db_err = Some(format!("{}", err));
                            break 'bfs;
                        }
                    };
                    let nxt = edge.to_object.clone();
                    if nxt == to {
                        parent.insert(nxt, edge);
                        reached = true;
                        break 'bfs;
                    }
                    if visited.insert(nxt.clone()) {
                        parent.insert(nxt.clone(), edge);
                        queue.push_back((nxt, depth + 1));
                    }
                }
            }

            if let Some(err) = db_err {
                return crate::tools::wrap_error(json!({ "error": format!("database error: {}", err) }));
            }

            let result_value = if reached {
                let mut edges: Vec<Value> = Vec::new();
                let mut cur = to.clone();
                while let Some(edge) = parent.get(&cur) {
                    edges.push(json!({
                        "from_object": edge.from_object,
                        "from_path": edge.from_path,
                        "to_object": edge.to_object,
                        "link_kind": edge.link_kind,
                    }));
                    let prev = edge.from_object.clone();
                    if prev == from {
                        break;
                    }
                    cur = prev;
                }
                edges.reverse();
                json!({ "from": from, "to": to, "found": true, "path": edges })
            } else {
                json!({
                    "from": from, "to": to, "found": false, "path": [], "max_depth": max_depth,
                })
            };
            crate::tools::wrap_with_meta("find_data_path", result_value, Vec::new())
        })
    }
}
