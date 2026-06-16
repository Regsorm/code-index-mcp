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

/// Потолок строк по умолчанию. Занижен с 200 до 50: безфильтровый вызов с 200
/// подписками (каждая с sources) раздувал ответ до ~52K токенов; truncated+total
/// в ответе подсказывают сузить фильтр или дослать больший limit.
const DEFAULT_LIMIT: i64 = 50;
/// Жёсткий максимум (защита от выгрузки всех подписок в контекст).
const MAX_LIMIT: i64 = 2000;

/// Допустимые параметры tool'а — неизвестные ключи отклоняются с подсказкой
/// (агент передавал object=… и молча получал ВСЕ подписки вместо фильтра).
const KNOWN_PARAMS: &[&str] = &["repo", "handler_module", "event", "source", "limit"];

/// Совпадает ли подписка с фильтром source по её источникам
/// (sources — JSON-массив строк вида "cfg:DocumentObject.ЗаказКлиента").
/// Принимает 'Document.ЗаказКлиента', 'DocumentObject.ЗаказКлиента' или
/// короткое имя 'ЗаказКлиента'; регистр не учитывается (Unicode).
fn source_matches(sources: &Value, filter: &str) -> bool {
    let filter = filter.trim().trim_start_matches("cfg:");
    // 'Документ.X' → 'Document.X': источники в БД хранятся с английским типом.
    let filter_norm = crate::code_usages::normalize_object_ref(filter);
    let filter = filter_norm.as_ref();
    let (f_type, f_name) = match filter.split_once('.') {
        Some((t, n)) => (Some(t.to_lowercase()), n.to_lowercase()),
        None => (None, filter.to_lowercase()),
    };
    let Some(arr) = sources.as_array() else {
        return false;
    };
    arr.iter().filter_map(|v| v.as_str()).any(|entry| {
        let entry = entry.trim_start_matches("cfg:");
        let (e_type, e_name) = match entry.split_once('.') {
            Some((t, n)) => (t.to_lowercase(), n.to_lowercase()),
            None => (String::new(), entry.to_lowercase()),
        };
        if e_name != f_name {
            return false;
        }
        match &f_type {
            None => true,
            // 'Document' матчит 'DocumentObject' (в sources тип хранится
            // с суффиксом Object), точное совпадение — тоже.
            Some(ft) => e_type == *ft || e_type == format!("{}object", ft),
        }
    })
}

pub struct GetEventSubscriptionsTool;

impl IndexTool for GetEventSubscriptionsTool {
    fn name(&self) -> &str {
        "get_event_subscriptions"
    }

    fn description(&self) -> &str {
        "Возвращает список подписок на события 1С: name, event, handler_module, \
         handler_proc, sources. Опциональные фильтры: handler_module, event, \
         source (объект-источник: 'Document.ЗаказКлиента' или короткое имя). \
         Ответ ограничен limit (default 50, max 2000); при превышении рядом — \
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
                "source": {
                    "type": "string",
                    "description": "Опционально: фильтр по объекту-источнику подписки. Принимает 'Document.ЗаказКлиента', 'DocumentObject.ЗаказКлиента' или короткое имя 'ЗаказКлиента'"
                },
                "limit": {
                    "type": "integer",
                    "description": "Потолок строк (default 50, max 2000). При превышении — первые limit + total + truncated=true.",
                    "default": 50,
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
            // Неизвестные параметры → ошибка с перечнем допустимых (а не
            // молчаливое игнорирование с выгрузкой всех подписок в контекст).
            if let Some(obj) = args.as_object() {
                let unknown: Vec<&str> = obj
                    .keys()
                    .map(|k| k.as_str())
                    .filter(|k| !KNOWN_PARAMS.contains(k))
                    .collect();
                if !unknown.is_empty() {
                    return crate::tools::wrap_error(json!({
                        "error": format!("неизвестные параметры: {}", unknown.join(", ")),
                        "hint": "Допустимые фильтры: handler_module (модуль-обработчик), \
                                 event (событие, рус./англ.), source (объект-источник, \
                                 например 'Document.ЗаказКлиента' или короткое имя), limit.",
                    }));
                }
            }
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
            // Фильтр по объекту-источнику: применяется в Rust после выборки
            // (sources_json — JSON-массив, SQL LIKE по нему ловит ложные
            // подстроки; подписок в репо сотни, полный проход дёшев).
            let source = args
                .get("source")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
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
            // С фильтром source SQL-LIMIT снимается (LIMIT -1 = без предела в
            // SQLite): фильтрация идёт после выборки, обрезать раньше нельзя.
            let lim_plus = if source.is_some() { -1 } else { limit + 1 };
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
                                if let Some(ref f) = source {
                                    if !source_matches(&sources_v, f) {
                                        continue;
                                    }
                                }
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

            let full_len = out.len() as i64;
            let truncated = full_len > limit;
            if truncated {
                out.truncate(limit as usize);
            }
            // total: с source выборка была полной — total известен из full_len;
            // без source при обрезке — отдельный COUNT по тому же WHERE.
            let total = if source.is_some() {
                full_len
            } else if truncated {
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

#[cfg(test)]
mod tests {
    use super::source_matches;
    use serde_json::json;

    #[test]
    fn source_filter_matches_all_accepted_forms() {
        let sources = json!(["cfg:DocumentObject.ЗаказКлиента", "cfg:CatalogObject.Контрагенты"]);
        // Singular-тип, тип с суффиксом Object, короткое имя, cfg-префикс, регистр.
        assert!(source_matches(&sources, "Document.ЗаказКлиента"));
        assert!(source_matches(&sources, "DocumentObject.ЗаказКлиента"));
        assert!(source_matches(&sources, "ЗаказКлиента"));
        assert!(source_matches(&sources, "cfg:Document.ЗаказКлиента"));
        assert!(source_matches(&sources, "documentobject.заказклиента"));
        assert!(source_matches(&sources, "Catalog.Контрагенты"));
    }

    #[test]
    fn source_filter_rejects_mismatches() {
        let sources = json!(["cfg:DocumentObject.ЗаказКлиента"]);
        // Чужое имя, чужой тип при совпавшем имени, не-массив.
        assert!(!source_matches(&sources, "Document.ЗаказПоставщику"));
        assert!(!source_matches(&sources, "Catalog.ЗаказКлиента"));
        assert!(!source_matches(&json!(null), "ЗаказКлиента"));
        // Имя — подстрока, а не точное совпадение → не матчится.
        assert!(!source_matches(&sources, "Заказ"));
    }
}
