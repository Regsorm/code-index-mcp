// MCP-tool `get_data_links` — окрестность объекта 1С в графе связей данных.
//
// Отвечает на вопросы «на что ссылается объект» (direction=out) и
// «кто ссылается на объект» (direction=in) по таблице `data_links`,
// собирая рёбра до глубины `depth`.
//
// Закрывает паттерн «блуждания по структуре»: вместо N последовательных
// get_object_structure модель одним вызовом получает кластер связей
// вокруг объекта.
//
// Терминальные `*`-узлы (is_universal: *CatalogRef / *AnyRef /
// *DefinedType.X) не разворачиваются дальше — у них нет исходящих рёбер,
// обход на них останавливается (защита от разрастания и шума).
//
// Обход — поиск в ширину с множеством посещённых узлов: каждый объект
// разворачивается ровно один раз. Прежняя реализация на рекурсивном запросе
// шла по всем путям, а на циклическом графе связей 1С их миллионы: замер по
// центральному справочнику давал 42,9 с на глубине 4 в направлении «кто
// ссылается», причём прерывания по времени не было вовсе и соединение из пула
// оставалось занятым все эти секунды. Образец правильного обхода лежал рядом —
// в поиске пути по тому же графу.
//
// Размер ответа: рёбра отдаются страницей по бюджету, рядом всегда счётчики по
// видам связи и полное число. Потолок числа рёбер сам по себе от переполнения
// не спасал — 1 000 рёбер это порядка 540 КБ.

use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use code_index_core::extension::{IndexTool, ToolContext};
use code_index_core::mcp::cap::{next_call_hint, page_by_bytes, resolve_request_budget};
use rusqlite::params;
use serde_json::{json, Map, Value};

/// Потолок рёбер на одно направление по умолчанию.
const DEFAULT_LIMIT: i64 = 100;
/// Жёсткий максимум запрашиваемого потолка рёбер.
const MAX_LIMIT: i64 = 1000;
/// Сколько рёбер обход собирает максимум, независимо от `limit`: дальше расти
/// бессмысленно — в ответ всё равно уйдёт страница, а память и время конечны.
const MAX_WALK_EDGES: usize = 20_000;
/// Прерывание обхода по времени — как у произвольного SQL и паспорта объекта.
const WALK_TIMEOUT_SECS: u64 = 8;

pub struct GetDataLinksTool;

impl IndexTool for GetDataLinksTool {
    fn name(&self) -> &str {
        "get_data_links"
    }

    fn description(&self) -> &str {
        "Связи данных объекта 1С: с чем он связан по ссылочным реквизитам. \
         На обзорный вопрос («на что ссылается и кто ссылается на объект») хватает \
         ОДНОГО вызова с depth=1: ответ несёт счётчики по видам связи (<dir>_by_kind) \
         и первую страницу рёбер. Не листай страницы и не увеличивай глубину без \
         нужды — полное перечисление рёбер требуется редко, а объём ответа растёт \
         быстро. Подробности: \
         возвращает связи по таблице data_links: \
         'out' — на какие объекты ссылается (реквизиты/измерения ссылочного \
         типа), 'in' — какие объекты ссылаются на него. Обходит граф до глубины \
         depth (по умолчанию 1, максимум 4) поиском в ширину с учётом уже \
         посещённых узлов. Цель вида '*CatalogRef'/'*AnyRef'/'*DefinedType.X' — \
         обобщённая ссылка (терминал, дальше не разворачивается). \
         Рёбра отдаются страницей по размеру ответа: рядом идут <dir>_shown / \
         _total / _offset / _has_more и <dir>_by_kind — счётчики по видам связи. \
         Следующая страница — тот же вызов с offset (нужно одно направление, \
         не 'both'); часто вместо перебора страниц дешевле сузить запрос по виду \
         связи, глядя на счётчики. Обход прерывается по времени: при этом в \
         ответе walk_stopped_by_time, а собранное отдаётся. \
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
                    "description": "out — на что ссылается; in — кто ссылается; both — оба. По умолчанию both. Для страниц (offset) нужно одно направление.",
                    "default": "both"
                },
                "depth": {
                    "type": "integer",
                    "description": "Глубина обхода (число шагов). По умолчанию 1, максимум 4. Глубина больше 1 — это уже исследование графа: рёбер становится кратно больше, а обход может прерваться по времени.",
                    "default": 1,
                    "minimum": 1,
                    "maximum": 4
                },
                "limit": {
                    "type": "integer",
                    "description": "Потолок рёбер на направление (default 100, max 1000). Фактический размер страницы ограничен ещё и бюджетом ответа.",
                    "default": 100,
                    "minimum": 1
                },
                "offset": {
                    "type": "integer",
                    "description": "Смещение страницы рёбер (работает при direction='out' или 'in'). Без параметра — с начала.",
                    "minimum": 0
                },
                "max_response_bytes": {
                    "type": "integer",
                    "description": "Бюджет размера ЭТОГО ответа в байтах — перекрывает серверный на один вызов. Больше бюджет — больше рёбер в странице."
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
            let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let budget = resolve_request_budget(
                args.get("max_response_bytes")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as usize),
            );
            if offset > 0 && direction == "both" {
                return crate::tools::wrap_error(json!({
                    "error": "offset требует одного направления",
                    "hint": "Повторите вызов с direction='out' или direction='in' — страницы считаются по одному направлению.",
                }));
            }

