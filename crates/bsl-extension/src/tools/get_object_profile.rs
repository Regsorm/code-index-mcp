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
use std::time::Duration;

use code_index_core::extension::{IndexTool, ToolContext};
use rusqlite::params;
use serde_json::{json, Value};

/// Таймаут набора запросов (как в bsl_sql): sqlite3_interrupt против runaway
/// COUNT/SELECT на больших data_links (центральные регистры/объекты).
const QUERY_TIMEOUT_SECS: u64 = 8;

pub struct GetObjectProfileTool;

impl IndexTool for GetObjectProfileTool {
    fn name(&self) -> &str {
        "get_object_profile"
    }

    fn description(&self) -> &str {
        "Обзорный паспорт объекта конфигурации 1С за ОДИН вызов по полному имени \
         ('Document.РеализацияТоваровУслуг', 'Catalog.Контрагенты'): structure \
         (реквизиты/табличные части/измерения/ресурсы/значения перечислений), forms \
         (перечень форм + число обработчиков у каждой), modules (счётчики модулей по \
         типам; полный список с object_id/property_id — UUID для dbgs-breakpoints — по \
         expand), data_links (счётчики по видам связи, регистры движений для документов / \
         регистраторы для регистров, число входящих ссылок). \
         Отдаёт ОБЗОР: перечни и счётчики, не содержимое. Обработчики конкретной формы \
         — get_form_handlers(owner_full_name, form_name); связи вглубь — get_data_links. \
         expand=['forms.handlers'] вернёт обработчики всех форм, expand=['modules.list'] — \
         полный список модулей: на крупном объекте это сотни килобайт, берите адресно. \
         Обе секции отдаются и без expand, если весь ответ укладывается в бюджет \
         (признаки — forms_handlers_included, modules_listed). \
         Имя — singular meta_type ('<MetaType>.<Name>'). \
         Параметр sections=['structure'|'forms'|'modules'|'data_links'] сужает ответ \
         (по умолчанию все секции) — удешевляет вызов, когда нужна только часть. For \
         BSL/1C repositories only."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo": { "type": "string", "description": "Алиас репозитория" },
                "full_name": {
                    "type": "string",
                    "description": "Полное имя объекта вида '<MetaType>.<Name>' (singular), например 'Document.РеализацияТоваровУслуг'"
                },
                "sections": {
                    "type": "array",
                    "items": { "type": "string", "enum": ["structure", "forms", "modules", "data_links"] },
                    "description": "Какие секции вернуть. По умолчанию (опущено) — все. Рычаг удешевления: ['structure'] вернёт только реквизиты/ТЧ/измерения/ресурсы без форм, модулей и связей данных."
                },
                "expand": {
                    "type": "array",
                    "items": { "type": "string", "enum": ["forms.handlers", "modules.list"] },
                    "description": "Что вернуть подробно, а не перечнем. 'forms.handlers' — обработчики всех форм объекта; 'modules.list' — полный список модулей с UUID (object_id/property_id для точек останова). На крупном объекте каждая из этих секций — сотни килобайт, поэтому берите их по одной и вместе с sections=['forms'|'modules']. Не путать с sections: sections отвечает, КАКИЕ разделы вернуть, expand — насколько подробно."
                },
                "max_response_bytes": {
                    "type": "integer",
                    "description": "Бюджет размера ЭТОГО ответа в байтах — перекрывает серверный [cap].max_response_bytes на один вызов. Применяй, когда обработчики форм пришли перечнем, а нужны полностью: повтори тот же вызов с большим значением. Запрос сверх серверного потолка зажимается до потолка; фактически применённое возвращается полем response_budget_applied."
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
            let full_name = match crate::tools::object_value(&args) {
                Some(s) => crate::code_usages::normalize_object_ref(s).into_owned(),
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
            // Выбор секций (опц.) — рычаг удешевления ответа: ['structure'] и т.п.
            let sections: Vec<String> = args
                .get("sections")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            // Что вернуть подробно, а не перечнем (ось развёрнутости, не выбора
            // секций): сейчас единственное значение — обработчики всех форм.
            let expand_of = |what: &str| {
                args.get("expand")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().any(|x| x.as_str() == Some(what)))
                    .unwrap_or(false)
            };
            let expand_handlers = expand_of("forms.handlers");
            let expand_modules = expand_of("modules.list");
            // Бюджет размера этого ответа: клиентский max_response_bytes поверх
            // серверного, с зажимом по потолку.
            let budget = code_index_core::mcp::cap::resolve_request_budget(
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

            // interrupt-таймаут против runaway-запросов на больших data_links
            // (центральные регистры/объекты). Паттерн как в bsl_sql: handle живёт
            // в отдельной задаче, по истечении дёргает sqlite3_interrupt; гасим
            // после сборки.
            let handle = conn.get_interrupt_handle();
            let timer = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(QUERY_TIMEOUT_SECS)).await;
                handle.interrupt();
            });

