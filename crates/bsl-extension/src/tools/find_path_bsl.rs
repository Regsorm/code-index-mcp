// MCP-tool `find_path_bsl` — находит путь от одной процедуры до другой
// в BSL-графе вызовов: поиск в ширину по `proc_call_graph` с памятью о
// посещённых узлах, потолком рёбер и бюджетом времени.
//
// BSL-специфичный аналог универсального `find_path` (ядро, таблица `calls`):
// `proc_call_graph` богаче — хранит `call_type` и ключи процедур,
// дедуплицирован и repo-scoped. Доступен только для репозиториев с
// `language = "bsl"`.
//
// Ключи процедур — в формате `<rel_path>::<name>` (как caller_proc_key и
// callee_proc_key после резолва, этап 4e). Между хопами обход идёт по
// `COALESCE(callee_proc_key, callee_proc_name)`: по резолвленному адресу цели,
// а где он NULL (нерезолвленный лист) — по сырому имени.
//
// Запрос:
//   from = "base/Documents/РеализацияТоваровУслуг/Ext/ObjectModule.bsl::ОбработкаПроведения"
//   to   = "base/CommonModules/ОбщегоНазначения/Ext/Module.bsl::ЗначениеРеквизитаОбъекта"
//   max_depth = 3
//
// Ответ — список рёбер первого найденного пути (BFS; каждое ребро —
// caller/callee/callee_key/call_type), либо пустой массив если не нашли.
// Используется агентами 1С для анализа «как процедура A может в итоге
// вызвать процедуру B».

use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use code_index_core::extension::{IndexTool, ToolContext};
use rusqlite::params;
use serde_json::{json, Value};

/// Потолок пройденных рёбер: страховка от плотного графа.
const NODE_CAP: usize = 40_000;
/// Бюджет времени на обход. Лучше честный «не досмотрел» с подсказкой,
/// чем зависший на минуты вызов.
const TIME_BUDGET: Duration = Duration::from_secs(5);

pub struct FindPathBslTool;

impl IndexTool for FindPathBslTool {
    fn name(&self) -> &str {
        "find_path_bsl"
    }

    fn description(&self) -> &str {
        "Цепочка вызовов между двумя процедурами 1С за ОДИН вызов. Бери его на \
         вопросы вида «есть ли путь от процедуры A до процедуры B», «как из A \
         попадают в B», «через что вызывается B»: обход по графу делает сервер. \
         НЕ восстанавливай цепочку вручную — перебор get_callers/get_callees по \
         одному узлу с чтением тел стоит десятки вызовов и даёт тот же ответ. \
         Ищет по таблице proc_call_graph. И 'from', и 'to' можно задавать просто \
         ИМЕНЕМ процедуры — сервер сам подберёт подходящие ключи; полный ключ \
         формата '<rel_path>::<name>' тоже принимается и сужает поиск. \
         Возвращает первый найденный \
         путь (BFS) длиной до max_depth (по умолчанию 3) — массив рёбер с \
         caller/callee/callee_key/call_type. Пустой массив, если пути нет. \
         BSL-вариант (с видом вызова) универсального find_path. \
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
                    "description": "caller_proc_key начальной точки в формате '<rel_path>::<name>', например 'base/Documents/РеализацияТоваровУслуг/Ext/ObjectModule.bsl::ОбработкаПроведения'"
                },
                "to": {
                    "type": "string",
                    "description": "Цель: callee_proc_key '<rel_path>::<name>' (для резолвленных) либо голое callee_proc_name (для нерезолвленных листьев)"
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

            let storage = match ctx.storage.get().await {
                Ok(s) => s,
                Err(e) => {
                    return crate::tools::wrap_error(serde_json::json!({
                        "error": format!("storage pool: {}", e)
                    }));
                }
            };
            let conn = storage.conn();

            // Recursive CTE по proc_call_graph: обход в ширину, берём первый
            // достигнутый путь. Сортировки по глубине здесь быть НЕ должно:
            // рекурсивный CTE SQLite обходит очередью, то есть первое попадание
            // и есть кратчайшее, а `ORDER BY depth` заставляет материализовать
            // весь достижимый подграф до выбора строки — на живой конфигурации
            // это минуты вместо миллисекунд (поймано замером 28.08.2026).
            //
            // path_json — массив рёбер в порядке обхода. Глубина
            // (`depth`) ограничена max_depth для защиты от
            // экспоненциального взрыва на густых графах.
            // Связь между хопами — `COALESCE(callee_proc_key, callee_proc_name)`:
            // идём по резолвленному адресу цели (`<rel_path>::<name>`), когда он
            // есть (заполняет этап 4e), иначе по сырому имени (нерезолвленный
            // лист / синтетические рёбра без ключа). `from`/`to` принимают тот же
            // ключ `<rel_path>::<name>` (предпочтительно) либо голое имя.
            // 'from' приходит либо полным ключом '<путь>::<имя>', либо голым
            // именем процедуры — модель обычно знает только имя. Голое имя
            // разворачиваем в ключи-кандидаты по суффиксу и пробуем по очереди:
            // без этого вызов молча отвечал «пути нет».
            let from_keys: Vec<String> = if from.contains("::") {
                vec![from.clone()]
            } else {
                // Само значение пробуем первым: ключ может храниться и голым
                // (нерезолвленные листья, синтетические рёбра).
                let mut keys = vec![from.clone()];
                // Кандидатов держим немного: каждый — отдельный обход графа,
                // а тёзок в конфигурации бывают десятки.
                let mut stmt = match conn.prepare(
                    "SELECT DISTINCT caller_proc_key FROM proc_call_graph \
                     WHERE repo = ?1 AND caller_proc_key LIKE '%::' || ?2 LIMIT 5",
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        return crate::tools::wrap_error(
                            json!({ "error": format!("database error: {}", e) }),
                        );
                    }
                };
                let collected =
                    match stmt.query_map(params!["default", &from], |r| r.get::<_, String>(0)) {
                        Ok(it) => it.filter_map(Result::ok).collect::<Vec<String>>(),
                        Err(e) => {
                            return crate::tools::wrap_error(
                                json!({ "error": format!("database error: {}", e) }),
                            );
                        }
                    };
                keys.extend(collected);
                keys
            };

