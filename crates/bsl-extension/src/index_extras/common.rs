//! Общие части слоя надстройки: состав разбираемых папок, разбор путей
//! объектов, корни областей выгрузки, обслуживание статистики планировщика.

use std::path::Path;
use anyhow::Result;
use rusqlite::params;
use walkdir::WalkDir;
use crate::xml::object_attributes::{
    parse_object_structure_file, ObjectStructure,
};


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



/// По пути к корневому XML объекта определить `(meta_type, full_name)`.
/// Возвращает `None`, если файл не лежит прямо в одной из `OBJECT_FOLDERS`
/// (т.е. это не корневой XML объекта со ссылочными реквизитами/структурой).
pub(crate) fn object_full_name_from_path(xml_path: &Path) -> Option<(&'static str, String)> {
    if xml_path.extension().and_then(|e| e.to_str()) != Some("xml") {
        return None;
    }
    let stem = xml_path.file_stem().and_then(|s| s.to_str())?;
    let parent_name = xml_path.parent()?.file_name()?.to_str()?;
    for (folder, meta_type) in OBJECT_FOLDERS {
        if *folder == parent_name {
            return Some((meta_type, format!("{}.{}", meta_type, stem)));
        }
    }
    None
}


/// Как [`object_full_name_from_path`], но для ВСЕХ типов верхнего уровня
/// (`ALL_OBJECT_FOLDERS`), а не только объектов со ссылочной структурой.
/// Используется upsert-веткой перечня/синонима, которая должна вести те же
/// типы, что попадают в `metadata_objects` из Configuration.xml.
pub(crate) fn object_full_name_any(xml_path: &Path) -> Option<(&'static str, String)> {
    if xml_path.extension().and_then(|e| e.to_str()) != Some("xml") {
        return None;
    }
    let stem = xml_path.file_stem().and_then(|s| s.to_str())?;
    let parent_name = xml_path.parent()?.file_name()?.to_str()?;
    for (folder, meta_type) in ALL_OBJECT_FOLDERS {
        if *folder == parent_name {
            return Some((meta_type, format!("{}.{}", meta_type, stem)));
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
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
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


/// Корни sub-config'ов репо: каталоги, содержащие `Configuration.xml` на
/// глубине ≤ 3 (base/ + extensions/<name>/). base-роуты идут ПЕРВЫМИ — их
/// структура приоритетна при мердже одноимённых реквизитов (см.
/// `ObjectStructure::merge_from`).
pub(crate) fn sub_config_roots(repo_root: &Path) -> Vec<std::path::PathBuf> {
    let mut roots: Vec<std::path::PathBuf> = Vec::new();
    for entry in WalkDir::new(repo_root).max_depth(3).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file()
            && entry.file_name().to_str() == Some("Configuration.xml")
        {
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
        return Some((format!("CommonForms.{}", name), name.to_string()));
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
    let owner_full = format!("{}.{}", meta_type, owner_name);
    Some((owner_full, form_name.to_string()))
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


/// Число строк в таблице, записанное в `sqlite_stat1` на момент последнего
/// `ANALYZE` (первый токен колонки `stat`). `None` — статистики нет (таблицу
/// ни разу не анализировали, либо самой `sqlite_stat1` ещё нет).
pub(crate) fn analyzed_row_count(conn: &rusqlite::Connection, table: &str) -> Option<i64> {
    let stat: Option<String> = conn
        .query_row(
            "SELECT stat FROM sqlite_stat1 WHERE tbl = ?1 LIMIT 1",
            params![table],
            |r| r.get(0),
        )
        .ok();
    stat.and_then(|s| s.split_whitespace().next().and_then(|t| t.parse::<i64>().ok()))
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
/// `sqlite_stat1` перебирает все рёбра вместо seek). Полный `ANALYZE` дёшев
/// (~0.6 с) относительно этой деградации (сек→минуты), но зовём его лишь при
/// реальном дрейфе, а не на каждый батч. `ANALYZE` идёт по всей БД (не только
/// по этим двум таблицам) — это штатно и дёшево.
pub(crate) fn maybe_analyze_graph_tables(conn: &rusqlite::Connection) -> Result<()> {
    let mut need = false;
    for table in ["data_links", "proc_call_graph"] {
        let current: i64 = conn
            .query_row(&format!("SELECT count(*) FROM {}", table), [], |r| r.get(0))
            .unwrap_or(0);
        if stats_drifted(current, analyzed_row_count(conn, table)) {
            need = true;
            break;
        }
    }
    if need {
        let _ = conn.execute("ROLLBACK", []); // ANALYZE не может идти внутри транзакции
        conn.execute("ANALYZE", [])?;
        tracing::info!("ANALYZE: статистика графовых таблиц пересчитана (дрейф ≥1.5×)");
    }
    Ok(())
}