            let storage = match ctx.storage.get().await {
                Ok(s) => s,
                Err(e) => {
                    return crate::tools::wrap_error(serde_json::json!({
                        "error": format!("storage pool: {}", e)
                    }));
                }
            };
            let conn = storage.conn();

            // Имя приводим к записи из конфигурации: с иным регистром кириллицы
            // точное сравнение промахивалось молча, и ответ «на объект никто не
            // ссылается» выглядел содержательным выводом. Дальше обход идёт по
            // каноническим значениям из самой базы, поэтому резолв нужен один раз.
            let object = crate::tools::canonical_object_name(conn, &object);

            // Прерывание по времени: обход сам смотрит на часы между шагами, а
            // прерыватель страхует от одиночного долгого запроса внутри шага.
            let handle = conn.get_interrupt_handle();
            let timer = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(WALK_TIMEOUT_SECS + 1)).await;
                handle.interrupt();
            });
            let deadline = Instant::now() + Duration::from_secs(WALK_TIMEOUT_SECS);

            let both = direction == "both";
            // При двух направлениях бюджет делится: иначе первое направление
            // съедает его целиком и второе приходит пустым.
            let share = if both { budget.applied / 2 } else { budget.applied };

            let mut result = json!({
                "object": object,
                "depth": depth,
                "direction": direction,
                "limit": limit,
            });
            let mut hints: Vec<String> = Vec::new();
            let mut stopped_by_time = false;
            let mut edges_capped = false;

            for dir in [Direction::Out, Direction::In] {
                let key = dir.key();
                if !(both || direction == key) {
                    continue;
                }
                let walk = match walk_links(conn, &object, depth, &dir, limit, deadline) {
                    Ok(w) => w,
                    Err(e) => {
                        timer.abort();
                        return crate::tools::wrap_error(json!({
                            "error": format!("database error ({}): {}", key, e)
                        }));
                    }
                };
                stopped_by_time |= walk.stopped_by_time;
                edges_capped |= walk.edges_capped;

                let total = walk.total();
                let total_partial = walk.total_is_partial();
                let mut page = page_by_bytes(walk.edges, offset, share);
                let shown = page.shown;
                // Полное число берём у обхода, а не у страницы: страница видит
                // лишь собранные рёбра и объявила бы конец набора раньше срока.
                page.total = total;
                page.has_more = offset + shown < total;
                let items = std::mem::take(&mut page.items);
                if let Some(obj) = result.as_object_mut() {
                    obj.insert(key.to_string(), Value::Array(items));
                    page.annotate(obj, key);
                    obj.insert(format!("{}_by_kind", key), Value::Object(walk.by_kind));
                    // Прежнее имя признака неполноты — читатели на него смотрят.
                    obj.insert(format!("{}_truncated", key), json!(page.has_more));
                    if total_partial {
                        obj.insert(format!("{}_total_partial", key), json!(true));
                    }
                }
                if page.has_more {
                    let call = next_call_hint(
                        "get_data_links",
                        &[
                            ("repo", json!(ctx.repo)),
                            ("object", json!(&object)),
                            ("direction", json!(key)),
                            ("depth", json!(depth)),
                            ("offset", json!(offset + shown)),
                        ],
                    );
                    hints.push(format!(
                        "Направление '{}': показано {} рёбер из {}. Следующая страница — 1 вызов:\n{}\n\
                         Часто дешевле не листать, а сузить запрос: счётчики по видам связи — в {}_by_kind.",
                        key, shown, total, call, key
                    ));
                }
            }
            timer.abort();

            if let Some(obj) = result.as_object_mut() {
                if stopped_by_time {
                    obj.insert("walk_stopped_by_time".to_string(), json!(true));
                    hints.push(format!(
                        "Обход прерван по времени ({} с) — отдано то, что успело собраться. \
                         Уменьшите depth: на глубине больше 1 число рёбер растёт кратно.",
                        WALK_TIMEOUT_SECS
                    ));
                }
                if edges_capped {
                    obj.insert("walk_edges_capped".to_string(), json!(true));
                    hints.push(format!(
                        "Обход остановлен на {} рёбрах — столько в один ответ всё равно не уходит. \
                         Уменьшите depth или разбирайте по видам связи.",
                        MAX_WALK_EDGES
                    ));
                }
                if !hints.is_empty() {
                    obj.insert("hint".to_string(), json!(hints.join("\n")));
                }
                if budget.requested.is_some() {
                    obj.insert("response_budget_applied".to_string(), json!(budget.applied));
                }
            }

            crate::tools::wrap_with_meta("get_data_links", result, Vec::new())
        })
    }
}

