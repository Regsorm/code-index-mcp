// MCP-tool `get_event_subscriptions` — возвращает список подписок на
// события 1С (event subscriptions) опционально с фильтрацией.
//
// Источник: таблица `event_subscriptions`, заполняется
// `index_extras::index_event_subscriptions` (этап 4c) из
// EventSubscriptions/<Name>.xml.
//
// Защита контекста: отдаётся СТРАНИЦА набора, а не весь список. Размер страницы
// задаёт бюджет ответа (`[mcp].max_response_bytes` либо параметр вызова
// `max_response_bytes`), `limit` — потолок строк на страницу. Смещение — `offset`;
// рядом с массивом идут `subscriptions_shown/_total/_offset/_has_more`, а в `hint`
// — готовая строка следующего вызова. Без этого полный список (97 подписок на
// центральный документ — около 60 КБ на кириллице) терялся на слепом обрезе
// массивов в `cap_response`: тот делил массив вдвое и оставлял 48 записей из 97
// при счётчике `count: 97` — второго способа получить остаток не было.

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;

use super::subscription_sources::{is_extension, SourceResolver};
use crate::xml::event_subscriptions::SourceTypes;
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
const KNOWN_PARAMS: &[&str] = &[
    "repo",
    "handler_module",
    "event",
    "source",
    "exact_only",
    "limit",
    "offset",
    "max_response_bytes",
];

