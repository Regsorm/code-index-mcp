// MCP-tool `get_form_handlers` — возвращает обработчики событий формы 1С по
// (owner_full_name, form_name) либо все формы объекта сразу, если form_name
// не задан.
//
// Источник: таблица `metadata_forms`, заполняется
// `index_extras::index_metadata_forms` (этап 4c) из Form.xml-файлов
// в выгрузке конфигурации.
//
// Размер ответа: у крупной формы сотни обработчиков (замер по восьми
// конфигурациям — до 42 КБ на одну форму при бюджете 48 КБ, то есть запас
// всего 12 %), а у объекта с 255 формами полный список — 242 КБ. Поэтому обе
// ветки ужимаются одинаково: не влезли — отдаём перечень с числами и готовый
// вызов за нужным куском.

use std::future::Future;
use std::pin::Pin;

use code_index_core::extension::{IndexTool, ToolContext};
use code_index_core::mcp::cap::{fold_to_budget, next_call_hint, resolve_request_budget};
use rusqlite::params;
use serde_json::{json, Value};

pub struct GetFormHandlersTool;

impl IndexTool for GetFormHandlersTool {
    fn name(&self) -> &str {
        "get_form_handlers"
    }

    fn description(&self) -> &str {
        "Возвращает обработчики событий управляемой формы 1С — тройки \
         (event, handler, element), где element — элемент формы, которому \
         принадлежит обработчик (нет поля — обработчик самой формы). \
         form_name НЕобязателен: без него возвращаются обработчики ВСЕХ форм \
         объекта одним вызовом (на объекте с 255 формами — сотни килобайт, \
         поэтому при нехватке бюджета вернётся перечень форм с числом \
         обработчиков и готовый вызов за нужной формой). \
         Крупные формы отдают сотни записей: сузить выдачу параметром element. \
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
                "owner_full_name": {
                    "type": "string",
                    "description": "Полное имя владельца формы — 'Document.РеализацияТоваровУслуг' или в формате папки выгрузки 'Documents.РеализацияТоваровУслуг' (принимаются оба)"
                },
                "form_name": {
                    "type": "string",
                    "description": "Имя формы — то, что было каталогом внутри Forms/, например 'ФормаДокумента'. Без параметра возвращаются все формы объекта."
                },
                "element": {
                    "type": "string",
                    "description": "Необязательный фильтр по элементу формы: имя элемента ('СуммаДокумента') — только его обработчики; пустая строка — только обработчики самой формы. Без параметра возвращаются все. Работает вместе с form_name."
                },
                "max_response_bytes": {
                    "type": "integer",
                    "description": "Бюджет размера ЭТОГО ответа в байтах — перекрывает серверный на один вызов. Применяй, когда пришёл перечень вместо обработчиков: повтори тот же вызов с большим значением. Запрос сверх серверного потолка зажимается до потолка."
                }
            },
            "required": ["repo", "owner_full_name"]
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
            let owner = match args.get("owner_full_name").and_then(|v| v.as_str()) {
                Some(s) => crate::code_usages::normalize_object_ref(s).into_owned(),
                None => {
                    return crate::tools::wrap_error(json!({
                        "error": "missing required parameter 'owner_full_name' (string)"
                    }));
                }
            };
            let form_name = args.get("form_name").and_then(|v| v.as_str()).map(|s| s.to_string());
            let budget = resolve_request_budget(
                args.get("max_response_bytes")
                    .and_then(|v| v.as_u64())
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

            // В БД owner_full_name хранится в формате папки выгрузки
            // ('Documents.X', plural). Принимаем оба формата: сначала точный
            // матч как есть, при промахе — повтор с конвертацией
            // '<Singular>.<Name>' → '<PluralFolder>.<Name>'.
            let mut owner_keys: Vec<String> = vec![owner.clone()];
            if let Some((meta_type, name)) = owner.split_once('.') {
                if let Some(folder) = crate::tools::meta_type_to_folder(meta_type) {
                    let candidate = format!("{}.{}", folder, name);
                    if candidate != owner {
                        owner_keys.push(candidate);
                    }
                }
            }

            let Some(form_name) = form_name else {
                // ── Все формы объекта одним вызовом ───────────────────────────
                let mut matched: Option<(String, Vec<(String, Value)>)> = None;
                for key in &owner_keys {
                    match query_all_forms(conn, key) {
                        Ok(forms) if !forms.is_empty() => {
                            matched = Some((key.clone(), forms));
                            break;
                        }
                        Ok(_) => continue,
                        Err(e) => {
                            return crate::tools::wrap_error(json!({
                                "error": format!("database error: {}", e)
                            }));
                        }
                    }
                }
                let Some((matched_owner, forms)) = matched else {
                    return crate::tools::wrap_error(owner_not_found(&owner, ctx.repo));
                };
                let value = all_forms_response(&matched_owner, forms, budget.applied, ctx.repo);
                return crate::tools::wrap_with_meta("get_form_handlers", value, Vec::new());
            };

            let mut found: Option<(String, Option<String>)> = None;
            for key in &owner_keys {
                let row = conn.query_row(
                    "SELECT handlers_json \
                     FROM metadata_forms \
                     WHERE repo = ? AND owner_full_name = ? AND form_name = ?",
                    params!["default", key, &form_name],
                    |r| r.get::<_, Option<String>>(0),
                );
                match row {
                    Ok(handlers_json) => {
                        found = Some((key.clone(), handlers_json));
                        break;
                    }
                    Err(rusqlite::Error::QueryReturnedNoRows) => continue,
                    Err(e) => {
                        return crate::tools::wrap_error(json!({
                            "error": format!("database error: {}", e)
                        }));
                    }
                }
            }

            let result_value = match found {
                Some((matched_owner, handlers_json)) => {
                    let handlers = handlers_json
                        .as_deref()
                        .and_then(|s| serde_json::from_str::<Value>(s).ok())
                        .unwrap_or_else(|| Value::Array(Vec::new()));
                    let element_filter = args.get("element").and_then(|v| v.as_str());
                    one_form_response(
                        &matched_owner,
                        &form_name,
                        handlers,
                        element_filter,
                        budget.applied,
                        ctx.repo,
                    )
                }
                None => {
                    // Умная ошибка: если владелец есть, но формы с таким именем
                    // нет — показать его реальные формы; если владельца нет
                    // вовсе — подсказать формат и как проверить имя.
                    let mut available: Vec<String> = Vec::new();
                    for key in &owner_keys {
                        let stmt = conn.prepare(
                            "SELECT form_name FROM metadata_forms \
                             WHERE repo = ? AND owner_full_name = ? \
                             ORDER BY form_name LIMIT 50",
                        );
                        if let Ok(mut stmt) = stmt {
                            let rows = stmt
                                .query_map(params!["default", key], |r| r.get::<_, String>(0));
                            if let Ok(rows) = rows {
                                available.extend(rows.flatten());
                            }
                        }
                        if !available.is_empty() {
                            break;
                        }
                    }
                    if available.is_empty() {
                        owner_not_found(&owner, ctx.repo)
                    } else {
                        json!({
                            "error": format!(
                                "form not found: owner='{}', form_name='{}', repo='{}'",
                                owner, form_name, ctx.repo
                            ),
                            "available_forms": available,
                        })
                    }
                }
            };
            crate::tools::wrap_with_meta("get_form_handlers", result_value, Vec::new())
        })
    }
}

