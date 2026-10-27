//! Общие части слоя надстройки: состав разбираемых папок, разбор путей
//! объектов, корни областей выгрузки, обслуживание статистики планировщика.

use crate::xml::object_attributes::{
    parse_object_belonging, parse_object_structure_file, ObjectStructure,
};
use anyhow::Result;
use rusqlite::params;
use std::path::Path;
use walkdir::WalkDir;

/// Папки выгрузки → singular meta_type. Объектные XML лежат прямо в этих
/// папках (`Catalogs/<Имя>.xml`). Перечислены типы со ссылочными
/// реквизитами/измерениями плюс те, у кого есть собственный тип значения
/// (константа, определяемый тип, параметр сеанса, общий реквизит, критерий
/// отбора) — у них нет реквизитов, но корневой `<Type>` есть, и без разбора
/// секция `value_types` теряется. Для типов без структуры вовсе
/// (CommonModule и прочие) открывать XML по-прежнему незачем.
pub(crate) const OBJECT_FOLDERS: &[(&str, &str)] = &[
    ("Catalogs", "Catalog"),
    ("Documents", "Document"),
    // Обработки и отчёты: их реквизиты не хранятся в базе данных, но ссылочные
    // типы у них настоящие. Разбор выгрузки EDT их читал, формат Конфигуратора —
    // нет, из-за чего графы связей двух форматов расходились (E-9).
    ("DataProcessors", "DataProcessor"),
    ("Reports", "Report"),
    ("InformationRegisters", "InformationRegister"),
    ("AccumulationRegisters", "AccumulationRegister"),
    ("AccountingRegisters", "AccountingRegister"),
    ("CalculationRegisters", "CalculationRegister"),
    ("ChartsOfCharacteristicTypes", "ChartOfCharacteristicTypes"),
    ("ChartsOfAccounts", "ChartOfAccounts"),
    ("ChartsOfCalculationTypes", "ChartOfCalculationTypes"),
    ("ExchangePlans", "ExchangePlan"),
    ("BusinessProcesses", "BusinessProcess"),
    ("Tasks", "Task"),
    // Перечисления: ссылочных реквизитов нет (data_links → 0 рёбер), но
    // нужны для get_object_structure → enum_values (B2). parse_object_structure_xml
    // собирает <EnumValue>, index_object_attributes пишет в attributes_json.
    ("Enums", "Enum"),
    // Типы без реквизитов, но с собственным типом значения (корневой <Type>).
    // Рёбер data_links не дают: парсер связей корневой <Type> не обрабатывает.
    ("Constants", "Constant"),
    ("DefinedTypes", "DefinedType"),
    ("SessionParameters", "SessionParameter"),
    ("CommonAttributes", "CommonAttribute"),
    ("FilterCriteria", "FilterCriterion"),
    // Регламентные задания: реквизитов нет, но в шапке лежит имя вызываемой
    // процедуры (MethodName) — связь «задание → код», и параметры перезапуска.
    ("ScheduledJobs", "ScheduledJob"),
];

