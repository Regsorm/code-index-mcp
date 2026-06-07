// MCP-tool `get_object_profile` — полный «паспорт» объекта конфигурации 1С
// за ОДИН вызов: структура (реквизиты/ТЧ/измерения/ресурсы) + формы + модули
// (с UUID для dbgs) + связи данных (исходящие/входящие/движения).
//
// Зачем отдельный tool, а не серия get_object_structure + get_form_handlers +
// get_data_links: для «горячего» сценария «расскажи всё про этот объект» это
// 1 round-trip вместо 4–5, и в контекст уходит один компактный агрегат, а не
// четыре отдельных JSON-ответа (экономия токенов — цель проекта).
//
// КЛЮЧЕВОЙ нюанс форматов (рассинхрон в индексе):
//   * `metadata_objects.full_name` и `data_links.*` — singular meta_type:
//     `Document.РеализацияТоваровУслуг`, `InformationRegister.Цены`.
//   * `metadata_forms.owner_full_name` и `metadata_modules.full_name` — папка
//     выгрузки (plural): `Documents.РеализацияТоваровУслуг`,
//     `Documents.X.ManagerModule`.
// Поэтому вход (singular `<MetaType>.<Name>`) конвертируется в папку через
// `meta_type_to_folder` для запросов к формам/модулям, а к metadata_objects и
// data_links идёт как есть.

use std::future::Future;
use std::pin::Pin;

use code_index_core::extension::{IndexTool, ToolContext};
use rusqlite::params;
use serde_json::{json, Value};

/// Сколько исходящих/входящих рёбер связей данных отдавать максимум
/// (защита от выгрузки тысяч строк по «центральным» объектам вроде Организации).
const LINKS_CAP: usize = 200;

pub struct GetObjectProfileTool;

impl IndexTool for GetObjectProfileTool {
    fn name(&self) -> &str {
        "get_object_profile"
    }

    fn description(&self) -> &str {
        "Полный паспорт объекта конфигурации 1С за ОДИН вызов по полному имени \
         ('Document.РеализацияТоваровУслуг', 'Catalog.Контрагенты'): structure \
         (реквизиты/табличные части/измерения/ресурсы/значения перечислений), forms \
         (формы + обработчики событий), modules (модули объекта с object_id/property_id \
         — UUID для dbgs-breakpoints — и code_path), data_links (исходящие ссылки, \
         движения в регистры для документов / регистраторы для регистров, число входящих \
         ссылок). Заменяет серию get_object_structure + get_form_handlers + get_data_links \
         одним round-trip'ом. Имя — singular meta_type ('<MetaType>.<Name>'). For BSL/1C \
         repositories only."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo": { "type": "string", "description": "Алиас репозитория" },
                "full_name": {
                    "type": "string",
                    "description": "Полное имя объекта вида '<MetaType>.<Name>' (singular), например 'Document.РеализацияТоваровУслуг'"
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

            // Разбор `<MetaType>.<Name>` (по первой точке — имена бывают с точками? нет,
            // в 1С имя объекта без точек, но берём split_once для надёжности).
            let (meta_type, name) = match full_name.split_once('.') {
                Some((mt, nm)) => (mt.to_string(), nm.to_string()),
                None => {
                    return crate::tools::wrap_error(json!({
                        "error": format!("full_name '{}' must be '<MetaType>.<Name>'", full_name)
                    }));
                }
            };
            let folder = meta_type_to_folder(&meta_type);

            let storage = ctx.storage.lock().await;
            let conn = storage.conn();

            // ── Заголовок + структура (metadata_objects, singular key) ────────
            let header = conn.query_row(
                "SELECT meta_type, name, synonym, attributes_json \
                 FROM metadata_objects WHERE repo = ?1 AND full_name = ?2",
                params![REPO, &full_name],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, Option<String>>(3)?,
                    ))
                },
            );

            let (found, db_meta_type, db_name, synonym, structure) = match header {
                Ok((mt, nm, syn, attrs)) => {
                    let structure = attrs
                        .as_deref()
                        .and_then(|s| serde_json::from_str::<Value>(s).ok())
                        .unwrap_or(Value::Null);
                    (true, mt, nm, syn, structure)
                }
                Err(rusqlite::Error::QueryReturnedNoRows) => {
                    // Объект может не иметь записи в metadata_objects (тип вне
                    // OBJECT_FOLDERS — например DataProcessor/Report), но формы/модули
                    // у него есть. Не выходим — отдаём что найдём, found=false.
                    (false, meta_type.clone(), name.clone(), None, Value::Null)
                }
                Err(e) => {
                    return crate::tools::wrap_error(json!({
                        "error": format!("database error (header): {}", e)
                    }));
                }
            };

            // ── Формы (metadata_forms, plural folder key) ─────────────────────
            let forms = match folder.as_deref() {
                Some(fld) => {
                    let owner = format!("{}.{}", fld, name);
                    match query_forms(conn, &owner) {
                        Ok(v) => v,
                        Err(e) => {
                            return crate::tools::wrap_error(json!({
                                "error": format!("database error (forms): {}", e)
                            }))
                        }
                    }
                }
                None => Vec::new(),
            };

            // ── Модули (metadata_modules, plural folder key) ──────────────────
            let modules = match folder.as_deref() {
                Some(fld) => {
                    let prefix = format!("{}.{}.", fld, name);
                    match query_modules(conn, &prefix) {
                        Ok(v) => v,
                        Err(e) => {
                            return crate::tools::wrap_error(json!({
                                "error": format!("database error (modules): {}", e)
                            }))
                        }
                    }
                }
                None => Vec::new(),
            };

            // ── Связи данных (data_links, singular key) ───────────────────────
            let data_links = match query_data_links(conn, &full_name) {
                Ok(v) => v,
                Err(e) => {
                    return crate::tools::wrap_error(json!({
                        "error": format!("database error (data_links): {}", e)
                    }))
                }
            };

            crate::tools::wrap_with_meta(
                json!({
                    "full_name": full_name,
                    "found": found,
                    "meta_type": db_meta_type,
                    "name": db_name,
                    "synonym": synonym,
                    "structure": structure,
                    "forms": forms,
                    "modules": modules,
                    "data_links": data_links,
                }),
                Vec::new(),
            )
        })
    }
}

