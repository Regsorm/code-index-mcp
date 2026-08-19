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
use code_index_core::mcp::cap;
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
         МАССОВЫЙ РЕЖИМ ('full_names'): батчи список ТОЛЬКО когда точно нужен ВЕСЬ набор и структура одного объекта не отменит надобность в остальных (например, разбираешь уже подтверждённый список). Если ОТБИРАЕШЬ, какие из объектов релевантны, или результат одного может сделать остальные ненужными — НЕ батчи, запрашивай по одному с остановкой по ходу. Сомневаешься — по одному. Ответ на батч — {results:[...]} в том же порядке. КРИТЕРИЙ-СЕЛЕКТОР ('name_like' + опц. 'meta_type'): когда нужны структуры ВСЕХ объектов одной темы — не зови по одному и не перечисляй имена, передай подстроку имени: name_like='ЭДО' вернёт структуры всех объектов, чьё имя содержит 'ЭДО', ОДНИМ вызовом. Сочетай с sections= (узкие секции на каждый объект). Лимит 50 объектов (truncated=true, если совпало больше — уточни критерий). Ответ — {matched:N, truncated, results:[...]}. РАЗМЕР ОТВЕТА: опознание найденного (full_name/meta_type/name/synonym) сохраняется всегда — при нехватке бюджета опускаются тяжёлые секции ВНУТРИ элементов (`<секция>_omitted` + `<секция>_count`), список объектов не пропадает. Дёшево искать — 'names_only=true' (только паспорта); нужны полные секции — повтори вызов с 'max_response_bytes' побольше."
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
                "name_like": {
                    "type": "string",
                    "description": "КРИТЕРИЙ-СЕЛЕКТОР: подстрока имени объекта (без префикса типа). Вернёт структуры ВСЕХ объектов, чьё имя содержит подстроку, ОДНИМ вызовом — вместо серии вызовов по одному. Применяй, когда нужны все объекты одной темы (name_like='ЭДО' → все объекты ЭДО). Сочетай с sections= (узкие секции) и при необходимости meta_type=. Лимит 50 объектов (truncated=true, если совпало больше — уточни подстроку). Регистр учитывается."
                },
                "meta_type": {
                    "type": "string",
                    "description": "Необязательный фильтр типа для name_like: 'Catalog'/'Document'/'InformationRegister'/'Enum'/… (RU тоже: 'Справочник'/'Документ'). Сужает критерий до одного вида метаданных. Без name_like не действует."
                },
                "sections": {
                    "type": "array",
                    "items": { "type": "string", "enum": ["attributes", "tabular_sections", "dimensions", "resources", "posting", "enum_values", "predefined", "owners", "value_types", "properties", "enum_synonyms", "commands"] },
                    "description": "Узкая выборка секций структуры (как sections у get_object_profile): вернуть ТОЛЬКО указанные ключи. Без параметра — все секции. Рычаг экономии контекста: ['posting'] (поведение проведения, ~0.2 КБ вместо полного объекта), ['attributes'] (только реквизиты шапки без табличных частей), ['tabular_sections'], ['dimensions','resources'] (для регистров)."
                },
                "offset": {
                    "type": "integer",
                    "description": "Смещение страницы внутри ОДНОЙ запрошенной секции (работает при sections=['enum_values'] и т.п.). Крупные секции — значения перечислений на сотни позиций — отдаются порциями по размеру ответа: рядом приходят <секция>_shown/_total/_offset/_has_more и готовый вызов следующей страницы. Без параметра — с начала."
                },
                "names_only": {
                    "type": "boolean",
                    "description": "Вернуть ТОЛЬКО паспорт каждого объекта (full_name, meta_type, name, synonym) без структуры. Самый дешёвый режим поиска: с name_like на 38 объектах ~9 КБ вместо ~63 КБ. Бери, когда нужно СНАЧАЛА понять, какие объекты подходят, а структуру запросить потом по конкретному full_name. Учти: sections=[] означает «все секции» (обратная совместимость), а не «только имена» — для имён нужен именно этот параметр."
                },
                "max_response_bytes": {
                    "type": "integer",
                    "description": "Бюджет размера ЭТОГО ответа в байтах — перекрывает серверный [cap].max_response_bytes на один вызов. Применяй, когда ответ пришёл ужатым и нужны полные секции: повтори тот же вызов с большим значением. Запрос сверх серверного потолка не отклоняется, а зажимается до потолка; фактически применённое значение возвращается полем response_budget_applied. Ноль (снять ограничение) на вызов не разрешён."
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
            // Явный режим «только имена». Отдельный вход, потому что sections=[]
            // означает «все секции» (обратная совместимость, тест
            // apply_sections_filters_top_level_keys) — попросить одни имена
            // через sections клиент не может.
            let names_only = args
                .get("names_only")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            // Смещение страницы — работает, когда запрошена ровно одна секция
            // (sections=['enum_values'] и т.п.). Продолжение выдачи, а не выбор.
            let offset = args
                .get("offset")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            // Бюджет размера этого ответа: клиентский max_response_bytes
            // поверх серверного, но не выше серверного потолка.
            let budget = code_index_core::mcp::cap::resolve_request_budget(
                args.get("max_response_bytes")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as usize),
            );
            // Критерий-селектор (name_like) приоритетен: сервер сам разворачивает
            // плоский предикат в список объектов и отдаёт их структуры за 1 ход
            // (общая конвенция объектно-ключевых инструментов). Массовый режим
            // (full_names) — следующий по приоритету; иначе одиночный full_name.
            let result_value = if let Some(name_like) = args
                .get("name_like")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
            {
                // Лимит объектов: защита от слишком широкого критерия (иначе
                // LIKE '%а%' вытащит пол-конфигурации). Больше лимита → truncated.
                const NAME_LIKE_CAP: usize = 50;
                let meta_type = args
                    .get("meta_type")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty());
                // Развернуть критерий в список full_name (одно соединение, до mass_map).
                let expanded = {
                    let storage = match ctx.storage.get().await {
                        Ok(s) => s,
                        Err(e) => {
                            return crate::tools::wrap_error(json!({
                                "error": format!("storage pool: {}", e)
                            }));
                        }
                    };
                    crate::tools::expand_object_criterion(
                        storage.conn(),
                        name_like,
                        meta_type,
                        NAME_LIKE_CAP,
                    )
                };
                let (full_names, truncated) = match expanded {
                    Ok(t) => t,
                    Err(e) => {
                        return crate::tools::wrap_error(json!({
                            "error": format!("name_like: {}", e)
                        }));
                    }
                };
                if full_names.is_empty() {
                    json!({
                        "matched": 0,
                        "results": [],
                        "hint": format!(
                            "Критерий name_like='{}'{} не нашёл объектов. Проверь подстроку/тип \
                             (регистр учитывается) или используй search_terms для поиска по теме.",
                            name_like,
                            meta_type
                                .map(|m| format!(", meta_type='{}'", m))
                                .unwrap_or_default()
                        )
                    })
                } else {
                    let matched = full_names.len();
                    let repo_label = ctx.repo.to_string();
                    let sections_c = sections.clone();
                    let rows = code_index_core::mcp::tools::mass_map(
                        ctx.storage,
                        full_names,
                        move |st, fqn| {
                            // Страницы (offset/budget) — только для одиночного объекта: в массовом
// режиме размером управляет shrink_search_results, и постраничная выдача
// внутри каждого элемента сделала бы ответ невнятным.
resolve_one(st.conn(), &repo_label, &fqn, sections_c.as_deref(), names_only, 0, 0)
                        },
                    )
                    .await;
                    let results: Vec<Value> = rows
                        .into_iter()
                        .map(|r| match r {
                            Ok(v) => v,
                            Err(e) => json!({ "error": e }),
                        })
                        .collect();
                    json!({ "matched": matched, "truncated": truncated, "results": results })
                }
            } else if let Some(arr) = args.get("full_names").and_then(|v| v.as_array())
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
                        // Страницы (offset/budget) — только для одиночного объекта: в массовом