            let result = assemble_profile(
                conn,
                &full_name,
                &meta_type,
                &name,
                &sections,
                expand_handlers,
                expand_modules,
                budget.applied,
            );
            timer.abort();

            let (value, folded) = match result {
                Ok(v) => v,
                Err(e) => {
                    return crate::tools::wrap_error(json!({
                        "error": format!("database error: {}", e)
                    }));
                }
            };
            // Паспорт — СТРУКТУРНЫЙ ответ: слепой обрез массивов (cap_response)
            // исказил бы структуру объекта — «1 реквизит из 200». Поэтому
            // посекционный omit и обёртка без cap, как у get_object_structure.
            // Бюджет тот же, под который собирался ответ: иначе страж выбросит
            // секцию, которую по `expand` только что намеренно отдали.
            let (value, omitted) = code_index_core::mcp::cap::omit_oversize_sections(
                value,
                effective_budget(budget.applied, expand_handlers, expand_modules),
            );
            let mut out = crate::tools::wrap_with_meta_structural(value, Vec::new(), omitted);
            if let Some(obj) = out.as_object_mut() {
                if let Some(info) = folded.as_ref() {
                    obj.insert(
                        "sections_folded_hint".to_string(),
                        json!(info.hint(ctx.repo, &full_name)),
                    );
                }
                if budget.requested.is_some() || folded.is_some() {
                    obj.insert("response_budget_applied".to_string(), json!(budget.applied));
                }
            }
            out
        })
    }
}

/// Бюджет, под который реально собирается ответ. `expand` поднимает его до
/// потолка сервера — это и есть смысл параметра: «дай подробно, сколько вообще
/// разрешено отдать». Выключенный страж (0) остаётся выключенным.
fn effective_budget(budget: usize, expand_handlers: bool, expand_modules: bool) -> usize {
    if budget == 0 || !(expand_handlers || expand_modules) {
        budget
    } else {
        budget.max(code_index_core::mcp::cap::response_cap_hard())
    }
}

/// Какие секции ушли перечнем вместо содержимого.
struct Folded {
    forms: Option<FoldedForms>,
    modules: Option<FoldedModules>,
}

impl Folded {
    /// Общая подсказка: по строке-вызову на каждую свёрнутую секцию.
    fn hint(&self, repo: &str, full_name: &str) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(f) = self.forms.as_ref() {
            parts.push(f.hint(repo, full_name));
        }
        if let Some(m) = self.modules.as_ref() {
            parts.push(m.hint(repo, full_name));
        }
        parts.join("\n")
    }
}

/// Что нужно знать подсказке, когда модули отданы счётчиками по типам.
struct FoldedModules {
    /// Сколько модулей у объекта вместе с модулями его форм и команд.
    total: usize,
    /// Сколько весил бы полный список.
    full_bytes: usize,
}

