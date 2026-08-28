// MCP-tool `get_role_rights` — права ролей на объект и состав прав роли.
//
// Отвечает на два встречных вопроса по таблице `role_rights`:
//   * «какие роли имеют права на объект и какие именно» (задан `object`) →
//     поле `roles`;
//   * «что разрешено роли» (задан `role`) → поле `objects`.
//
// Появился по итогам замера 28.08.2026: вопрос про права ролей агент решал
// перебором — листал каталог Roles и читал Rights.xml по кускам (27-33 хода).
// Универсальный `bsl_sql` эти данные отдаёт одним запросом, но модель к нему
// не шла: она уверенно берёт ИМЕНОВАННЫЕ инструменты под конкретный вопрос
// (get_form_handlers, get_register_writers) и не опознаёт произвольный SQL как
// ответ на прикладной вопрос. Поэтому права вынесены в отдельный tool.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use code_index_core::extension::{IndexTool, ToolContext};
use rusqlite::params;
use serde_json::{json, Value};

/// Потолок строк выборки. Права — короткие строки, поэтому потолок высокий;
/// защита от роли «Полные права», где объектов тысячи.
const CAP: i64 = 4000;

pub struct GetRoleRightsTool;

impl IndexTool for GetRoleRightsTool {
    fn name(&self) -> &str {
        "get_role_rights"
    }

    fn description(&self) -> &str {
        "Права ролей 1С за ОДИН вызов. Вопрос «какие роли имеют права на объект и \
         какие именно» — задай object ('Catalog.Организации'): вернутся роли со \
         списком прав каждой. Вопрос «что разрешено роли» — задай role \
         ('ПолныеПрава' либо 'Role.ПолныеПрава'): вернутся объекты с правами. \
         Данные берутся из индекса (таблица role_rights) — листать каталог Roles \
         и читать файлы Rights.xml по кускам НЕ нужно, это тот же ответ ценой \
         десятков вызовов. Имена прав — как в конфигурации (Read, Insert, Update, \
         View, Edit и т.д.). Ответ несёт счётчики (roles_count/objects_count) и \
         признак обрезки. For BSL/1C repositories only."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo": { "type": "string", "description": "Алиас репозитория" },
                "object": {
                    "type": "string",
                    "description": "Объект конфигурации: 'Catalog.Организации', 'Document.ЗаказКлиента', 'AccumulationRegister.ТоварыНаСкладах'"
                },
                "role": {
                    "type": "string",
                    "description": "Имя роли: 'ПолныеПрава' или 'Role.ПолныеПрава'"
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
            let object = crate::tools::object_value(&args)
                .map(|s| crate::code_usages::normalize_object_ref(s).into_owned());
            let role = args
                .get("role")
                .and_then(|v| v.as_str())
                .map(|s| s.trim_start_matches("Role.").to_string());

            if object.is_none() && role.is_none() {
                return crate::tools::wrap_error(json!({
                    "error": "укажите 'object' (права каких ролей на объект) либо 'role' (что разрешено роли)"
                }));
            }

            let storage = match ctx.storage.get().await {
                Ok(s) => s,
                Err(e) => {
                    return crate::tools::wrap_error(json!({
                        "error": format!("storage pool: {}", e)
                    }));
                }
            };
            let conn = storage.conn();

            let mut payload = serde_json::Map::new();

            if let Some(obj) = object {
                // Имя приводим к записи из конфигурации: SQLite не сворачивает
                // регистр кириллицы, иначе 'catalog.организации' дал бы пустоту.
                let obj = crate::tools::canonical_object_name(conn, &obj);
                let rows = match select_pairs(
                    conn,
                    "SELECT role_name, right_name FROM role_rights \
                     WHERE repo = ?1 AND object_name = ?2 \
                     ORDER BY role_name, right_name LIMIT ?3",
                    &obj,
                ) {
                    Ok(v) => v,
                    Err(e) => {
                        return crate::tools::wrap_error(json!({
                            "error": format!("database error (object): {}", e)
                        }))
                    }
                };
                let truncated = rows.len() as i64 > CAP;
                let grouped = group(rows, CAP as usize);
                let roles: Vec<Value> = grouped
                    .iter()
                    .map(|(role_name, rights)| json!({ "role": role_name, "rights": rights }))
                    .collect();
                payload.insert("object".into(), json!(obj));
                payload.insert("roles_count".into(), json!(roles.len()));
                payload.insert("roles".into(), json!(roles));
                if truncated {
                    payload.insert("roles_truncated".into(), json!(true));
                }
                if roles.is_empty() {
                    payload.insert(
                        "hint".into(),
                        json!("Ни одна роль не упоминает объект: проверьте имя объекта \
                               (get_object_structure) — в правах оно пишется как 'Catalog.Имя'."),
                    );
                }
            }

            if let Some(role_name) = role {
                let rows = match select_pairs(
                    conn,
                    "SELECT object_name, right_name FROM role_rights \
                     WHERE repo = ?1 AND role_name = ?2 \
                     ORDER BY object_name, right_name LIMIT ?3",
                    &role_name,
                ) {
                    Ok(v) => v,
                    Err(e) => {
                        return crate::tools::wrap_error(json!({
                            "error": format!("database error (role): {}", e)
                        }))
                    }
                };
                let truncated = rows.len() as i64 > CAP;
                let grouped = group(rows, CAP as usize);
                let objects: Vec<Value> = grouped
                    .iter()
                    .map(|(obj_name, rights)| json!({ "object": obj_name, "rights": rights }))
                    .collect();
                payload.insert("role".into(), json!(role_name));
                payload.insert("objects_count".into(), json!(objects.len()));
                payload.insert("objects".into(), json!(objects));
                if truncated {
                    payload.insert("objects_truncated".into(), json!(true));
                }
            }

            crate::tools::wrap_with_meta("get_role_rights", Value::Object(payload), Vec::new())
        })
    }
}

/// Выбрать пары (ключ, право) по готовому запросу с параметрами repo/ключ/потолок.
/// Берём CAP+1 строк, чтобы отличить «ровно потолок» от «есть ещё».
fn select_pairs(
    conn: &rusqlite::Connection,
    sql: &str,
    key: &str,
) -> rusqlite::Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params!["default", key, CAP + 1], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Схлопнуть пары в «ключ → список прав», не больше `cap` строк на входе.
fn group(rows: Vec<(String, String)>, cap: usize) -> Vec<(String, Vec<String>)> {
    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (key, right) in rows.into_iter().take(cap) {
        map.entry(key).or_default().push(right);
    }
    map.into_iter().collect()
}