// режиме размером управляет shrink_search_results, и постраничная выдача
// внутри каждого элемента сделала бы ответ невнятным.
resolve_one(st.conn(), &repo_label, &fqn, sections_c.as_deref(), names_only, 0, 0)
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
                resolve_one(
                    storage.conn(),
                    ctx.repo,
                    fqn,
                    sections.as_deref(),
                    names_only,
                    offset,
                    budget.applied,
                )
            } else {
                json!({
                    "error": "missing parameter: передайте 'full_name' — полное имя вида '<MetaType>.<Name>' (строка)"
                })
            };
            // Структурный инструмент (cap::STRUCTURAL_TOOLS): вместо слепого
            // cap_response — посекционный omit (тяжёлую секцию ЦЕЛИКОМ, не обрезая
            // частично), затем wrap БЕЗ cap. Так enum_synonyms (сотни ключей)
            // выкидывается с count, а enum_values/имена остаются полными.
            if !code_index_core::mcp::cap::is_structural_tool("get_object_structure") {
                return crate::tools::wrap_with_meta(
                    "get_object_structure",
                    result_value,
                    Vec::new(),
                );
            }
            // Ответ-ПОИСК (name_like/full_names) ужимается иначе, чем структура
            // одного объекта: там самая тяжёлая секция — сам массив results, и
            // общий omit выбрасывал его первым же шагом, оставляя клиенту три
            // числа вместо списка найденного. Опознание объектов неприкосновенно.
            if result_value.get("results").is_some() {
                let (result_value, shrink) = code_index_core::mcp::cap::shrink_search_results(
                    result_value,
                    budget.applied,
                );
                // Маркеры ставим свои: общий OMIT_HINT здесь не годится (советует
                // grep_code/grep_body и не называет ни размера, ни ручки).
                let mut out =
                    crate::tools::wrap_with_meta_structural(result_value, Vec::new(), false);
                if let Some(obj) = out.as_object_mut() {
                    if shrink.sections_omitted {
                        obj.insert("response_sections_omitted".to_string(), json!(true));
                    }
                    if shrink.results_shortened {
                        obj.insert("response_results_shortened".to_string(), json!(true));
                    }
                    if shrink.any() {
                        obj.insert(
                            "response_shrink_hint".to_string(),
                            json!(shrink_hint(&shrink, &budget)),
                        );
                    }
                    if shrink.any() || budget.requested.is_some() {
                        obj.insert("response_budget_applied".to_string(), json!(budget.applied));
                    }
                }
                return out;
            }
            // Структура ОДНОГО объекта — прежнее поведение: тяжёлая секция
            // (сотни значений перечисления) опускается целиком с `_count`.
            let (result_value, omitted) =
                code_index_core::mcp::cap::omit_oversize_sections(result_value, budget.applied);
            let mut out = crate::tools::wrap_with_meta_structural(result_value, Vec::new(), omitted);
            if budget.requested.is_some() {
                if let Some(obj) = out.as_object_mut() {
                    obj.insert("response_budget_applied".to_string(), json!(budget.applied));
                }
            }
            out
        })
    }
}

