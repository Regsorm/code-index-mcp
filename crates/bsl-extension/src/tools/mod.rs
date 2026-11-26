// MCP-инструменты, специфичные для конфигураций 1С.
//
// Все четыре инструмента опираются на таблицы, заполняемые
// `index_extras::run_index_extras` (этап 4):
//
// - `get_object_structure` — читает строку из `metadata_objects` по
//   full_name и возвращает meta_type/name/synonym/attributes.
// - `get_form_handlers` — читает запись из `metadata_forms` по
//   (owner_full_name, form_name) и возвращает массив (event, handler).
// - `get_event_subscriptions` — отдаёт все подписки репо из
//   `event_subscriptions` (с опциональной фильтрацией по handler-модулю).
// - `find_path_bsl` — проходит по `proc_call_graph` через recursive CTE
//   и возвращает первый путь из caller в callee длиной до max_depth
//   (BSL-вариант универсального `find_path` ядра по таблице `calls`).
//
// Регистрируются в `BslLanguageProcessor::additional_tools()` и
// попадают в MCP `tools/list` только если хотя бы у одного репо
// `language = "bsl"` (этап 1.5/1.6 → conditional registration).

pub mod bsl_sql;
pub mod find_data_path;
pub mod find_path_bsl;
pub mod find_references;
pub mod get_data_links;
pub mod get_dcs_schema;
pub mod get_event_subscriptions;
pub mod get_form_handlers;
pub mod get_object_profile;
pub mod get_object_structure;
pub mod get_register_writers;
pub mod get_role_rights;
pub mod posting_map;
pub mod search_terms;
pub mod structure;
mod subscription_sources;

pub use bsl_sql::BslSqlTool;
pub use find_data_path::FindDataPathTool;
pub use find_path_bsl::FindPathBslTool;
pub use find_references::FindReferencesTool;
pub use get_data_links::GetDataLinksTool;
pub use get_dcs_schema::GetDcsSchemaTool;
pub use get_event_subscriptions::GetEventSubscriptionsTool;
pub use get_form_handlers::GetFormHandlersTool;
pub use get_object_profile::GetObjectProfileTool;
pub use get_object_structure::GetObjectStructureTool;
pub use get_register_writers::GetRegisterWritersTool;
pub use get_role_rights::GetRoleRightsTool;
pub use posting_map::PostingMapTool;
pub use search_terms::SearchTermsTool;
pub use structure::{GetLineContextTool, GetNodeStructureTool, QueryCallSitesTool};

use serde_json::{json, Value};

/// Свежесть файла-описания объекта. Данные из форм, модулей и расширений
/// могут собираться из других файлов, поэтому у всего объекта нет одного mtime.
fn add_descriptor_freshness(
    storage: &code_index_core::storage::Storage,
    root: Option<&std::path::Path>,
    object: &mut Value,
) {
    let Some((root, full_name)) = root.zip(object.get("full_name").and_then(Value::as_str)) else {
        return;
    };
    let Some((kind, name, owner)) =
        crate::index_extras::lookup_object_meta(storage.conn(), full_name)
    else {
        return;
    };
    let Some(folder) = crate::index_extras::plural_folder(&kind) else {
        return;
    };
    let xml = format!("{folder}/{name}.xml");
    let mdo = format!("{folder}/{name}/{name}.mdo");
    let candidates = if owner.is_empty() {
        vec![format!("base/{xml}"), xml, format!("src/{mdo}"), mdo]
    } else {
        vec![format!("{owner}/{xml}")]
    };
    let mut files = candidates
        .iter()
        .filter_map(|path| storage.get_file_by_path(path).ok().flatten());
    if let (Some(file), None) = (files.next(), files.next()) {
        let mut freshness = code_index_core::mcp::tools::file_freshness(root, &file);
        freshness.insert("path".into(), json!(file.path));
        object["descriptor_freshness"] = Value::Object(freshness);
    }
}

fn descriptor_paths(object: &Value) -> Vec<String> {
    let objects = match object.get("results").and_then(Value::as_array) {
        Some(results) => results.as_slice(),
        None => std::slice::from_ref(object),
    };
    objects
        .iter()
        .filter_map(|item| {
            item.pointer("/descriptor_freshness/path")?
                .as_str()
                .map(str::to_owned)
        })
        .collect()
}

