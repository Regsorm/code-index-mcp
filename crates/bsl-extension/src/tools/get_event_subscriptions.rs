// MCP-tool `get_event_subscriptions` — возвращает список подписок на
// события 1С (event subscriptions) опционально с фильтрацией.
//
// Источник: таблица `event_subscriptions`, заполняется
// `index_extras::index_event_subscriptions` (этап 4c) из
// EventSubscriptions/<Name>.xml.
//
// Защита контекста: ответ ограничен `limit` строками (default 200, max 2000).
// При превышении возвращаются первые `limit` подписок, рядом — `total`
// (полное число) и `truncated=true`, чтобы модель сузила фильтр
// (handler_module/event) или дослала больший limit. Без этого
// безфильтровый вызов на крупной конфигурации (сотни подписок, каждая с
// sources_json) переполнял контекст агента.

use std::future::Future;
use std::pin::Pin;

use code_index_core::extension::{IndexTool, ToolContext};
use serde_json::{json, Value};

/// Потолок строк по умолчанию.
const DEFAULT_LIMIT: i64 = 200;
/// Жёсткий максимум (защита от выгрузки всех подписок в контекст).
const MAX_LIMIT: i64 = 2000;

pub struct GetEventSubscriptionsTool;

impl IndexTool for GetEventSubscriptionsTool {
    fn name(&self) -> &str {
        "get_event_subscriptions"
    }

    fn description(&self) -> &str {
        "Возвращает список подписок на события 1С: name, event, handler_module, \
         handler_proc, sources. Опциональные фильтры: handler_module, event. \
         Ответ ограничен limit (default 200, max 2000); при превышении рядом — \
         total и truncated=true (сузьте фильтр или дошлите больший limit). \
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
                "handler_module": {
                    "type": "string",
                    "description": "Опционально: вернуть только подписки с заданным handler_module"
                },
                "event": {
                    "type": "string",
                    "description": "Опционально: фильтр по событию. Принимает русское имя ('ПриЗаписи', 'ОбработкаПроведения') либо английское ('OnWrite', 'Posting') — нормализуется автоматически"
                },
                "limit": {
                    "type": "integer",
                    "description": "Потолок строк (default 200, max 2000). При превышении — первые limit + total + truncated=true.",
                    "default": 200,
                    "minimum": 1
                }
            },
            "required": ["repo"]
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
            let handler_module = args
                .get("handler_module")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            // D1: фильтр матчит и полное имя (`CommonModule.X`), и короткое
            // (`X`) — через суффиксный LIKE `%.X`. Строка владеющая, для ToSql.
            let like_module = handler_module.as_ref().map(|m| format!("%.{}", m));
            // Фильтр по событию — двусторонний: в БД событие хранится в русском
            // виде (`ПриЗаписи`), поэтому вход нормализуем тем же маппингом
            // (англ. `OnWrite` → рус., рус./неизвестное — без изменений), чтобы
            // матчились оба варианта.
            let event = args
                .get("event")
                .and_then(|v| v.as_str())
                .map(|s| crate::xml::event_subscriptions::event_to_russian(s).to_string());
            let limit: i64 = args
                .get("limit")
                .and_then(|v| v.as_i64())
                .unwrap_or(DEFAULT_LIMIT)
                .clamp(1, MAX_LIMIT);

            let storage = ctx.storage.lock().await;
            let conn = storage.conn();

            // Динамический WHERE для опциональных фильтров.
            let mut where_parts: Vec<&str> = vec!["repo = ?"];
            if handler_module.is_some() {
                where_parts.push("(handler_module = ? OR handler_module LIKE ?)");
            }
            if event.is_some() {
                where_parts.push("event = ?");
            }
            let where_sql = where_parts.join(" AND ");

            // Базовые параметры WHERE (без LIMIT) — пересобираются для data и count
            // запросов (Vec<&dyn ToSql> не клонируется). Замыкание захватывает
            // владеющие строки, которые живут до конца блока.
            let base_params = || -> Vec<&dyn rusqlite::ToSql> {
                let mut v: Vec<&dyn rusqlite::ToSql> = vec![&"default" as &dyn rusqlite::ToSql];
                if let Some(ref m) = handler_module {
                    v.push(m as &dyn rusqlite::ToSql);
                }
                if let Some(ref lm) = like_module {
                    v.push(lm as &dyn rusqlite::ToSql);
                }
                if let Some(ref e) = event {
                    v.push(e as &dyn rusqlite::ToSql);
                }
                v
            };

            // Берём limit+1, чтобы отличить «ровно limit» от «есть ещё».
            let lim_plus = limit + 1;
            let data_sql = format!(
                "SELECT name, event, handler_module, handler_proc, sources_json \
                 FROM event_subscriptions WHERE {} ORDER BY name LIMIT ?",
                where_sql
            );
            let mut data_params = base_params();
            data_params.push(&lim_plus as &dyn rusqlite::ToSql);

            let mut stmt = match conn.prepare(&data_sql) {
                Ok(s) => s,
                Err(e) => {
                    return crate::tools::wrap_error(json!({
                        "error": format!("prepare failed: {}", e)
                    }))
                }
            };
            let rows = stmt.query_map(data_params.as_slice(), |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                ))
            });

            let mut out: Vec<Value> = Vec::new();
            match rows {
                Ok(iter) => {
                    for row in iter {
                        match row {
                            Ok((name, event, module, proc_, sources)) => {
                                let sources_v = sources
                                    .as_deref()
                                    .and_then(|s| serde_json::from_str::<Value>(s).ok())
                                    .unwrap_or(Value::Array(Vec::new()));
                                out.push(json!({
                                    "name": name,
                                    "event": event,
                                    "handler_module": module,
                                    "handler_proc": proc_,
                                    "sources": sources_v,
                                }));
                            }
                            Err(e) => {
                                return crate::tools::wrap_error(json!({
                                    "error": format!("row error: {}", e)
                                }))
                            }
                        }
                    }
                }
                Err(e) => {
                    return crate::tools::wrap_error(json!({
                        "error": format!("query failed: {}", e)
                    }))
                }
            }

            let truncated = out.len() as i64 > limit;
            if truncated {
                out.truncate(limit as usize);
            }
            // total: при обрезке — отдельный COUNT по тому же WHERE; иначе len.
            let total = if truncated {
                let count_sql = format!(
                    "SELECT COUNT(*) FROM event_subscriptions WHERE {}",
                    where_sql
                );
                conn.query_row(&count_sql, base_params().as_slice(), |r| r.get::<_, i64>(0))
                    .unwrap_or(out.len() as i64)
            } else {
                out.len() as i64
            };

            let count = out.len();
            crate::tools::wrap_with_meta(
                json!({
                    "subscriptions": out,
                    "count": count,
                    "total": total,
                    "truncated": truncated,
                    "limit": limit,
                }),
                Vec::new(),
            )
        })
    }
}