/// Подсказка при ужатом ответе-поиске. Обязательны ОБА числа (сколько весил бы
/// полный ответ и каков был бюджет) и КОНКРЕТНАЯ ручка: из ответа «38 объектов,
/// секции опущены» слабая модель не понимает, что делать дальше, — на этом и
/// сорвался разобранный прогон. Совет про grep_code сюда не годится: этих
/// инструментов у вызывающего агента может не быть в разрешённых.
fn shrink_hint(shrink: &cap::SearchShrink, budget: &cap::RequestBudget) -> String {
    let kb = |b: usize| (b + 512) / 1024;
    let hard = cap::response_cap_hard();
    // Запас сверх фактического размера, округлённый до килобайта.
    let suggested = (shrink.full_bytes / 1000 + 2) * 1000;
    let mut s = format!(
        "Ответ ужат под бюджет: полный ответ ≈{} КБ ({} байт) при бюджете {} КБ ({} байт). \
         Опознание объектов (full_name/meta_type/name/synonym) сохранено полностью — \
         работай по нему. ",
        kb(shrink.full_bytes),
        shrink.full_bytes,
        kb(budget.applied),
        budget.applied
    );
    if shrink.sections_omitted {
        s.push_str(
            "Тяжёлые секции внутри элементов опущены — рядом `<секция>_omitted` + \
             `<секция>_count`. ",
        );
    }
    if shrink.results_shortened {
        s.push_str(&format!(
            "Отдано {} элементов из {} (`results_shown` / `results_total`). ",
            shrink.results_shown, shrink.results_total
        ));
    }
    if budget.clamped {
        s.push_str(&format!(
            "Запрошенный `max_response_bytes`={} зажат до потолка сервера {}. ",
            budget.requested.unwrap_or_default(),
            hard
        ));
    }
    if suggested <= hard {
        s.push_str(&format!(
            "Нужны полные секции — повтори ТОТ ЖЕ вызов с `max_response_bytes={}` \
             (потолок сервера {}). ",
            suggested, hard
        ));
    } else {
        s.push_str(&format!(
            "Полный ответ не влезает даже в потолок сервера ({} байт) — сузь набор. ",
            hard
        ));
    }
    s.push_str(
        "Нужны только имена — повтори с `names_only=true` (самый дешёвый режим). \
         Нужен ОДИН объект из списка — вызови с его `full_name`. \
         Сузить набор — `name_like` поточнее и/или `meta_type`.",
    );
    s
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
    names_only: bool,
    offset: usize,
    budget: usize,
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
            // Режим «только паспорт»: структуру даже не разбираем.
            if names_only {
                return json!({
                    "full_name": full_name,
                    "meta_type": meta_type,
                    "name": name,
                    "synonym": synonym,
                });
            }
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
            // Страницы по ОДНОЙ запрошенной секции. До этого крупные секции
            // (значения перечисления — до 87 КБ) не помещались в бюджет, и
            // посекционный страж выбрасывал их целиком: получить значения было
            // нельзя в принципе. Страница меряется байтами, а не числом строк.
            let mut page_meta: Option<(String, code_index_core::mcp::cap::Page)> = None;
            let attrs_value = match (sections, attrs_value) {
                (Some(secs), Value::Object(mut map)) if secs.len() == 1 => {
                    if let Some(Value::Array(items)) = map.remove(secs[0].as_str()) {
                        let mut page = code_index_core::mcp::cap::page_by_bytes(
                            items,
                            offset,
                            budget,
                        );
                        let items = std::mem::take(&mut page.items);
                        map.insert(secs[0].clone(), Value::Array(items));
                        page_meta = Some((secs[0].clone(), page));
                    }
                    Value::Object(map)
                }
                (_, v) => v,
            };
            let mut out = json!({
                "full_name": full_name,
                "meta_type": meta_type,
                "name": name,
                "synonym": synonym,
                "attributes": attrs_value,
                "counts": counts,
            });
            if let (Some((key, page)), Some(obj)) = (page_meta.as_ref(), out.as_object_mut()) {
                page.annotate(obj, key);
                if let Some(next) = page.next_offset() {
                    let call = code_index_core::mcp::cap::next_call_hint(
                        "get_object_structure",
                        &[
                            ("repo", json!(repo_label)),
                            ("full_name", json!(full_name)),
                            ("sections", json!([key])),
                            ("offset", json!(next)),
                        ],
                    );
                    obj.insert(
                        "hint".to_string(),
                        json!(format!(
                            "Показано {} из {}. Следующая страница — 1 вызов:\n{}",
                            page.shown, page.total, call
                        )),
                    );
                }
            }
            out
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

    /// `names_only=true` → только паспорт объекта, структуры нет.
    /// Отдельный вход нужен именно потому, что `sections=[]` означает
    /// «все секции» (см. тест выше) и попросить одни имена через него нельзя.
    #[test]
    fn names_only_returns_identity_without_structure() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        for ddl in crate::schema::SCHEMA_EXTENSIONS {
            conn.execute_batch(ddl).unwrap();
        }
        conn.execute(
            "INSERT INTO metadata_objects (repo, full_name, meta_type, name, synonym, attributes_json) \
             VALUES ('default', 'Catalog.СоглашенияСКлиентами', 'Catalog', 'СоглашенияСКлиентами', \
             'Соглашения с клиентами', ?)",
            params![r#"{"attributes":[{"name":"Партнер"},{"name":"Организация"}]}"#],
        )
        .unwrap();

        let v = resolve_one(&conn, "ut", "Catalog.СоглашенияСКлиентами", None, true, 0, 0);
        assert_eq!(v["full_name"], json!("Catalog.СоглашенияСКлиентами"));
        assert_eq!(v["meta_type"], json!("Catalog"));
        assert_eq!(v["name"], json!("СоглашенияСКлиентами"));
        assert_eq!(v["synonym"], json!("Соглашения с клиентами"));
        assert!(v.get("attributes").is_none(), "структура не должна отдаваться");
        assert!(v.get("counts").is_none());

        // names_only=false → структура на месте (прежнее поведение).
        let full = resolve_one(&conn, "ut", "Catalog.СоглашенияСКлиентами", None, false, 0, 0);
        assert_eq!(full["counts"]["attributes"], json!(2));
    }

    #[test]
    fn single_section_is_paged_by_bytes_with_next_call() {
        use rusqlite::Connection;
        let conn = Connection::open_in_memory().unwrap();
        for ddl in crate::schema::SCHEMA_EXTENSIONS {
            conn.execute_batch(ddl).unwrap();
        }
        // Перечисление с сотней значений — как крупные типовые перечисления,
        // где раньше секция выбрасывалась целиком и была недоступна.
        let values: Vec<Value> = (0..100)
            .map(|i| json!({ "name": format!("Значение{}", i), "synonym": format!("Подпись {}", i) }))
            .collect();
        let attrs = json!({ "enum_values": values, "attributes": [] }).to_string();
        conn.execute(
            "INSERT INTO metadata_objects (repo, full_name, meta_type, name, synonym, attributes_json) \
             VALUES ('default','Enum.ХозяйственныеОперации','Enum','ХозяйственныеОперации',NULL,?1)",
            params![attrs],
        )
        .unwrap();

        let secs = vec!["enum_values".to_string()];
        let first = resolve_one(&conn, "ut", "Enum.ХозяйственныеОперации", Some(&secs), false, 0, 3_000);
        let shown = first["enum_values_shown"].as_u64().unwrap() as usize;
        assert!(shown > 0 && shown < 100, "страница набирается по бюджету: {shown}");
        assert_eq!(first["enum_values_total"], json!(100));
        assert_eq!(first["enum_values_has_more"], json!(true));
        assert_eq!(first["attributes"]["enum_values"].as_array().unwrap().len(), shown);
        let hint = first["hint"].as_str().unwrap();
        assert!(
            hint.contains(&format!("offset={}", shown)),
            "подсказка несёт смещение следующей страницы: {hint}"
        );

        // Продолжение со смещения доходит до конца набора.
        let last = resolve_one(&conn, "ut", "Enum.ХозяйственныеОперации", Some(&secs), false, 90, 3_000);
        assert_eq!(last["enum_values_offset"], json!(90));
        assert_eq!(last["enum_values_shown"], json!(10));
        assert_eq!(last["enum_values_has_more"], json!(false));
        assert!(last.get("hint").is_none(), "конец набора — продолжать некуда");
    }
}
