// MCP-tool `get_object_structure` — отдаёт структуру объекта конфигурации
// 1С (Catalog/Document/...) по его full_name (`Catalog.Контрагенты`).
//
// Источник данных: таблица `metadata_objects`. Имя/тип заполняет
// `index_extras::index_metadata_objects` (из Configuration.xml), а
// `attributes_json` — `index_extras::index_object_attributes` (парсит
// корневой XML объекта `Catalogs/<Name>.xml` через
// `xml::object_attributes::parse_object_structure_file`): реквизиты с
// типами, табличные части, измерения и ресурсы регистров.
//
// `attributes` в ответе = распарсенный `attributes_json` (Null, если объект
// без полей либо его XML не найден — например, для типов вне OBJECT_FOLDERS).

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
        "Возвращает полную структуру объекта конфигурации 1С по полному имени \
         ('Catalog.Контрагенты', 'Document.РеализацияТоваровУслуг'): реквизиты с типами, \
         табличные части, измерения/ресурсы регистров; 'enum_values' для перечислений; \
         'predefined' для объектов с предопределёнными элементами. Базовые секции \
         (attributes/dimensions/resources/tabular_sections) присутствуют всегда (пустые — []). \
         Это единственный источник структуры объекта — XML объектов НЕ индексируется как \
         текст, не ищите его через list_files/grep_text. For BSL/1C repositories only."
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
                    return crate::tools::wrap_error(json!({
                        "error": "missing required parameter 'full_name' (string)"
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

            let result_value = match row {
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
                Err(rusqlite::Error::QueryReturnedNoRows) => {
                    // fuzzy-подсказка: объект не найден — предложим похожие по
                    // префиксу имени. Ловит опечатки в середине слова, напр.
                    // 'Document.РеализацияТоваровИУслуг' → 'РеализацияТоваровУслуг'
                    // (префикс 'Реализ' совпадает). Слабое место #5 прогона УТ-11.
                    let (mtype, short) = match full_name.split_once('.') {
                        Some((t, n)) => (Some(t.to_string()), n.to_string()),
                        None => (None, full_name.clone()),
                    };
                    let prefix: String = short.chars().take(6).collect();
                    let like_prefix = format!("{}%", prefix);
                    let mut suggestions: Vec<String> = Vec::new();
                    // 1) тот же meta_type + префикс имени
                    if let Some(ref t) = mtype {
                        if let Ok(mut s) = conn.prepare(
                            "SELECT full_name FROM metadata_objects \
                             WHERE repo = 'default' AND meta_type = ?1 AND name LIKE ?2 \
                             ORDER BY name LIMIT 8",
                        ) {
                            if let Ok(rows) =
                                s.query_map(params![t, like_prefix], |r| r.get::<_, String>(0))
                            {
                                suggestions.extend(rows.flatten());
                            }
                        }
                    }
                    // 2) добор по подстроке имени без учёта meta_type
                    if suggestions.len() < 8 {
                        let sub: String = short.chars().take(8).collect();
                        let like_sub = format!("%{}%", sub);
                        if let Ok(mut s) = conn.prepare(
                            "SELECT full_name FROM metadata_objects \
                             WHERE repo = 'default' AND name LIKE ?1 \
                             ORDER BY name LIMIT 8",
                        ) {
                            if let Ok(rows) =
                                s.query_map(params![like_sub], |r| r.get::<_, String>(0))
                            {
                                for fqn in rows.flatten() {
                                    if !suggestions.contains(&fqn) {
                                        suggestions.push(fqn);
                                    }
                                }
                            }
                        }
                    }
                    suggestions.truncate(8);
                    json!({
                        "error": format!("object '{}' not found in repo '{}'", full_name, ctx.repo),
                        "did_you_mean": suggestions,
                        "hint": "Формат '<MetaType>.<Name>': MetaType англ. (Catalog/Document/AccumulationRegister/InformationRegister/ChartOfAccounts/…), Name — точное имя из конфигурации. Список объектов типа — через MCP 1c list_metadata_objects."
                    })
                }
                Err(e) => json!({
                    "error": format!("database error: {}", e)
                }),
            };
            crate::tools::wrap_with_meta(result_value, Vec::new())
        })
    }
}