/// Завернуть результат BSL-tool'а в `{result, _meta: {dependent_files: [...]}}`
/// для cache-ci event-based invalidation (Phase 2). Инструменты без
/// известного файла-источника передают пустой массив.
pub(crate) fn wrap_with_meta(tool: &str, result: Value, dependent_files: Vec<String>) -> Value {
    // cap_response (обрез массивов с сэмплом) применяется ТОЛЬКО если инструмент
    // в списке `[mcp].cap_tools` (параметр сервера; дефолт — cap::DEFAULT_CAP_TOOLS).
    // Иначе ответ как есть. Серверная нода ужимает ДО federation-провода и клиента
    // (не давая harness'у сбросить громадный tool_result на диск).
    let (result, truncated) = if code_index_core::mcp::cap::cap_applies(tool) {
        code_index_core::mcp::cap::cap_response(result, code_index_core::mcp::cap::response_cap())
    } else {
        (result, false)
    };
    let mut out = json!({
        "result": result,
        "_meta": { "dependent_files": dependent_files },
    });
    if truncated {
        if let Some(obj) = out.as_object_mut() {
            obj.insert("response_truncated".to_string(), json!(true));
            obj.insert(
                "response_truncated_hint".to_string(),
                json!(code_index_core::mcp::cap::CAP_HINT),
            );
        }
    }
    out
}

/// Обёртка для СТРУКТУРНЫХ инструментов (get_object_structure и др. из
/// `cap::STRUCTURAL_TOOLS`): `{result, _meta}` БЕЗ `cap_response` — слепой обрез
/// массивов исказил бы авторитетную структуру объекта 1С (получишь «1 значение
/// перечисления из 816»). Размером такие tools управляют сами через
/// `cap::omit_oversize_sections` (тяжёлую секцию целиком) ДО этой обёртки.
/// `omitted` → добавить верхнеуровневый маркер + hint.
pub(crate) fn wrap_with_meta_structural(
    result: Value,
    dependent_files: Vec<String>,
    omitted: bool,
) -> Value {
    let mut out = json!({
        "result": result,
        "_meta": { "dependent_files": dependent_files },
    });
    if omitted {
        if let Some(obj) = out.as_object_mut() {
            obj.insert("response_sections_omitted".to_string(), json!(true));
            obj.insert(
                "response_sections_omitted_hint".to_string(),
                json!(code_index_core::mcp::cap::OMIT_HINT),
            );
        }
    }
    out
}

/// Сохранить _meta даже на ошибке, чтобы клиенты всегда получали единый формат
/// `{result, _meta}`. Tool сам помещает в `result` что нужно (включая `{error: ...}`).
pub(crate) fn wrap_error(error_value: Value) -> Value {
    // Ошибки крошечные и капу не подлежат — без cap, без hint.
    wrap_with_meta_structural(error_value, Vec::new(), false)
}

/// Имя объекта для single-object инструмента — берётся ЗНАЧЕНИЕ без оглядки на имя
/// ключа. Агент мог назвать параметр `object`/`full_name`/`name`/как угодно — не
/// важно: у такого инструмента ровно один объект, поэтому имя ключа не анализируем.
/// Пропускаются служебные ключи (repo и общие модификаторы), первое непустое
/// строковое значение трактуется как имя объекта.
///
/// НЕ применять в multi-object инструментах (`find_data_path` from/to,
/// `get_form_handlers` owner+form_name) — там имя ключа значимо.
pub(crate) fn object_value(args: &Value) -> Option<&str> {
    const SERVICE: &[&str] = &[
        "repo",
        "depth",
        "limit",
        "direction",
        "sections",
        "language",
        "max_depth",
        "name_like",
        "meta_type",
        "names_only",
        "max_response_bytes",
    ];
    args.as_object()?
        .iter()
        .filter(|(k, _)| !SERVICE.contains(&k.as_str()))
        .find_map(|(_, v)| v.as_str().filter(|s| !s.trim().is_empty()))
}