/// Ошибка «владельца нет в индексе» с подсказкой про формат имени.
fn owner_not_found(owner: &str, repo: &str) -> Value {
    json!({
        "error": format!("form not found: owner='{}', repo='{}'", owner, repo),
        "hint": "Владелец не найден в metadata_forms. Формат owner_full_name — \
                 'Document.X' или 'Documents.X' (папка выгрузки). Проверьте имя \
                 объекта через get_object_structure, список форм — \
                 get_object_profile(sections=['forms']).",
    })
}

/// Все формы владельца: (имя формы, разобранные обработчики).
fn query_all_forms(
    conn: &rusqlite::Connection,
    owner_key: &str,
) -> rusqlite::Result<Vec<(String, Value)>> {
    let mut stmt = conn.prepare(
        "SELECT form_name, handlers_json FROM metadata_forms \
         WHERE repo = ? AND owner_full_name = ? ORDER BY form_name",
    )?;
    let rows = stmt.query_map(params!["default", owner_key], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (form_name, handlers_json) = row?;
        let handlers = handlers_json
            .as_deref()
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            .unwrap_or_else(|| Value::Array(Vec::new()));
        out.push((form_name, handlers));
    }
    Ok(out)
}

/// Сколько элементов в массиве-значении (0 для всего остального).
fn len_of(v: &Value) -> usize {
    v.as_array().map(|a| a.len()).unwrap_or(0)
}