/// Repo-ключ внутри per-repo index.db. Все BSL-таблицы пишут 'default'
/// (каждый репо — отдельный файл БД). См. index_extras::REPO_DEFAULT.
const REPO: &str = "default";

/// Формы объекта: имя + распарсенный список обработчиков.
fn query_forms(conn: &rusqlite::Connection, owner_full_name: &str) -> rusqlite::Result<Vec<Value>> {
    let mut stmt = conn.prepare(
        "SELECT form_name, handlers_json FROM metadata_forms \
         WHERE repo = ?1 AND owner_full_name = ?2 ORDER BY form_name",
    )?;
    let rows = stmt.query_map(params![REPO, owner_full_name], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (form_name, handlers_json) = row?;
        let handlers = handlers_json
            .as_deref()
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            .unwrap_or(Value::Array(Vec::new()));
        out.push(json!({ "form_name": form_name, "handlers": handlers }));
    }
    Ok(out)
}

/// Модули объекта: тип + UUID (object_id/property_id для dbgs) + путь + расширение.
fn query_modules(conn: &rusqlite::Connection, full_name_prefix: &str) -> rusqlite::Result<Vec<Value>> {
    // full_name вида 'Documents.X.ManagerModule' — берём по префиксу 'Documents.X.'.
    let like = format!("{}%", full_name_prefix.replace('%', "\\%").replace('_', "\\_"));
    let mut stmt = conn.prepare(
        "SELECT module_type, object_id, property_id, config_version, code_path, extension_name \
         FROM metadata_modules WHERE repo = ?1 AND full_name LIKE ?2 ESCAPE '\\' \
         ORDER BY extension_name, module_type",
    )?;
    let rows = stmt.query_map(params![REPO, like], |r| {
        Ok(json!({
            "module_type": r.get::<_, String>(0)?,
            "object_id": r.get::<_, Option<String>>(1)?,
            "property_id": r.get::<_, Option<String>>(2)?,
            "config_version": r.get::<_, Option<String>>(3)?,
            "code_path": r.get::<_, Option<String>>(4)?,
            "extension_name": r.get::<_, Option<String>>(5)?,
        }))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Связи данных объекта: исходящие рёбра (с капом), движения в обе стороны
/// (recorder) и число входящих ссылок.
fn query_data_links(conn: &rusqlite::Connection, object: &str) -> rusqlite::Result<Value> {
    // Исходящие (на что ссылается / куда пишет), кроме recorder — он отдельно.
    let mut out_stmt = conn.prepare(
        "SELECT link_kind, to_object, from_path FROM data_links \
         WHERE repo = ?1 AND from_object = ?2 AND link_kind != 'recorder' \
         ORDER BY link_kind, to_object LIMIT ?3",
    )?;
    let out_rows = out_stmt.query_map(params![REPO, object, LINKS_CAP as i64], |r| {
        Ok(json!({
            "link_kind": r.get::<_, String>(0)?,
            "to_object": r.get::<_, String>(1)?,
            "from_path": r.get::<_, String>(2)?,
        }))
    })?;
    let mut out_links = Vec::new();
    for row in out_rows {
        out_links.push(row?);
    }
    let out_total: i64 = conn.query_row(
        "SELECT COUNT(*) FROM data_links WHERE repo = ?1 AND from_object = ?2 AND link_kind != 'recorder'",
        params![REPO, object],
        |r| r.get(0),
    )?;

    // Движения: документ → регистры (from_object) и кто пишет в этот регистр (to_object).
    let writes_to = collect_col(
        conn,
        "SELECT DISTINCT to_object FROM data_links \
         WHERE repo = ?1 AND link_kind = 'recorder' AND from_object = ?2 ORDER BY to_object",
        object,
    )?;
    let written_by = collect_col(
        conn,
        "SELECT DISTINCT from_object FROM data_links \
         WHERE repo = ?1 AND link_kind = 'recorder' AND to_object = ?2 ORDER BY from_object",
        object,
    )?;

    // Входящие ссылки (кто ссылается на объект) — только счётчик (полный список
    // дороже и редко нужен целиком; за деталями — find_references / bsl_sql).
    let in_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM data_links WHERE repo = ?1 AND to_object = ?2 AND link_kind != 'recorder'",
        params![REPO, object],
        |r| r.get(0),
    )?;

    Ok(json!({
        "out": out_links,
        "out_total": out_total,
        "out_truncated": out_total as usize > out_links.len(),
        "writes_to_registers": writes_to,
        "written_by_documents": written_by,
        "incoming_refs_count": in_count,
    }))
}

/// Выбрать один текстовый столбец в Vec<String> по запросу с (repo, object).
fn collect_col(conn: &rusqlite::Connection, sql: &str, object: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params![REPO, object], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// singular meta_type → имя папки выгрузки (plural), под которым хранятся
/// формы (`owner_full_name`) и модули (`full_name`). Возвращает `None` для
/// типов без форм/модулей (Constant и т.п. могут не иметь — тогда формы/модули
/// просто пусты). Покрывает все типы, у которых бывают формы или модули.
fn meta_type_to_folder(meta_type: &str) -> Option<String> {
    let folder = match meta_type {
        "Catalog" => "Catalogs",
        "Document" => "Documents",
        "DocumentJournal" => "DocumentJournals",
        "Enum" => "Enums",
        "Report" => "Reports",
        "DataProcessor" => "DataProcessors",
        "InformationRegister" => "InformationRegisters",
        "AccumulationRegister" => "AccumulationRegisters",
        "AccountingRegister" => "AccountingRegisters",
        "CalculationRegister" => "CalculationRegisters",
        "ChartOfCharacteristicTypes" => "ChartsOfCharacteristicTypes",
        "ChartOfAccounts" => "ChartsOfAccounts",
        "ChartOfCalculationTypes" => "ChartsOfCalculationTypes",
        "ExchangePlan" => "ExchangePlans",
        "BusinessProcess" => "BusinessProcesses",
        "Task" => "Tasks",
        "SettingsStorage" => "SettingsStorages",
        "CommonForm" => "CommonForms",
        "Constant" => "Constants",
        "FilterCriterion" => "FilterCriteria",
        "Sequence" => "Sequences",
        // Незнакомый тип — эвристика 1С «+s» (Document→Documents, Report→Reports);
        // покрывает регулярные случаи, нерегулярные (ChartOf*) перечислены явно выше.
        other if !other.is_empty() => return Some(format!("{}s", other)),
        _ => return None,
    };
    Some(folder.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_mapping_handles_regular_and_irregular() {
        assert_eq!(meta_type_to_folder("Document").as_deref(), Some("Documents"));
        assert_eq!(meta_type_to_folder("Catalog").as_deref(), Some("Catalogs"));
        assert_eq!(
            meta_type_to_folder("ChartOfAccounts").as_deref(),
            Some("ChartsOfAccounts")
        );
        assert_eq!(
            meta_type_to_folder("ChartOfCharacteristicTypes").as_deref(),
            Some("ChartsOfCharacteristicTypes")
        );
        // Регулярная эвристика +s для неперечисленного типа.
        assert_eq!(meta_type_to_folder("Report").as_deref(), Some("Reports"));
        assert_eq!(meta_type_to_folder("SomeNewKind").as_deref(), Some("SomeNewKinds"));
        assert_eq!(meta_type_to_folder("").as_deref(), None);
    }

    #[test]
    fn profile_assembly_on_in_memory_db() {
        use rusqlite::Connection;
        let conn = Connection::open_in_memory().unwrap();
        for ddl in crate::schema::SCHEMA_EXTENSIONS {
            conn.execute_batch(ddl).unwrap();
        }
        // Объект (singular) + структура.
        conn.execute(
            "INSERT INTO metadata_objects (repo, full_name, meta_type, name, synonym, attributes_json) \
             VALUES ('default','Document.Реализация','Document','Реализация','Реализация товаров', \
             '{\"attributes\":[{\"name\":\"Контрагент\",\"type\":\"СправочникСсылка.Контрагенты\"}],\"tabular_sections\":[]}')",
            [],
        ).unwrap();
        // Форма (plural folder key).
        conn.execute(
            "INSERT INTO metadata_forms (repo, owner_full_name, form_name, handlers_json) \
             VALUES ('default','Documents.Реализация','ФормаДокумента','[{\"event\":\"ПриОткрытии\",\"handler\":\"ПриОткрытии\"}]')",
            [],
        ).unwrap();
        // Модуль (plural folder key) с UUID.
        conn.execute(
            "INSERT INTO metadata_modules (repo, full_name, object_name, module_type, object_id, property_id, code_path, extension_name) \
             VALUES ('default','Documents.Реализация.ObjectModule','Реализация','ObjectModule','uuid-obj','uuid-prop','Documents/Реализация/Ext/ObjectModule.bsl','')",
            [],
        ).unwrap();
        // Связи: документ ссылается на контрагента + пишет движение в регистр.
        conn.execute(
            "INSERT INTO data_links (repo, from_object, from_path, to_object, link_kind) \
             VALUES ('default','Document.Реализация','Контрагент','Catalog.Контрагенты','attr')",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO data_links (repo, from_object, from_path, to_object, link_kind) \
             VALUES ('default','Document.Реализация','','AccumulationRegister.Продажи','recorder')",
            [],
        ).unwrap();

        // forms
        let forms = query_forms(&conn, "Documents.Реализация").unwrap();
        assert_eq!(forms.len(), 1);
        assert_eq!(forms[0]["form_name"], json!("ФормаДокумента"));
        assert_eq!(forms[0]["handlers"][0]["event"], json!("ПриОткрытии"));
        // modules
        let modules = query_modules(&conn, "Documents.Реализация.").unwrap();
        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0]["module_type"], json!("ObjectModule"));
        assert_eq!(modules[0]["object_id"], json!("uuid-obj"));
        // data_links
        let dl = query_data_links(&conn, "Document.Реализация").unwrap();
        assert_eq!(dl["out"].as_array().unwrap().len(), 1);
        assert_eq!(dl["out"][0]["to_object"], json!("Catalog.Контрагенты"));
        assert_eq!(dl["writes_to_registers"][0], json!("AccumulationRegister.Продажи"));
        assert_eq!(dl["incoming_refs_count"], json!(0));
    }
}