/// Совпадает ли подписка с фильтром source по её источникам
/// (sources — JSON-массив строк вида "cfg:DocumentObject.ЗаказКлиента").
/// Принимает 'Document.ЗаказКлиента', 'DocumentObject.ЗаказКлиента' или
/// короткое имя 'ЗаказКлиента'; регистр не учитывается (Unicode).
#[cfg(test)]
fn source_matches(sources: &Value, filter: &str) -> bool {
    sources.as_array().is_some_and(|a| {
        a.iter()
            .filter_map(Value::as_str)
            .any(|entry| super::subscription_sources::exact_matches(entry, filter))
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
         Учитывает TypeSet, определяемые типы и расширения; Document.X включает \
         Object/Manager/RecordSet, явный суффикс ограничивает вид. exact_only=true \
         оставляет явные источники. matched_via объясняет совпадение, owner — область. \
         complete=false и warnings показывают нераскрытые источники/старый индекс. \
         Выдача ПОСТРАНИЧНАЯ: в ответе subscriptions_shown/_total/_offset/_has_more, \
         а в hint — готовый следующий вызов с offset. Размер страницы ограничен \
         бюджетом ответа ([mcp].max_response_bytes, либо параметр max_response_bytes \
         на один вызов); limit (default 50, max 2000) — потолок строк на страницу. \
         Полный список обходится по offset; одним вызовом — с max_response_bytes \
         и limit побольше. \
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
                },
                "exact_only": {
                    "type": "boolean",
                    "default": false,
                    "description": "С source возвращать только явные типы, без TypeSet и определяемых типов"
                },
                "offset": {
                    "type": "integer",
                    "description": "Смещение в отфильтрованном наборе (по умолчанию 0). Берётся из subscriptions_offset/hint предыдущей страницы",
                    "default": 0,
                    "minimum": 0
                },
                "max_response_bytes": {
                    "type": "integer",
                    "description": "Бюджет ЭТОГО ответа в байтах: перекрывает [mcp].max_response_bytes, но не выше [cap].max_response_bytes_hard. Позволяет забрать весь список одним вызовом",
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
                                 например 'Document.ЗаказКлиента' или короткое имя), exact_only, limit.",
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
            let exact_only = args
                .get("exact_only")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let limit: i64 = args
                .get("limit")
                .and_then(|v| v.as_i64())
                .unwrap_or(DEFAULT_LIMIT)
                .clamp(1, MAX_LIMIT);
            // Смещение по отфильтрованному набору и бюджет одной страницы.
            // Бюджет разрешается тем же способом, что у find_references и
            // get_data_links: клиентский `max_response_bytes` главнее серверного,
            // но не выше потолка `[cap].max_response_bytes_hard`.
            let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
            let budget = code_index_core::mcp::cap::resolve_request_budget(
                args.get("max_response_bytes")
                    .and_then(Value::as_u64)
                    .map(|n| n as usize),
            );

            let storage = match ctx.storage.get().await {
                Ok(s) => s,
                Err(e) => {
                    return crate::tools::wrap_error(serde_json::json!({
                        "error": format!("storage pool: {}", e)
                    }));
                }
            };
            let conn = storage.conn();
            let resolver = match source
                .as_deref()
                .map(|s| SourceResolver::load(conn, s))
                .transpose()
            {
                Ok(r) => r,
                Err(e) => {
                    return crate::tools::wrap_error(
                        json!({"error":format!("source resolution: {e}")}),
                    )
                }
            };
            let mut warnings = BTreeSet::new();
            let built = code_index_core::storage::schema::index_data_version(conn);
            if built < code_index_core::storage::schema::INDEX_DATA_VERSION {
                warnings.insert("reindex_required: source sets and configuration owners may be missing; rebuild the index with index --force".to_string());
            }

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

            // Inspect all rows even without source: omitted rows can contain
            // warnings that affect the completeness of this answer.
            let lim_plus: i64 = -1;
            let data_sql = format!(
                "SELECT name, event, handler_module, handler_proc, sources_json, \
                 owner, source_path, type_sets_json, warnings_json \
                 FROM event_subscriptions WHERE {} ORDER BY name, owner LIMIT ?",
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
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, String>(8)?,
                ))
            });

            let mut out: Vec<Value> = Vec::new();
            match rows {
                Ok(iter) => {
                    for row in iter {
                        match row {
                            Ok((
                                name,
                                event,
                                module,
                                proc_,
                                sources,
                                owner,
                                source_path,
                                sets,
                                source_warnings,
                            )) => {
                                let types = (|| -> serde_json::Result<SourceTypes> {
                                    Ok(SourceTypes {
                                        sources: serde_json::from_str(
                                            sources.as_deref().unwrap_or("[]"),
                                        )?,
                                        type_sets: serde_json::from_str(&sets)?,
                                        warnings: serde_json::from_str(&source_warnings)?,
                                    })
                                })();
                                let types = match types {
                                    Ok(t) => t,
                                    Err(e) => {
                                        warnings.insert(format!("{owner}/{name}: invalid stored source description: {e}"));
                                        continue;
                                    }
                                };
                                let mut row_warnings = BTreeSet::new();
                                row_warnings.extend(types.warnings.iter().cloned());
                                let matched = resolver.as_ref().and_then(|r| {
                                    r.matches(&types, &owner, exact_only, &mut row_warnings)
                                });
                                warnings.extend(
                                    row_warnings
                                        .into_iter()
                                        .map(|w| format!("{owner}/{name}: {w}")),
                                );
                                if resolver.is_some() && matched.is_none() {
                                    continue;
                                }
                                // При заданном source полный список источников НЕ печатаем:
                                // мы уже знаем, что запрошенный объект в нём есть, а у глобальных
                                // подписок (ПередЗаписью, ПриЗаписи…) sources — до сотен типов;
                                // эхопечать раздувала ответ (80К+ → срыв лимита вывода). Отдаём
                                // только размер. Без фильтра — полный sources, как раньше.
                                let mut entry = json!({
                                    "name": name,
                                    "event": event,
                                    "handler_module": module,
                                    "handler_proc": proc_,
                                    "owner": owner,
                                });
                                if is_extension(&owner) {
                                    entry["extension"] = json!(owner);
                                }
                                if let Some(matched) = matched {
                                    let n = types.sources.len() + types.type_sets.len();
                                    entry["sources_count"] = json!(n);
                                    entry["matches_source"] = json!(true);
                                    entry["matched_via"] = json!(matched.via);
                                    entry["matched_source"] = json!(matched.source);
                                    if let Some(name) = matched.defined_type {
                                        entry["defined_type"] = json!(name);
                                    }
                                } else {
                                    entry["source_path"] = json!(source_path);
                                    entry["sources"] = json!(types.sources);
                                    entry["type_sets"] = json!(types.type_sets);
                                }
                                out.push(entry);
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

            // Страница набора: окно [offset, offset+limit) разрезается по бюджету
            // ответа. `total` и `has_more` считаются по ПОЛНОМУ набору, а не по
            // окну — иначе страница объявила бы конец набора раньше срока.
            let mut page = subscription_page(out, offset, limit, budget.applied);
            let shown = page.shown;
            let next_offset = page.next_offset();
            let items = std::mem::take(&mut page.items);

            let mut result = json!({
                "subscriptions": items,
                // count — РЕАЛЬНОЕ число отданных записей (было числом до обрезки:
                // при срабатывании стража ответ говорил 97 при 48 элементах).
                "count": shown,
                "total": page.total,
                "truncated": page.has_more,
                "limit": limit,
                "complete": warnings.is_empty(),
                "warnings_count": warnings.len(),
                "warnings": warnings.into_iter().take(80).collect::<Vec<_>>(),
            });
            if let Some(obj) = result.as_object_mut() {
                page.annotate(obj, "subscriptions");
                if budget.requested.is_some() {
                    obj.insert("response_budget_applied".to_string(), json!(budget.applied));
                }
                if let Some(next) = next_offset {
                    // Подсказка обязана нести фильтры текущего вызова: без них
                    // следующая страница была бы страницей ДРУГОГО набора.
                    let mut call: Vec<(&str, Value)> = vec![("repo", json!(ctx.repo))];
                    if let Some(ref s) = source {
                        call.push(("source", json!(s)));
                    }
                    if let Some(ref e) = event {
                        call.push(("event", json!(e)));
                    }
                    if let Some(ref m) = handler_module {
                        call.push(("handler_module", json!(m)));
                    }
                    if exact_only {
                        call.push(("exact_only", json!(true)));
                    }
                    call.push(("offset", json!(next)));
                    obj.insert(
                        "hint".to_string(),
                        json!(format!(
                            "Показано {} из {} (со смещения {}). Следующая страница — 1 вызов:\n{}",
                            shown,
                            page.total,
                            offset,
                            code_index_core::mcp::cap::next_call_hint(
                                "get_event_subscriptions",
                                &call
                            )
                        )),
                    );
                }
            }

            crate::tools::wrap_with_meta("get_event_subscriptions", result, Vec::new())
        })
    }
}

/// Одна страница подписок: окно `[offset, offset+limit)` набора, разрезанное по
/// бюджету байт. `total` и `has_more` считаются по ПОЛНОМУ набору — страница
/// видит только своё окно и иначе объявила бы конец набора раньше срока.
/// Первый элемент окна берётся всегда (`page_by_bytes`): пустая страница не
/// сдвинула бы `offset`, и обход зациклился бы на одном вызове.
fn subscription_page(
    out: Vec<Value>,
    offset: usize,
    limit: i64,
    budget: usize,
) -> code_index_core::mcp::cap::Page {
    let total = out.len();
    let offset = offset.min(total);
    let window: Vec<Value> = out
        .into_iter()
        .skip(offset)
        .take(limit.max(1) as usize)
        .collect();
    let mut page = code_index_core::mcp::cap::page_by_bytes(window, 0, budget);
    page.offset = offset;
    page.total = total;
    page.has_more = offset + page.shown < total;
    page
}

#[cfg(test)]
mod tests {
    use super::{source_matches, subscription_page};
    use serde_json::{json, Value};

    /// Запись подписки без source-фильтра: те же поля, что отдаёт инструмент.
    fn subs(n: usize) -> Vec<Value> {
        (0..n)
            .map(|i| {
                json!({
                    "name": format!("Подписка_{}", i),
                    "event": "ПриЗаписи",
                    "handler_module": "CommonModule.Обмен",
                    "handler_proc": format!("Обработчик{}", i),
                    "owner": "cfg",
                })
            })
            .collect()
    }

    /// Регрессия по #25: набор из 97 подписок обходится страницами целиком —
    /// без пропусков, повторов и зацикливания на одном смещении.
    #[test]
    fn page_walks_whole_set_without_gaps_or_repeats() {
        let items = subs(97);
        let mut offset = 0usize;
        let mut seen: Vec<String> = Vec::new();
        let mut pages = 0;
        loop {
            pages += 1;
            assert!(pages < 20, "обход обязан завершиться");
            let page = subscription_page(items.clone(), offset, 50, 6_000);
            assert!(
                page.shown > 0,
                "пустая страница на offset={offset} — обход зациклится"
            );
            assert_eq!(page.total, 97, "total — размер всего набора");
            for it in &page.items {
                seen.push(it["name"].as_str().unwrap().to_string());
            }
            match page.next_offset() {
                Some(next) => {
                    assert!(next > offset, "курсор двигается вперёд");
                    offset = next;
                }
                None => break,
            }
        }
        assert_eq!(seen.len(), 97, "весь набор собран");
        let unique: std::collections::BTreeSet<&String> = seen.iter().collect();
        assert_eq!(unique.len(), 97, "повторов нет");
    }

    /// `limit` — потолок строк на страницу, а не размер набора: маркеры должны
    /// говорить правду (97 в total, has_more=true, next=limit).
    #[test]
    fn limit_caps_rows_and_markers_stay_honest() {
        let page = subscription_page(subs(97), 0, 10, 1_000_000);
        assert_eq!(page.shown, 10, "limit — потолок строк на страницу");
        assert_eq!(page.total, 97, "total — размер всего набора");
        assert!(page.has_more);
        assert_eq!(page.next_offset(), Some(10));
    }

    /// Бюджет режет страницу ниже limit — иначе крупный sources-набор рвал бы
    /// ответ клиента (прежнее поведение: 48 записей из 97 без пути к остатку).
    #[test]
    fn budget_shrinks_the_page_below_limit() {
        let page = subscription_page(subs(97), 0, 2_000, 4_000);
        assert!(
            page.shown > 0 && page.shown < 97,
            "страница по бюджету: {}",
            page.shown
        );
        assert!(page.has_more);
        assert_eq!(page.next_offset(), Some(page.shown));
    }

    /// Смещение за концом набора — пустая страница с честным offset, без паники
    /// и без ложного has_more.
    #[test]
    fn offset_past_the_end_is_empty_and_terminal() {
        let page = subscription_page(subs(5), 99, 50, 48_000);
        assert_eq!(page.shown, 0);
        assert_eq!(page.offset, 5);
        assert!(!page.has_more);
        assert_eq!(page.next_offset(), None);
    }

    #[test]
    fn source_filter_matches_all_accepted_forms() {
        let sources = json!([
            "cfg:DocumentObject.ЗаказКлиента",
            "cfg:CatalogObject.Контрагенты"
        ]);
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
