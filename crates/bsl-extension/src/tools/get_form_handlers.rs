// MCP-tool `get_form_handlers` — возвращает список обработчиков событий
// формы 1С по (owner_full_name, form_name).
//
// Источник: таблица `metadata_forms`, заполняется
// `index_extras::index_metadata_forms` (этап 4c) из Form.xml-файлов
// в выгрузке конфигурации.

use std::future::Future;
use std::pin::Pin;

use code_index_core::extension::{IndexTool, ToolContext};
use rusqlite::params;
use serde_json::{json, Value};

pub struct GetFormHandlersTool;

impl IndexTool for GetFormHandlersTool {
    fn name(&self) -> &str {
        "get_form_handlers"
    }

    fn description(&self) -> &str {
        "Возвращает обработчики событий управляемой формы 1С — пары \
         (event, handler), извлечённые из <Events> в Form.xml. \
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
                    "description": "Имя формы — то, что было каталогом внутри Forms/, например 'ФормаДокумента'"
                }
            },
            "required": ["repo", "owner_full_name", "form_name"]
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
            let form_name = match args.get("form_name").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => {
                    return crate::tools::wrap_error(json!({
                        "error": "missing required parameter 'form_name' (string)"
                    }));
                }
            };

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
                    json!({
                        "owner_full_name": matched_owner,
                        "form_name": form_name,
                        "handlers": handlers,
                    })
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
                        json!({
                            "error": format!(
                                "form not found: owner='{}', form_name='{}', repo='{}'",
                                owner, form_name, ctx.repo
                            ),
                            "hint": "Владелец не найден в metadata_forms. Формат owner_full_name — \
                                     'Document.X' или 'Documents.X' (папка выгрузки). Проверьте имя \
                                     объекта через get_object_structure, список форм — \
                                     bsl_sql: SELECT owner_full_name, form_name FROM metadata_forms.",
                        })
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