enum Direction {
    Out,
    In,
}

impl Direction {
    fn key(&self) -> &'static str {
        match self {
            Direction::Out => "out",
            Direction::In => "in",
        }
    }

    /// Запрос рёбер, инцидентных узлу, в сторону обхода.
    fn sql(&self) -> &'static str {
        match self {
            Direction::Out => {
                "SELECT from_object, from_path, to_object, link_kind, is_composite, is_universal \
                 FROM data_links WHERE repo = ?1 AND from_object = ?2"
            }
            Direction::In => {
                "SELECT from_object, from_path, to_object, link_kind, is_composite, is_universal \
                 FROM data_links WHERE repo = ?1 AND to_object = ?2"
            }
        }
    }
}

/// Результат обхода окрестности.
struct Walk {
    /// Рёбра в порядке обхода (по возрастанию глубины).
    edges: Vec<Value>,
    /// Счётчики по видам связи по всем собранным рёбрам.
    by_kind: Map<String, Value>,
    /// Обход прерван по времени.
    stopped_by_time: bool,
    /// Обход остановлен на потолке числа рёбер.
    edges_capped: bool,
    /// Точное число рёбер, когда его можно получить дёшево (глубина 1 —
    /// подсчёт по индексу). `None` — полного числа мы не знаем.
    exact_total: Option<usize>,
}

impl Walk {
    /// Сколько рёбер сообщаем читателю.
    fn total(&self) -> usize {
        self.exact_total.unwrap_or(self.edges.len())
    }

    /// Это число — лишь собранное обходом, а в графе рёбер больше. Выдавать
    /// такое за полное нельзя: читатель решит, что видит весь граф.
    fn total_is_partial(&self) -> bool {
        self.exact_total.is_none() && (self.edges_capped || self.stopped_by_time)
    }
}

