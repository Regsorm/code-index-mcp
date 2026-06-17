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
         табличные части, измерения/ресурсы регистров; 'enum_values' для перечислений \
         (+'enum_synonyms' — UI-подписи значений); 'predefined' для объектов с \
         предопределёнными элементами; 'owners' — владельцы подчинённого справочника; \
         'value_types' — тип значения характеристик ПВХ (доступные аналитики) / константы; \
         'properties' — свойства шапки (периодичность ИР, режим записи, нумерация документа, \
         иерархия); 'commands' — команды объекта (имя + UI-подпись: «Создать на основании», \
         печатные формы и т.п.). У реквизитов есть 'synonym' (UI-подпись) и 'required' \
         (обязательность заполнения), когда они заданы. Базовые секции \
         (attributes/dimensions/resources/tabular_sections) присутствуют всегда (пустые — []). \
         Это единственный источник структуры объекта — XML объектов НЕ индексируется как \
         текст, не ищите его через list_files/grep_text. For BSL/1C repositories only. \
         МАССОВЫЙ РЕЖИМ ('full_names'): батчи список ТОЛЬКО когда точно нужен ВЕСЬ набор и структура одного объекта не отменит надобность в остальных (например, разбираешь уже подтверждённый список). Если ОТБИРАЕШЬ, какие из объектов релевантны, или результат одного может сделать остальные ненужными — НЕ батчи, запрашивай по одному с остановкой по ходу. Сомневаешься — по одному. Ответ на батч — {results:[...]} в том же порядке."
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
                    "description": "Полное имя ОДНОГО объекта вида '<MetaType>.<Name>', например 'Catalog.Контрагенты'. Для нескольких объектов используйте 'full_names'."
                },
                "full_names": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Список полных имён для МАССОВОГО запроса. Применяй ТОЛЬКО когда заведомо нужен весь набор (см. описание инструмента); если отбираешь релевантные — по одному 'full_name'. Ответ — {results:[...]} в том же порядке."
                },
                "sections": {
                    "type": "array",
                    "items": { "type": "string", "enum": ["attributes", "tabular_sections", "dimensions", "resources", "posting", "enum_values", "predefined", "owners", "value_types", "properties", "enum_synonyms", "commands"] },
                    "description": "Узкая выборка секций структуры (как sections у get_object_profile): вернуть ТОЛЬКО указанные ключи. Без параметра — все секции. Рычаг экономии контекста: ['posting'] (поведение проведения, ~0.2 КБ вместо полного объекта), ['attributes'] (только реквизиты шапки без табличных частей), ['tabular_sections'], ['dimensions','resources'] (для регистров)."
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
            // Узкая выборка секций (sections): без параметра — все секции.
            let sections: Option<Vec<String>> = args
                .get("sections")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect());
            // Массовый режим (full_names) приоритетен; иначе одиночный full_name.
            let result_value = if let Some(arr) = args.get("full_names").and_then(|v| v.as_array())
            {
                // Конкуррентно: каждый элемент берёт своё соединение из пула и
                // исполняется в spawn_blocking (mass_map). Нестроковые элементы
                // получают {error} на своей позиции без обращения к пулу.
                let mut results: Vec<Value> = arr
                    .iter()
                    .map(|v| match v.as_str() {
                        Some(_) => Value::Null, // заполнится результатом ниже
                        None => {
                            json!({ "error": "full_names: каждый элемент должен быть строкой" })
                        }
                    })
                    .collect();
                let positions: Vec<usize> = arr
                    .iter()
                    .enumerate()
                    .filter_map(|(i, v)| v.as_str().map(|_| i))
                    .collect();
                let items: Vec<String> = arr
                    .iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect();
                let repo_label = ctx.repo.to_string();
                let sections_c = sections.clone();
                let rows =
                    code_index_core::mcp::tools::mass_map(ctx.storage, items, move |st, fqn| {
                        resolve_one(st.conn(), &repo_label, &fqn, sections_c.as_deref())
                    })
                    .await;
                for (pos, row) in positions.into_iter().zip(rows) {
                    results[pos] = match row {
                        Ok(v) => v,
                        Err(e) => json!({ "error": e }),
                    };
                }
                json!({ "results": results })
            } else if let Some(fqn) = crate::tools::object_value(&args) {
                let storage = match ctx.storage.get().await {
                    Ok(s) => s,
                    Err(e) => {
                        return crate::tools::wrap_error(serde_json::json!({
                            "error": format!("storage pool: {}", e)
                        }));
                    }
                };
                resolve_one(storage.conn(), ctx.repo, fqn, sections.as_deref())
            } else {
                json!({
                    "error": "missing parameter: передайте 'full_name' — полное имя вида '<MetaType>.<Name>' (строка)"
                })
            };
            // Структурный инструмент (cap::STRUCTURAL_TOOLS): вместо слепого
            // cap_response — посекционный omit (тяжёлую секцию ЦЕЛИКОМ, не обрезая
            // частично), затем wrap БЕЗ cap. Так enum_synonyms (сотни ключей)
            // выкидывается с count, а enum_values/имена остаются полными.
            if code_index_core::mcp::cap::is_structural_tool("get_object_structure") {
                let (result_value, omitted) = code_index_core::mcp::cap::omit_oversize_sections(
                    result_value,
                    code_index_core::mcp::cap::response_cap(),
                );
                crate::tools::wrap_with_meta_structural(result_value, Vec::new(), omitted)
            } else {
                crate::tools::wrap_with_meta("get_object_structure", result_value, Vec::new())
            }
        })
    }
}

