// MCP-tool `get_event_subscriptions` — возвращает список подписок на
// события 1С (event subscriptions) опционально с фильтрацией.
//
// Источник: таблица `event_subscriptions`, заполняется
// `index_extras::index_event_subscriptions` (этап 4c) из
// EventSubscriptions/<Name>.xml.

use std::future::Future;
use std::pin::Pin;

use code_index_core::extension::{IndexTool, ToolContext};
use serde_json::{json, Value};

pub struct GetEventSubscriptionsTool;

impl IndexTool for GetEventSubscriptionsTool {
    fn name(&self) -> &str {
        "get_event_subscriptions"
    }

    fn description(&self) -> &str {
        "Возвращает список подписок на события 1С: name, event, handler_module, \
         handler_proc, sources. Опциональные фильтры: handler_module, event. \
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
                    "description": "Опционально: вернуть только подписки на заданное событие (например, 'ПриЗаписи')"
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
            let event = args
                .get("event")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            let storage = ctx.storage.lock().await;
            let conn = storage.conn();

            // Динамический WHERE для опциональных фильтров.
            let mut where_parts: Vec<&str> = vec!["repo = ?"];
            let mut params_vec: Vec<&dyn rusqlite::ToSql> = vec![&"default" as &dyn rusqlite::ToSql];
            if handler_module.is_some() {
                where_parts.push("handler_module = ?");
            }
            if event.is_some() {
                where_parts.push("event = ?");
            }
            if let Some(ref m) = handler_module {
                params_vec.push(m as &dyn rusqlite::ToSql);
            }
            if let Some(ref e) = event {
                params_vec.push(e as &dyn rusqlite::ToSql);
            }
            let sql = format!(
                "SELECT name, event, handler_module, handler_proc, sources_json \
                 FROM event_subscriptions WHERE {} ORDER BY name",
                where_parts.join(" AND ")
            );

            let mut stmt = match conn.prepare(&sql) {
                Ok(s) => s,
                Err(e) => return json!({"error": format!("prepare failed: {}", e)}),
            };
            let rows = stmt.query_map(params_vec.as_slice(), |r| {
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
                            Err(e) => return json!({"error": format!("row error: {}", e)}),
                        }
                    }
                }
                Err(e) => return json!({"error": format!("query failed: {}", e)}),
            }
            json!({"subscriptions": out, "count": out.len()})
        })
    }
}