/// singular meta_type → имя папки выгрузки (plural), под которым хранятся
/// формы (`metadata_forms.owner_full_name`) и модули (`metadata_modules.full_name`).
/// Возвращает `None` для пустого типа. Покрывает все типы, у которых бывают
/// формы или модули; общий хелпер get_object_profile и get_form_handlers.
pub(crate) fn meta_type_to_folder(meta_type: &str) -> Option<String> {
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

/// Развернуть плоский критерий-селектор в список `full_name` по индексу
/// метаданных. ОБЩАЯ конвенция объектно-ключевых инструментов: вместо одного
/// имени модель передаёт предикат (`name_like` — подстрока имени объекта +
/// опц. `meta_type`), сервер сам разворачивает его в набор за один SQL по
/// `metadata_objects` (`repo='default'` — как в `resolve_one`), дальше набор
/// уходит в `mass_map`. Это форма ПРЕДИКАТА (высокий adoption, как sections=),
/// а не форма списка (`full_names[]` модель спонтанно не собирает).
///
/// `cap` — потолок объектов (защита от широкого критерия вроде LIKE '%а%').
/// Останавливаемся на `cap + 1`-м подходящем объекте, чтобы отличить «ровно cap»
/// от «больше cap». Возвращает `(full_names ≤ cap, truncated)`.
///
/// Совпадение проверяется в Rust, а не в SQL: SQLite сворачивает регистр только
/// для ASCII, и кириллица `LIKE` регистрозначима. Здесь работает
/// Unicode-регистронезависимое сравнение (см. [`LikePattern`]).
///
/// Адаптер «только имена» — для тестов критерия: рабочему пути нужна ещё и
/// причина совпадения, поэтому он зовёт [`expand_object_criterion_matched`].
#[cfg(test)]
pub(crate) fn expand_object_criterion(
    conn: &rusqlite::Connection,
    name_like: &str,
    meta_type: Option<&str>,
    cap: usize,
) -> rusqlite::Result<(Vec<String>, bool)> {
    let (rows, truncated) = expand_object_criterion_matched(conn, name_like, meta_type, cap)?;
    Ok((rows.into_iter().map(|(fqn, _)| fqn).collect(), truncated))
}

/// Причина совпадения объекта с критерием-селектором.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MatchedBy {
    /// Совпало имя объекта (`metadata_objects.name`).
    Name,
    /// Совпал синоним (`metadata_objects.synonym`) при несовпавшем имени.
    Synonym,
    /// Совпали и имя, и синоним — объект всё равно возвращается один раз.
    NameAndSynonym,
}

impl MatchedBy {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            MatchedBy::Name => "name",
            MatchedBy::Synonym => "synonym",
            MatchedBy::NameAndSynonym => "name+synonym",
        }
    }
}

/// LIKE-шаблон критерия, скомпилированный в Unicode-регистронезависимый regex.
///
/// Семантика сохранена фактическая (SQLite `LIKE`), но регистр сворачивается
/// по-юникодному:
/// - `%` — произвольная последовательность символов (в том числе пустая);
/// - `_` — ровно один Unicode-символ;
/// - всё остальное — буквально, включая `\` (обратная косая черта НЕ экранирует);
/// - сравнение — поиск ВНУТРИ строки, а не полное совпадение;
/// - обе строки предварительно приводятся к NFC: имена 1С приходят в разных
///   формах нормализации, и без этого «й» из двух кодовых точек не совпадала.
///
/// Сворачивание регистра делает regex (`case_insensitive`), а не SQLite
/// `lower`/`NOCASE` — те знают только ASCII.
pub(crate) struct LikePattern {
    re: regex::Regex,
}

impl LikePattern {
    pub(crate) fn new(pattern: &str) -> Result<Self, regex::Error> {
        use unicode_normalization::UnicodeNormalization;
        let mut src = String::with_capacity(pattern.len() * 2 + 8);
        let mut literal = String::new();
        for ch in pattern.nfc() {
            match ch {
                '%' | '_' => {
                    if !literal.is_empty() {
                        src.push_str(&regex::escape(&literal));
                        literal.clear();
                    }
                    src.push_str(if ch == '%' { ".*" } else { "." });
                }
                other => literal.push(other),
            }
        }
        if !literal.is_empty() {
            src.push_str(&regex::escape(&literal));
        }
        // `(?s)` — `_`/`%` захватывают и перевод строки (как в SQLite).
        let re = regex::RegexBuilder::new(&format!("(?s){src}"))
            .case_insensitive(true)
            .build()?;
        Ok(Self { re })
    }

    /// Есть ли совпадение внутри `haystack` (поиск, не полное совпадение).
    pub(crate) fn is_match(&self, haystack: &str) -> bool {
        use unicode_normalization::UnicodeNormalization;
        let normalized: String = haystack.nfc().collect();
        self.re.is_match(&normalized)
    }
}