/// Обработка ОДНОГО объекта по full_name → Value (структура либо
/// {error, did_you_mean}). Свободная fn, а не замыкание: одиночный путь зовёт
/// её inline, массовый — из spawn_blocking со своим соединением из пула
/// (mass_map). `repo_label` — алиас репо, только для текста ошибки.
/// Сузить структуру до запрошенных секций (узкая выборка `sections`). None или
/// пустой список → без изменений. Фильтрует ключи верхнего уровня
/// `attributes_json` (attributes/dimensions/resources/tabular_sections/posting/
/// enum_values/predefined) — рычаг гигиены контекста.
fn apply_sections(value: Value, sections: Option<&[String]>) -> Value {
    match (sections, value) {
        (Some(secs), Value::Object(mut map)) if !secs.is_empty() => {
            map.retain(|k, _| secs.iter().any(|s| s == k));
            Value::Object(map)
        }
        (_, v) => v,
    }
}

fn resolve_one(
    conn: &rusqlite::Connection,
    repo_label: &str,
    full_name: &str,
    sections: Option<&[String]>,
) -> Value {
    // Нормализация типа метаданных: 'Документ.X' → 'Document.X' (RU/EN, регистр неважен).
    // В metadata_objects.full_name хранится canonical (англ.) тип; без этого
    // 'Документ.РеализацияТоваровУслуг' не находился, хотя объект есть. См. META_FORMS.
    let normalized = match full_name.split_once('.') {
        Some((t, n)) => match crate::code_usages::canonical_meta_type(t) {
            Some(canon) if canon != t => std::borrow::Cow::Owned(format!("{canon}.{n}")),
            _ => std::borrow::Cow::Borrowed(full_name),
        },
        None => std::borrow::Cow::Borrowed(full_name),
    };
    let full_name = normalized.as_ref();
    let row = conn.query_row(
        "SELECT meta_type, name, synonym, attributes_json \
                     FROM metadata_objects WHERE repo = ? AND full_name = ?",
        params!["default", full_name],
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
            let attrs_value = apply_sections(attrs_value, sections);
            // Готовые счётчики секций (детерминированно): число элементов каждой
            // секции-массива (tabular_sections, attributes, dimensions, resources,
            // enum_values, …). Модель цитирует counts.tabular_sections, а не
            // пересчитывает массив — LLM занижает длину (10 ТЧ → 5).
            let counts: serde_json::Map<String, Value> = match &attrs_value {
                Value::Object(m) => m
                    .iter()
                    .filter_map(|(k, v)| v.as_array().map(|a| (k.clone(), json!(a.len()))))
                    .collect(),
                _ => serde_json::Map::new(),
            };
            json!({
                "full_name": full_name,
                "meta_type": meta_type,
                "name": name,
                "synonym": synonym,
                "attributes": attrs_value,
                "counts": counts,
            })
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            // fuzzy-подсказка: объект не найден — предложим похожие по
            // префиксу имени. Ловит опечатки в середине слова, напр.
            // 'Document.РеализацияТоваровИУслуг' → 'РеализацияТоваровУслуг'
            // (префикс 'Реализ' совпадает). Слабое место #5 прогона УТ-11.
            let (mtype, short) = match full_name.split_once('.') {
                Some((t, n)) => (Some(t.to_string()), n.to_string()),
                None => (None, full_name.to_string()),
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
                    if let Ok(rows) = s.query_map(params![like_sub], |r| r.get::<_, String>(0)) {
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
                "error": format!("object '{}' not found in repo '{}'", full_name, repo_label),
                "did_you_mean": suggestions,
                "hint": "Формат '<MetaType>.<Name>': MetaType англ. (Catalog/Document/AccumulationRegister/InformationRegister/ChartOfAccounts/…), Name — точное имя из конфигурации. Список объектов типа — через MCP 1c list_metadata_objects."
            })
        }
        Err(e) => json!({
            "error": format!("database error: {}", e)
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_sections_filters_top_level_keys() {
        let v = json!({
            "attributes": [1, 2],
            "tabular_sections": [3],
            "posting": { "Posting": "Allow" },
            "dimensions": []
        });
        // None → без изменений.
        assert_eq!(apply_sections(v.clone(), None), v);
        // Пустой список → без изменений.
        let empty: Vec<String> = vec![];
        assert_eq!(apply_sections(v.clone(), Some(&empty)), v);
        // Только запрошенные ключи (['posting']) остаются.
        let only = vec!["posting".to_string()];
        let filtered = apply_sections(v.clone(), Some(&only));
        let obj = filtered.as_object().unwrap();
        assert_eq!(obj.len(), 1);
        assert!(obj.contains_key("posting"));
        assert!(!obj.contains_key("attributes"));
        // Не-объект (Null) → без изменений (ненайденный объект отдаёт error-Value).
        assert_eq!(apply_sections(Value::Null, Some(&only)), Value::Null);
    }
}