/// Полный маппинг «папка (plural) → meta_type» для ВСЕХ типов верхнего уровня,
/// выгружаемых как `<sub_root>/<Папка>/<Имя>.xml`. Надмножество `OBJECT_FOLDERS`
/// (там только типы со ссылочной структурой для data_links/attributes). Нужен
/// upsert-ветке перечня/синонима: она должна покрывать те же типы, что попадают
/// в `metadata_objects` из Configuration.xml (все `KNOWN_META_TYPES`), а не
/// только объекты со структурой. Полноту стережёт тест
/// `all_object_folders_cover_known_meta_types`.
pub(crate) const ALL_OBJECT_FOLDERS: &[(&str, &str)] = &[
    ("Subsystems", "Subsystem"),
    ("Catalogs", "Catalog"),
    ("Documents", "Document"),
    ("Enums", "Enum"),
    ("Constants", "Constant"),
    ("InformationRegisters", "InformationRegister"),
    ("AccumulationRegisters", "AccumulationRegister"),
    ("AccountingRegisters", "AccountingRegister"),
    ("CalculationRegisters", "CalculationRegister"),
    ("DataProcessors", "DataProcessor"),
    ("Reports", "Report"),
    ("CommonModules", "CommonModule"),
    ("ChartsOfCharacteristicTypes", "ChartOfCharacteristicTypes"),
    ("ChartsOfAccounts", "ChartOfAccounts"),
    ("ChartsOfCalculationTypes", "ChartOfCalculationTypes"),
    ("ExchangePlans", "ExchangePlan"),
    ("BusinessProcesses", "BusinessProcess"),
    ("Tasks", "Task"),
    ("DocumentJournals", "DocumentJournal"),
    ("FilterCriteria", "FilterCriterion"),
    ("EventSubscriptions", "EventSubscription"),
    ("ScheduledJobs", "ScheduledJob"),
    ("FunctionalOptions", "FunctionalOption"),
    ("FunctionalOptionsParameters", "FunctionalOptionsParameter"),
    ("DefinedTypes", "DefinedType"),
    ("CommonAttributes", "CommonAttribute"),
    ("DocumentNumerators", "DocumentNumerator"),
    ("StyleItems", "StyleItem"),
    ("SettingsStorages", "SettingsStorage"),
    ("WSReferences", "WSReference"),
    ("WebServices", "WebService"),
    ("HTTPServices", "HTTPService"),
    ("Styles", "Style"),
    ("Languages", "Language"),
    ("SessionParameters", "SessionParameter"),
    ("Roles", "Role"),
    ("CommonForms", "CommonForm"),
    ("CommonCommands", "CommonCommand"),
    ("CommandGroups", "CommandGroup"),
    ("CommonTemplates", "CommonTemplate"),
    ("CommonPictures", "CommonPicture"),
    ("XDTOPackages", "XDTOPackage"),
    ("Sequences", "Sequence"),
    ("Bots", "Bot"),
    ("ExternalDataSources", "ExternalDataSource"),
];

/// Repo-key для оффлайн-индексации (через `bsl-indexer index .`).
/// В реальном демоне используется alias из daemon.toml; пока этой
/// связки нет на стороне индексер — пишем как «default».
pub(crate) const REPO_DEFAULT: &str = "default";

/// Find the actual on-disk spelling of a file or directory. Do not pick an
/// arbitrary entry when a filesystem contains multiple canonical equivalents.
pub(crate) fn equivalent_path(path: &Path) -> Option<std::path::PathBuf> {
    use unicode_normalization::UnicodeNormalization;
    if path.exists() {
        return Some(path.to_path_buf());
    }
    let name = path.file_name()?.to_str()?;
    let wanted: String = name.nfc().collect();
    let mut found = None;
    for entry in std::fs::read_dir(path.parent()?).ok()?.flatten() {
        let entry_name = entry.file_name();
        let Some(entry_name) = entry_name.to_str() else {
            continue;
        };
        if entry_name.nfc().eq(wanted.chars()) {
            if found.is_some() {
                tracing::warn!("ambiguous Unicode-equivalent path: {}", path.display());
                return None;
            }
            found = Some(entry.path());
        }
    }
    found
}

/// По пути к корневому XML объекта определить `(meta_type, full_name)`.
/// Возвращает `None`, если файл не лежит прямо в одной из `OBJECT_FOLDERS`
/// (т.е. это не корневой XML объекта со ссылочными реквизитами/структурой).
pub(crate) fn object_full_name_from_path(xml_path: &Path) -> Option<(&'static str, String)> {
    use unicode_normalization::UnicodeNormalization;
    if xml_path.extension().and_then(|e| e.to_str()) != Some("xml") {
        return None;
    }
    let stem = xml_path.file_stem().and_then(|s| s.to_str())?;
    let parent_name = xml_path.parent()?.file_name()?.to_str()?;
    for (folder, meta_type) in OBJECT_FOLDERS {
        if *folder == parent_name {
            return Some((
                meta_type,
                format!("{}.{}", meta_type, stem.nfc().collect::<String>()),
            ));
        }
    }
    None
}