impl FoldedModules {
    fn hint(&self, repo: &str, full_name: &str) -> String {
        use code_index_core::mcp::cap::next_call_hint;
        let kb = (self.full_bytes + 512) / 1024;
        // Список всегда достаём одним вызовом: узкая секция + запас бюджета.
        let call = next_call_hint(
            "get_object_profile",
            &[
                ("repo", json!(repo)),
                ("full_name", json!(full_name)),
                ("sections", json!(["modules"])),
                ("expand", json!(["modules.list"])),
            ],
        );
        format!(
            "Модули отданы счётчиками по типам: всего {}, список не включён (целиком ≈{} КБ).\n\
             Полный список с UUID (для точек останова) — 1 вызов:\n{}",
            self.total, kb, call
        )
    }
}

/// Что нужно знать подсказке, когда обработчики форм отданы перечнем.
struct FoldedForms {
    /// Сколько форм у объекта.
    forms: usize,
    /// Сколько обработчиков во всех формах суммарно.
    handlers: usize,
    /// Сколько весила бы секция форм целиком.
    full_bytes: usize,
    /// Форма с наибольшим числом обработчиков — вероятная цель следующего вызова.
    top_form: String,
}

impl FoldedForms {
    /// Подсказка «что делать дальше»: только готовые вызовы и число ходов.
    /// Причин, почему свернули, здесь нет намеренно — слабой модели нужно
    /// следующее действие, а не объяснение.
    fn hint(&self, repo: &str, full_name: &str) -> String {
        use code_index_core::mcp::cap::{next_call_hint, response_cap_hard};
        let kb = (self.full_bytes + 512) / 1024;
        let one = next_call_hint(
            "get_form_handlers",
            &[
                ("repo", json!(repo)),
                ("owner_full_name", json!(full_name)),
                ("form_name", json!(self.top_form)),
            ],
        );
        let mut s = format!(
            "Формы отданы перечнем: {} форм, обработчиков всего {}, содержимое не включено \
             (целиком ≈{} КБ).\nОбработчики одной формы — 1 вызов:\n{}\n",
            self.forms, self.handlers, kb, one
        );
        // Запас сверх фактического размера, округлённый до килобайта.
        let suggested = (self.full_bytes / 1000 + 2) * 1000;
        if suggested <= response_cap_hard() {
            let all = next_call_hint(
                "get_object_profile",
                &[
                    ("repo", json!(repo)),
                    ("full_name", json!(full_name)),
                    ("expand", json!(["forms.handlers"])),
                    ("max_response_bytes", json!(suggested)),
                ],
            );
            s.push_str(&format!(
                "Обработчики всех {} форм — 1 вызов:\n{}",
                self.forms, all
            ));
        } else {
            s.push_str(&format!(
                "Все формы разом не влезут даже в потолок сервера ({} байт) — берите по формам \
                 через get_form_handlers.",
                response_cap_hard()
            ));
        }
        s
    }
}

/// Repo-ключ внутри per-repo index.db. Все BSL-таблицы пишут 'default'
/// (каждый репо — отдельный файл БД). См. index_extras::REPO_DEFAULT.
const REPO: &str = "default";