/// Развернуть критерий в список `full_name` вместе с причиной совпадения.
///
/// Чтение потоковое: SQL отбирает строки по `repo='default'` (и `meta_type`,
/// если он задан) в порядке `full_name`, совпадение по имени/синониму считается
/// в Rust, обход останавливается на `cap + 1`-м подходящем объекте — весь
/// `metadata_objects` в память не поднимается.
pub(crate) fn expand_object_criterion_matched(
    conn: &rusqlite::Connection,
    name_like: &str,
    meta_type: Option<&str>,
    cap: usize,
) -> rusqlite::Result<(Vec<(String, MatchedBy)>, bool)> {
    // meta_type канонизируем (RU→EN, как resolve_one); неизвестный тип оставляем
    // как есть — SQL просто вернёт 0 строк (критерий ни с чем не совпал).
    let canon = meta_type.map(|t| match crate::code_usages::canonical_meta_type(t) {
        Some(c) => c.to_string(),
        None => t.to_string(),
    });
    let pattern = match LikePattern::new(name_like) {
        Ok(p) => p,
        // Трансляция LIKE→regex не может не собраться, но паниковать в поиске
        // незачем: невалидный шаблон = «ничего не совпало».
        Err(_) => return Ok((Vec::new(), false)),
    };
    let sql = if canon.is_some() {
        "SELECT full_name, name, synonym FROM metadata_objects \
         WHERE repo = 'default' AND meta_type = ?1 ORDER BY full_name"
    } else {
        "SELECT full_name, name, synonym FROM metadata_objects \
         WHERE repo = 'default' ORDER BY full_name"
    };
    let mut stmt = conn.prepare(sql)?;
    let mut rows = match &canon {
        Some(mt) => stmt.query(rusqlite::params![mt])?,
        None => stmt.query([])?,
    };
    let mut out: Vec<(String, MatchedBy)> = Vec::new();
    let mut truncated = false;
    while let Some(row) = rows.next()? {
        if code_index_core::storage::search_support::deadline_expired() {
            truncated = true;
            break;
        }
        let full_name: String = row.get(0)?;
        let name: Option<String> = row.get(1)?;
        let synonym: Option<String> = row.get(2)?;
        let name_hit = name.as_deref().is_some_and(|n| pattern.is_match(n));
        let synonym_hit = synonym
            .as_deref()
            .filter(|s| !s.is_empty())
            .is_some_and(|s| pattern.is_match(s));
        let matched_by = match (name_hit, synonym_hit) {
            (true, true) => MatchedBy::NameAndSynonym,
            (true, false) => MatchedBy::Name,
            (false, true) => MatchedBy::Synonym,
            // NULL и пустой синоним просто не совпадают — пропускаем строку.
            (false, false) => continue,
        };
        if out.len() >= cap {
            // Нашли `cap + 1`-й подходящий объект — набор шире потолка.
            truncated = true;
            break;
        }
        out.push((full_name, matched_by));
    }
    Ok((out, truncated))
}