/// Ответ по ВСЕМ формам объекта: обработчики целиком, а если не укладываются —
/// перечень форм с числами и готовый вызов за самой крупной.
fn all_forms_response(
    owner: &str,
    forms: Vec<(String, Value)>,
    budget: usize,
    repo: &str,
) -> Value {
    let handlers_total: usize = forms.iter().map(|(_, h)| len_of(h)).sum();
    let top_form = forms
        .iter()
        .max_by_key(|(_, h)| len_of(h))
        .map(|(n, _)| n.clone())
        .unwrap_or_default();
    let map: Vec<Value> = forms
        .iter()
        .map(|(n, h)| json!({ "form_name": n, "handlers_count": len_of(h) }))
        .collect();
    let full: Vec<Value> = forms
        .into_iter()
        .map(|(n, h)| json!({ "form_name": n, "handlers": h }))
        .collect();
    let forms_total = full.len();

    let head = |forms_value: Vec<Value>, included: bool| -> Value {
        json!({
            "owner_full_name": owner,
            "forms": forms_value,
            "forms_total": forms_total,
            "handlers_total": handlers_total,
            "handlers_included": included,
        })
    };
    let full_bytes = serde_json::to_string(&full).map(|s| s.len()).unwrap_or(0);
    let (mut value, folded) = fold_to_budget(head(full, true), head(map, false), budget);
    if folded {
        let call = next_call_hint(
            "get_form_handlers",
            &[
                ("repo", json!(repo)),
                ("owner_full_name", json!(owner)),
                ("form_name", json!(top_form)),
            ],
        );
        value["hint"] = json!(format!(
            "Формы отданы перечнем: {} форм, обработчиков всего {}, содержимое не включено \
             (целиком ≈{} КБ).\nОбработчики одной формы — 1 вызов:\n{}",
            forms_total,
            handlers_total,
            (full_bytes + 512) / 1024,
            call
        ));
    }
    value
}