/// Обойти окрестность объекта поиском в ширину, разворачивая каждый узел ровно
/// один раз. Прежний рекурсивный запрос шёл по путям, а не по узлам, и на
/// плотном графе не заканчивался за разумное время.
///
/// `limit` ограничивает число рёбер, которое собирается на одном уровне
/// сверх необходимого: обход всё равно останавливается по времени, по потолку
/// рёбер или по исчерпании графа.
fn walk_links(
    conn: &rusqlite::Connection,
    object: &str,
    depth: i64,
    dir: &Direction,
    limit: i64,
    deadline: Instant,
) -> rusqlite::Result<Walk> {
    let mut stmt = conn.prepare(dir.sql())?;
    let mut visited: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<(String, i64)> = VecDeque::new();
    let mut edges: Vec<Value> = Vec::new();
    let mut by_kind: Map<String, Value> = Map::new();
    let mut stopped_by_time = false;
    let mut edges_capped = false;

    visited.insert(object.to_string());
    queue.push_back((object.to_string(), 0));

    // Потолок числа рёбер: запрошенный limit — нижняя граница (страница не
    // должна оказаться короче запрошенного), общий потолок — верхняя.
    let cap = (limit.max(1) as usize * 10).clamp(limit.max(1) as usize, MAX_WALK_EDGES);

    while let Some((node, node_depth)) = queue.pop_front() {
        if node_depth >= depth {
            continue;
        }
        if Instant::now() >= deadline {
            stopped_by_time = true;
            break;
        }
        let rows = stmt.query_map(params!["default", &node], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)? != 0,
                r.get::<_, i64>(5)? != 0,
            ))
        })?;
        for row in rows {
            let (from_object, from_path, to_object, link_kind, is_composite, is_universal) = row?;
            let entry = by_kind.entry(link_kind.clone()).or_insert_with(|| json!(0));
            *entry = json!(entry.as_u64().unwrap_or(0) + 1);
            edges.push(json!({
                "from_object": from_object,
                "from_path": from_path,
                "to_object": to_object,
                "link_kind": link_kind,
                "is_composite": is_composite,
                "is_universal": is_universal,
                "depth": node_depth + 1,
            }));
            if edges.len() >= cap {
                edges_capped = true;
                break;
            }
            // Следующий узел — противоположный конец ребра. Терминальные
            // обобщённые ссылки не разворачиваются: исходящих рёбер у них нет.
            let next = match dir {
                Direction::Out if !is_universal => Some(to_object),
                Direction::Out => None,
                Direction::In => Some(from_object),
            };
            if let Some(next) = next {
                if node_depth + 1 < depth && visited.insert(next.clone()) {
                    queue.push_back((next, node_depth + 1));
                }
            }
        }
        if edges_capped {
            break;
        }
    }

    // На глубине 1 полное число рёбер берётся точным подсчётом по индексу —
    // это дёшево и честно. Глубже точный ответ стоил бы полного обхода графа:
    // там либо собрано всё (обход дошёл до конца), либо число неполное.
    let exact_total = if depth == 1 && (edges_capped || stopped_by_time) {
        let count_sql = match dir {
            Direction::Out => "SELECT COUNT(*) FROM data_links WHERE repo = ?1 AND from_object = ?2",
            Direction::In => "SELECT COUNT(*) FROM data_links WHERE repo = ?1 AND to_object = ?2",
        };
        let exact: i64 = conn.query_row(count_sql, params!["default", object], |r| r.get(0))?;
        Some(exact.max(0) as usize)
    } else {
        None
    };

    Ok(Walk { edges, by_kind, stopped_by_time, edges_capped, exact_total })
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

    fn link(conn: &Connection, from: &str, path: &str, to: &str, kind: &str, universal: bool) {
        conn.execute(
            "INSERT INTO data_links \
             (repo, from_object, from_path, to_object, link_kind, is_composite, is_universal, to_object_key) \
             VALUES ('default', ?1, ?2, ?3, ?4, 0, ?5, lower(?3))",
            params![from, path, to, kind, universal as i64],
        )
        .unwrap();
    }

    fn far_deadline() -> Instant {
        Instant::now() + Duration::from_secs(30)
    }

    #[test]
    fn cycle_does_not_hang_the_walk() {
        let conn = mem();
        // Замкнутый треугольник — на обходе по путям он давал бы бесконечное
        // ветвление; с множеством посещённых узлов каждый разворачивается раз.
        link(&conn, "Catalog.A", "Б", "Catalog.B", "attr", false);
        link(&conn, "Catalog.B", "В", "Catalog.C", "attr", false);
        link(&conn, "Catalog.C", "А", "Catalog.A", "attr", false);

        let w = walk_links(&conn, "Catalog.A", 4, &Direction::Out, 100, far_deadline()).unwrap();
        assert_eq!(w.edges.len(), 3, "каждое ребро обойдено один раз");
        assert!(!w.stopped_by_time && !w.edges_capped);
        assert_eq!(w.by_kind["attr"], json!(3));
    }

    #[test]
    fn universal_target_is_terminal() {
        let conn = mem();
        link(&conn, "Document.X", "Партнёр", "*CatalogRef", "attr", true);
        link(&conn, "*CatalogRef", "Хвост", "Catalog.Z", "attr", false);

        let w = walk_links(&conn, "Document.X", 3, &Direction::Out, 100, far_deadline()).unwrap();
        assert_eq!(w.edges.len(), 1, "обобщённая ссылка дальше не разворачивается");
    }

    #[test]
    fn depth_limits_the_walk() {
        let conn = mem();
        link(&conn, "Catalog.A", "Б", "Catalog.B", "attr", false);
        link(&conn, "Catalog.B", "В", "Catalog.C", "attr", false);

        let one = walk_links(&conn, "Catalog.A", 1, &Direction::Out, 100, far_deadline()).unwrap();
        assert_eq!(one.edges.len(), 1);
        let two = walk_links(&conn, "Catalog.A", 2, &Direction::Out, 100, far_deadline()).unwrap();
        assert_eq!(two.edges.len(), 2);
        assert_eq!(two.edges[1]["depth"], json!(2));
    }

    #[test]
    fn incoming_direction_walks_backwards() {
        let conn = mem();
        link(&conn, "Document.Заказ", "Партнёр", "Catalog.Партнёры", "attr", false);
        link(&conn, "Document.Счёт", "Партнёр", "Catalog.Партнёры", "attr", false);

        let w = walk_links(&conn, "Catalog.Партнёры", 1, &Direction::In, 100, far_deadline()).unwrap();
        assert_eq!(w.edges.len(), 2, "оба ссылающихся документа найдены");
    }

    #[test]
    fn expired_deadline_stops_immediately() {
        let conn = mem();
        link(&conn, "Catalog.A", "Б", "Catalog.B", "attr", false);

        let past = Instant::now() - Duration::from_secs(1);
        let w = walk_links(&conn, "Catalog.A", 4, &Direction::Out, 100, past).unwrap();
        assert!(w.stopped_by_time, "истёкший срок прерывает обход");
        assert!(w.edges.is_empty());
    }

    #[test]
    fn edge_cap_stops_the_walk() {
        let conn = mem();
        for i in 0..50 {
            link(&conn, "Catalog.Центр", &format!("Реквизит{}", i), &format!("Catalog.Ц{}", i), "attr", false);
        }
        // limit=1 → потолок обхода 10 рёбер (limit × 10).
        let w = walk_links(&conn, "Catalog.Центр", 1, &Direction::Out, 1, far_deadline()).unwrap();
        assert!(w.edges_capped);
        assert_eq!(w.edges.len(), 10);
        // На глубине 1 полное число известно точно, поэтому «неполным» оно не
        // считается: собрали 10, а в графе 50 — и это видно читателю.
        assert_eq!(w.total(), 50);
        assert!(!w.total_is_partial());
    }

    #[test]
    fn partial_total_is_marked_deeper_than_one() {
        let conn = mem();
        // Цепочка вширь: на глубине 2 обход упрётся в потолок, а точного числа
        // рёбер окрестности дёшево не получить — значит число неполное.
        for i in 0..30 {
            link(&conn, "Catalog.Корень", &format!("Р{}", i), &format!("Catalog.У{}", i), "attr", false);
            for j in 0..5 {
                link(&conn, &format!("Catalog.У{}", i), &format!("П{}", j), &format!("Catalog.Л{}_{}", i, j), "attr", false);
            }
        }
        let w = walk_links(&conn, "Catalog.Корень", 2, &Direction::Out, 2, far_deadline()).unwrap();
        assert!(w.edges_capped, "потолок обхода сработал");
        assert!(w.total_is_partial(), "на глубине >1 число рёбер помечается неполным");
        assert_eq!(w.total(), w.edges.len());
    }
}
