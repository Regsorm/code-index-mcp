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
                    "description": "Полное имя владельца формы, например 'Document.РеализацияТоваровУслуг'"
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
                Some(s) => s.to_string(),
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

            let storage = ctx.storage.lock().await;
            let conn = storage.conn();
            let row = conn.query_row(
                "SELECT handlers_json \
                 FROM metadata_forms \
                 WHERE repo = ? AND owner_full_name = ? AND form_name = ?",
                params!["default", &owner, &form_name],
                |r| r.get::<_, Option<String>>(0),
            );

            let result_value = match row {
                Ok(handlers_json) => {
                    let handlers = handlers_json
                        .as_deref()
                        .and_then(|s| serde_json::from_str::<Value>(s).ok())
                        .unwrap_or_else(|| Value::Array(Vec::new()));
                    json!({
                        "owner_full_name": owner,
                        "form_name": form_name,
                        "handlers": handlers,
                    })
                }
                Err(rusqlite::Error::QueryReturnedNoRows) => json!({
                    "error": format!(
                        "form not found: owner='{}', form_name='{}', repo='{}'",
                        owner, form_name, ctx.repo
                    )
                }),
                Err(e) => json!({"error": format!("database error: {}", e)}),
            };
            crate::tools::wrap_with_meta(result_value, Vec::new())
        })
    }
}