/// Ответ по ОДНОЙ форме: обработчики целиком, а если не укладываются — счётчики
/// по элементам формы и готовый вызов за обработчиками самого крупного из них.
fn one_form_response(
    owner: &str,
    form_name: &str,
    handlers: Value,
    element_filter: Option<&str>,
    budget: usize,
    repo: &str,
) -> Value {
    let total = len_of(&handlers);
    let handlers = match (element_filter, handlers) {
        (Some(filter), Value::Array(items)) => Value::Array(
            items
                .into_iter()
                .filter(|h| h.get("element").and_then(|v| v.as_str()).unwrap_or("") == filter)
                .collect(),
        ),
        (_, other) => other,
    };
    let shown = len_of(&handlers);

    // Счётчики по элементам формы: ключ "" — обработчики самой формы.
    let mut by_element = serde_json::Map::new();
    if let Some(items) = handlers.as_array() {
        for h in items {
            let key = h.get("element").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let n = by_element.get(&key).and_then(|v| v.as_u64()).unwrap_or(0) + 1;
            by_element.insert(key, json!(n));
        }
    }
    let top_element = by_element
        .iter()
        .filter(|(k, _)| !k.is_empty())
        .max_by_key(|(_, v)| v.as_u64().unwrap_or(0))
        .map(|(k, _)| k.clone())
        .unwrap_or_default();

    let head = |body: (&str, Value), included: bool| -> Value {
        let mut out = json!({
            "owner_full_name": owner,
            "form_name": form_name,
            "handlers_included": included,
            "handlers_total": total,
        });
        out[body.0] = body.1;
        if element_filter.is_some() {
            out["handlers_shown"] = json!(shown);
        }
        out
    };
    let full_bytes = serde_json::to_string(&handlers).map(|s| s.len()).unwrap_or(0);
    let (mut value, folded) = fold_to_budget(
        head(("handlers", handlers), true),
        head(("handlers_by_element", Value::Object(by_element)), false),
        budget,
    );
    if folded {
        let call = next_call_hint(
            "get_form_handlers",
            &[
                ("repo", json!(repo)),
                ("owner_full_name", json!(owner)),
                ("form_name", json!(form_name)),
                ("element", json!(top_element)),
            ],
        );
        value["hint"] = json!(format!(
            "Обработчики отданы счётчиками по элементам формы (ключ \"\" — события самой формы): \
             всего {}, целиком ≈{} КБ.\nОбработчики одного элемента — 1 вызов:\n{}",
            total,
            (full_bytes + 512) / 1024,
            call
        ));
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handlers(n: usize, element: &str) -> Vec<Value> {
        (0..n)
            .map(|i| {
                json!({
                    "event": format!("Событие{}", i),
                    "handler": format!("Обработчик{}", i),
                    "element": element,
                })
            })
            .collect()
    }

    #[test]
    fn all_forms_returned_whole_while_they_fit() {
        let forms = vec![
            ("ФормаСписка".to_string(), json!(handlers(2, "Список"))),
            ("ФормаДокумента".to_string(), json!(handlers(5, "Сумма"))),
        ];
        let v = all_forms_response("Documents.Реализация", forms, 48_000, "ut");
        assert_eq!(v["handlers_included"], json!(true));
        assert_eq!(v["forms_total"], json!(2));
        assert_eq!(v["handlers_total"], json!(7));
        assert_eq!(v["forms"][0]["handlers"][0]["event"], json!("Событие0"));
        assert!(v.get("hint").is_none(), "укладывается — подсказка не нужна");
    }

    #[test]
    fn all_forms_folded_to_counts_with_call_for_the_biggest() {
        let forms = vec![
            ("ФормаСписка".to_string(), json!(handlers(2, "Список"))),
            ("ФормаДокумента".to_string(), json!(handlers(40, "Сумма"))),
        ];
        let v = all_forms_response("Documents.Реализация", forms, 500, "ut");
        assert_eq!(v["handlers_included"], json!(false));
        assert_eq!(v["forms"][1]["handlers_count"], json!(40));
        assert!(v["forms"][1].get("handlers").is_none());
        let hint = v["hint"].as_str().unwrap();
        assert!(
            hint.contains("form_name='ФормаДокумента'"),
            "подсказка ведёт к самой крупной форме: {hint}"
        );
    }

    #[test]
    fn one_form_folds_to_element_counts() {
        let mut items = handlers(30, "Товары");
        items.extend(handlers(3, "Список"));
        let v = one_form_response(
            "Documents.Реализация",
            "ФормаДокумента",
            json!(items),
            None,
            500,
            "ut",
        );
        assert_eq!(v["handlers_included"], json!(false));
        assert_eq!(v["handlers_total"], json!(33));
        assert_eq!(v["handlers_by_element"]["Товары"], json!(30));
        assert!(v.get("handlers").is_none(), "содержимое не отдаётся");
        let hint = v["hint"].as_str().unwrap();
        assert!(hint.contains("element='Товары'"), "подсказка ведёт к элементу: {hint}");
    }

    #[test]
    fn one_form_keeps_element_filter_and_counters() {
        let mut items = handlers(3, "Товары");
        items.extend(handlers(2, "Список"));
        let v = one_form_response(
            "Documents.Реализация",
            "ФормаДокумента",
            json!(items),
            Some("Список"),
            48_000,
            "ut",
        );
        assert_eq!(v["handlers_included"], json!(true));
        assert_eq!(v["handlers_total"], json!(5), "всего до фильтра");
        assert_eq!(v["handlers_shown"], json!(2), "после фильтра");
        assert_eq!(v["handlers"].as_array().unwrap().len(), 2);
    }
}