/// Привести имя объекта к тому, как оно записано в конфигурации: `Catalog.организации`
/// → `Catalog.Организации`. Возвращает `None`, если такого объекта в перечне нет
/// (тогда вызывающий работает с исходной строкой — так узлы, которых нет в
/// `metadata_objects`, вроде обобщённых `*DefinedType.…`, остаются рабочими).
///
/// Зачем: имена объектов 1С кириллические, регистр в них легко перепутать, а
/// SQLite кириллицу не сворачивает — точное сравнение промахивалось МОЛЧА, и
/// связи данных отвечали «на объект никто не ссылается», регистраторы —
/// «регистраторов нет». Пустой ответ читался как содержательный вывод.
/// Один резолв на входе инструмента лечит это для всех запросов сразу: дальше
/// обход идёт по каноническим значениям, взятым из самой базы.
///
/// Сначала точное совпадение (обычный путь, один seek по UNIQUE), затем поиск
/// по ключу `full_name_key` (= lower(full_name), индекс `idx_mo_full_key`).
pub(crate) fn resolve_object_name(conn: &rusqlite::Connection, input: &str) -> Option<String> {
    use rusqlite::OptionalExtension;
    use unicode_normalization::UnicodeNormalization;
    if input.is_empty() {
        return None;
    }
    let input: String = input.nfc().collect();
    let exact: Option<String> = conn
        .query_row(
            "SELECT full_name FROM metadata_objects WHERE repo = 'default' AND full_name = ?1",
            rusqlite::params![&input],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten();
    if exact.is_some() {
        return exact;
    }
    conn.query_row(
        "SELECT full_name FROM metadata_objects \
         WHERE repo = 'default' AND full_name_key = ?1 ORDER BY full_name LIMIT 1",
        rusqlite::params![input.to_lowercase()],
        |r| r.get(0),
    )
    .optional()
    .ok()
    .flatten()
}

/// Имя объекта для запроса: канон из перечня, а если объекта там нет — как
/// передали. Обёртка над [`resolve_object_name`] для мест, где нужна строка,
/// а не признак «нашёлся ли объект».
pub(crate) fn canonical_object_name(conn: &rusqlite::Connection, input: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    resolve_object_name(conn, input).unwrap_or_else(|| input.nfc().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn mem_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        for ddl in crate::schema::SCHEMA_EXTENSIONS {
            conn.execute_batch(ddl).unwrap();
        }
        let rows = [
            ("Catalog.НастройкиЭДО", "Catalog", "НастройкиЭДО"),
            ("Document.СообщениеЭДО", "Document", "СообщениеЭДО"),
            (
                "InformationRegister.АбонентыЭДО",
                "InformationRegister",
                "АбонентыЭДО",
            ),
            ("Catalog.Контрагенты", "Catalog", "Контрагенты"),
        ];
        for (fqn, mt, nm) in rows {
            conn.execute(
                "INSERT INTO metadata_objects (repo, full_name, meta_type, name) \
                 VALUES ('default', ?, ?, ?)",
                rusqlite::params![fqn, mt, nm],
            )
            .unwrap();
        }
        // Как при индексации: ключи имён догоняются после вставок.
        crate::schema::backfill_metadata_object_keys(&conn).unwrap();
        conn
    }

    #[test]
    fn resolve_object_name_exact_and_case_insensitive() {
        let conn = mem_db();
        // Точное имя — возвращается как есть.
        assert_eq!(
            resolve_object_name(&conn, "Catalog.Контрагенты").as_deref(),
            Some("Catalog.Контрагенты")
        );
        // Иной регистр кириллицы — канон из перечня. Прежде такой запрос
        // молча не находил объект, и инструменты отвечали пустотой.
        assert_eq!(
            resolve_object_name(&conn, "Catalog.контрагенты").as_deref(),
            Some("Catalog.Контрагенты")
        );
        assert_eq!(
            resolve_object_name(&conn, "catalog.КОНТРАГЕНТЫ").as_deref(),
            Some("Catalog.Контрагенты")
        );
    }

    #[test]
    fn resolve_object_name_unknown_stays_none() {
        let conn = mem_db();
        assert!(resolve_object_name(&conn, "Catalog.НетТакого").is_none());
        assert!(resolve_object_name(&conn, "").is_none());
        // Узлы, которых нет в перечне (обобщённые *-типы), остаются рабочими:
        // вызывающий получает исходную строку, а не пустоту.
        assert_eq!(
            canonical_object_name(&conn, "*DefinedType.ЛюбаяСумма"),
            "*DefinedType.ЛюбаяСумма"
        );
    }

    #[test]
    fn expand_filters_by_name_substring() {
        let conn = mem_db();
        let (names, truncated) = expand_object_criterion(&conn, "ЭДО", None, 50).unwrap();
        assert_eq!(names.len(), 3);
        assert!(!truncated);
        assert!(names.iter().all(|n| n.contains("ЭДО")));
        assert!(!names.contains(&"Catalog.Контрагенты".to_string()));
    }

    #[test]
    fn expand_filters_by_meta_type() {
        let conn = mem_db();
        let (names, _) = expand_object_criterion(&conn, "ЭДО", Some("Catalog"), 50).unwrap();
        assert_eq!(names, vec!["Catalog.НастройкиЭДО".to_string()]);
    }

    #[test]
    fn expand_meta_type_accepts_russian_singular() {
        let conn = mem_db();
        // RU singular «Документ» канонизируется в Document.
        let (names, _) = expand_object_criterion(&conn, "ЭДО", Some("Документ"), 50).unwrap();
        assert_eq!(names, vec!["Document.СообщениеЭДО".to_string()]);
    }

    #[test]
    fn expand_cap_sets_truncated() {
        let conn = mem_db();
        // cap=2 при 3 совпадениях → ровно 2 имени + truncated.
        let (names, truncated) = expand_object_criterion(&conn, "ЭДО", None, 2).unwrap();
        assert_eq!(names.len(), 2);
        assert!(truncated);
    }

    #[test]
    fn expand_empty_when_no_match() {
        let conn = mem_db();
        let (names, truncated) =
            expand_object_criterion(&conn, "НесуществующаяТема", None, 50).unwrap();
        assert!(names.is_empty());
        assert!(!truncated);
    }
}