/// Как [`object_full_name_from_path`], но для ВСЕХ типов верхнего уровня
/// (`ALL_OBJECT_FOLDERS`), а не только объектов со ссылочной структурой.
/// Используется upsert-веткой перечня/синонима, которая должна вести те же
/// типы, что попадают в `metadata_objects` из Configuration.xml.
pub(crate) fn object_full_name_any(xml_path: &Path) -> Option<(&'static str, String)> {
    use unicode_normalization::UnicodeNormalization;
    if xml_path.extension().and_then(|e| e.to_str()) != Some("xml") {
        return None;
    }
    let stem = xml_path.file_stem().and_then(|s| s.to_str())?;
    let parent_name = xml_path.parent()?.file_name()?.to_str()?;
    for (folder, meta_type) in ALL_OBJECT_FOLDERS {
        if *folder == parent_name {
            return Some((
                meta_type,
                format!("{}.{}", meta_type, stem.nfc().collect::<String>()),
            ));
        }
    }
    None
}

/// Множественная папка выгрузки по singular meta_type (обратный поиск в
/// `ALL_OBJECT_FOLDERS`): `Document` → `Documents`, `Report` → `Reports`.
/// `metadata_forms.owner_full_name` и `metadata_modules.object_name` хранят имя в
/// формате `<PluralFolder>.<Имя>` (проверено на боевом индексе; комментарий в
/// schema.rs про singular full_name модулей устарел — реально плюрал).
pub(crate) fn plural_folder(meta_type: &str) -> Option<&'static str> {
    ALL_OBJECT_FOLDERS
        .iter()
        .find(|(_folder, mt)| *mt == meta_type)
        .map(|(folder, _mt)| *folder)
}