/// Сборка паспорта объекта одним проходом — под общим interrupt-таймаутом из
/// execute (все запросы используют один conn, прерываются разом по таймауту).
fn assemble_profile(
    conn: &rusqlite::Connection,
    full_name: &str,
    meta_type: &str,
    name: &str,
    sections: &[String],
    expand_handlers: bool,
    expand_modules: bool,
    budget: usize,
) -> rusqlite::Result<(Value, Option<Folded>)> {
    let folder = crate::tools::meta_type_to_folder(meta_type);
    // Выбор секций: пустой список → все (обратная совместимость). Иначе — только
    // запрошенные (рычаг удешевления: ['structure'] вернёт лишь реквизиты/ТЧ).
    let all = sections.is_empty();
    let want = |s: &str| all || sections.iter().any(|x| x == s);
    let (want_structure, want_forms, want_modules, want_links) =
        (want("structure"), want("forms"), want("modules"), want("data_links"));

    // ── Заголовок + структура (metadata_objects, singular key) ────────────
    let header = conn.query_row(
        "SELECT meta_type, name, synonym, attributes_json \
         FROM metadata_objects WHERE repo = ?1 AND full_name = ?2",
        params![REPO, full_name],
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
        // Объект может не иметь записи в metadata_objects (тип вне OBJECT_FOLDERS —
        // например DataProcessor/Report), но формы/модули у него есть. Не выходим —
        // отдаём что найдём, found=false.
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            (false, meta_type.to_string(), name.to_string(), None, Value::Null)
        }
        Err(e) => return Err(e),
    };

    // ── Формы / модули (plural folder key) ── только если запрошены ────────
    let forms = if want_forms {
        match folder.as_deref() {
            Some(fld) => query_forms(conn, &format!("{}.{}", fld, name))?,
            None => Vec::new(),
        }
    } else {
        Vec::new()
    };
    let modules = if want_modules {
        match folder.as_deref() {
            Some(fld) => query_modules(conn, &format!("{}.{}.", fld, name))?,
            None => Vec::new(),
        }
    } else {
        Vec::new()
    };

    // ── Связи данных (data_links, singular key) ── только если запрошены ───
    let data_links = if want_links {
        query_data_links(conn, full_name)?
    } else {
        Value::Null
    };

    // Карта модулей: счётчики по типам. Под объект попадают модули его форм и
    // команд (у обработки с 255 формами — 267 модулей, ~100 КБ), поэтому секция
    // тяжелеет так же, как формы. Полный список с UUID — по expand.
    let modules_total = modules.len();
    let mut modules_by_type = serde_json::Map::new();
    for m in &modules {
        let t = m["module_type"].as_str().unwrap_or("?").to_string();
        let n = modules_by_type.get(&t).and_then(|v| v.as_u64()).unwrap_or(0) + 1;
        modules_by_type.insert(t, json!(n));
    }
    let modules_map = json!({ "by_type": Value::Object(modules_by_type.clone()) });

    // Карта форм: имя + число обработчиков. Само содержимое отдаём, только если
    // его попросили явно (expand) либо весь ответ укладывается в бюджет — по
    // замерам на восьми конфигурациях так у подавляющего большинства объектов,
    // и для них поведение не меняется, лишних ходов нет.
    let handlers_count = |f: &Value| f["handlers"].as_array().map_or(0, |a| a.len());
    let forms_count = forms.len();
    let handlers_total: usize = forms.iter().map(handlers_count).sum();
    let forms_map: Vec<Value> = forms
        .iter()
        .map(|f| json!({ "form_name": f["form_name"], "handlers_count": handlers_count(f) }))
        .collect();
    // Форма с наибольшим числом обработчиков — вероятная цель следующего вызова,
    // её и подставим в подсказку.
    let top_form = forms
        .iter()
        .max_by_key(|f| handlers_count(f))
        .and_then(|f| f["form_name"].as_str())
        .unwrap_or_default()
        .to_string();

    // Сборка: заголовок всегда; секции — только запрошенные (омитим ключ, а не
    // null, чтобы агент видел, что секция не запрашивалась, и мог дозапросить).
    let build = |forms_value: Vec<Value>,
                 handlers_included: bool,
                 modules_value: Value,
                 modules_listed: bool|
     -> Value {
        let mut obj = serde_json::Map::new();
        obj.insert("full_name".into(), json!(full_name));
        obj.insert("found".into(), json!(found));
        obj.insert("meta_type".into(), json!(&db_meta_type));
        obj.insert("name".into(), json!(&db_name));
        obj.insert("synonym".into(), json!(&synonym));
        if want_structure {
            obj.insert("structure".into(), json!(&structure));
        }
        if want_forms {
            obj.insert("forms".into(), json!(forms_value));
            obj.insert("forms_total".into(), json!(forms_count));
            obj.insert("forms_handlers_total".into(), json!(handlers_total));
            obj.insert("forms_handlers_included".into(), json!(handlers_included));
        }
        if want_modules {
            obj.insert("modules".into(), modules_value);
            obj.insert("modules_total".into(), json!(modules_total));
            obj.insert("modules_listed".into(), json!(modules_listed));
        }
        if want_links {
            obj.insert("data_links".into(), json!(&data_links));
        }
        if !all {
            obj.insert("sections_returned".into(), json!(sections));
            obj.insert(
                "sections_available".into(),
                json!(["structure", "forms", "modules", "data_links"]),
            );
        }
        Value::Object(obj)
    };

    // Деградация по шагам: сначала пробуем отдать всё, затем сворачиваем формы
    // (обычно самая тяжёлая секция), затем — модули. Каждый следующий шаг
    // делается, только если предыдущий не уложился в бюджет.
    //
    // `expand` поднимает бюджет до потолка сервера: попросили подробно — отдаём
    // столько, сколько вообще разрешено отдать. Отменить бюджет expand не может:
    // ответ сверх потолка клиент всё равно сбросит в файл, и модель получит
    // обрывок вместо данных. Не влезли и в потолок — перечень плюс подсказка
    // «берите по частям», а не молчаливая пустая секция.
    let modules_full = json!(&modules);
    let modules_bytes = serde_json::to_string(&modules_full).map(|s| s.len()).unwrap_or(0);
    let full = build(forms.clone(), true, modules_full.clone(), true);
    let forms_bytes = serde_json::to_string(&full["forms"]).map(|s| s.len()).unwrap_or(0);

    // Первой сворачивается секция, которую НЕ просили подробно: иначе `expand`
    // одной секции поднимает бюджет и заодно разворачивает соседнюю — на
    // объекте с 255 формами это лишние 94 КБ модулей, за которыми никто не шёл.
    let middle = if expand_handlers && !expand_modules {
        build(forms, true, modules_map.clone(), false)
    } else {
        build(forms_map.clone(), false, modules_full, true)
    };
    let effective = effective_budget(budget, expand_handlers, expand_modules);
    let (value, middle_used) =
        code_index_core::mcp::cap::fold_to_budget(full, middle, effective);
    let (value, least_used) = code_index_core::mcp::cap::fold_to_budget(
        value,
        build(forms_map, false, modules_map, false),
        effective,
    );
    // Какая секция свернулась на каждой ступени — зависит от того же порядка.
    let (forms_folded, modules_folded) = if expand_handlers && !expand_modules {
        (least_used, middle_used || least_used)
    } else {
        (middle_used || least_used, least_used)
    };
    if !forms_folded && !modules_folded {
        return Ok((value, None));
    }
    Ok((
        value,
        Some(Folded {
            forms: forms_folded.then(|| FoldedForms {
                forms: forms_count,
                handlers: handlers_total,
                full_bytes: forms_bytes,
                top_form,
            }),
            modules: modules_folded.then(|| FoldedModules {
                total: modules_total,
                full_bytes: modules_bytes,
            }),
        }),
    ))
}

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
    // Исходящие связи — ТОЛЬКО счётчики по видам, без единого ребра. Объём этой
    // секции зависит не от самого объекта, а от того, сколько связей у него во
    // всей конфигурации (у центральных справочников — до 1 600 рёбер), поэтому
    // паспорт не должен от неё зависеть вовсе. Сами рёбра — get_data_links,
    // у него для этого есть глубина, направление и страницы.
    let mut kind_stmt = conn.prepare(
        "SELECT link_kind, COUNT(*) FROM data_links \
         WHERE repo = ?1 AND from_object = ?2 AND link_kind != 'recorder' \
         GROUP BY link_kind ORDER BY link_kind",
    )?;
    let kind_rows = kind_stmt.query_map(params![REPO, object], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    })?;
    let mut out_by_kind = serde_json::Map::new();
    let mut out_total: i64 = 0;
    for row in kind_rows {
        let (kind, count) = row?;
        out_total += count;
        out_by_kind.insert(kind, json!(count));
    }

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
        "out_total": out_total,
        "out_by_kind": Value::Object(out_by_kind),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::meta_type_to_folder;

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
        // data_links: рёбра в паспорт больше не идут — только счётчики по видам
        let dl = query_data_links(&conn, "Document.Реализация").unwrap();
        assert_eq!(dl["out_total"], json!(1));
        assert_eq!(dl["out_by_kind"]["attr"], json!(1));
        assert!(dl.get("out").is_none(), "рёбра в паспорте не отдаются");
        assert_eq!(dl["writes_to_registers"][0], json!("AccumulationRegister.Продажи"));
        assert_eq!(dl["incoming_refs_count"], json!(0));

        // sections=['structure'] → только structure, без forms/modules/data_links
        let (only, _) = assemble_profile(&conn, "Document.Реализация", "Document", "Реализация",
            &["structure".to_string()], false, false, 48_000).unwrap();
        let o = only.as_object().unwrap();
        assert!(o.contains_key("structure"), "structure должна быть");
        assert!(!o.contains_key("forms"), "forms не запрашивалась → ключа нет");
        assert!(!o.contains_key("modules"));
        assert!(!o.contains_key("data_links"));
        assert_eq!(o["sections_returned"], json!(["structure"]));

        // пустой список → все секции, без sections_returned (обратная совместимость)
        let (full, folded) = assemble_profile(&conn, "Document.Реализация", "Document",
            "Реализация", &[], false, false, 48_000).unwrap();
        let f = full.as_object().unwrap();
        assert!(f.contains_key("structure") && f.contains_key("forms")
            && f.contains_key("modules") && f.contains_key("data_links"));
        assert!(!f.contains_key("sections_returned"));
        // Ответ мелкий — обработчики и модули отданы полностью, свёртки нет.
        assert!(folded.is_none());
        assert_eq!(f["forms_handlers_included"], json!(true));
        assert_eq!(f["forms_total"], json!(1));
        assert_eq!(f["forms_handlers_total"], json!(1));
        assert_eq!(f["modules_listed"], json!(true));
        assert_eq!(f["modules_total"], json!(1));
        assert_eq!(full["forms"][0]["handlers"][0]["event"], json!("ПриОткрытии"));
        assert_eq!(full["modules"][0]["object_id"], json!("uuid-obj"));

        // Тесный бюджет → формы и модули сворачиваются, а подсказка называет
        // форму и несёт готовые вызовы за содержимым обеих секций.
        let (small, folded) = assemble_profile(&conn, "Document.Реализация", "Document",
            "Реализация", &[], false, false, 200).unwrap();
        assert_eq!(small["forms_handlers_included"], json!(false));
        assert_eq!(small["forms"][0]["handlers_count"], json!(1));
        assert!(small["forms"][0].get("handlers").is_none(), "содержимое не отдаётся");
        assert_eq!(small["modules_listed"], json!(false));
        assert_eq!(small["modules"]["by_type"]["ObjectModule"], json!(1));
        let info = folded.expect("свёртка должна быть отмечена");
        assert_eq!(info.forms.as_ref().unwrap().top_form, "ФормаДокумента");
        assert_eq!(info.modules.as_ref().unwrap().total, 1);
        let hint = info.hint("ut", "Document.Реализация");
        assert!(hint.contains("get_form_handlers(repo='ut', owner_full_name='Document.Реализация', form_name='ФормаДокумента')"),
            "подсказка должна нести готовый вызов за обработчиками: {hint}");
        assert!(hint.contains("expand=[\"modules.list\"]"),
            "подсказка должна нести вызов за списком модулей: {hint}");

        // expand → содержимое отдаётся даже при тесном бюджете (просили явно).
        let (expanded, folded) = assemble_profile(&conn, "Document.Реализация", "Document",
            "Реализация", &[], true, false, 200).unwrap();
        assert!(folded.is_none());
        assert_eq!(expanded["forms_handlers_included"], json!(true));
        assert_eq!(expanded["forms"][0]["handlers"][0]["event"], json!("ПриОткрытии"));
    }
}