            // Обход в ширину с памятью о посещённых узлах, потолком и бюджетом
            // времени. Рекурсивный запрос SQLite здесь держать нельзя: он не
            // помнит, где уже был, и на живой конфигурации разрастается
            // экспоненциально — замер 28.08.2026 дал 8 минут на глубине 4 там,
            // где обход с посещёнными отвечает за миллисекунды. Тем же способом
            // и по той же причине переписан обход графа связей данных.
            let mut stmt = match conn.prepare(
                "SELECT callee_proc_name, callee_proc_key, call_type FROM proc_call_graph \
                 WHERE repo = ?1 AND caller_proc_key = ?2",
            ) {
                Ok(s) => s,
                Err(e) => {
                    return crate::tools::wrap_error(
                        json!({ "error": format!("database error: {}", e) }),
                    );
                }
            };

            // Рёбра храним плоско: индекс родителя позволяет собрать путь
            // обратным ходом, не копируя его в каждый узел очереди.
            struct Step {
                parent: Option<usize>,
                caller: String,
                callee: String,
                callee_key: Option<String>,
                call_type: String,
            }
            let mut steps: Vec<Step> = Vec::new();
            let mut queue: VecDeque<(String, i64, Option<usize>)> = VecDeque::new();
            let mut visited: HashSet<String> = HashSet::new();
            for key in &from_keys {
                if visited.insert(key.clone()) {
                    queue.push_back((key.clone(), 0, None));
                }
            }

            let started = Instant::now();
            let mut found: Option<(String, Vec<Value>)> = None;
            let mut db_error: Option<String> = None;
            let mut stopped_by_time = false;
            let mut nodes_capped = false;

            'walk: while let Some((node, depth, parent_idx)) = queue.pop_front() {
                if started.elapsed() > TIME_BUDGET {
                    stopped_by_time = true;
                    break;
                }
                if depth >= max_depth {
                    continue;
                }
                let rows = match stmt.query_map(params!["default", &node], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                }) {
                    Ok(it) => it.filter_map(Result::ok).collect::<Vec<_>>(),
                    Err(e) => {
                        db_error = Some(format!("database error: {}", e));
                        break;
                    }
                };
                for (callee, callee_key, call_type) in rows {
                    let link = callee_key.clone().unwrap_or_else(|| callee.clone());
                    steps.push(Step {
                        parent: parent_idx,
                        caller: node.clone(),
                        callee: callee.clone(),
                        callee_key: callee_key.clone(),
                        call_type,
                    });
                    let idx = steps.len() - 1;
                    let hit = callee == to
                        || callee_key.as_deref() == Some(to.as_str())
                        || callee_key
                            .as_deref()
                            .is_some_and(|k| k.ends_with(&format!("::{}", to)));
                    if hit {
                        let mut path = Vec::new();
                        let mut cur = Some(idx);
                        while let Some(i) = cur {
                            let s = &steps[i];
                            path.push(json!({
                                "caller": s.caller,
                                "callee": s.callee,
                                "callee_key": s.callee_key,
                                "call_type": s.call_type,
                            }));
                            cur = s.parent;
                        }
                        path.reverse();
                        found = Some((node.clone(), path));
                        break 'walk;
                    }
                    if steps.len() >= NODE_CAP {
                        nodes_capped = true;
                        break 'walk;
                    }
                    if visited.insert(link.clone()) {
                        queue.push_back((link, depth + 1, Some(idx)));
                    }
                }
            }

            let result_value = if let Some(err) = db_error {
                json!({ "error": err })
            } else if let Some((key, path)) = found {
                json!({
                    "from": from,
                    "from_key": key,
                    "to": to,
                    "found": true,
                    "path": path,
                })
            } else {
                // Обход мог оборваться — тогда «пути нет» значит лишь «не
                // досмотрели», и подсказка обязана это различать.
                let hint = if stopped_by_time {
                    "Обход остановлен по времени — «пути нет» отсюда НЕ следует. \
                     Уменьшите max_depth либо задайте более точный конец пути (полный ключ)."
                } else if nodes_capped {
                    "Обход остановлен на потолке узлов — «пути нет» отсюда НЕ следует. \
                     Уменьшите max_depth либо задайте более точный конец пути."
                } else if steps.is_empty() {
                    // Ни одного исходящего ребра ни у одного стартового ключа —
                    // значит такой вызывающей процедуры в графе нет. Считать по
                    // числу стартовых ключей нельзя: у точного ключа кандидат
                    // всегда один, и подсказка врала на каждом таком вызове.
                    "Процедура 'from' не найдена среди вызывающих: проверьте имя \
                     (search_terms или find_symbol) либо задайте полный ключ '<путь>::<имя>'."
                } else {
                    "Пути нет в пределах max_depth: увеличьте max_depth либо проверьте имя 'to'."
                };
                json!({
                    "from": from,
                    "to": to,
                    "found": false,
                    "path": [],
                    "max_depth": max_depth,
                    "from_keys_tried": from_keys.len(),
                    "walk_stopped_by_time": stopped_by_time,
                    "walk_nodes_capped": nodes_capped,
                    "visited_nodes": visited.len(),
                    "hint": hint,
                })
            };
            crate::tools::wrap_with_meta("find_path_bsl", result_value, Vec::new())
        })
    }
}