/// Экранировать спецсимволы LIKE (`\`, `%`, `_`) для поиска по префиксу имени —
/// в именах 1С встречается `_` (например `ent_ВводНачислений`), без экранирования
/// он схлопнулся бы в «любой символ».
pub(crate) fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Заполняет `data_links.to_object_key = lower(to_object)` для строк с пустым
/// ключом. SQLite `lower()` кириллицу не берёт — считаем в Rust. Идемпотентно и
/// инкремент-безопасно: трогает только свежевставленные строки (`to_object_key=''`),
/// уже заполненные пропускает. Вызывать в той же транзакции после INSERT-ов.
pub(crate) fn backfill_data_link_keys(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    let pending: Vec<(i64, String)> = {
        let mut sel = conn.prepare(
            "SELECT id, to_object FROM data_links \
             WHERE repo = ?1 AND to_object_key = '' AND to_object <> ''",
        )?;
        let rows = sel.query_map(params![REPO_DEFAULT], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut upd = conn.prepare("UPDATE data_links SET to_object_key = ?2 WHERE id = ?1")?;
    for (id, to_object) in pending {
        upd.execute(params![id, to_object.to_lowercase()])?;
    }
    Ok(())
}

/// Заполняет `role_rights.object_name_key = lower(object_name)` для строк с
/// пустым ключом (см. backfill_data_link_keys — та же мотивация по кириллице).
pub(crate) fn backfill_role_right_keys(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    let pending: Vec<(i64, String)> = {
        let mut sel = conn.prepare(
            "SELECT id, object_name FROM role_rights \
             WHERE repo = ?1 AND object_name_key = '' AND object_name <> ''",
        )?;
        let rows = sel.query_map(params![REPO_DEFAULT], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut upd = conn.prepare("UPDATE role_rights SET object_name_key = ?2 WHERE id = ?1")?;
    for (id, object_name) in pending {
        upd.execute(params![id, object_name.to_lowercase()])?;
    }
    Ok(())
}

/// Путь модуля относительно корня репо в формате `files.path`
/// (forward slash). Совпадает с конвенцией direct_edge_files/code_path.
pub(crate) fn rel_path(repo_root: &Path, abs: &Path) -> String {
    abs.strip_prefix(repo_root)
        .unwrap_or(abs)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Фильтр каталогов при обходе репозитория — то же правило, что у файлового
/// индекса ядра: встроенный список + `exclude_dirs` из `.code-index/config.json`.
#[derive(Clone)]
pub(crate) struct DirFilter {
    config: code_index_core::indexer::config::IndexConfig,
}

impl DirFilter {
    /// Загрузить исключения из `.code-index/config.json` репозитория. Ошибка
    /// чтения или разбора фильтрацию не отменяет: встроенный список исключений
    /// действует и на конфиге по умолчанию.
    pub(crate) fn load(repo_root: &Path) -> Self {
        match code_index_core::indexer::config::IndexConfig::load(repo_root) {
            Ok(config) => Self { config },
            Err(e) => {
                tracing::warn!(
                    "{}: не читается .code-index/config.json ({}), беру встроенный список исключений",
                    repo_root.display(),
                    e
                );
                Self {
                    config: code_index_core::indexer::config::IndexConfig::default(),
                }
            }
        }
    }

    /// Оставлять ли запись при обходе. Начальный каталог (depth 0) не
    /// отбрасывается никогда, файлы фильтр не трогает.
    pub(crate) fn allows(&self, entry: &walkdir::DirEntry) -> bool {
        if entry.depth() == 0 || !entry.file_type().is_dir() {
            return true;
        }
        match entry.file_name().to_str() {
            Some(name) => !self.config.is_excluded_dir(name),
            None => true,
        }
    }
}

/// Корни sub-config'ов репо: каталоги, содержащие `Configuration.xml` на
/// глубине ≤ 3 (base/ + extensions/<name>/). base-роуты идут ПЕРВЫМИ — их
/// структура приоритетна при мердже одноимённых реквизитов (см.
/// `ObjectStructure::merge_from`).
pub(crate) fn sub_config_roots(repo_root: &Path) -> Vec<std::path::PathBuf> {
    let mut roots: Vec<std::path::PathBuf> = Vec::new();
    let filter = DirFilter::load(repo_root);
    for entry in WalkDir::new(repo_root)
        .max_depth(3)
        .into_iter()
        .filter_entry(|e| filter.allows(e))
        .filter_map(|e| e.ok())
    {
        if entry.file_type().is_file() && entry.file_name().to_str() == Some("Configuration.xml") {
            if let Some(parent) = entry.path().parent() {
                roots.push(parent.to_path_buf());
            }
        }
    }
    // base-роуты первыми: путь без компонента "extensions". sort_by_key стабилен,
    // поэтому относительный порядок внутри групп сохраняется.
    roots.sort_by_key(|p| u8::from(p.components().any(|c| c.as_os_str() == "extensions")));
    roots
}

/// Структура объекта, слитая по всем его копиям в sub-config'ах (base +
/// расширения). Роуты должны быть отсортированы base-first (см.
/// `sub_config_roots`) — тогда базовые типы реквизитов приоритетны, а
/// расширения добавляют только свои новые поля/ТЧ. Возвращает `None`, если ни в
/// одной sub-config нет непустой структуры этого объекта.
pub(crate) fn merged_object_structure(
    roots: &[std::path::PathBuf],
    folder: &str,
    stem: &str,
) -> Option<ObjectStructure> {
    let mut acc: Option<ObjectStructure> = None;
    for root in roots {
        let path = root.join(folder).join(format!("{}.xml", stem));
        let Some(path) = equivalent_path(&path) else {
            continue;
        };
        match parse_object_structure_file(&path) {
            Ok(Some(s)) if !s.is_empty() => match acc.as_mut() {
                Some(a) => a.merge_from(&s),
                None => acc = Some(s),
            },
            _ => {}
        }
    }
    acc.filter(|s| !s.is_empty())
}

/// Извлечь (`owner_full_name`, `form_name`) из пути к Form.xml.
/// Возвращает None, если структура каталогов не похожа на выгрузку 1С.
pub(crate) fn decode_form_path(repo_root: &Path, form_xml_path: &Path) -> Option<(String, String)> {
    use unicode_normalization::UnicodeNormalization;
    // Берём отрезок пути относительно корня репо и разбираем сегменты.
    let rel = form_xml_path.strip_prefix(repo_root).ok()?;
    let segments: Vec<&str> = rel
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();

    // Общая форма — самостоятельный объект (`CommonForms/<Имя>/Ext/Form.xml`),
    // каталога `Forms` у неё нет вовсе. Под общий разбор пути такой файл не
    // подходил, и ни одна общая форма в индекс не попадала — 428 форм типовой
    // бухгалтерии (E-3). Владелец — сама форма, ключом берём папку выгрузки
    // `CommonForms.<Имя>`: под ней же лежит её модуль в перечне модулей.
    if let Some(idx) = segments.iter().position(|s| *s == "CommonForms") {
        let name = segments.get(idx + 1)?;
        let name: String = name.nfc().collect();
        return Some((format!("CommonForms.{}", name), name));
    }
    // Ищем индекс "Forms" — он точно есть в правильной структуре.
    let forms_idx = segments.iter().position(|s| *s == "Forms")?;
    if forms_idx < 2 {
        // Должно быть как минимум `<MetaType>/<OwnerName>/Forms/...`.
        return None;
    }
    let meta_type = segments[forms_idx - 2];
    let owner_name = segments[forms_idx - 1];
    let form_name = segments.get(forms_idx + 1)?;
    let owner_full = format!("{}.{}", meta_type, owner_name.nfc().collect::<String>());
    Some((owner_full, form_name.nfc().collect()))
}

/// Ближайший предок пути (в пределах `repo_root`), содержащий `Configuration.xml`
/// — sub-config, которому принадлежит файл. Нужен точечным веткам, чтобы взять
/// `extension_name`/`config_version` без полного обхода репо. `None`, если ни у
/// одного предка (вплоть до `repo_root`) нет `Configuration.xml`.
pub(crate) fn sub_root_for_path(repo_root: &Path, path: &Path) -> Option<std::path::PathBuf> {
    let mut cur = path.parent();
    while let Some(dir) = cur {
        if dir.join("Configuration.xml").is_file() {
            return Some(dir.to_path_buf());
        }
        if dir == repo_root {
            break;
        }
        cur = dir.parent();
    }
    None
}

/// `extension_name` для записи в `metadata_modules` — относительный путь
/// от корня репо до sub-config. Пустая строка для случая когда
/// Configuration.xml лежит в самом корне (single-config выгрузка) или
/// для `base/` (рассматриваем base как «не-расширение», чтобы агенты
/// фильтровали отдельно `extension_name = ''` для основного).
pub(crate) fn compute_extension_name(repo_root: &Path, sub_root: &Path) -> String {
    if sub_root == repo_root {
        return String::new();
    }
    let rel = match sub_root.strip_prefix(repo_root) {
        Ok(r) => r,
        Err(_) => return String::new(),
    };
    let s = rel.to_string_lossy().replace('\\', "/");
    // base/ — это не расширение, оставляем пустую строку.
    if s == "base" {
        return String::new();
    }
    s
}

/// Владелец объекта (`metadata_objects.sub_config`) по фактическим копиям
/// объекта во ВСЕХ sub-config'ах (base-first). Тем же правилом, что пофайловый
/// `upsert_metadata_object`: копия в базовой области, копия с
/// `ObjectBelonging=Adopted` либо без тега → владелец база (`''`); собственный
/// (`Native`) объект расширения → путь области-расширения. Нужен полному
/// проходу (`index_metadata_objects`), который иначе писал бы `sub_config=''`
/// всем подряд (расхождение с инкрементом).
///
/// `folder` — папка выгрузки объекта (`Catalogs`), `stem` — имя файла без
/// `.xml`. Отсутствие копий на диске → `''` (строка перечня без владельца).
pub(crate) fn object_sub_config(
    repo_root: &Path,
    roots: &[std::path::PathBuf],
    folder: &str,
    stem: &str,
) -> String {
    let mut owner = String::new();
    let mut owner_is_base = false;
    for sub_root in roots {
        if owner_is_base {
            break;
        }
        let Some(cand) = equivalent_path(&sub_root.join(folder).join(format!("{}.xml", stem)))
        else {
            continue;
        };
        if !cand.is_file() {
            continue;
        }
        let content = match std::fs::read_to_string(&cand) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let ext_name = compute_extension_name(repo_root, sub_root);
        let belonging = parse_object_belonging(&content);
        if ext_name.is_empty() || belonging.as_deref() != Some("Native") {
            // base-root, либо Adopted, либо тега нет → владелец база.
            owner = String::new();
            owner_is_base = true;
        } else {
            // Native в расширении.
            owner = ext_name;
        }
    }
    owner
}

/// Число строк в таблице, записанное в `sqlite_stat1` на момент последнего
/// `ANALYZE` (первый токен колонки `stat`). `None` — статистики нет (таблицу
/// ни разу не анализировали, либо самой `sqlite_stat1` ещё нет).
///
/// Берём максимум по всем строкам таблицы, а не первую попавшуюся: в
/// `sqlite_stat1` лежит по строке на КАЖДЫЙ индекс, и первое число в `stat` —
/// это число записей в конкретном индексе. У частичного индекса оно меньше
/// числа строк таблицы: `idx_pcg_callee_key` хранит только рёбра с известным
/// адресом цели (~⅔ строк), `idx_pcg_call_type_nd` — только НЕ-direct рёбра.
/// `LIMIT 1` отдавал такое заниженное число, и `stats_drifted` находил дрейф
/// ≥1.5× на неизменных данных — полный `ANALYZE` запускался после каждой правки
/// `.bsl` и ничем не помогал (частичный индекс и после него остаётся меньше
/// таблицы). Полный индекс (или автоиндекс) содержит все строки таблицы,
/// поэтому максимум равен `count(*)`.
pub(crate) fn analyzed_row_count(conn: &rusqlite::Connection, table: &str) -> Option<i64> {
    conn.query_row(
        "SELECT max(CAST(stat AS INTEGER)) FROM sqlite_stat1 WHERE tbl = ?1",
        params![table],
        |r| r.get::<_, Option<i64>>(0),
    )
    .ok()
    .flatten()
}

/// Разошлась ли реальная величина таблицы со статистикой настолько, что пора
/// пересчитать `ANALYZE`. Планировщик SQLite меняет план (seek по индексу ↔
/// перебор всех рёбер) при кратном расхождении, поэтому порог — дрейф в 1.5×
/// в любую сторону. Пол `FLOOR`: на мелких таблицах статистика неважна.
pub(crate) fn stats_drifted(current: i64, recorded: Option<i64>) -> bool {
    const FLOOR: i64 = 1000;
    match recorded {
        // Статистики нет: анализируем, только если таблица уже крупная.
        None => current >= FLOOR,
        Some(rec) => {
            if current < FLOOR && rec < FLOOR {
                return false;
            }
            // current ≥ 1.5×rec  ⟺  current*2 ≥ rec*3 (без плавающей точки);
            // current ≤ rec/1.5  ⟺  current*3 ≤ rec*2.
            current * 2 >= rec * 3 || current * 3 <= rec * 2
        }
    }
}

/// Пересчитать `ANALYZE`, если величина графовых таблиц (`data_links`,
/// `proc_call_graph`) разошлась со статистикой в ≥1.5× — иначе рекурсивные
/// обходы `find_data_path` / `find_path_bsl` деградируют (планировщик без свежей
/// `sqlite_stat1` перебирает все рёбра вместо seek). Зовём его лишь при реальном
/// дрейфе, а не на каждый батч, и анализируем только те таблицы, по которым
/// дрейф найден: полный `ANALYZE` по всей БД на базе 5.7 ГБ идёт ~5 с на
/// свободной машине и до минуты под нагрузкой, тогда как две графовые таблицы —
/// ~1.4 с. Остальные таблицы получают свежую статистику от полного `ANALYZE` в
/// конце полной переиндексации (`run_index_extras`).
pub(crate) fn maybe_analyze_graph_tables(conn: &rusqlite::Connection) -> Result<()> {
    let mut drifted: Vec<&str> = Vec::new();
    for table in ["data_links", "proc_call_graph"] {
        let current: i64 = conn
            .query_row(&format!("SELECT count(*) FROM {}", table), [], |r| r.get(0))
            .unwrap_or(0);
        if stats_drifted(current, analyzed_row_count(conn, table)) {
            drifted.push(table);
        }
    }
    if drifted.is_empty() {
        return Ok(());
    }
    let _ = conn.execute("ROLLBACK", []); // ANALYZE не может идти внутри транзакции
    for table in &drifted {
        // Имя таблицы — константа из списка выше, а не пользовательский ввод.
        conn.execute(&format!("ANALYZE {}", table), [])?;
    }
    tracing::info!(
        tables = ?drifted,
        "ANALYZE: статистика графовых таблиц пересчитана (дрейф ≥1.5×)"
    );
    Ok(())
}
