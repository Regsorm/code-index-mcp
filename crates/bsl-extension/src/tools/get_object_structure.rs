// MCP-tool `get_object_structure` — отдаёт структуру объекта конфигурации
// 1С (Catalog/Document/...) по его full_name (`Catalog.Контрагенты`).
//
// Источник данных: таблица `metadata_objects`, заполняется
// `index_extras::index_metadata_objects` (этап 4c).
//
// На текущем этапе attributes_json в `metadata_objects` пуст — парсер
// детальных XML-файлов объекта (Catalogs/<Name>.xml с реквизитами и
// табличными частями) не реализован, это будущая работа. Пока tool
// возвращает `meta_type` и `name`, чего достаточно для большинства
// LLM-запросов «что за объект `Document.РеализацияТоваровУслуг`».

use std::future::Future;
use std::pin::Pin;

use code_index_core::extension::{IndexTool, ToolContext};
use rusqlite::params;
use serde_json::{json, Value};

pub struct GetObjectStructureTool;

impl IndexTool for GetObjectStructureTool {
    fn name(&self) -> &str {
        "get_object_structure"
    }

    fn description(&self) -> &str {
        "Возвращает структуру объекта конфигурации 1С (справочника, документа, регистра и т.д.) \
         по его полному имени вида 'Catalog.Контрагенты' или 'Document.РеализацияТоваровУслуг'. \
         For BSL/1C repositories only."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo": {
                    "type": "string",
                    "description": "Алиас репозитория (из --path alias=dir или daemon.toml)"
                },
                "full_name": {
                    "type": "string",
                    "description": "Полное имя объекта вида '<MetaType>.<Name>', например 'Catalog.Контрагенты'"
                }
            },
            "required": ["repo", "full_name"]
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
            let full_name = match args.get("full_name").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => {
                    return json!({
                        "error": "missing required parameter 'full_name' (string)"
                    });
                }
            };

            let storage = ctx.storage.lock().await;
            let conn = storage.conn();
            let row = conn.query_row(
                "SELECT meta_type, name, synonym, attributes_json \
                 FROM metadata_objects WHERE repo = ? AND full_name = ?",
                params!["default", &full_name],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, Option<String>>(3)?,
                    ))
                },
            );

            match row {
                Ok((meta_type, name, synonym, attrs)) => {
                    let attrs_value = attrs
                        .as_deref()
                        .and_then(|s| serde_json::from_str::<Value>(s).ok())
                        .unwrap_or(Value::Null);
                    json!({
                        "full_name": full_name,
                        "meta_type": meta_type,
                        "name": name,
                        "synonym": synonym,
                        "attributes": attrs_value,
                    })
                }
                Err(rusqlite::Error::QueryReturnedNoRows) => json!({
                    "error": format!("object '{}' not found in repo '{}'", full_name, ctx.repo)
                }),
                Err(e) => json!({
                    "error": format!("database error: {}", e)
                }),
            }
        })
    }
}
