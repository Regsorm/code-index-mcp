// Реализация `LanguageProcessor::index_extras` для BSL.
//
// Полный обход репо после стандартной индексации, разбор XML-метаданных
// и заполнение трёх таблиц расширения:
//
//  - `metadata_objects` — из Configuration.xml (имена и типы объектов).
//  - `metadata_forms` — из всех `Form.xml` (handlers формы).
//  - `event_subscriptions` — из всех `EventSubscriptions/<Name>.xml`.
//
// Граф вызовов (`proc_call_graph`) подключается отдельно на этапе 4d.
//
// Repo пишется через имя «default». Когда index_extras вызывается из
// `bsl-indexer index <path>` — это offline-команда, без указания alias,
// поэтому используется константа REPO_DEFAULT. Когда мы перейдём на
// демон-режим (этап 4d/8), repo будет приходить из конфига.

use std::path::Path;

use anyhow::Result;
use code_index_core::storage::Storage;
use rusqlite::params;
use walkdir::WalkDir;

use crate::module_constants::{module_type_by_filename, property_id_by_type};
use crate::xml::config_dump_info::parse_config_dump_info;
use crate::xml::configuration::parse_configuration_file;
use crate::xml::event_subscriptions::parse_event_subscription_file;
use crate::xml::forms::parse_form_file;
use crate::code_usages::extract_code_usages;
use crate::xml::metadata_refs::{
    parse_defined_type_targets_file, parse_exchange_plan_content_file,
    parse_functional_option_content_file, parse_functional_option_location_file,
    parse_role_rights_file, parse_subsystem_content_file,
};
use crate::xml::object_attributes::{
    parse_object_attributes_file, parse_object_header_xml, parse_object_structure_file,
    ObjectStructure,
};
use crate::xml::object_uuid::{extract_form_uuid_from_file, extract_object_uuid_from_file};

/// Папки выгрузки → singular meta_type. Объектные XML лежат прямо в этих
/// папках (`Catalogs/<Имя>.xml`). Перечислены только типы со ссылочными
/// реквизитами/измерениями — для остальных (CommonModule, Enum, Constant…)
/// открывать XML смысла нет.
const OBJECT_FOLDERS: &[(&str, &str)] = &[
    ("Catalogs", "Catalog"),
    ("Documents", "Document"),
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
];

/// Repo-key для оффлайн-индексации (через `bsl-indexer index .`).
/// В реальном демоне используется alias из daemon.toml; пока этой
/// связки нет на стороне индексер — пишем как «default».
const REPO_DEFAULT: &str = "default";

/// Запустить полный проход по репо и заполнить специфичные таблицы.
/// Реализация публичная, чтобы её можно было звать из тестов.
pub fn run_index_extras(repo_root: &Path, storage: &mut Storage) -> Result<()> {
    let conn = storage.conn();

    // Каждая фаза независима. Если одна ошибка — пишем warning, идём
    // дальше; одна сломанная подписка не должна валить весь процесс.
    if let Err(e) = index_metadata_objects(repo_root, conn) {
        tracing::warn!("metadata_objects: {}", e);
    }
    // Граф связей данных: ссылочные реквизиты/измерения → рёбра data_links.
    // Открывает XML отдельных объектов (которые остальные проходы не читают).
    if let Err(e) = index_data_links(repo_root, conn) {
        tracing::warn!("data_links: {}", e);
    }
    // Рёбра data_links КОНФИГУРАЦИОННОГО уровня (подсистемы, планы обмена,
    // определяемые типы, расположение ФО). Строго ПОСЛЕ index_data_links —
    // та wipe-ит все рёбра repo и пишет объектные; эта добавляет свои link_kind.
    if let Err(e) = index_metadata_refs(repo_root, conn) {
        tracing::warn!("data_links(config-level): {}", e);
    }
    // Права ролей → отдельная таблица role_rights.
    if let Err(e) = index_role_rights(repo_root, conn) {
        tracing::warn!("role_rights: {}", e);
    }
    // Обратный индекс использований объектов МД в коде (.bsl) → metadata_code_usages.
    if let Err(e) = index_metadata_code_usages(repo_root, conn) {
        tracing::warn!("metadata_code_usages: {}", e);
    }
    // Полная структура объектов (реквизиты+типы, ТЧ, измерения, ресурсы)
    // → metadata_objects.attributes_json. Зависит от строк, созданных
    // index_metadata_objects (выполняется выше), — делает UPDATE по full_name.
    if let Err(e) = index_object_attributes(repo_root, conn) {
        tracing::warn!("object_attributes: {}", e);
    }
    // Синонимы (русские представления) ВСЕХ объектов — отдельный лёгкий проход
    // по корневым XML всех папок типов. Покрывает и объекты без структуры
    // реквизитов (CommonModule/Constant/CommonPicture/FunctionalOption/…),
    // которых нет в OBJECT_FOLDERS. UPDATE по full_name; зависит от строк,
    // созданных index_metadata_objects.
    if let Err(e) = index_object_synonyms(repo_root, conn) {
        tracing::warn!("object_synonyms: {}", e);
    }
    // Механические термы процедур (имя + объект + синоним + комментарий) —
    // после синонимов (использует metadata_objects.synonym) и после core-
    // индексации (читает functions/files). Без LLM, секунды на конфигурацию.
    if let Err(e) = index_procedure_terms(repo_root, conn) {
        tracing::warn!("procedure_terms: {}", e);
    }
    if let Err(e) = index_metadata_forms(repo_root, conn) {
        tracing::warn!("metadata_forms: {}", e);
    }
    if let Err(e) = index_event_subscriptions(repo_root, conn) {
        tracing::warn!("event_subscriptions: {}", e);
    }
    // metadata_modules зависят от UUID объектов (читают XML-файлы напрямую)
    // и от ConfigDumpInfo.xml каждой sub-config. Не зависят от других
    // *_index_extras-функций; порядок не критичен.
    //
    // TODO(инкрементальность, после этапа 8): когда демон будет вызывать
    // index_extras на отдельные file-events (а не полный reindex репо),
    // нужно сделать здесь UPSERT по хешу XML-файла, а не DELETE+INSERT.
    // Сейчас полный пересбор оправдан: после `DumpConfigToFiles` платформа
    // 1С перезаписывает всю выгрузку, включая ConfigDumpInfo.xml.
    if let Err(e) = index_metadata_modules(repo_root, conn) {
        tracing::warn!("metadata_modules: {}", e);
    }
    // Граф вызовов строится ПОСЛЕ заполнения metadata_forms и
    // event_subscriptions — он опирается на их содержимое.
    if let Err(e) = build_call_graph(conn) {
        tracing::warn!("proc_call_graph: {}", e);
    }
    // ANALYZE: без статистики SQLite в рекурсивном шаге find_path_bsl/
    // find_data_path использует лишь префикс индекса (repo=) и сканирует
    // все рёбра repo на каждой итерации (depth=3 ~240с на КА1.1). После
    // ANALYZE планировщик знает селективность (~5 рёбер на caller_proc_key)
    // и берёт seek по двум столбцам → depth=3 падает до ~0.05с. Хинт
    // INDEXED BY это НЕ чинит — решает только статистика. Графы строятся
    // заново при каждом reindex (DELETE+INSERT), поэтому ANALYZE здесь, в
    // конце прохода, освежает статистику синхронно с ними (~0.6с на 2.4ГБ).
    if let Err(e) = conn.execute_batch("ANALYZE;") {
        tracing::warn!("ANALYZE: {}", e);
    }
    Ok(())
}

// ───────────────────────── Инкрементальное обновление ─────────────────────
//
// Slice-rebuild графа вызовов и per-object/per-file апдейт XML-слоёв для
// файлов одного watcher-батча. Семантика идентична полному `run_index_extras`
// (см. тест эквивалентности в конце файла). Новых таблиц/колонок не вводит —
// все slice-функции дедуплицированы так же, как полное построение
// (`build_call_graph`), и `find_path_bsl`/`find_data_path` это не затрагивает.

/// Точечно обновить слой `direct` графа вызовов для ОДНОГО файла.
///
/// proc_call_graph дедуплицирован и не помнит источник ребра, поэтому
/// «прежние» рёбра файла берём из side-таблицы `direct_edge_files`, а
/// «текущие» — из core-таблицы `calls` (её базовый индексатор уже обновил
/// по этому файлу к моменту вызова). Трогаем только рёбра этого файла:
///   1) прежние рёбра файла, которых больше нет ни в одном файле
///      (проверка `calls` — она глобальна и актуальна), удаляем из графа;
///   2) текущие рёбра файла доинсертим (существующие отсекает UNIQUE).
/// Стоимость — O(рёбер одного файла), не зависит от размера графа.
fn update_call_graph_direct_for_file(
    repo_root: &Path,
    conn: &rusqlite::Connection,
    abs_path: &Path,
) -> Result<()> {
    // rel-путь в формате files.path (forward slash, относительно корня репо).
    let rel = abs_path
        .strip_prefix(repo_root)
        .unwrap_or(abs_path)
        .to_string_lossy()
        .replace('\\', "/");

    let _ = conn.execute("ROLLBACK", []);
    conn.execute("BEGIN", [])?;

    // Прежние рёбра файла (из side-карты).
    let old: Vec<(String, String)> = {
        let mut st = conn.prepare(
            "SELECT caller, callee FROM direct_edge_files \
             WHERE repo = ?1 AND source_file = ?2",
        )?;
        let v = st
            .query_map(params![REPO_DEFAULT, &rel], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<(String, String)>>>()?;
        v
    };
    // Текущие рёбра файла (из calls; для удалённого файла — пусто, files-строки нет).
    let new: Vec<(String, String)> = {
        let mut st = conn.prepare(
            "SELECT DISTINCT c.caller, c.callee \
             FROM calls c JOIN files f ON f.id = c.file_id \
             WHERE f.path = ?1 AND c.caller IS NOT NULL AND c.callee IS NOT NULL",
        )?;
        let v = st
            .query_map(params![&rel], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<(String, String)>>>()?;
        v
    };

    // Обновляем side-карту файла: снести прежние записи, записать текущие.
    conn.execute(
        "DELETE FROM direct_edge_files WHERE repo = ?1 AND source_file = ?2",
        params![REPO_DEFAULT, &rel],
    )?;
    {
        let mut ins = conn.prepare(
            "INSERT OR IGNORE INTO direct_edge_files (repo, caller, callee, source_file) \
             VALUES (?1, ?2, ?3, ?4)",
        )?;
        for (caller, callee) in &new {
            ins.execute(params![REPO_DEFAULT, caller, callee, &rel])?;
        }
    }

    use std::collections::HashSet;
    let new_set: HashSet<&(String, String)> = new.iter().collect();

    // Рёбра, которые файл перестал давать → удалить из графа. Ключ теперь
    // привязан к файлу (`<rel>::<caller>`), поэтому ребро принадлежит ровно
    // этому файлу и не делится с другими — удаляем безусловно, как только
    // файл его больше не даёт. Прежняя глобальная проверка по `calls` (нужная
    // для голых ключей, чтобы не снести ребро, которое даёт другой файл) стала
    // не только лишней, но и неверной: при path-привязке она удержала бы
    // мёртвое ребро файла, если одноимённую пару даёт другой модуль.
    {
        let mut del = conn.prepare(
            "DELETE FROM proc_call_graph \
             WHERE repo = ?1 AND call_type = 'direct' \
               AND caller_proc_key = ?2 AND callee_proc_name = ?3",
        )?;
        for e in &old {
            if new_set.contains(e) {
                continue;
            }
            let caller_key = format!("{}::{}", rel, e.0);
            del.execute(params![REPO_DEFAULT, caller_key, &e.1])?;
        }
    }

    // Текущие рёбра файла → в граф (существующие отсекает UNIQUE без записи).
    // caller_proc_key привязан к файлу: `<rel>::<caller>` (как в build_call_graph).
    {
        let mut ins = conn.prepare(
            "INSERT OR IGNORE INTO proc_call_graph \
             (repo, caller_proc_key, callee_proc_name, call_type) \
             VALUES (?1, ?2, ?3, 'direct')",
        )?;
        for (caller, callee) in &new {
            let caller_key = format!("{}::{}", rel, caller);
            ins.execute(params![REPO_DEFAULT, caller_key, callee])?;
        }
    }

    // Этап 4e (инкремент): резолв callee_proc_key для рёбер ЭТОГО файла. Та же
    // логика, что в resolve_direct_callee_keys, но в области одного файла —
    // через точное сравнение пути вызывателя (НЕ LIKE: путь содержит '_', а это
    // wildcard LIKE). Ограничение: правка файла не переразрешает входящие рёбра
    // ДРУГИХ файлов, чья цель — процедура этого файла (напр. при переименовании
    // экспортной процедуры). Эти ключи могут устареть до полного пересбора —
    // приемлемо (демон периодически делает полный rebuild).
    {
        // (а) локальный вызов в том же файле.
        conn.execute(
            "UPDATE proc_call_graph \
             SET callee_proc_key = ?2 || '::' || callee_proc_name \
             WHERE repo = ?1 AND call_type = 'direct' AND callee_proc_key IS NULL \
               AND substr(caller_proc_key, 1, instr(caller_proc_key, '::') - 1) = ?2 \
               AND EXISTS (SELECT 1 FROM functions fn JOIN files fl ON fl.id = fn.file_id \
                           WHERE fl.path = ?2 AND fn.name = proc_call_graph.callee_proc_name)",
            params![REPO_DEFAULT, &rel],
        )?;
        // (б) уникальный экспорт во всей конфигурации.
        conn.execute(
            "UPDATE proc_call_graph \
             SET callee_proc_key = ( \
                 SELECT fl.path || '::' || fn.name FROM functions fn JOIN files fl ON fl.id = fn.file_id \
                 WHERE fn.name = proc_call_graph.callee_proc_name AND fn.args LIKE '%) Экспорт%' LIMIT 1) \
             WHERE repo = ?1 AND call_type = 'direct' AND callee_proc_key IS NULL \
               AND substr(caller_proc_key, 1, instr(caller_proc_key, '::') - 1) = ?2 \
               AND (SELECT COUNT(*) FROM functions fn \
                    WHERE fn.name = proc_call_graph.callee_proc_name AND fn.args LIKE '%) Экспорт%') = 1",
            params![REPO_DEFAULT, &rel],
        )?;
    }

    // (в) квалифицированный вызов общего модуля (склеенный `Модуль.Метод`) —
    // точная привязка по квалификатору, для рёбер этого файла.
    build_common_module_methods(conn)?;
    resolve_callee_keys_by_qualifier(conn, Some(&rel))?;
    conn.execute_batch("DROP TABLE IF EXISTS tmp_pcg_cmeth;")?;

    // (г) менеджер-вызовы `Коллекция.Объект.Метод` для рёбер этого файла.
    resolve_callee_keys_by_manager(conn, Some(&rel))?;

    // Этап 4e-prune (инкремент): отсев платформенного балласта для этого файла.
    prune_platform_balast(conn, Some(&rel))?;
    // + объектные вызовы `Объект.Метод` для рёбер этого файла.
    prune_object_method_calls(conn, Some(&rel))?;

    conn.execute("COMMIT", [])?;
    tracing::debug!(
        "call_graph direct per-file {}: old={} new={}",
        rel,
        old.len(),
        new.len()
    );
    Ok(())
}

/// Пересобрать слой `subscription` графа вызовов из таблицы
/// `event_subscriptions`. Идентично subscription-части `build_call_graph`.
fn rebuild_call_graph_subscription(conn: &rusqlite::Connection) -> Result<()> {
    let _ = conn.execute("ROLLBACK", []);
    conn.execute("BEGIN", [])?;
    conn.execute(
        "DELETE FROM proc_call_graph WHERE repo = ? AND call_type = 'subscription'",
        params![REPO_DEFAULT],
    )?;
    let n = conn.execute(
        "INSERT OR IGNORE INTO proc_call_graph \
         (repo, caller_proc_key, callee_proc_name, call_type) \
         SELECT ?, 'event::' || event, handler_module || '.' || handler_proc, 'subscription' \
         FROM event_subscriptions \
         WHERE repo = ? AND handler_module != '' AND handler_proc != ''",
        params![REPO_DEFAULT, REPO_DEFAULT],
    )?;
    conn.execute("COMMIT", [])?;
    tracing::debug!("proc_call_graph subscription (slice-rebuild): {} рёбер", n);
    Ok(())
}

/// Пересобрать слой `form_event` графа вызовов из таблицы `metadata_forms`.
/// Идентично form_event-части `build_call_graph`.
fn rebuild_call_graph_form_event(conn: &rusqlite::Connection) -> Result<()> {
    let _ = conn.execute("ROLLBACK", []);
    conn.execute("BEGIN", [])?;
    conn.execute(
        "DELETE FROM proc_call_graph WHERE repo = ? AND call_type = 'form_event'",
        params![REPO_DEFAULT],
    )?;
    let rows: Vec<(String, String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT owner_full_name, form_name, handlers_json \
             FROM metadata_forms WHERE repo = ?",
        )?;
        let mapped = stmt
            .query_map(params![REPO_DEFAULT], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        mapped
    };
    let mut form_count = 0usize;
    {
        let mut insert = conn.prepare(
            "INSERT OR IGNORE INTO proc_call_graph \
             (repo, caller_proc_key, callee_proc_name, call_type) \
             VALUES (?, ?, ?, 'form_event')",
        )?;
        for (owner, form_name, handlers_json) in rows {
            let parsed: Vec<serde_json::Value> =
                serde_json::from_str(&handlers_json).unwrap_or_default();
            for h in parsed {
                let event = h.get("event").and_then(|v| v.as_str()).unwrap_or("");
                let handler = h.get("handler").and_then(|v| v.as_str()).unwrap_or("");
                if event.is_empty() || handler.is_empty() {
                    continue;
                }
                let caller_key = format!("form::{}::{}::{}", owner, form_name, event);
                let callee_name = format!("{}::{}::{}", owner, form_name, handler);
                insert.execute(params![REPO_DEFAULT, caller_key, callee_name])?;
                form_count += 1;
            }
        }
    }
    conn.execute("COMMIT", [])?;
    tracing::debug!("proc_call_graph form_event (slice-rebuild): {} рёбер", form_count);
    Ok(())
}

/// По пути к корневому XML объекта определить `(meta_type, full_name)`.
/// Возвращает `None`, если файл не лежит прямо в одной из `OBJECT_FOLDERS`
/// (т.е. это не корневой XML объекта со ссылочными реквизитами/структурой).
fn object_full_name_from_path(xml_path: &Path) -> Option<(&'static str, String)> {
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

/// Per-object обновление `data_links` для одного объекта: удалить его прежние
/// рёбра (`from_object = X`) и переразобрать только его XML. Покрывает и
/// recorder-рёбра (движения документа), т.к. они тоже имеют `from_object`
/// = документ. Если файл удалён — рёбра просто исчезают.
fn update_data_links_for_object(conn: &rusqlite::Connection, xml_path: &Path) -> Result<()> {
    let owner_full = match object_full_name_from_path(xml_path) {
        Some((_mt, full)) => full,
        None => return Ok(()),
    };
    let _ = conn.execute("ROLLBACK", []);
    conn.execute("BEGIN", [])?;
    conn.execute(
        "DELETE FROM data_links WHERE repo = ? AND from_object = ?",
        params![REPO_DEFAULT, &owner_full],
    )?;
    if xml_path.is_file() {
        match parse_object_attributes_file(xml_path, &owner_full) {
            Ok(edges) => {
                let mut stmt = conn.prepare(
                    "INSERT OR IGNORE INTO data_links \
                     (repo, from_object, from_path, to_object, link_kind, is_composite, is_universal) \
                     VALUES (?, ?, ?, ?, ?, ?, ?)",
                )?;
                for edge in edges {
                    stmt.execute(params![
                        REPO_DEFAULT,
                        &owner_full,
                        &edge.from_path,
                        &edge.to_object,
                        edge.link_kind,
                        edge.is_composite as i64,
                        edge.is_universal as i64,
                    ])?;
                }
            }
            Err(e) => tracing::warn!("update_data_links_for_object {}: {}", xml_path.display(), e),
        }
    }
    backfill_data_link_keys(conn)?;
    conn.execute("COMMIT", [])?;
    Ok(())
}

/// Per-object обновление `metadata_objects.attributes_json` для одного объекта.
/// Переразбирает структуру по ВСЕМ sub-config'ам этого объекта (base + копии в
/// расширениях) и пишет СЛИТУЮ структуру (или NULL, если ни в одной sub-config
/// нет непустой структуры). Мердж нужен, чтобы правка XML объекта в одном
/// расширении не затирала базовые реквизиты (см. `ObjectStructure::merge_from`);
/// без него инкремент расходился бы с полным пересбором. Строка объекта должна
/// уже существовать (создаётся `index_metadata_objects`).
fn update_object_attributes_for_object(
    repo_root: &Path,
    conn: &rusqlite::Connection,
    xml_path: &Path,
) -> Result<()> {
    let owner_full = match object_full_name_from_path(xml_path) {
        Some((_mt, full)) => full,
        None => return Ok(()),
    };
    // Папка (plural) и имя объекта — из пути изменённого XML; ищем копии этого
    // объекта во всех sub-config и мерджим (base-first).
    let folder = match xml_path.parent().and_then(|d| d.file_name()).and_then(|s| s.to_str()) {
        Some(s) => s.to_string(),
        None => return Ok(()),
    };
    let stem = match xml_path.file_stem().and_then(|s| s.to_str()) {
        Some(s) => s.to_string(),
        None => return Ok(()),
    };
    let roots = sub_config_roots(repo_root);
    let json_opt =
        merged_object_structure(&roots, &folder, &stem).map(|s| s.to_json().to_string());
    conn.execute(
        "UPDATE metadata_objects SET attributes_json = ? WHERE repo = ? AND full_name = ?",
        params![json_opt, REPO_DEFAULT, &owner_full],
    )?;
    Ok(())
}

/// Per-file обновление строки `metadata_forms` для одной формы по её Form.xml.
/// Слой `form_event` графа пересобирается отдельно (после всех форм батча).
fn update_metadata_forms_for_file(
    repo_root: &Path,
    conn: &rusqlite::Connection,
    form_xml_path: &Path,
) -> Result<()> {
    let (owner_full, form_name) = match decode_form_path(repo_root, form_xml_path) {
        Some(t) => t,
        None => return Ok(()),
    };
    let _ = conn.execute("ROLLBACK", []);
    conn.execute("BEGIN", [])?;
    conn.execute(
        "DELETE FROM metadata_forms WHERE repo = ? AND owner_full_name = ? AND form_name = ?",
        params![REPO_DEFAULT, &owner_full, &form_name],
    )?;
    if form_xml_path.is_file() {
        match parse_form_file(form_xml_path) {
            Ok(handlers) => {
                let handlers_json = serde_json::to_string(
                    &handlers
                        .iter()
                        .map(|h| serde_json::json!({"event": h.event, "handler": h.handler}))
                        .collect::<Vec<_>>(),
                )?;
                conn.execute(
                    "INSERT OR IGNORE INTO metadata_forms \
                     (repo, owner_full_name, form_name, handlers_json) VALUES (?, ?, ?, ?)",
                    params![REPO_DEFAULT, &owner_full, &form_name, &handlers_json],
                )?;
            }
            Err(e) => tracing::warn!("update_metadata_forms_for_file {}: {}", form_xml_path.display(), e),
        }
    }
    conn.execute("COMMIT", [])?;
    Ok(())
}

/// Per-file обновление строки `event_subscriptions` по её XML. Слой
/// `subscription` графа пересобирается отдельно (после всех подписок батча).
fn update_event_subscription_for_file(conn: &rusqlite::Connection, xml_path: &Path) -> Result<()> {
    let in_dir = xml_path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        == Some("EventSubscriptions");
    if !in_dir || xml_path.extension().and_then(|e| e.to_str()) != Some("xml") {
        return Ok(());
    }
    let _ = conn.execute("ROLLBACK", []);
    conn.execute("BEGIN", [])?;
    if xml_path.is_file() {
        match parse_event_subscription_file(xml_path) {
            Ok(Some(sub)) => {
                let sources_json = serde_json::to_string(&sub.sources)?;
                conn.execute(
                    "DELETE FROM event_subscriptions WHERE repo = ? AND name = ?",
                    params![REPO_DEFAULT, &sub.name],
                )?;
                conn.execute(
                    "INSERT OR IGNORE INTO event_subscriptions \
                     (repo, name, event, handler_module, handler_proc, sources_json) \
                     VALUES (?, ?, ?, ?, ?, ?)",
                    params![
                        REPO_DEFAULT,
                        &sub.name,
                        &sub.event,
                        &sub.handler_module,
                        &sub.handler_proc,
                        &sources_json
                    ],
                )?;
            }
            Ok(None) => {}
            Err(e) => tracing::warn!("update_event_subscription_for_file {}: {}", xml_path.display(), e),
        }
    } else {
        // Файл удалён — имя подписки прочитать неоткуда; в выгрузке 1С имя
        // подписки совпадает с именем файла (EventSubscriptions/<Name>.xml),
        // удаляем по stem как приближению.
        if let Some(stem) = xml_path.file_stem().and_then(|s| s.to_str()) {
            conn.execute(
                "DELETE FROM event_subscriptions WHERE repo = ? AND name = ?",
                params![REPO_DEFAULT, stem],
            )?;
        }
    }
    conn.execute("COMMIT", [])?;
    Ok(())
}

/// Инкрементально обновить extras для файлов одного watcher-батча.
///
/// Маршрутизация по типу файла:
///   * `.bsl` → slice-rebuild слоя `direct` из `calls` (без чтения файлов);
///   * объектный XML (в `OBJECT_FOLDERS`) → per-object `data_links` +
///     структура (только этот объект);
///   * `Form.xml` → per-form строка + slice-rebuild слоя `form_event`;
///   * `EventSubscriptions/*.xml` → per-sub строка + slice-rebuild слоя
///     `subscription`.
///
/// Изменение `Configuration.xml` = структурное изменение состава объектов
/// (добавление/удаление/переименование): редкое, приходит большим батчом —
/// для него делаем полный `run_index_extras` (проще и корректнее, чем
/// частично латать состав + attributes_json всех объектов).
///
/// `ANALYZE` здесь не вызываем (в отличие от полного пути): статистика,
/// собранная при initial reindex, остаётся достаточной; ежебатчевый ANALYZE
/// (~0.6 с) убил бы выигрыш. Содержимое таблиц от ANALYZE не зависит, поэтому
/// эквивалентность full↔incremental не нарушается.
pub fn run_incremental_extras(
    repo_root: &Path,
    storage: &mut Storage,
    changed: &[std::path::PathBuf],
    deleted: &[std::path::PathBuf],
) -> Result<()> {
    let mut bsl_paths: Vec<&std::path::PathBuf> = Vec::new();
    let mut config_changed = false;
    let mut object_xmls: Vec<&std::path::PathBuf> = Vec::new();
    let mut form_xmls: Vec<&std::path::PathBuf> = Vec::new();
    let mut sub_xmls: Vec<&std::path::PathBuf> = Vec::new();
    // Источники data_links конфиг-уровня / role_rights изменились в этом батче.
    // Они лежат вне OBJECT_FOLDERS и не привязаны к одному объекту → при
    // попадании дешевле полностью пересобрать соответствующую таблицу.
    let mut refs_dirty = false;
    let mut roles_dirty = false;

    // changed + deleted объединяем: конкретное действие (reinsert vs delete)
    // функции решают по наличию файла на диске.
    for p in changed.iter().chain(deleted.iter()) {
        let fname = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
        let has_comp =
            |name: &str| p.components().any(|c| c.as_os_str().to_str() == Some(name));
        if fname == "Rights.xml" && has_comp("Roles") {
            roles_dirty = true;
        }
        if has_comp("Subsystems")
            || (fname == "Content.xml" && has_comp("ExchangePlans"))
            || has_comp("DefinedTypes")
            || has_comp("FunctionalOptions")
        {
            refs_dirty = true;
        }
        if ext.eq_ignore_ascii_case("bsl") {
            bsl_paths.push(p);
        } else if fname == "Configuration.xml" {
            config_changed = true;
        } else if fname == "Form.xml" {
            form_xmls.push(p);
        } else if p
            .parent()
            .and_then(|d| d.file_name())
            .and_then(|s| s.to_str())
            == Some("EventSubscriptions")
            && ext == "xml"
        {
            sub_xmls.push(p);
        } else if object_full_name_from_path(p).is_some() {
            object_xmls.push(p);
        }
    }

    // Структурное изменение состава объектов → полный пересбор.
    if config_changed {
        return run_index_extras(repo_root, storage);
    }

    let conn = storage.conn();
    for p in &object_xmls {
        update_data_links_for_object(conn, p)?;
        update_object_attributes_for_object(repo_root, conn, p)?;
    }
    for p in &form_xmls {
        update_metadata_forms_for_file(repo_root, conn, p)?;
    }
    for p in &sub_xmls {
        update_event_subscription_for_file(conn, p)?;
    }
    // .bsl — точечный per-file апдейт слоя direct (O(рёбер файла)) + обратного
    // индекса использований объектов МД в коде (metadata_code_usages).
    for p in &bsl_paths {
        update_call_graph_direct_for_file(repo_root, conn, p)?;
        update_code_usages_for_file(repo_root, conn, p)?;
        update_procedure_terms_for_file(repo_root, conn, p)?;
    }
    // Слой extension_override зависит от functions.override_* (обновляется
    // core-индексатором при правке .bsl) — полный пересбор дёшев (один SELECT).
    if !bsl_paths.is_empty() {
        rebuild_call_graph_extension_override(conn)?;
    }
    if !form_xmls.is_empty() {
        rebuild_call_graph_form_event(conn)?;
    }
    if !sub_xmls.is_empty() {
        rebuild_call_graph_subscription(conn)?;
    }
    // Конфиг-уровневые источники: полный пересбор затронутой таблицы. Каждая
    // функция сносит только свои строки (data_links config link_kind / всю
    // role_rights), не трогая объектные рёбра графа данных.
    if refs_dirty {
        index_metadata_refs(repo_root, conn)?;
    }
    if roles_dirty {
        index_role_rights(repo_root, conn)?;
    }
    Ok(())
}

/// Построить граф вызовов из заполненных metadata_forms,
/// event_subscriptions и core-таблицы `calls`. Удаляет старые ребра
/// этого репо и вставляет свежие — идемпотентно.
/// Полный пересбор слоя `extension_override` из `functions.override_*`.
/// Идентично subscription-/form_event-частям `build_call_graph`. Вызывается
/// инкрементально при изменении `.bsl` — override-данные живут в `functions`,
/// которую core-индексатор обновляет на правку модуля расширения.
fn rebuild_call_graph_extension_override(conn: &rusqlite::Connection) -> Result<()> {
    let _ = conn.execute("ROLLBACK", []);
    conn.execute("BEGIN", [])?;
    conn.execute(
        "DELETE FROM proc_call_graph WHERE repo = ? AND call_type = 'extension_override'",
        params![REPO_DEFAULT],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO proc_call_graph \
         (repo, caller_proc_key, callee_proc_name, call_type) \
         SELECT ?, f.override_target, f.name, 'extension_override' \
         FROM functions f \
         WHERE f.override_type IS NOT NULL AND f.override_target IS NOT NULL \
           AND f.override_target != '' AND f.name != ''",
        params![REPO_DEFAULT],
    )?;
    conn.execute("COMMIT", [])?;
    Ok(())
}

fn build_call_graph(conn: &rusqlite::Connection) -> Result<()> {
    let _ = conn.execute("ROLLBACK", []); // защита от cascade-ошибки
    conn.execute("BEGIN", [])?;
    conn.execute(
        "DELETE FROM proc_call_graph WHERE repo = ?",
        params![REPO_DEFAULT],
    )?;

    // ── direct: из core::calls ────────────────────────────────────────
    // Таблица `calls` core содержит ребра «caller имя → callee имя»
    // на уровне исходников. Преобразуем в proc_call_graph с типом
    // `direct`. caller_proc_key — стабильный ключ вызывателя в формате
    // `<rel_path>::<caller>` (через JOIN calls ⋈ files): тот же формат,
    // что у procedure_enrichment.proc_key, что даёт джойн граф↔термы и
    // разводит одноимённые процедуры из разных модулей (две
    // `ОбработкаПроведения` больше не схлопываются в одну строку).
    // callee_proc_name остаётся сырым именем; callee_proc_key (адрес
    // цели) заполняет резолвер на этапе 4e.
    let direct_count = conn.execute(
        "INSERT OR IGNORE INTO proc_call_graph \
         (repo, caller_proc_key, callee_proc_name, call_type) \
         SELECT DISTINCT ?, f.path || '::' || c.caller, c.callee, 'direct' \
         FROM calls c JOIN files f ON f.id = c.file_id \
         WHERE c.caller IS NOT NULL AND c.callee IS NOT NULL",
        params![REPO_DEFAULT],
    )?;

    // Привязка direct-рёбер к файлам (для per-file инкремента). Полный
    // пересбор: очищаем и наполняем заново из calls ⋈ files. proc_call_graph
    // остаётся дедуплицированной — это лишь side-карта «файл → его рёбра».
    conn.execute(
        "DELETE FROM direct_edge_files WHERE repo = ?",
        params![REPO_DEFAULT],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO direct_edge_files (repo, caller, callee, source_file) \
         SELECT DISTINCT ?, c.caller, c.callee, f.path \
         FROM calls c JOIN files f ON f.id = c.file_id \
         WHERE c.caller IS NOT NULL AND c.callee IS NOT NULL",
        params![REPO_DEFAULT],
    )?;

    // ── subscription: event_subscriptions → ребро ────────────────────
    // caller_proc_key для подписок — это «виртуальный триггер» вида
    // `<source>::<event>`, например `cfg:DocumentRef.Реализация::ПриЗаписи`.
    // Это не реальная процедура, а событие платформы — но в графе оно
    // занимает позицию вызывателя. callee — `<handler_module>.<handler_proc>`.
    let subscription_count = conn.execute(
        "INSERT OR IGNORE INTO proc_call_graph \
         (repo, caller_proc_key, callee_proc_name, call_type) \
         SELECT \
            ?, \
            'event::' || event, \
            handler_module || '.' || handler_proc, \
            'subscription' \
         FROM event_subscriptions \
         WHERE repo = ? AND handler_module != '' AND handler_proc != ''",
        params![REPO_DEFAULT, REPO_DEFAULT],
    )?;

    // ── form_event: metadata_forms → ребра ───────────────────────────
    // Каждый `(event, handler)` в handlers_json превращается в ребро.
    // Source — `form::<owner_full_name>::<form_name>::<event>`,
    // callee — `<owner_full_name>::<form_name>::<handler>`. Это
    // не классические module.proc — просто стабильные ключи для графа.
    //
    // SQLite до 3.45 не имеет чистого parsed-JSON для array-iteration,
    // поэтому обрабатываем построчно через rusqlite.
    let mut form_count = 0usize;
    let rows: Vec<(String, String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT owner_full_name, form_name, handlers_json \
             FROM metadata_forms WHERE repo = ?",
        )?;
        let mapped = stmt
            .query_map(params![REPO_DEFAULT], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        mapped
    };

    {
        let mut insert = conn.prepare(
            "INSERT OR IGNORE INTO proc_call_graph \
             (repo, caller_proc_key, callee_proc_name, call_type) \
             VALUES (?, ?, ?, 'form_event')",
        )?;
        for (owner, form_name, handlers_json) in rows {
            let parsed: Vec<serde_json::Value> =
                serde_json::from_str(&handlers_json).unwrap_or_default();
            for h in parsed {
                let event = h.get("event").and_then(|v| v.as_str()).unwrap_or("");
                let handler = h.get("handler").and_then(|v| v.as_str()).unwrap_or("");
                if event.is_empty() || handler.is_empty() {
                    continue;
                }
                let caller_key = format!("form::{}::{}::{}", owner, form_name, event);
                let callee_name = format!("{}::{}::{}", owner, form_name, handler);
                insert.execute(params![REPO_DEFAULT, caller_key, callee_name])?;
                form_count += 1;
            }
        }
    }

    // ── extension_override: перехваты расширений (&Перед/&После/&Вместо) ──
    // Данные уже в functions.override_type/override_target (заполняет парсер
    // bsl::extract_override_info при core-индексации) — отдельный парсер CFE НЕ
    // нужен. Ребро: вызов БАЗОВОГО метода (override_target) достигает
    // реализации-перехватчика (имя функции-перехватчика). По голому имени — как
    // direct-рёбра (общий предел резолва, этап 4e). Так `find_path_bsl` проходит
    // «сквозь &Вместо»: путь до базового метода продолжается в перехватчик.
    let override_count = conn.execute(
        "INSERT OR IGNORE INTO proc_call_graph \
         (repo, caller_proc_key, callee_proc_name, call_type) \
         SELECT ?, f.override_target, f.name, 'extension_override' \
         FROM functions f \
         WHERE f.override_type IS NOT NULL AND f.override_target IS NOT NULL \
           AND f.override_target != '' AND f.name != ''",
        params![REPO_DEFAULT],
    )?;

    // ── этап 4e: резолв адреса цели (callee_proc_key) для direct-рёбер ──
    // Полный пересбор графа → резолвим разом по всему набору рёбер (set-based,
    // через временные таблицы). Инкремент по файлу резолвит свои рёбра сам
    // (см. update_call_graph_direct_for_file).
    resolve_direct_callee_keys(conn)?;

    // ── этап 4e-D: резолв менеджер-вызовов `Коллекция.Объект.Метод` ──
    resolve_callee_keys_by_manager(conn, None)?;

    // ── этап 4e-prune: отсев платформенного балласта ──
    // Рёбра в методы коллекций/объектов и глобальные функции платформы (цель
    // вне кода конфигурации). Только полный набор; инкремент чистит свои рёбра.
    prune_platform_balast(conn, None)?;
    // Объектные вызовы `Объект.Метод` (квалификатор — переменная, не модуль).
    prune_object_method_calls(conn, None)?;

    conn.execute("COMMIT", [])?;

    tracing::info!(
        "proc_call_graph: {} direct + {} subscription + {} form_event + {} extension_override ребер",
        direct_count,
        subscription_count,
        form_count,
        override_count
    );

    // TODO(этап 4f): extension_override — резолв override_target/имени перехватчика
    // в `<rel_path>::<name>` (сейчас голые имена, как direct до 4e).
    // TODO(этап 4g): external_assignment — runtime-анализ переменных
    // неопределённого типа. Опционально, очень дорогая фича.

    Ok(())
}

/// Этап 4e: заполнить `callee_proc_key` для direct-рёбер графа — адрес
/// вызываемой процедуры в формате `<rel_path>::<name>` (тот же, что у
/// `caller_proc_key` и `procedure_enrichment.proc_key`). Две безопасные
/// ступени; всё, что статически не выводится однозначно, остаётся NULL
/// (ложная привязка хуже честного NULL).
///
///   (а) **локальный вызов** — голое имя callee объявлено как процедура в том
///       же файле, что и вызыватель (1С: безымянный вызов разрешается в
///       локальный модуль). Адрес = `<файл вызывателя>::<callee>`.
///   (б) **уникальный экспорт** — имя callee принадлежит ровно одной экспортной
///       процедуре во всей конфигурации. Ядро при разборе вызова теряет
///       квалификатор модуля (`Модуль.Метод` → `Метод`), но единственность
///       цели снимает неоднозначность: любой вызов этого имени ведёт именно
///       туда. Экспортность определяется по ключевому слову `Экспорт` после
///       `)` в сигнатуре (поле `functions.args`; отдельного флага нет).
///
/// Неоднозначные (имя экспортно в ≥2 модулях), динамические (`Объект.Метод`
/// по переменной) и платформенные (`Сообщить`, `СтрНайти` — цель вне кода
/// конфигурации) остаются NULL.
fn resolve_direct_callee_keys(conn: &rusqlite::Connection) -> Result<()> {
    // Карта всех процедур (path, name) — для локального резолва.
    conn.execute_batch(
        "DROP TABLE IF EXISTS tmp_pcg_funcs;
         CREATE TEMP TABLE tmp_pcg_funcs AS
           SELECT fl.path AS path, fn.name AS nm
           FROM functions fn JOIN files fl ON fl.id = fn.file_id
           WHERE fn.name IS NOT NULL AND fn.name != '';
         CREATE INDEX tmp_pcg_funcs_idx ON tmp_pcg_funcs(path, nm);",
    )?;
    // Карта уникальных экспортных имён → путь единственного носителя.
    conn.execute_batch(
        "DROP TABLE IF EXISTS tmp_pcg_uexp;
         CREATE TEMP TABLE tmp_pcg_uexp AS
           SELECT nm, MIN(path) AS path FROM (
             SELECT fn.name AS nm, fl.path AS path
             FROM functions fn JOIN files fl ON fl.id = fn.file_id
             WHERE fn.name IS NOT NULL AND fn.name != '' AND fn.args LIKE '%) Экспорт%'
           ) GROUP BY nm HAVING COUNT(*) = 1;
         CREATE INDEX tmp_pcg_uexp_idx ON tmp_pcg_uexp(nm);",
    )?;

    // (а) локальный вызов: callee объявлен в файле вызывателя.
    conn.execute(
        "UPDATE proc_call_graph \
         SET callee_proc_key = substr(caller_proc_key, 1, instr(caller_proc_key, '::') - 1) \
                               || '::' || callee_proc_name \
         WHERE repo = ?1 AND call_type = 'direct' AND callee_proc_key IS NULL \
           AND EXISTS ( \
             SELECT 1 FROM tmp_pcg_funcs t \
             WHERE t.path = substr(proc_call_graph.caller_proc_key, 1, \
                                   instr(proc_call_graph.caller_proc_key, '::') - 1) \
               AND t.nm = proc_call_graph.callee_proc_name)",
        params![REPO_DEFAULT],
    )?;

    // (б) уникальный экспорт: имя callee экспортно ровно в одном месте.
    conn.execute(
        "UPDATE proc_call_graph \
         SET callee_proc_key = ( \
             SELECT u.path || '::' || u.nm FROM tmp_pcg_uexp u \
             WHERE u.nm = proc_call_graph.callee_proc_name) \
         WHERE repo = ?1 AND call_type = 'direct' AND callee_proc_key IS NULL \
           AND callee_proc_name IN (SELECT nm FROM tmp_pcg_uexp)",
        params![REPO_DEFAULT],
    )?;

    // (в) квалифицированный вызов общего модуля: callee хранится склеенным
    // `Модуль.Метод`; по квалификатору точно находим файл общего модуля и его
    // экспортный метод. Заменяет эвристику уникального экспорта для имён,
    // экспортных в ≥2 модулях. Только вызовы с ОДНОЙ точкой (общий модуль);
    // цепочки `Справочники.X.Метод` (менеджеры) — следующий шаг, остаются NULL.
    build_common_module_methods(conn)?;
    resolve_callee_keys_by_qualifier(conn, None)?;

    conn.execute_batch(
        "DROP TABLE IF EXISTS tmp_pcg_funcs; \
         DROP TABLE IF EXISTS tmp_pcg_uexp; \
         DROP TABLE IF EXISTS tmp_pcg_cmeth;",
    )?;
    Ok(())
}

/// Построить temp-таблицу `tmp_pcg_cmeth` экспортных методов общих модулей:
/// `(mname, method, path)`, где `mname` — имя общего модуля (сегмент пути после
/// `CommonModules/`). Используется Tier C резолва (`resolve_callee_keys_by_qualifier`)
/// и в полном пересборе, и в инкременте.
fn build_common_module_methods(conn: &rusqlite::Connection) -> Result<()> {
    conn.execute_batch(
        "DROP TABLE IF EXISTS tmp_pcg_cmeth;\n\
         CREATE TEMP TABLE tmp_pcg_cmeth AS\n\
           SELECT substr(s.seg, 1, instr(s.seg, '/') - 1) AS mname,\n\
                  fn.name AS method,\n\
                  s.path  AS path\n\
           FROM (SELECT id, path,\n\
                        substr(path, instr(path,'CommonModules/')+length('CommonModules/')) AS seg\n\
                 FROM files\n\
                 WHERE path LIKE '%CommonModules/%/Ext/Module.bsl') s\n\
           JOIN functions fn ON fn.file_id = s.id\n\
           WHERE instr(s.seg, '/') > 0\n\
             AND fn.name IS NOT NULL AND fn.name != ''\n\
             AND fn.args LIKE '%) Экспорт%';\n\
         CREATE INDEX tmp_pcg_cmeth_idx ON tmp_pcg_cmeth(mname, method);",
    )?;
    Ok(())
}

/// Tier C: резолв `callee_proc_key` по квалификатору общего модуля. callee
/// хранится склеенным `Модуль.Метод`; берём часть до точки как имя модуля,
/// после — как метод, и точно адресуем в файл общего модуля. Требует заранее
/// построенной `tmp_pcg_cmeth`. Работает только для вызовов с ОДНОЙ точкой
/// (общий модуль); цепочки `Справочники.X.Метод` пропускаются (остаются NULL).
/// `file_scope = Some(rel)` ограничивает рёбрами одного файла (инкремент).
fn resolve_callee_keys_by_qualifier(
    conn: &rusqlite::Connection,
    file_scope: Option<&str>,
) -> Result<()> {
    let mut sql = String::from(
        "UPDATE proc_call_graph \
         SET callee_proc_key = ( \
             SELECT MIN(cm.path || '::' || cm.method) FROM tmp_pcg_cmeth cm \
             WHERE cm.mname = substr(proc_call_graph.callee_proc_name, 1, instr(proc_call_graph.callee_proc_name,'.')-1) \
               AND cm.method = substr(proc_call_graph.callee_proc_name, instr(proc_call_graph.callee_proc_name,'.')+1)) \
         WHERE repo = ?1 AND call_type = 'direct' AND callee_proc_key IS NULL \
           AND instr(callee_proc_name,'.') > 0 \
           AND instr(substr(callee_proc_name, instr(callee_proc_name,'.')+1), '.') = 0 \
           AND EXISTS ( \
             SELECT 1 FROM tmp_pcg_cmeth cm \
             WHERE cm.mname = substr(proc_call_graph.callee_proc_name, 1, instr(proc_call_graph.callee_proc_name,'.')-1) \
               AND cm.method = substr(proc_call_graph.callee_proc_name, instr(proc_call_graph.callee_proc_name,'.')+1))",
    );
    match file_scope {
        Some(rel) => {
            sql.push_str(" AND substr(caller_proc_key, 1, instr(caller_proc_key, '::') - 1) = ?2");
            conn.execute(&sql, params![REPO_DEFAULT, rel])?;
        }
        None => {
            conn.execute(&sql, params![REPO_DEFAULT])?;
        }
    }
    Ok(())
}

/// Имена-«балласт»: методы коллекций/объектов/запросов/выборок и глобальные
/// функции платформы, чья цель лежит ВНЕ кода конфигурации. Ядро стирает
/// приёмник вызова (`Коллекция.Добавить` → `Добавить`), поэтому такие рёбра
/// ведут «в никуда» (callee_proc_key не резолвится) и составляют ~⅓ графа.
/// Список курируемый и намеренно консервативный: имена методов БСП/общих
/// модулей (`ЗначениеРеквизитаОбъекта`, `ПодсистемаСуществует`,
/// `СообщитьПользователю`, `КодОсновногоЯзыка`…) сюда НЕ входят — они резолвятся
/// в реальные процедуры. Дополнительная страховка от коллизий имён — в
/// `prune_platform_balast` удаляются только рёбра с `callee_proc_key IS NULL`.
const PLATFORM_BALAST: &[&str] = &[
    // методы коллекций / объектов / запросов / выборок (приёмник стёрт ядром)
    "Вставить", "Добавить", "Количество", "Найти", "Выбрать", "Следующий",
    "Получить", "Выгрузить", "ВыгрузитьКолонку", "Записать", "НайтиСтроки",
    "Очистить", "Удалить", "Закрыть", "ПолучитьОбъект", "Прочитать",
    "Установить", "ПолучитьЭлементы", "НайтиПоИдентификатору", "Свойство",
    "Метаданные", "ПолноеИмя", "УникальныйИдентификатор", "ПустаяСсылка",
    "СоздатьНаборЗаписей",
    // глобальные функции / процедуры платформы
    "ЗначениеЗаполнено", "НСтр", "Тип", "ТипЗнч", "Выполнить", "СтрЗаменить",
    "СтрШаблон", "ПодставитьПараметрыВСтроку", "Строка", "СокрЛП",
    "СтрСоединить", "СтрНайти", "СтрДлина", "Лев", "Сред", "Прав", "Формат",
    "ТекущаяДатаСеанса", "ПредопределенноеЗначение", "ОткрытьФорму", "Сообщить",
    "УстановитьПривилегированныйРежим", "ПолучитьФункциональнуюОпцию",
    "ЗаписьЖурналаРегистрации", "НачатьТранзакцию", "ЗафиксироватьТранзакцию",
    "ОтменитьТранзакцию", "ОчиститьСообщения", "ИнформацияОбОшибке",
    "ПодробноеПредставлениеОшибки", "ПоместитьВоВременноеХранилище",
    "ПолучитьИзВременногоХранилища", "ВыполнитьОбработкуОповещения",
    "ОбщийМодуль", "ЗаполнитьЗначенияСвойств", "УстановитьПараметр",
    "ОписаниеОповещения", "ОписаниеТипов", "ПустаяСтрока",
    // конструкторы типов (Новый X — ядро пишет callee = имя типа)
    "Структура", "Массив", "Запрос", "Соответствие", "ТаблицаЗначений",
    "СписокЗначений",
];

/// Удалить direct-рёбра-балласт (см. [`PLATFORM_BALAST`]). Две защиты от потери
/// реальных рёбер: (1) удаляются только рёбра с `callee_proc_key IS NULL` —
/// резолвленные в реальную процедуру сохраняются; (2) имя, экспортное где-либо
/// в конфигурации, не трогается вовсе (адаптивно к УТ/БП/ЗУП). `file_scope=
/// Some(rel)` ограничивает удаление рёбрами одного файла (инкремент), `None` —
/// весь граф (полный пересбор).
fn prune_platform_balast(conn: &rusqlite::Connection, file_scope: Option<&str>) -> Result<()> {
    // Имена — статические кириллические идентификаторы без SQL-метасимволов,
    // поэтому инлайн в IN(...) безопасен (не пользовательский ввод).
    let in_list = PLATFORM_BALAST
        .iter()
        .map(|n| format!("'{}'", n))
        .collect::<Vec<_>>()
        .join(",");
    // Защита от коллизий имён, адаптивная под конфигурацию: НЕ трогаем имя,
    // которое где-либо в конфигурации экспортно (`Записать`/`Удалить`/`Получить`
    // и т.п. могут быть и методом объекта платформы, и реальной экспортной
    // процедурой). Стерев квалификатор, ядро делает их неотличимыми; для
    // экспортных-в-конфиге имён это означало бы потерю реальных рёбер при
    // неоднозначном (NULL) резолве — а потеря хуже шума. Чистая платформа
    // (`Вставить`/`НСтр`/`Структура`…, нигде не экспортна) отсеивается.
    // Имя метода для сопоставления с балластом: callee хранится склеенным
    // (`Объект.Записать`), поэтому берём часть ПОСЛЕ точки (`Записать`); у голых
    // имён (точки нет) — имя целиком. По первой точке — для одноточечных вызовов
    // это и есть метод; многоточечные цепочки в балласт не попадут (не страшно).
    let meth = "substr(callee_proc_name, CASE WHEN instr(callee_proc_name,'.')>0 \
                THEN instr(callee_proc_name,'.')+1 ELSE 1 END)";
    let mut sql = format!(
        "DELETE FROM proc_call_graph \
         WHERE repo = ?1 AND call_type = 'direct' AND callee_proc_key IS NULL \
           AND {meth} IN ({in_list}) \
           AND {meth} NOT IN ( \
             SELECT name FROM functions \
             WHERE name IS NOT NULL AND args LIKE '%) Экспорт%')"
    );
    match file_scope {
        Some(rel) => {
            sql.push_str(" AND substr(caller_proc_key, 1, instr(caller_proc_key, '::') - 1) = ?2");
            conn.execute(&sql, params![REPO_DEFAULT, rel])?;
        }
        None => {
            conn.execute(&sql, params![REPO_DEFAULT])?;
        }
    }
    Ok(())
}

/// Коллекции метаданных 1С — менеджеры, доступные как `Справочники.X`,
/// `Документы.X` и т.п. Одноточечный вызов с таким префиксом — обращение к
/// менеджеру (вызов менеджер-модуля), НЕ метод локального объекта. Прун
/// объектных вызовов их щадит: резолв менеджер-модулей — отдельный шаг.
const METADATA_COLLECTIONS: &[&str] = &[
    "Справочники", "Документы", "ЖурналыДокументов", "Перечисления",
    "Отчеты", "Обработки", "ПланыВидовХарактеристик", "ПланыСчетов",
    "ПланыВидовРасчета", "РегистрыСведений", "РегистрыНакопления",
    "РегистрыБухгалтерии", "РегистрыРасчета", "БизнесПроцессы", "Задачи",
    "ПланыОбмена", "Константы", "Последовательности", "КритерииОтбора",
    "ОпределяемыеТипы",
    // англоязычные эквиваленты (EN-конфигурации)
    "Catalogs", "Documents", "DocumentJournals", "Enums", "Reports",
    "DataProcessors", "ChartsOfCharacteristicTypes", "ChartsOfAccounts",
    "ChartsOfCalculationTypes", "InformationRegisters", "AccumulationRegisters",
    "AccountingRegisters", "CalculationRegisters", "BusinessProcesses",
    "Tasks", "ExchangePlans", "Constants", "Sequences",
];

/// Прун объектных вызовов (CORE B): удалить склеенные ОДНОТОЧЕЧНЫЕ рёбра
/// `Объект.Метод`, где квалификатор — локальная переменная / объект платформы
/// (`Запрос.Выполнить`, `Выборка.Следующий`, `НаборЗаписей.Записать`), цель
/// которых вне кода конфигурации. Квалификатор-driven — точнее списочного
/// балласта: знаем, что приёмник не модуль, поэтому режем даже коллизионные
/// имена методов. ТРИ ЗАЩИТЫ, чтобы не снести реальные вызовы:
///   1) только ОДНА точка — цепочки `Справочники.X.Метод` (менеджеры) не трогаем;
///   2) квалификатор НЕ имя общего модуля (его резолвит Tier C);
///   3) квалификатор НЕ коллекция метаданных (`Справочники`/`Документы`/… —
///      вызовы менеджеров, резолв отложен).
/// Удаляются только рёбра с `callee_proc_key IS NULL`. `file_scope=Some(rel)` —
/// в области одного файла (инкремент).
fn prune_object_method_calls(conn: &rusqlite::Connection, file_scope: Option<&str>) -> Result<()> {
    // tmp_pmods — имена общих модулей (сегмент пути после CommonModules/).
    conn.execute_batch(
        "DROP TABLE IF EXISTS tmp_pmods;\n\
         CREATE TEMP TABLE tmp_pmods AS\n\
           SELECT DISTINCT substr(seg,1,instr(seg,'/')-1) AS q FROM (\n\
             SELECT substr(path, instr(path,'CommonModules/')+length('CommonModules/')) AS seg\n\
             FROM files WHERE path LIKE '%CommonModules/%/Ext/Module.bsl') WHERE instr(seg,'/')>0;\n\
         CREATE INDEX tmp_pmods_idx ON tmp_pmods(q);",
    )?;
    // tmp_pcolls — коллекции метаданных (защита одноточечных менеджер-вызовов).
    conn.execute_batch("DROP TABLE IF EXISTS tmp_pcolls; CREATE TEMP TABLE tmp_pcolls(q TEXT);")?;
    {
        let mut ins = conn.prepare("INSERT INTO tmp_pcolls(q) VALUES (?1)")?;
        for c in METADATA_COLLECTIONS {
            ins.execute(params![c])?;
        }
    }
    conn.execute_batch("CREATE INDEX tmp_pcolls_idx ON tmp_pcolls(q);")?;

    let first = "substr(callee_proc_name, 1, instr(callee_proc_name,'.')-1)";
    let single_dot = "instr(substr(callee_proc_name, instr(callee_proc_name,'.')+1), '.') = 0";
    // (1) ОДНОТОЧЕЧНЫЕ объектные вызовы `Объект.Метод`: первый сегмент НЕ общий
    //     модуль и НЕ коллекция метаданных → это метод локального объекта.
    let mut sql1 = format!(
        "DELETE FROM proc_call_graph \
         WHERE repo = ?1 AND call_type = 'direct' AND callee_proc_key IS NULL \
           AND instr(callee_proc_name,'.') > 0 AND {single_dot} \
           AND {first} NOT IN (SELECT q FROM tmp_pmods) \
           AND {first} NOT IN (SELECT q FROM tmp_pcolls)"
    );
    // (2) МНОГОТОЧЕЧНЫЕ цепочки `X.Y.Метод`, оставшиеся NULL после Tier C/D:
    //     первый сегмент НЕ общий модуль → объектная цепочка (`Запрос.Поле.Метод`)
    //     либо платформенный метод менеджера (`Справочники.Объект.ПустаяСсылка` —
    //     Tier D его уже проверил и не нашёл юзер-экспорт). Цепочки общих модулей
    //     (first = модуль) щадим. Резолвленные менеджер-вызовы тут не NULL.
    let mut sql2 = format!(
        "DELETE FROM proc_call_graph \
         WHERE repo = ?1 AND call_type = 'direct' AND callee_proc_key IS NULL \
           AND instr(substr(callee_proc_name, instr(callee_proc_name,'.')+1), '.') > 0 \
           AND {first} NOT IN (SELECT q FROM tmp_pmods)"
    );
    match file_scope {
        Some(rel) => {
            let f = " AND substr(caller_proc_key, 1, instr(caller_proc_key, '::') - 1) = ?2";
            sql1.push_str(f);
            sql2.push_str(f);
            conn.execute(&sql1, params![REPO_DEFAULT, rel])?;
            conn.execute(&sql2, params![REPO_DEFAULT, rel])?;
        }
        None => {
            conn.execute(&sql1, params![REPO_DEFAULT])?;
            conn.execute(&sql2, params![REPO_DEFAULT])?;
        }
    }
    conn.execute_batch("DROP TABLE IF EXISTS tmp_pmods; DROP TABLE IF EXISTS tmp_pcolls;")?;
    Ok(())
}

/// Построить temp-таблицу `tmp_pcg_mmeth` экспортных методов менеджер-модулей:
/// `(folder, object, method, path)`. folder/object извлекаем из пути
/// `<...>/<Folder>/<Object>/Ext/ManagerModule.bsl` в Rust (в SQLite нет «последнего
/// вхождения» для надёжного разбора двух хвостовых сегментов).
fn build_manager_module_methods(conn: &rusqlite::Connection) -> Result<()> {
    conn.execute_batch(
        "DROP TABLE IF EXISTS tmp_pcg_mmeth; \
         CREATE TEMP TABLE tmp_pcg_mmeth(folder TEXT, object TEXT, method TEXT, path TEXT);",
    )?;
    let rows: Vec<(String, String)> = {
        let mut st = conn.prepare(
            "SELECT fl.path, fn.name FROM functions fn JOIN files fl ON fl.id = fn.file_id \
             WHERE fl.path LIKE '%/Ext/ManagerModule.bsl' \
               AND fn.name IS NOT NULL AND fn.name != '' AND fn.args LIKE '%) Экспорт%'",
        )?;
        let v = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        v
    };
    {
        let mut ins = conn
            .prepare("INSERT INTO tmp_pcg_mmeth(folder, object, method, path) VALUES (?1,?2,?3,?4)")?;
        for (path, method) in &rows {
            if let Some(prefix) = path.strip_suffix("/Ext/ManagerModule.bsl") {
                let mut segs = prefix.rsplit('/');
                if let (Some(object), Some(folder)) = (segs.next(), segs.next()) {
                    ins.execute(params![folder, object, method, path])?;
                }
            }
        }
    }
    conn.execute_batch("CREATE INDEX tmp_pcg_mmeth_idx ON tmp_pcg_mmeth(folder, object, method);")?;
    Ok(())
}

/// Построить temp-таблицу `tmp_pcg_coll` (форма-обращения → папка метаданных) из
/// единой таблицы META_FORMS (`code_usages`). RU и EN формы ведут в одну папку.
fn build_collection_folder_map(conn: &rusqlite::Connection) -> Result<()> {
    conn.execute_batch(
        "DROP TABLE IF EXISTS tmp_pcg_coll; CREATE TEMP TABLE tmp_pcg_coll(coll TEXT, folder TEXT);",
    )?;
    {
        let mut ins = conn.prepare("INSERT INTO tmp_pcg_coll(coll, folder) VALUES (?1,?2)")?;
        for (coll, folder) in crate::code_usages::collection_folder_pairs() {
            ins.execute(params![coll, folder])?;
        }
    }
    conn.execute_batch("CREATE INDEX tmp_pcg_coll_idx ON tmp_pcg_coll(coll);")?;
    Ok(())
}

/// Tier D: резолв менеджер-вызовов `Коллекция.Объект.Метод` (ровно 2 точки).
/// Коллекцию маппим в папку метаданных, ищем экспортный метод в
/// `<Папка>/<Объект>/Ext/ManagerModule.bsl`. Платформенные методы менеджера
/// (`ПустаяСсылка`, `НайтиПоКоду`) не экспортны в модуле → остаются NULL.
/// `file_scope=Some(rel)` — в области одного файла (инкремент).
fn resolve_callee_keys_by_manager(conn: &rusqlite::Connection, file_scope: Option<&str>) -> Result<()> {
    build_manager_module_methods(conn)?;
    build_collection_folder_map(conn)?;
    let col = "proc_call_graph.callee_proc_name";
    let s1 = format!("substr({col},1,instr({col},'.')-1)");
    let rest = format!("substr({col},instr({col},'.')+1)");
    let s2 = format!("substr({rest},1,instr({rest},'.')-1)");
    let s3 = format!("substr({rest},instr({rest},'.')+1)");
    let twodots = format!("(length({col})-length(replace({col},'.','')))=2");
    let join_cond = format!("cc.coll = {s1} AND mm.object = {s2} AND mm.method = {s3}");
    let mut sql = format!(
        "UPDATE proc_call_graph \
         SET callee_proc_key = ( \
             SELECT MIN(mm.path || '::' || mm.method) \
             FROM tmp_pcg_coll cc JOIN tmp_pcg_mmeth mm ON mm.folder = cc.folder \
             WHERE {join_cond}) \
         WHERE repo = ?1 AND call_type = 'direct' AND callee_proc_key IS NULL AND {twodots} \
           AND EXISTS ( \
             SELECT 1 FROM tmp_pcg_coll cc JOIN tmp_pcg_mmeth mm ON mm.folder = cc.folder \
             WHERE {join_cond})"
    );
    match file_scope {
        Some(rel) => {
            sql.push_str(" AND substr(caller_proc_key, 1, instr(caller_proc_key, '::') - 1) = ?2");
            conn.execute(&sql, params![REPO_DEFAULT, rel])?;
        }
        None => {
            conn.execute(&sql, params![REPO_DEFAULT])?;
        }
    }
    conn.execute_batch("DROP TABLE IF EXISTS tmp_pcg_mmeth; DROP TABLE IF EXISTS tmp_pcg_coll;")?;
    Ok(())
}

fn index_metadata_objects(repo_root: &Path, conn: &rusqlite::Connection) -> Result<()> {
    // Сначала собираем все Configuration.xml в репо (multi-config layout):
    //   * <root>/Configuration.xml — классическая выгрузка одной конфигурации;
    //   * <root>/<sub>/Configuration.xml — типичный git-репо с base/ + extensions/<EF_X>/;
    //   * глубина ограничена 3 уровнями (см. processor::detects()).
    //
    // Для каждого Configuration.xml парсим объекты и пишем в общий
    // `metadata_objects` (UNIQUE по `(repo, full_name)`, INSERT OR IGNORE
    // — заимствованные в расширениях объекты с тем же full_name просто
    // пропускаются, в выдаче остаётся base-версия).
    let mut config_paths: Vec<std::path::PathBuf> = Vec::new();
    for entry in WalkDir::new(repo_root).max_depth(3).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file()
            && entry.file_name().to_str() == Some("Configuration.xml")
        {
            config_paths.push(entry.path().to_path_buf());
        }
    }

    if config_paths.is_empty() {
        return Ok(());
    }

    // Защита от cascade-ошибки: если предыдущая функция оставила
    // открытую транзакцию (например, упала между BEGIN и COMMIT),
    // SQLite ругнётся «cannot start a transaction within a transaction».
    // Идемпотентный ROLLBACK закрывает её без ошибок если она была.
    let _ = conn.execute("ROLLBACK", []);
    conn.execute("BEGIN", [])?;
    // Идемпотентность: при повторном run_index_extras очищаем все
    // прежние объекты репо — иначе при удалении расширения старые
    // записи остались бы навсегда.
    conn.execute(
        "DELETE FROM metadata_objects WHERE repo = ?",
        params![REPO_DEFAULT],
    )?;
    let mut stmt = conn.prepare(
        "INSERT OR IGNORE INTO metadata_objects (repo, full_name, meta_type, name) \
         VALUES (?, ?, ?, ?)",
    )?;
    let mut total = 0usize;
    let mut sources: Vec<(String, usize)> = Vec::with_capacity(config_paths.len());
    for cfg_path in &config_paths {
        let objects = match parse_configuration_file(cfg_path) {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!("parse_configuration_file({}): {}", cfg_path.display(), e);
                continue;
            }
        };
        let count_before = total;
        for obj in &objects {
            stmt.execute(params![
                REPO_DEFAULT,
                &obj.full_name,
                &obj.meta_type,
                &obj.name,
            ])?;
            total += 1;
        }
        sources.push((cfg_path.display().to_string(), total - count_before));
    }
    drop(stmt);
    conn.execute("COMMIT", [])?;

    tracing::info!(
        "metadata_objects: записано {} объектов из {} Configuration.xml",
        total,
        config_paths.len(),
    );
    for (src, n) in sources {
        tracing::debug!("  {} → {} объектов", src, n);
    }
    Ok(())
}

/// Заполнить `data_links` — граф связей данных конфигурации.
///
/// Для каждой sub-config обходит папки объектов со ссылочными реквизитами
/// (`OBJECT_FOLDERS`), открывает корневой XML каждого объекта
/// (`Catalogs/<Имя>.xml`) и через `parse_object_attributes_file` извлекает
/// рёбра «объект → объект» по ссылочным типам реквизитов/измерений.
///
/// Полный пересбор (DELETE+INSERT всего репо) — идемпотентно, как остальной
/// `index_extras`. Объём IO невелик (для УТ ~1900 XML / ~68 МБ, ~1-3 сек),
/// поэтому инкрементальность здесь не нужна.
fn index_data_links(repo_root: &Path, conn: &rusqlite::Connection) -> Result<()> {
    // Корни sub-config — родители найденных Configuration.xml.
    let mut sub_roots: Vec<std::path::PathBuf> = Vec::new();
    for entry in WalkDir::new(repo_root).max_depth(3).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file()
            && entry.file_name().to_str() == Some("Configuration.xml")
        {
            if let Some(parent) = entry.path().parent() {
                sub_roots.push(parent.to_path_buf());
            }
        }
    }
    if sub_roots.is_empty() {
        return Ok(());
    }

    let _ = conn.execute("ROLLBACK", []); // защита от cascade-ошибки
    conn.execute("BEGIN", [])?;
    conn.execute("DELETE FROM data_links WHERE repo = ?", params![REPO_DEFAULT])?;
    let mut stmt = conn.prepare(
        "INSERT OR IGNORE INTO data_links \
         (repo, from_object, from_path, to_object, link_kind, is_composite, is_universal) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )?;

    let mut total: usize = 0;
    let mut objects: usize = 0;
    for sub_root in &sub_roots {
        for (folder, meta_type) in OBJECT_FOLDERS {
            let dir = sub_root.join(folder);
            if !dir.is_dir() {
                continue;
            }
            // Только файлы верхнего уровня (Catalogs/<Имя>.xml), не подпапки
            // (Catalogs/<Имя>/Forms/... — это формы, не структура объекта).
            let read = match std::fs::read_dir(&dir) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("data_links: read_dir({}): {}", dir.display(), e);
                    continue;
                }
            };
            for entry in read.filter_map(|e| e.ok()) {
                let path = entry.path();
                if !path.is_file() || path.extension().and_then(|x| x.to_str()) != Some("xml") {
                    continue;
                }
                let stem = match path.file_stem().and_then(|s| s.to_str()) {
                    Some(s) => s,
                    None => continue,
                };
                let owner_full = format!("{}.{}", meta_type, stem);
                let edges = match parse_object_attributes_file(&path, &owner_full) {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::warn!("data_links: {}: {}", path.display(), e);
                        continue;
                    }
                };
                objects += 1;
                for edge in edges {
                    stmt.execute(params![
                        REPO_DEFAULT,
                        &owner_full,
                        &edge.from_path,
                        &edge.to_object,
                        edge.link_kind,
                        edge.is_composite as i64,
                        edge.is_universal as i64,
                    ])?;
                    total += 1;
                }
            }
        }
    }
    drop(stmt);
    backfill_data_link_keys(conn)?;
    conn.execute("COMMIT", [])?;

    tracing::info!(
        "data_links: {} рёбер из {} объектов ({} sub-config)",
        total,
        objects,
        sub_roots.len()
    );
    Ok(())
}

/// Заполнить рёбра `data_links` КОНФИГУРАЦИОННОГО уровня (этап 3.1):
/// `subsystem_content`, `exchange_plan_content`, `defined_type_content`,
/// `functional_option_location`. Источники — отдельные XML, которые
/// `index_data_links` не читает (Subsystems/**, ExchangePlans/<X>/Ext/Content.xml,
/// DefinedTypes/<X>.xml, FunctionalOptions/<X>.xml).
///
/// ВАЖНО: вызывать ПОСЛЕ `index_data_links` — она wipe-ит все рёбра repo и
/// пишет объектные. Эта функция сносит только СВОИ `link_kind` (идемпотентность
/// + корректность инкрементального пути, где `index_data_links` не вызывается).
fn index_metadata_refs(repo_root: &Path, conn: &rusqlite::Connection) -> Result<()> {
    let roots = sub_config_roots(repo_root);
    if roots.is_empty() {
        return Ok(());
    }

    let _ = conn.execute("ROLLBACK", []);
    conn.execute("BEGIN", [])?;
    conn.execute(
        "DELETE FROM data_links WHERE repo = ?1 AND link_kind IN \
         ('subsystem_content','exchange_plan_content','defined_type_content',\
          'functional_option_location','functional_option_content')",
        params![REPO_DEFAULT],
    )?;
    let mut stmt = conn.prepare(
        "INSERT OR IGNORE INTO data_links \
         (repo, from_object, from_path, to_object, link_kind, is_composite, is_universal) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )?;

    let mut total: usize = 0;
    for root in &roots {
        // ── Подсистемы: Subsystems/**.xml ──────────────────────────────────
        // Файл-определение подсистемы лежит прямо в папке "Subsystems"
        // (вложенные — в <Parent>/Subsystems/<Child>.xml). Ext/Forms — пропуск.
        let sub_dir = root.join("Subsystems");
        if sub_dir.is_dir() {
            for entry in WalkDir::new(&sub_dir).into_iter().filter_map(|e| e.ok()) {
                if !entry.file_type().is_file() {
                    continue;
                }
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("xml") {
                    continue;
                }
                if path.parent().and_then(|d| d.file_name()).and_then(|s| s.to_str())
                    != Some("Subsystems")
                {
                    continue;
                }
                let stem = match path.file_stem().and_then(|s| s.to_str()) {
                    Some(s) => s,
                    None => continue,
                };
                let from_object = format!("Subsystem.{}", stem);
                match parse_subsystem_content_file(path) {
                    Ok(items) => {
                        for to_object in items {
                            stmt.execute(params![
                                REPO_DEFAULT,
                                &from_object,
                                "",
                                &to_object,
                                "subsystem_content",
                                0_i64,
                                0_i64
                            ])?;
                            total += 1;
                        }
                    }
                    Err(e) => tracing::warn!("subsystem_content {}: {}", path.display(), e),
                }
            }
        }

        // ── Планы обмена: ExchangePlans/<Имя>/Ext/Content.xml ───────────────
        let ep_dir = root.join("ExchangePlans");
        if ep_dir.is_dir() {
            for entry in WalkDir::new(&ep_dir).into_iter().filter_map(|e| e.ok()) {
                if !entry.file_type().is_file()
                    || entry.file_name().to_str() != Some("Content.xml")
                {
                    continue;
                }
                let path = entry.path();
                // <Имя> = папка на два уровня выше (…/<Имя>/Ext/Content.xml).
                let name = path
                    .parent()
                    .and_then(|ext| ext.parent())
                    .and_then(|d| d.file_name())
                    .and_then(|s| s.to_str());
                let name = match name {
                    Some(n) => n,
                    None => continue,
                };
                let from_object = format!("ExchangePlan.{}", name);
                match parse_exchange_plan_content_file(path) {
                    Ok(items) => {
                        for to_object in items {
                            stmt.execute(params![
                                REPO_DEFAULT,
                                &from_object,
                                "",
                                &to_object,
                                "exchange_plan_content",
                                0_i64,
                                0_i64
                            ])?;
                            total += 1;
                        }
                    }
                    Err(e) => tracing::warn!("exchange_plan_content {}: {}", path.display(), e),
                }
            }
        }

        // ── Определяемые типы: DefinedTypes/<Имя>.xml ───────────────────────
        let dt_dir = root.join("DefinedTypes");
        if dt_dir.is_dir() {
            if let Ok(read) = std::fs::read_dir(&dt_dir) {
                for entry in read.filter_map(|e| e.ok()) {
                    let path = entry.path();
                    if !path.is_file()
                        || path.extension().and_then(|e| e.to_str()) != Some("xml")
                    {
                        continue;
                    }
                    let stem = match path.file_stem().and_then(|s| s.to_str()) {
                        Some(s) => s,
                        None => continue,
                    };
                    let from_object = format!("DefinedType.{}", stem);
                    match parse_defined_type_targets_file(&path) {
                        Ok(targets) => {
                            let is_composite = targets.len() > 1;
                            for (to_object, is_universal) in targets {
                                stmt.execute(params![
                                    REPO_DEFAULT,
                                    &from_object,
                                    "",
                                    &to_object,
                                    "defined_type_content",
                                    is_composite as i64,
                                    is_universal as i64
                                ])?;
                                total += 1;
                            }
                        }
                        Err(e) => tracing::warn!("defined_type_content {}: {}", path.display(), e),
                    }
                }
            }
        }

        // ── Функциональные опции: FunctionalOptions/<Имя>.xml (<Location>) ──
        let fo_dir = root.join("FunctionalOptions");
        if fo_dir.is_dir() {
            if let Ok(read) = std::fs::read_dir(&fo_dir) {
                for entry in read.filter_map(|e| e.ok()) {
                    let path = entry.path();
                    if !path.is_file()
                        || path.extension().and_then(|e| e.to_str()) != Some("xml")
                    {
                        continue;
                    }
                    let stem = match path.file_stem().and_then(|s| s.to_str()) {
                        Some(s) => s,
                        None => continue,
                    };
                    let from_object = format!("FunctionalOption.{}", stem);
                    match parse_functional_option_location_file(&path) {
                        Ok(Some((to_object, raw_location))) => {
                            stmt.execute(params![
                                REPO_DEFAULT,
                                &from_object,
                                &raw_location,
                                &to_object,
                                "functional_option_location",
                                0_i64,
                                0_i64
                            ])?;
                            total += 1;
                        }
                        Ok(None) => {}
                        Err(e) => {
                            tracing::warn!("functional_option_location {}: {}", path.display(), e)
                        }
                    }
                    // W1: состав опции (<Content>) → рёбра functional_option_content
                    // (ФО → включаемый объект/реквизит).
                    match parse_functional_option_content_file(&path) {
                        Ok(items) => {
                            for to_object in items {
                                stmt.execute(params![
                                    REPO_DEFAULT,
                                    &from_object,
                                    "",
                                    &to_object,
                                    "functional_option_content",
                                    0_i64,
                                    0_i64
                                ])?;
                                total += 1;
                            }
                        }
                        Err(e) => {
                            tracing::warn!("functional_option_content {}: {}", path.display(), e)
                        }
                    }
                }
            }
        }
    }
    drop(stmt);
    backfill_data_link_keys(conn)?;
    conn.execute("COMMIT", [])?;

    tracing::info!(
        "data_links(config-level): {} рёбер ({} sub-config)",
        total,
        roots.len()
    );
    Ok(())
}

/// Заполнить `role_rights` из `Roles/<Имя>/Ext/Rights.xml` по всем sub-config.
/// Полный wipe+rebuild одной таблицы — идемпотентно. Хранятся только granted-
/// права (`<value>true</value>`). Имя роли = папка на два уровня выше Rights.xml.
fn index_role_rights(repo_root: &Path, conn: &rusqlite::Connection) -> Result<()> {
    let roots = sub_config_roots(repo_root);
    if roots.is_empty() {
        return Ok(());
    }

    let _ = conn.execute("ROLLBACK", []);
    conn.execute("BEGIN", [])?;
    conn.execute("DELETE FROM role_rights WHERE repo = ?", params![REPO_DEFAULT])?;
    let mut stmt = conn.prepare(
        "INSERT OR IGNORE INTO role_rights (repo, role_name, object_name, right_name) \
         VALUES (?, ?, ?, ?)",
    )?;

    let mut total: usize = 0;
    let mut roles: usize = 0;
    for root in &roots {
        let roles_dir = root.join("Roles");
        if !roles_dir.is_dir() {
            continue;
        }
        for entry in WalkDir::new(&roles_dir).into_iter().filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() || entry.file_name().to_str() != Some("Rights.xml") {
                continue;
            }
            let path = entry.path();
            let role_name = path
                .parent()
                .and_then(|ext| ext.parent())
                .and_then(|d| d.file_name())
                .and_then(|s| s.to_str());
            let role_name = match role_name {
                Some(n) => n,
                None => continue,
            };
            match parse_role_rights_file(path) {
                Ok(rights) => {
                    roles += 1;
                    for r in rights {
                        stmt.execute(params![
                            REPO_DEFAULT,
                            role_name,
                            &r.object_name,
                            &r.right_name
                        ])?;
                        total += 1;
                    }
                }
                Err(e) => tracing::warn!("role_rights {}: {}", path.display(), e),
            }
        }
    }
    drop(stmt);
    backfill_role_right_keys(conn)?;
    conn.execute("COMMIT", [])?;

    tracing::info!(
        "role_rights: {} прав из {} ролей ({} sub-config)",
        total,
        roles,
        roots.len()
    );
    Ok(())
}

/// Заполняет `data_links.to_object_key = lower(to_object)` для строк с пустым
/// ключом. SQLite `lower()` кириллицу не берёт — считаем в Rust. Идемпотентно и
/// инкремент-безопасно: трогает только свежевставленные строки (`to_object_key=''`),
/// уже заполненные пропускает. Вызывать в той же транзакции после INSERT-ов.
fn backfill_data_link_keys(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
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
fn backfill_role_right_keys(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
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
fn rel_path(repo_root: &Path, abs: &Path) -> String {
    abs.strip_prefix(repo_root)
        .unwrap_or(abs)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Заполнить `metadata_code_usages` (этап 3.2): обратный индекс использований
/// объектов МД в коде. Проходит ВСЕ `.bsl` репо, извлекает обращения лёгким
/// regex-слоем (`extract_code_usages`). Полный пересбор (DELETE по repo +
/// INSERT) — идемпотентно. Чтение .bsl с диска (как core-индексатор); файлы не
/// в UTF-8 пропускаются.
fn index_metadata_code_usages(repo_root: &Path, conn: &rusqlite::Connection) -> Result<()> {
    let _ = conn.execute("ROLLBACK", []);
    conn.execute("BEGIN", [])?;
    conn.execute(
        "DELETE FROM metadata_code_usages WHERE repo = ?",
        params![REPO_DEFAULT],
    )?;
    let mut stmt = conn.prepare(
        "INSERT INTO metadata_code_usages \
         (repo, object_ref, object_ref_key, member_path, usage_kind, file_path, line) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )?;

    let mut total: usize = 0;
    let mut files: usize = 0;
    for entry in WalkDir::new(repo_root).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let is_bsl = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("bsl"))
            == Some(true);
        if !is_bsl {
            continue;
        }
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue, // не UTF-8 / нечитаемый — пропуск
        };
        let usages = extract_code_usages(&content);
        if usages.is_empty() {
            continue;
        }
        let rel = rel_path(repo_root, path);
        files += 1;
        for u in usages {
            stmt.execute(params![
                REPO_DEFAULT,
                &u.object_ref,
                &u.object_ref_key,
                &u.member_path,
                u.usage_kind,
                &rel,
                u.line as i64,
            ])?;
            total += 1;
        }
    }
    drop(stmt);
    conn.execute("COMMIT", [])?;

    tracing::info!(
        "metadata_code_usages: {} обращений из {} .bsl",
        total,
        files
    );
    Ok(())
}

/// Per-file обновление `metadata_code_usages` для одного `.bsl`: снести прежние
/// строки файла и переразобрать (или просто снести, если файл удалён).
fn update_code_usages_for_file(
    repo_root: &Path,
    conn: &rusqlite::Connection,
    bsl_path: &Path,
) -> Result<()> {
    let rel = rel_path(repo_root, bsl_path);
    let _ = conn.execute("ROLLBACK", []);
    conn.execute("BEGIN", [])?;
    conn.execute(
        "DELETE FROM metadata_code_usages WHERE repo = ?1 AND file_path = ?2",
        params![REPO_DEFAULT, &rel],
    )?;
    if bsl_path.is_file() {
        if let Ok(content) = std::fs::read_to_string(bsl_path) {
            let usages = extract_code_usages(&content);
            if !usages.is_empty() {
                let mut stmt = conn.prepare(
                    "INSERT INTO metadata_code_usages \
                     (repo, object_ref, object_ref_key, member_path, usage_kind, file_path, line) \
                     VALUES (?, ?, ?, ?, ?, ?, ?)",
                )?;
                for u in usages {
                    stmt.execute(params![
                        REPO_DEFAULT,
                        &u.object_ref,
                        &u.object_ref_key,
                        &u.member_path,
                        u.usage_kind,
                        &rel,
                        u.line as i64,
                    ])?;
                }
            }
        }
    }
    conn.execute("COMMIT", [])?;
    Ok(())
}

/// Корни sub-config'ов репо: каталоги, содержащие `Configuration.xml` на
/// глубине ≤ 3 (base/ + extensions/<name>/). base-роуты идут ПЕРВЫМИ — их
/// структура приоритетна при мердже одноимённых реквизитов (см.
/// `ObjectStructure::merge_from`).
fn sub_config_roots(repo_root: &Path) -> Vec<std::path::PathBuf> {
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
fn merged_object_structure(
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

/// Заполнить `metadata_objects.attributes_json` полной структурой объектов.
///
/// Для КАЖДОГО объекта структура аккумулируется по ВСЕМ sub-config'ам (base +
/// расширения) и мерджится (base-first, см. `ObjectStructure::merge_from`) —
/// иначе последняя обработанная sub-config затирала бы базовую структуру (баг
/// до 0.21.0: тяжёлый документ с 145 реквизитами получал 1 реквизит из
/// расширения). Затем UPDATE строки `metadata_objects` по `full_name` (строки
/// уже созданы `index_metadata_objects`). Объекты без структуры остаются с
/// `attributes_json = NULL`.
fn index_object_attributes(repo_root: &Path, conn: &rusqlite::Connection) -> Result<()> {
    let sub_roots = sub_config_roots(repo_root);
    if sub_roots.is_empty() {
        return Ok(());
    }

    // Аккумулируем структуру каждого объекта по всем sub-config'ам. Каждый XML
    // парсится один раз; merge_from добавляет только новые поля расширений.
    let mut acc: std::collections::HashMap<String, ObjectStructure> =
        std::collections::HashMap::new();
    for sub_root in &sub_roots {
        for (folder, meta_type) in OBJECT_FOLDERS {
            let dir = sub_root.join(folder);
            if !dir.is_dir() {
                continue;
            }
            let read = match std::fs::read_dir(&dir) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("object_attributes: read_dir({}): {}", dir.display(), e);
                    continue;
                }
            };
            for entry in read.filter_map(|e| e.ok()) {
                let path = entry.path();
                if !path.is_file() || path.extension().and_then(|x| x.to_str()) != Some("xml") {
                    continue;
                }
                let stem = match path.file_stem().and_then(|s| s.to_str()) {
                    Some(s) => s,
                    None => continue,
                };
                let structure = match parse_object_structure_file(&path) {
                    Ok(Some(s)) => s,
                    Ok(None) => continue,
                    Err(e) => {
                        tracing::warn!("object_attributes: {}: {}", path.display(), e);
                        continue;
                    }
                };
                if structure.is_empty() {
                    continue;
                }
                let full_name = format!("{}.{}", meta_type, stem);
                match acc.get_mut(&full_name) {
                    Some(existing) => existing.merge_from(&structure),
                    None => {
                        acc.insert(full_name, structure);
                    }
                }
            }
        }
    }

    let _ = conn.execute("ROLLBACK", []); // защита от cascade-ошибки
    conn.execute("BEGIN", [])?;
    let mut stmt = conn.prepare(
        "UPDATE metadata_objects SET attributes_json = ? WHERE repo = ? AND full_name = ?",
    )?;
    let mut filled: usize = 0;
    for (full_name, structure) in &acc {
        if structure.is_empty() {
            continue;
        }
        stmt.execute(params![
            structure.to_json().to_string(),
            REPO_DEFAULT,
            full_name,
        ])?;
        filled += 1;
    }
    drop(stmt);
    conn.execute("COMMIT", [])?;

    tracing::info!(
        "object_attributes: заполнено attributes_json у {} объектов ({} sub-config, base-first merge)",
        filled,
        sub_roots.len()
    );
    Ok(())
}

/// Заполнить `metadata_objects.synonym` для ВСЕХ объектов (вариант B): отдельный
/// лёгкий проход по корневым XML всех папок типов в каждой sub-config. В отличие
/// от `index_object_attributes` (только OBJECT_FOLDERS — объекты со структурой),
/// покрывает и CommonModule/Constant/CommonPicture/FunctionalOption/… Берёт лишь
/// шапку (meta_type/name/synonym) — `parse_object_header_xml` прерывается на
/// `<ChildObjects>`, поэтому дёшев. UPDATE по full_name: записи уже созданы
/// `index_metadata_objects`; для отсутствующих UPDATE — no-op. base-приоритет
/// (sub_roots: base первым → его synonym не перетирается расширением).
fn index_object_synonyms(repo_root: &Path, conn: &rusqlite::Connection) -> Result<()> {
    let sub_roots = sub_config_roots(repo_root);
    if sub_roots.is_empty() {
        return Ok(());
    }
    let mut syn: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for sub_root in &sub_roots {
        let type_dirs = match std::fs::read_dir(sub_root) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for td in type_dirs.filter_map(|e| e.ok()) {
            let tdir = td.path();
            if !tdir.is_dir() {
                continue;
            }
            let files = match std::fs::read_dir(&tdir) {
                Ok(r) => r,
                Err(_) => continue,
            };
            for f in files.filter_map(|e| e.ok()) {
                let p = f.path();
                if !p.is_file() || p.extension().and_then(|x| x.to_str()) != Some("xml") {
                    continue;
                }
                let content = match std::fs::read_to_string(&p) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                if let Some((mt, nm, Some(s))) = parse_object_header_xml(&content) {
                    if !s.is_empty() {
                        syn.entry(format!("{}.{}", mt, nm)).or_insert(s);
                    }
                }
            }
        }
    }

    let _ = conn.execute("ROLLBACK", []); // защита от cascade-ошибки
    conn.execute("BEGIN", [])?;
    let mut stmt = conn.prepare(
        "UPDATE metadata_objects SET synonym = ? WHERE repo = ? AND full_name = ?",
    )?;
    let mut filled = 0usize;
    for (full_name, synonym) in &syn {
        filled += stmt.execute(params![synonym, REPO_DEFAULT, full_name])?;
    }
    drop(stmt);
    conn.execute("COMMIT", [])?;

    tracing::info!("object_synonyms: заполнен synonym у {} объектов", filled);
    Ok(())
}

/// Полный проход механического обогащения термов (без LLM): для каждой
/// процедуры из `functions` собрать `terms` (слова имени + слова объекта +
/// синоним объекта + комментарий над процедурой) и записать в
/// `procedure_enrichment` с подписью `mech:v1`. Строки с ДРУГОЙ подписью
/// (LLM-enrich) не трогаются: свои строки предварительно сносятся, вставка —
/// `ON CONFLICT DO NOTHING`. Комментарии читаются с диска (один read на файл,
/// файлы сгруппированы по пути). См. `crate::terms`.
fn index_procedure_terms(repo_root: &Path, conn: &rusqlite::Connection) -> Result<()> {
    use crate::terms::{
        build_terms, extract_leading_comment, object_from_module_path, MECH_SIGNATURE,
    };

    // Синонимы объектов: full_name → synonym (один SELECT на репо).
    let mut syn: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT full_name, synonym FROM metadata_objects \
             WHERE repo = ?1 AND synonym IS NOT NULL AND synonym != ''",
        )?;
        let rows = stmt.query_map(params![REPO_DEFAULT], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        for row in rows.flatten() {
            syn.insert(row.0, row.1);
        }
    }

    // Все BSL-процедуры, сгруппированные по файлу (ORDER BY path).
    let procs: Vec<(String, String, i64)> = {
        let mut stmt = conn.prepare(
            "SELECT fl.path, f.name, COALESCE(f.line_start, 0) FROM functions f \
             JOIN files fl ON fl.id = f.file_id \
             WHERE fl.path LIKE '%.bsl' ORDER BY fl.path, f.line_start",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
        })?;
        rows.flatten().collect()
    };

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let _ = conn.execute("ROLLBACK", []); // защита от cascade-ошибки
    conn.execute("BEGIN", [])?;
    conn.execute(
        "DELETE FROM procedure_enrichment WHERE repo = ?1 AND signature LIKE 'mech:%'",
        params![REPO_DEFAULT],
    )?;
    let mut ins = conn.prepare(
        "INSERT INTO procedure_enrichment (repo, proc_key, terms, signature, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(repo, proc_key) DO NOTHING",
    )?;

    let mut cur_path = String::new();
    let mut lines: Vec<String> = Vec::new();
    let mut filled = 0usize;
    for (path, name, line_start) in &procs {
        if *path != cur_path {
            cur_path = path.clone();
            lines = std::fs::read_to_string(repo_root.join(path.replace('\\', "/")))
                .map(|c| c.lines().map(String::from).collect())
                .unwrap_or_default();
        }
        let comment = extract_leading_comment(&lines, (*line_start).max(0) as usize);
        let object = object_from_module_path(path);
        let synonym = object
            .as_ref()
            .and_then(|(mt, nm)| syn.get(&format!("{}.{}", mt, nm)))
            .map(String::as_str);
        let terms = build_terms(
            name,
            object.as_ref().map(|(_, nm)| nm.as_str()),
            synonym,
            comment.as_deref(),
        );
        if terms.is_empty() {
            continue;
        }
        let proc_key = format!("{}::{}", path, name);
        filled += ins.execute(params![REPO_DEFAULT, proc_key, terms, MECH_SIGNATURE, now])?;
    }
    drop(ins);
    conn.execute("COMMIT", [])?;

    tracing::info!("procedure_terms: механически обогащено {} процедур", filled);
    Ok(())
}

/// Per-file обновление механических термов для одного `.bsl`: снести свои
/// (`mech:%`) строки файла и пересобрать по текущему состоянию `functions`
/// (или просто снести, если файл удалён). LLM-строки не трогаются.
fn update_procedure_terms_for_file(
    repo_root: &Path,
    conn: &rusqlite::Connection,
    bsl_path: &Path,
) -> Result<()> {
    use crate::terms::{
        build_terms, extract_leading_comment, object_from_module_path, MECH_SIGNATURE,
    };

    let rel = rel_path(repo_root, bsl_path);
    let _ = conn.execute("ROLLBACK", []);
    conn.execute("BEGIN", [])?;
    conn.execute(
        "DELETE FROM procedure_enrichment \
         WHERE repo = ?1 AND signature LIKE 'mech:%' AND proc_key LIKE ?2 || '::%'",
        params![REPO_DEFAULT, &rel],
    )?;
    if bsl_path.is_file() {
        let procs: Vec<(String, i64)> = {
            let mut stmt = conn.prepare(
                "SELECT f.name, COALESCE(f.line_start, 0) FROM functions f \
                 JOIN files fl ON fl.id = f.file_id WHERE fl.path = ?1",
            )?;
            let rows = stmt
                .query_map(params![&rel], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
            rows.flatten().collect()
        };
        if !procs.is_empty() {
            let lines: Vec<String> = std::fs::read_to_string(bsl_path)
                .map(|c| c.lines().map(String::from).collect())
                .unwrap_or_default();
            let object = object_from_module_path(&rel);
            let synonym: Option<String> = object.as_ref().and_then(|(mt, nm)| {
                conn.query_row(
                    "SELECT synonym FROM metadata_objects WHERE repo = ?1 AND full_name = ?2",
                    params![REPO_DEFAULT, format!("{}.{}", mt, nm)],
                    |r| r.get::<_, Option<String>>(0),
                )
                .ok()
                .flatten()
            });
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let mut ins = conn.prepare(
                "INSERT INTO procedure_enrichment (repo, proc_key, terms, signature, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(repo, proc_key) DO NOTHING",
            )?;
            for (name, line_start) in &procs {
                let comment = extract_leading_comment(&lines, (*line_start).max(0) as usize);
                let terms = build_terms(
                    name,
                    object.as_ref().map(|(_, nm)| nm.as_str()),
                    synonym.as_deref(),
                    comment.as_deref(),
                );
                if terms.is_empty() {
                    continue;
                }
                let proc_key = format!("{}::{}", rel, name);
                ins.execute(params![REPO_DEFAULT, proc_key, terms, MECH_SIGNATURE, now])?;
            }
        }
    }
    conn.execute("COMMIT", [])?;
    Ok(())
}

fn index_metadata_forms(repo_root: &Path, conn: &rusqlite::Connection) -> Result<()> {
    // Ищем `Form.xml` в любом дочернем `Forms/<Name>/[Ext/]Form.xml`.
    // Имя владельца восстанавливается из пути: ищем сегмент под
    // `Forms/`, значит путь выглядит как `<...>/<MetaType>/<OwnerName>/Forms/<FormName>/...Form.xml`.
    let mut count = 0usize;
    let _ = conn.execute("ROLLBACK", []); // защита от cascade-ошибки
    conn.execute("BEGIN", [])?;
    conn.execute(
        "DELETE FROM metadata_forms WHERE repo = ?",
        params![REPO_DEFAULT],
    )?;
    let mut stmt = conn.prepare(
        // INSERT OR IGNORE — заимствованные формы (одинаковый owner+form_name
        // в base/ и в extensions/<EF_X>/) дают UNIQUE-конфликт; считаем
        // что приоритет за первой записью (обычно base, поскольку
        // multi-config обход начинается от корня и base/ обычно идёт раньше).
        "INSERT OR IGNORE INTO metadata_forms (repo, owner_full_name, form_name, handlers_json) \
         VALUES (?, ?, ?, ?)",
    )?;

    for entry in WalkDir::new(repo_root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let path = entry.path();
        let file_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if file_name != "Form.xml" {
            continue;
        }
        // Path: .../<MetaType>/<OwnerName>/Forms/<FormName>/[Ext/]Form.xml
        let (owner_full, form_name) = match decode_form_path(repo_root, path) {
            Some(t) => t,
            None => continue,
        };
        let handlers = match parse_form_file(path) {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!("parse_form_file({}): {}", path.display(), e);
                continue;
            }
        };
        let handlers_json = serde_json::to_string(&handlers
            .iter()
            .map(|h| serde_json::json!({"event": h.event, "handler": h.handler}))
            .collect::<Vec<_>>())?;
        stmt.execute(params![
            REPO_DEFAULT,
            &owner_full,
            &form_name,
            &handlers_json,
        ])?;
        count += 1;
    }
    drop(stmt);
    conn.execute("COMMIT", [])?;

    tracing::info!("metadata_forms: проиндексировано {} форм", count);
    Ok(())
}

/// Извлечь (`owner_full_name`, `form_name`) из пути к Form.xml.
/// Возвращает None, если структура каталогов не похожа на выгрузку 1С.
fn decode_form_path(repo_root: &Path, form_xml_path: &Path) -> Option<(String, String)> {
    // Берём отрезок пути относительно корня репо и разбираем сегменты.
    let rel = form_xml_path.strip_prefix(repo_root).ok()?;
    let segments: Vec<&str> = rel
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();

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

fn index_event_subscriptions(repo_root: &Path, conn: &rusqlite::Connection) -> Result<()> {
    // Подписки на события могут быть в нескольких sub-config'ах
    // (base/EventSubscriptions/, extensions/<EF_X>/EventSubscriptions/...).
    // Обходим всё дерево рекурсивно (max_depth защищает от случайных
    // глубоко вложенных fixture-файлов, как и в index_metadata_objects).
    let mut count = 0usize;
    let _ = conn.execute("ROLLBACK", []); // защита от cascade-ошибки
    conn.execute("BEGIN", [])?;
    conn.execute(
        "DELETE FROM event_subscriptions WHERE repo = ?",
        params![REPO_DEFAULT],
    )?;
    let mut stmt = conn.prepare(
        "INSERT OR IGNORE INTO event_subscriptions (repo, name, event, handler_module, handler_proc, sources_json) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )?;

    for entry in WalkDir::new(repo_root)
        .max_depth(4) // root/<sub>/EventSubscriptions/<file>.xml = depth 3, +запас
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if !entry.file_type().is_file() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("xml") {
            continue;
        }
        // Должен лежать внутри директории `EventSubscriptions/`.
        let in_event_subs_dir = path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            == Some("EventSubscriptions");
        if !in_event_subs_dir {
            continue;
        }
        match parse_event_subscription_file(path) {
            Ok(Some(sub)) => {
                let sources_json = serde_json::to_string(&sub.sources)?;
                stmt.execute(params![
                    REPO_DEFAULT,
                    &sub.name,
                    &sub.event,
                    &sub.handler_module,
                    &sub.handler_proc,
                    &sources_json,
                ])?;
                count += 1;
            }
            Ok(None) => {}
            Err(e) => tracing::warn!("parse_event_subscription_file({}): {}", path.display(), e),
        }
    }
    drop(stmt);
    conn.execute("COMMIT", [])?;

    tracing::info!("event_subscriptions: проиндексировано {} подписок", count);
    Ok(())
}

/// Заполнить `metadata_modules` — таблицу с UUID/property_id/configVersion
/// каждого BSL-модуля, нужную для отладки через dbgs.
///
/// Алгоритм:
///   1. Найти все Configuration.xml в репо (multi-config layout).
///   2. Для каждой sub-config:
///      * extension_name = относительный путь от repo_root до родителя
///        Configuration.xml (например `extensions/EF_X`); пустая строка для
///        классической single-config-выгрузки и для `base/`.
///      * config_versions = parse_config_dump_info(<sub-root>) → uuid → ver.
///      * Обходим .bsl-файлы под этой sub-root, классифицируем тип модуля
///        по имени файла + сегментам пути, находим XML-владельца, извлекаем
///        его UUID и записываем тройку `(object_id, property_id, config_version)`.
fn index_metadata_modules(repo_root: &Path, conn: &rusqlite::Connection) -> Result<()> {
    // Находим все Configuration.xml — каждая определяет область sub-config.
    let mut sub_configs: Vec<std::path::PathBuf> = Vec::new();
    for entry in WalkDir::new(repo_root).max_depth(3).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file()
            && entry.file_name().to_str() == Some("Configuration.xml")
        {
            if let Some(parent) = entry.path().parent() {
                sub_configs.push(parent.to_path_buf());
            }
        }
    }
    if sub_configs.is_empty() {
        return Ok(());
    }

    let _ = conn.execute("ROLLBACK", []); // защита от cascade-ошибки
    conn.execute("BEGIN", [])?;
    conn.execute(
        "DELETE FROM metadata_modules WHERE repo = ?",
        params![REPO_DEFAULT],
    )?;
    let mut stmt = conn.prepare(
        "INSERT OR IGNORE INTO metadata_modules \
         (repo, full_name, object_name, module_type, object_id, property_id, \
          config_version, code_path, extension_name) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )?;

    let mut total: usize = 0;
    let mut skipped_no_uuid: usize = 0;

    for sub_root in &sub_configs {
        let extension_name = compute_extension_name(repo_root, sub_root);
        let config_versions =
            parse_config_dump_info(sub_root).unwrap_or_default();

        for entry in WalkDir::new(sub_root).into_iter().filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let file_name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n,
                None => continue,
            };
            // Берём только .bsl файлы с известными именами модулей.
            let module_type = match module_type_by_filename(file_name) {
                Some(t) => t,
                None => continue,
            };
            // Особый случай: Module.bsl в Forms/<...>/Ext/Form/Module.bsl —
            // это FormModule, а не CommonModule.
            let (effective_type, owner_xml_kind) = classify_module(path, module_type);
            let property_id = match property_id_by_type(effective_type) {
                Some(p) => p,
                None => continue,
            };

            let owner_info = match owner_xml_kind {
                OwnerKind::Form => find_form_owner(path),
                OwnerKind::Object => find_object_owner(path),
            };
            let (owner_xml_path, object_name) = match owner_info {
                Some(t) => t,
                None => continue,
            };
            // UUID берём из XML владельца. Для форм — uuid формы (атрибут
            // на корне Form), для объектов — uuid дочернего тега MetaDataObject.
            let uuid_opt = match owner_xml_kind {
                OwnerKind::Form => extract_form_uuid_from_file(&owner_xml_path).ok().flatten(),
                OwnerKind::Object => {
                    extract_object_uuid_from_file(&owner_xml_path).ok().flatten()
                }
            };
            let object_id = match uuid_opt {
                Some(u) if !u.is_empty() => u,
                _ => {
                    skipped_no_uuid += 1;
                    continue;
                }
            };
            let config_version = config_versions.get(&object_id).cloned();

            let full_name = format!("{}.{}", object_name, effective_type);
            let code_path_rel = path
                .strip_prefix(repo_root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");

            stmt.execute(params![
                REPO_DEFAULT,
                &full_name,
                &object_name,
                effective_type,
                &object_id,
                property_id,
                config_version.as_deref(),
                &code_path_rel,
                &extension_name,
            ])?;
            total += 1;
        }
    }
    drop(stmt);
    conn.execute("COMMIT", [])?;

    tracing::info!(
        "metadata_modules: записано {} модулей из {} sub-configs (без UUID пропущено: {})",
        total,
        sub_configs.len(),
        skipped_no_uuid,
    );
    Ok(())
}

/// `extension_name` для записи в `metadata_modules` — относительный путь
/// от корня репо до sub-config. Пустая строка для случая когда
/// Configuration.xml лежит в самом корне (single-config выгрузка) или
/// для `base/` (рассматриваем base как «не-расширение», чтобы агенты
/// фильтровали отдельно `extension_name = ''` для основного).
fn compute_extension_name(repo_root: &Path, sub_root: &Path) -> String {
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

/// Что искать как XML-владелец .bsl-файла модуля.
#[derive(Debug, Clone, Copy)]
enum OwnerKind {
    /// Форма: рядом с .bsl лежит Form.xml (его uuid — атрибут корня <Form>).
    Form,
    /// Обычный объект: на 1 уровень выше Ext-папки модуль/в самой папке
    /// объекта лежит `<Имя>.xml` с дочерним <Document/Catalog/.../> uuid="…".
    Object,
}

/// Уточнить тип модуля и определить как искать владельца.
/// Особый случай: Module.bsl внутри `Forms/<X>/Ext/Form/Module.bsl` — это
/// FormModule, а не CommonModule.Module.
fn classify_module(bsl_path: &Path, raw_type: &'static str) -> (&'static str, OwnerKind) {
    if raw_type == "Module" && path_has_segment(bsl_path, "Forms") {
        return ("FormModule", OwnerKind::Form);
    }
    // CommandModule в `<Object>/Commands/<CmdName>/Ext/CommandModule.bsl` —
    // владелец = Commands/<CmdName>.xml. Не реализуем сейчас, фолбэк ниже —
    // owner = ближайший XML «вверху». Большинство CommandModule всё равно
    // отработают через find_object_owner.
    (raw_type, OwnerKind::Object)
}

fn path_has_segment(p: &Path, segment: &str) -> bool {
    p.components().any(|c| match c {
        std::path::Component::Normal(s) => s.to_str() == Some(segment),
        _ => false,
    })
}

/// Найти Form.xml для модуля формы.
/// Layout: `<...>/Forms/<FormName>/[Ext/]Form/Module.bsl`
/// → искать `<...>/Forms/<FormName>/[Ext/]Form.xml`.
/// Возвращает (путь к Form.xml, owner_full_name = "<MetaType>.<OwnerName>.Form.<FormName>").
fn find_form_owner(bsl_path: &Path) -> Option<(std::path::PathBuf, String)> {
    let segments: Vec<&str> = bsl_path
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();
    let forms_idx = segments.iter().rposition(|s| *s == "Forms")?;
    if forms_idx + 1 >= segments.len() || forms_idx < 2 {
        return None;
    }
    let form_name = segments[forms_idx + 1];
    let owner_name = segments[forms_idx - 1];
    let meta_type = segments[forms_idx - 2];
    // Form.xml в директории формы. Пробуем оба варианта layout: с `Ext/` и без.
    let mut form_dir = bsl_path.to_path_buf();
    while let Some(parent) = form_dir.parent() {
        form_dir = parent.to_path_buf();
        // Дошли до папки с именем формы — в ней Form.xml (с `Ext/Form.xml`
        // или прямо `Form.xml`).
        if form_dir
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s == form_name)
            .unwrap_or(false)
        {
            break;
        }
    }
    let candidates = [form_dir.join("Ext").join("Form.xml"), form_dir.join("Form.xml")];
    let xml_path = candidates.into_iter().find(|p| p.is_file())?;
    let owner_full = format!("{}.{}.Form.{}", meta_type, owner_name, form_name);
    Some((xml_path, owner_full))
}

/// Найти XML-файл владельца для не-form модуля.
/// Layout: `<...>/<MetaType>/<OwnerName>/[Ext/]<ModuleFile>.bsl`
/// → искать `<...>/<MetaType>/<OwnerName>.xml`.
/// Возвращает (путь к XML, owner_full_name = "<MetaType>.<OwnerName>").
fn find_object_owner(bsl_path: &Path) -> Option<(std::path::PathBuf, String)> {
    let segments: Vec<&str> = bsl_path
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();

    // Ищем папку объекта: путь имеет вид .../MetaType/OwnerName/[Ext/]filename.bsl
    // → сегмент с именем .bsl-файла последний; снимаем 1 (или 2 если есть Ext) уровень
    // и берём имя папки = OwnerName, выше — MetaType.
    if segments.len() < 3 {
        return None;
    }
    // Снимаем filename.bsl
    let mut up = segments.len() - 1;
    // Возможно есть `/Ext/` — снимаем и его.
    if up > 0 && segments[up - 1] == "Ext" {
        up -= 1;
    }
    if up < 2 {
        return None;
    }
    let owner_name = segments[up - 1];
    let meta_type = segments[up - 2];

    // Конструируем путь до XML: до OwnerName + ".xml" в папке MetaType.
    let mut xml = bsl_path.to_path_buf();
    // Поднимаемся пока имя текущей папки не станет owner_name.
    while let Some(parent) = xml.parent() {
        xml = parent.to_path_buf();
        if xml
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s == owner_name)
            .unwrap_or(false)
        {
            break;
        }
    }
    // xml = .../MetaType/OwnerName, его сосед = .../MetaType/OwnerName.xml
    let owner_xml = xml.with_extension("xml");
    if !owner_xml.is_file() {
        return None;
    }
    let owner_full = format!("{}.{}", meta_type, owner_name);
    Some((owner_xml, owner_full))
}

#[cfg(test)]
mod tests {
    use super::*;
    use code_index_core::storage::Storage;
    use std::io::Write;
    use tempfile::TempDir;

    fn fresh_storage(tmp: &TempDir) -> Storage {
        let db_path = tmp.path().join("index.db");
        let storage = Storage::open_file(&db_path).unwrap();
        storage.apply_schema_extensions(crate::schema::SCHEMA_EXTENSIONS).unwrap();
        storage
    }

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::File::create(path)
            .unwrap()
            .write_all(content.as_bytes())
            .unwrap();
    }

    #[test]
    fn fills_metadata_objects_from_configuration_xml() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        write(
            &repo.join("Configuration.xml"),
            r#"<?xml version="1.0"?>
<MetaDataObject>
  <Configuration>
    <ChildObjects>
      <Catalog>Контрагенты</Catalog>
      <Document>РеализацияТоваровУслуг</Document>
    </ChildObjects>
  </Configuration>
</MetaDataObject>"#,
        );

        let mut storage = fresh_storage(&tmp);
        run_index_extras(&repo, &mut storage).unwrap();
        let conn = storage.conn();

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM metadata_objects WHERE repo = ?",
                params![REPO_DEFAULT],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn idempotent_repeated_runs_dont_dupe() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        write(
            &repo.join("Configuration.xml"),
            r#"<?xml version="1.0"?>
<MetaDataObject><Configuration><ChildObjects>
  <Catalog>X</Catalog>
</ChildObjects></Configuration></MetaDataObject>"#,
        );

        let mut storage = fresh_storage(&tmp);
        run_index_extras(&repo, &mut storage).unwrap();
        run_index_extras(&repo, &mut storage).unwrap();
        run_index_extras(&repo, &mut storage).unwrap();

        let count: i64 = storage
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM metadata_objects WHERE repo = ?",
                params![REPO_DEFAULT],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "повторный run не должен плодить дубликаты");
    }

    #[test]
    fn extras_present_requires_meta_and_terms() {
        use crate::processor::BslLanguageProcessor;
        use code_index_core::extension::processor::LanguageProcessor;

        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        write(
            &repo.join("Configuration.xml"),
            r#"<?xml version="1.0"?>
<MetaDataObject><Configuration><ChildObjects><Catalog>X</Catalog></ChildObjects></Configuration></MetaDataObject>"#,
        );
        let mut storage = fresh_storage(&tmp);
        let proc = BslLanguageProcessor::new();

        // 1. Свежая БД — extras пусты → false (демон сделает полный проход).
        assert!(!proc.extras_present(&storage), "пустые extras → false");

        // 2. metadata_objects наполнено, но .bsl нет → terms пусты → всё ещё false
        //    (гейт требует ОБЕ ключевые таблицы непустыми).
        run_index_extras(&repo, &mut storage).unwrap();
        assert!(
            !proc.extras_present(&storage),
            "metadata без механических terms → false"
        );

        // 3. Добавили механический терм → обе таблицы непусты → true
        //    (рестарт демона при неизменных данных может пропустить пересбор).
        storage
            .conn()
            .execute(
                "INSERT INTO procedure_enrichment (repo, proc_key, terms, signature, updated_at) \
                 VALUES (?1, 'X.bsl::П', 'термин', 'mech:v1', 0)",
                params![REPO_DEFAULT],
            )
            .unwrap();
        assert!(
            proc.extras_present(&storage),
            "metadata_objects + mech-terms непусты → true"
        );
    }

    /// Мини-репо для тестов механических термов: общий модуль с синонимом
    /// и процедурой с комментарием. files/functions заполняются вручную
    /// (как будто core-парсер уже отработал — extras его не запускают).
    fn write_terms_fixture(repo: &Path, storage: &Storage) {
        write(
            &repo.join("Configuration.xml"),
            r#"<?xml version="1.0"?>
<MetaDataObject><Configuration><ChildObjects>
  <CommonModule>РаботаСоШтрихкодами</CommonModule>
</ChildObjects></Configuration></MetaDataObject>"#,
        );
        write(
            &repo.join("CommonModules").join("РаботаСоШтрихкодами.xml"),
            r#"<?xml version="1.0"?>
<MetaDataObject xmlns:v8="http://v8.1c.ru/8.1/data/core">
  <CommonModule>
    <Properties>
      <Name>РаботаСоШтрихкодами</Name>
      <Synonym><v8:item><v8:lang>ru</v8:lang><v8:content>Работа со штрихкодами</v8:content></v8:item></Synonym>
    </Properties>
  </CommonModule>
</MetaDataObject>"#,
        );
        write(
            &repo
                .join("CommonModules")
                .join("РаботаСоШтрихкодами")
                .join("Ext")
                .join("Module.bsl"),
            "// Уточняет данные номенклатуры по штрихкоду.\n\
             &НаСервере\n\
             Процедура УточнитьДанныеПоШтрихкоду() Экспорт\n\
             КонецПроцедуры\n",
        );
        let conn = storage.conn();
        conn.execute(
            "INSERT INTO files (path, content_hash, language) \
             VALUES ('CommonModules/РаботаСоШтрихкодами/Ext/Module.bsl', 'h', 'bsl')",
            [],
        )
        .unwrap();
        let fid: i64 = conn
            .query_row("SELECT id FROM files WHERE language='bsl'", [], |r| r.get(0))
            .unwrap();
        conn.execute(
            "INSERT INTO functions (file_id, name, line_start) \
             VALUES (?, 'УточнитьДанныеПоШтрихкоду', 3)",
            params![fid],
        )
        .unwrap();
    }

    #[test]
    fn mechanical_terms_include_name_synonym_and_comment() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let mut storage = fresh_storage(&tmp);
        write_terms_fixture(&repo, &storage);

        run_index_extras(&repo, &mut storage).unwrap();

        let (terms, sig): (String, String) = storage
            .conn()
            .query_row(
                "SELECT terms, signature FROM procedure_enrichment \
                 WHERE repo = ?1 AND proc_key = ?2",
                params![
                    REPO_DEFAULT,
                    "CommonModules/РаботаСоШтрихкодами/Ext/Module.bsl::УточнитьДанныеПоШтрихкоду"
                ],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(terms.contains("уточнить данные по штрихкоду"), "слова имени: {terms}");
        assert!(terms.contains("работа со штрихкодами"), "синоним объекта: {terms}");
        assert!(terms.contains("уточняет данные номенклатуры"), "комментарий: {terms}");
        assert_eq!(sig, crate::terms::MECH_SIGNATURE);

        // FTS (trigram): словоформа и подстрока находят процедуру.
        for q in ["штрихкод", "уточн", "работа со штрихкодами"] {
            let hits: i64 = storage
                .conn()
                .query_row(
                    "SELECT COUNT(*) FROM fts_procedure_enrichment WHERE terms MATCH ?1",
                    params![q],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(hits >= 1, "FTS должен находить '{q}'");
        }
    }

    #[test]
    fn mechanical_terms_dont_touch_llm_rows() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let mut storage = fresh_storage(&tmp);
        write_terms_fixture(&repo, &storage);
        // Существующая LLM-запись той же процедуры.
        storage
            .conn()
            .execute(
                "INSERT INTO procedure_enrichment (repo, proc_key, terms, signature, updated_at) \
                 VALUES (?1, ?2, 'llm-термины, бережно сохранить', 'openai_compatible:m', 1)",
                params![
                    REPO_DEFAULT,
                    "CommonModules/РаботаСоШтрихкодами/Ext/Module.bsl::УточнитьДанныеПоШтрихкоду"
                ],
            )
            .unwrap();

        run_index_extras(&repo, &mut storage).unwrap();

        let (terms, sig): (String, String) = storage
            .conn()
            .query_row(
                "SELECT terms, signature FROM procedure_enrichment \
                 WHERE repo = ?1 AND proc_key = ?2",
                params![
                    REPO_DEFAULT,
                    "CommonModules/РаботаСоШтрихкодами/Ext/Module.bsl::УточнитьДанныеПоШтрихкоду"
                ],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(terms, "llm-термины, бережно сохранить", "LLM-строка не перетёрта");
        assert_eq!(sig, "openai_compatible:m");
    }

    #[test]
    fn incremental_terms_update_and_cleanup() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let mut storage = fresh_storage(&tmp);
        write_terms_fixture(&repo, &storage);
        run_index_extras(&repo, &mut storage).unwrap();

        let bsl_abs = repo
            .join("CommonModules")
            .join("РаботаСоШтрихкодами")
            .join("Ext")
            .join("Module.bsl");

        // Файл «изменился»: добавилась процедура (в functions и на диске).
        write(
            &bsl_abs,
            "// Уточняет данные номенклатуры по штрихкоду.\n\
             &НаСервере\n\
             Процедура УточнитьДанныеПоШтрихкоду() Экспорт\n\
             КонецПроцедуры\n\
             \n\
             // Печатает этикетку со штрихкодом.\n\
             Процедура НапечататьЭтикетку() Экспорт\n\
             КонецПроцедуры\n",
        );
        {
            let conn = storage.conn();
            let fid: i64 = conn
                .query_row("SELECT id FROM files WHERE language='bsl'", [], |r| r.get(0))
                .unwrap();
            conn.execute(
                "INSERT INTO functions (file_id, name, line_start) \
                 VALUES (?, 'НапечататьЭтикетку', 7)",
                params![fid],
            )
            .unwrap();
        }
        run_incremental_extras(&repo, &mut storage, &[bsl_abs.clone()], &[]).unwrap();

        let count: i64 = storage
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM procedure_enrichment WHERE repo = ?1 AND signature LIKE 'mech:%'",
                params![REPO_DEFAULT],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 2, "после инкремента — термы обеих процедур");
        let terms: String = storage
            .conn()
            .query_row(
                "SELECT terms FROM procedure_enrichment WHERE repo = ?1 AND proc_key LIKE '%НапечататьЭтикетку'",
                params![REPO_DEFAULT],
                |r| r.get(0),
            )
            .unwrap();
        assert!(terms.contains("напечатать этикетку"), "{terms}");
        assert!(terms.contains("печатает этикетку"), "{terms}");

        // Файл удалён → mech-строки файла зачищены.
        std::fs::remove_file(&bsl_abs).unwrap();
        run_incremental_extras(&repo, &mut storage, &[], &[bsl_abs]).unwrap();
        let after: i64 = storage
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM procedure_enrichment WHERE repo = ?1 AND signature LIKE 'mech:%'",
                params![REPO_DEFAULT],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(after, 0, "после удаления файла mech-строки зачищены");
    }

    #[test]
    fn fills_event_subscriptions() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        write(
            &repo.join("EventSubscriptions").join("MySub.xml"),
            r#"<?xml version="1.0"?>
<MetaDataObject>
  <EventSubscription>
    <Properties>
      <Name>MySub</Name>
      <Source><Type><v8:Type>cfg:DocumentRef.X</v8:Type></Type></Source>
      <Event>ПриЗаписи</Event>
      <Handler>МойМодуль.МойОбработчик</Handler>
    </Properties>
  </EventSubscription>
</MetaDataObject>"#,
        );

        let mut storage = fresh_storage(&tmp);
        run_index_extras(&repo, &mut storage).unwrap();

        let row: (String, String, String) = storage
            .conn()
            .query_row(
                "SELECT name, handler_module, handler_proc FROM event_subscriptions WHERE repo = ?",
                params![REPO_DEFAULT],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(row, ("MySub".into(), "МойМодуль".into(), "МойОбработчик".into()));
    }

    #[test]
    fn call_graph_includes_extension_override() {
        // Перехват &Вместо ПробитьЧек в расширении → ребро extension_override
        // ПробитьЧек → EEРМК_ПробитьЧек. Источник — functions.override_*.
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        write(
            &repo.join("Configuration.xml"),
            r#"<?xml version="1.0"?>
<MetaDataObject><Configuration><Properties><Name>C</Name></Properties></Configuration></MetaDataObject>"#,
        );
        let mut storage = fresh_storage(&tmp);
        run_index_extras(&repo, &mut storage).unwrap();
        let conn = storage.conn();
        // Перехватчик в functions (как будто его распарсил core-парсер из CFE).
        conn.execute(
            "INSERT INTO files (path, content_hash, language) \
             VALUES ('extensions/E/Documents/X/Ext/Form/Module.bsl', 'h', 'bsl')",
            [],
        )
        .unwrap();
        let fid: i64 = conn
            .query_row("SELECT id FROM files WHERE path LIKE '%Module.bsl'", [], |r| r.get(0))
            .unwrap();
        conn.execute(
            "INSERT INTO functions (file_id, name, override_type, override_target) \
             VALUES (?, 'EEРМК_ПробитьЧек', 'Вместо', 'ПробитьЧек')",
            params![fid],
        )
        .unwrap();
        build_call_graph(conn).unwrap();
        let cnt: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM proc_call_graph \
                 WHERE call_type = 'extension_override' \
                   AND caller_proc_key = 'ПробитьЧек' AND callee_proc_name = 'EEРМК_ПробитьЧек'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cnt, 1, "должно появиться ребро перехвата extension_override");

        // Инкрементальный rebuild идемпотентен (не дублирует ребро).
        rebuild_call_graph_extension_override(conn).unwrap();
        let cnt2: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM proc_call_graph WHERE call_type = 'extension_override'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cnt2, 1, "rebuild не должен дублировать ребро");
    }

    #[test]
    fn call_graph_combines_subscriptions_and_form_events() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        // EventSubscription
        write(
            &repo.join("EventSubscriptions").join("Sub.xml"),
            r#"<?xml version="1.0"?>
<MetaDataObject>
  <EventSubscription>
    <Properties>
      <Name>Sub</Name>
      <Source><Type><v8:Type>cfg:DocumentRef.X</v8:Type></Type></Source>
      <Event>ПриЗаписи</Event>
      <Handler>М.П</Handler>
    </Properties>
  </EventSubscription>
</MetaDataObject>"#,
        );
        // Form
        write(
            &repo
                .join("Documents")
                .join("X")
                .join("Forms")
                .join("Ф")
                .join("Ext")
                .join("Form.xml"),
            r#"<?xml version="1.0"?>
<Form><Events>
  <Event name="ПриОткрытии">ПриОткрытии</Event>
</Events></Form>"#,
        );

        let mut storage = fresh_storage(&tmp);
        run_index_extras(&repo, &mut storage).unwrap();
        let conn = storage.conn();

        let by_type: Vec<(String, i64)> = conn
            .prepare("SELECT call_type, COUNT(*) FROM proc_call_graph GROUP BY call_type")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let map: std::collections::HashMap<String, i64> = by_type.into_iter().collect();
        assert_eq!(
            map.get("subscription").copied(),
            Some(1),
            "одна подписка"
        );
        assert_eq!(
            map.get("form_event").copied(),
            Some(1),
            "один обработчик формы"
        );
        // direct рёбер не должно быть — `calls` core пуст (нет .bsl-кода).
        assert!(map.get("direct").copied().unwrap_or(0) == 0);
    }

    #[test]
    fn fills_metadata_forms_from_dump_layout() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        // Реалистичный layout DumpConfigToFiles:
        //   Documents/Реализация/Forms/ФормаДокумента/Ext/Form.xml
        let form_path = repo
            .join("Documents")
            .join("Реализация")
            .join("Forms")
            .join("ФормаДокумента")
            .join("Ext")
            .join("Form.xml");
        write(
            &form_path,
            r#"<?xml version="1.0"?>
<Form>
  <Events>
    <Event name="ПриОткрытии">ПриОткрытии</Event>
  </Events>
</Form>"#,
        );

        let mut storage = fresh_storage(&tmp);
        run_index_extras(&repo, &mut storage).unwrap();

        let row: (String, String, String) = storage
            .conn()
            .query_row(
                "SELECT owner_full_name, form_name, handlers_json FROM metadata_forms WHERE repo = ?",
                params![REPO_DEFAULT],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(row.0, "Documents.Реализация");
        assert_eq!(row.1, "ФормаДокумента");
        assert!(row.2.contains("ПриОткрытии"));
    }

    /// Создать фикстуру конфигурации с источниками конфиг-уровня и ролью.
    fn write_config_level_fixture(repo: &Path) {
        write(
            &repo.join("Configuration.xml"),
            r#"<?xml version="1.0"?>
<MetaDataObject><Configuration><ChildObjects/></Configuration></MetaDataObject>"#,
        );
        // Подсистема с составом (2 объекта).
        write(
            &repo.join("Subsystems").join("Продажи.xml"),
            r#"<?xml version="1.0"?>
<MetaDataObject xmlns:xr="x" xmlns:xsi="y"><Subsystem><Properties>
  <Name>Продажи</Name>
  <Content>
    <xr:Item xsi:type="xr:MDObjectRef">Document.РеализацияТоваровУслуг</xr:Item>
    <xr:Item xsi:type="xr:MDObjectRef">Catalog.Контрагенты</xr:Item>
  </Content>
</Properties><ChildObjects/></Subsystem></MetaDataObject>"#,
        );
        // План обмена: Content.xml.
        write(
            &repo.join("ExchangePlans").join("Обмен").join("Ext").join("Content.xml"),
            r#"<?xml version="1.0"?>
<ExchangePlanContent xmlns="z">
  <Item><Metadata>Catalog.Номенклатура</Metadata><AutoRecord>Deny</AutoRecord></Item>
</ExchangePlanContent>"#,
        );
        // Определяемый тип: составной (2 ссылочных, 1 примитив отброшен).
        write(
            &repo.join("DefinedTypes").join("Адресат.xml"),
            r#"<?xml version="1.0"?>
<MetaDataObject xmlns:v8="c"><DefinedType><Properties><Name>Адресат</Name>
  <Type>
    <v8:Type>cfg:CatalogRef.Пользователи</v8:Type>
    <v8:Type>cfg:EnumRef.ВидыДат</v8:Type>
    <v8:Type>xs:string</v8:Type>
  </Type>
</Properties></DefinedType></MetaDataObject>"#,
        );
        // Функциональная опция: Location в ресурс регистра.
        write(
            &repo.join("FunctionalOptions").join("ФО.xml"),
            r#"<?xml version="1.0"?>
<MetaDataObject><FunctionalOption><Properties><Name>ФО</Name>
  <Location>InformationRegister.Настройки.Resource.Значение</Location>
  <Content/></Properties></FunctionalOption></MetaDataObject>"#,
        );
        // Роль: Read=true и Posting=false на документе.
        write(
            &repo.join("Roles").join("Роль1").join("Ext").join("Rights.xml"),
            r#"<?xml version="1.0"?>
<Rights xmlns="r"><object>
  <name>Document.РеализацияТоваровУслуг</name>
  <right><name>Read</name><value>true</value></right>
  <right><name>Posting</name><value>false</value></right>
</object></Rights>"#,
        );
    }

    #[test]
    fn fills_metadata_refs_and_role_rights() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        write_config_level_fixture(&repo);

        let mut storage = fresh_storage(&tmp);
        run_index_extras(&repo, &mut storage).unwrap();
        let conn = storage.conn();

        // subsystem_content: 2 ребра, from_object = Subsystem.Продажи.
        let subs: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM data_links WHERE link_kind='subsystem_content' \
                 AND from_object='Subsystem.Продажи'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(subs, 2, "subsystem_content");

        // exchange_plan_content: ExchangePlan.Обмен → Catalog.Номенклатура.
        let ep: String = conn
            .query_row(
                "SELECT to_object FROM data_links WHERE link_kind='exchange_plan_content' \
                 AND from_object='ExchangePlan.Обмен'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ep, "Catalog.Номенклатура");

        // defined_type_content: 2 ссылочных, is_composite=1, примитив отброшен.
        let (dt_cnt, dt_comp): (i64, i64) = conn
            .query_row(
                "SELECT COUNT(*), MAX(is_composite) FROM data_links \
                 WHERE link_kind='defined_type_content' AND from_object='DefinedType.Адресат'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(dt_cnt, 2, "defined_type_content edges");
        assert_eq!(dt_comp, 1, "defined_type_content is_composite");

        // functional_option_location: FunctionalOption.ФО → InformationRegister.Настройки.
        let (fo_to, fo_path): (String, String) = conn
            .query_row(
                "SELECT to_object, from_path FROM data_links \
                 WHERE link_kind='functional_option_location' AND from_object='FunctionalOption.ФО'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(fo_to, "InformationRegister.Настройки");
        assert!(fo_path.ends_with("Resource.Значение"));

        // role_rights: только granted (Read), Posting=false отброшен.
        let rr: Vec<(String, String, String)> = {
            let mut s = conn
                .prepare("SELECT role_name, object_name, right_name FROM role_rights ORDER BY right_name")
                .unwrap();
            let rows = s
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap();
            rows.map(|x| x.unwrap()).collect()
        };
        assert_eq!(
            rr,
            vec![(
                "Роль1".to_string(),
                "Document.РеализацияТоваровУслуг".to_string(),
                "Read".to_string()
            )]
        );

        // Идемпотентность: повторный полный прогон не плодит дубли.
        run_index_extras(&repo, &mut storage).unwrap();
        let conn = storage.conn();
        let total: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM data_links WHERE link_kind IN \
                 ('subsystem_content','exchange_plan_content','defined_type_content','functional_option_location')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(total, 2 + 1 + 2 + 1, "config-level data_links после повтора");
        let rr_total: i64 = conn
            .query_row("SELECT COUNT(*) FROM role_rights", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rr_total, 1, "role_rights после повтора");
    }

    #[test]
    fn incremental_rebuilds_metadata_refs_and_role_rights() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        write_config_level_fixture(&repo);

        let mut storage = fresh_storage(&tmp);
        run_index_extras(&repo, &mut storage).unwrap();

        // Снимок «эталона» — полный пересбор отдельной свежей БД.
        let cnt = |st: &mut Storage, sql: &str| -> i64 {
            st.conn().query_row(sql, [], |r| r.get(0)).unwrap()
        };
        let dl_sql = "SELECT COUNT(*) FROM data_links WHERE link_kind IN \
             ('subsystem_content','exchange_plan_content','defined_type_content','functional_option_location')";
        let rr_sql = "SELECT COUNT(*) FROM role_rights";

        // Меняем состав подсистемы (добавили объект) и право роли (добавили Posting=true).
        write(
            &repo.join("Subsystems").join("Продажи.xml"),
            r#"<?xml version="1.0"?>
<MetaDataObject xmlns:xr="x" xmlns:xsi="y"><Subsystem><Properties>
  <Name>Продажи</Name>
  <Content>
    <xr:Item xsi:type="xr:MDObjectRef">Document.РеализацияТоваровУслуг</xr:Item>
    <xr:Item xsi:type="xr:MDObjectRef">Catalog.Контрагенты</xr:Item>
    <xr:Item xsi:type="xr:MDObjectRef">Catalog.Склады</xr:Item>
  </Content>
</Properties><ChildObjects/></Subsystem></MetaDataObject>"#,
        );
        write(
            &repo.join("Roles").join("Роль1").join("Ext").join("Rights.xml"),
            r#"<?xml version="1.0"?>
<Rights xmlns="r"><object>
  <name>Document.РеализацияТоваровУслуг</name>
  <right><name>Read</name><value>true</value></right>
  <right><name>Posting</name><value>true</value></right>
</object></Rights>"#,
        );

        let changed = vec![
            repo.join("Subsystems").join("Продажи.xml"),
            repo.join("Roles").join("Роль1").join("Ext").join("Rights.xml"),
        ];
        run_incremental_extras(&repo, &mut storage, &changed, &[]).unwrap();

        // Инкремент должен совпасть с полным пересбором с нуля — отдельная БД,
        // тот же (уже изменённый) репо.
        let tmp2 = TempDir::new().unwrap();
        let mut full = fresh_storage(&tmp2);
        run_index_extras(&repo, &mut full).unwrap();

        assert_eq!(cnt(&mut storage, dl_sql), 3 + 1 + 2 + 1, "data_links после инкремента");
        assert_eq!(cnt(&mut storage, rr_sql), 2, "role_rights после инкремента");
        assert_eq!(
            cnt(&mut storage, dl_sql),
            cnt(&mut full, dl_sql),
            "config data_links: инкремент != полный пересбор"
        );
        assert_eq!(
            cnt(&mut storage, rr_sql),
            cnt(&mut full, rr_sql),
            "role_rights: инкремент != полный пересбор"
        );
    }

    #[test]
    fn fills_metadata_code_usages_from_bsl() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        write(
            &repo.join("Configuration.xml"),
            r#"<?xml version="1.0"?>
<MetaDataObject><Configuration><ChildObjects/></Configuration></MetaDataObject>"#,
        );
        write(
            &repo.join("CommonModules").join("М").join("Ext").join("Module.bsl"),
            "Процедура П()\n\tДок = Документы.РеализацияТоваровУслуг.СоздатьДокумент();\n\tТекст = \"ВЫБРАТЬ Ссылка ИЗ Документ.Заказ.Товары\";\nКонецПроцедуры",
        );

        let mut storage = fresh_storage(&tmp);
        run_index_extras(&repo, &mut storage).unwrap();
        let conn = storage.conn();
        let rows: Vec<(String, Option<String>, String, i64)> = {
            let mut s = conn
                .prepare("SELECT object_ref, member_path, usage_kind, line FROM metadata_code_usages ORDER BY line")
                .unwrap();
            s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .unwrap()
                .map(|x| x.unwrap())
                .collect()
        };
        assert_eq!(
            rows,
            vec![
                ("Document.РеализацияТоваровУслуг".to_string(), None, "manager".to_string(), 2),
                ("Document.Заказ".to_string(), Some("Товары".to_string()), "query".to_string(), 3),
            ]
        );

        // file_path записан относительным с forward slash.
        let fp: String = conn
            .query_row("SELECT DISTINCT file_path FROM metadata_code_usages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fp, "CommonModules/М/Ext/Module.bsl");

        // Идемпотентность: повторный прогон не плодит дубли.
        run_index_extras(&repo, &mut storage).unwrap();
        let cnt: i64 = storage
            .conn()
            .query_row("SELECT COUNT(*) FROM metadata_code_usages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(cnt, 2);
    }

    #[test]
    fn fills_data_links_from_object_xml() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        // Configuration.xml нужен, чтобы index_data_links нашёл sub-root.
        write(
            &repo.join("Configuration.xml"),
            r#"<?xml version="1.0"?>
<MetaDataObject><Configuration><ChildObjects>
  <Document>РеализацияТоваровУслуг</Document>
</ChildObjects></Configuration></MetaDataObject>"#,
        );
        // Объектный XML документа: реквизит шапки + ТЧ + примитив.
        write(
            &repo.join("Documents").join("РеализацияТоваровУслуг.xml"),
            r#"<?xml version="1.0"?>
<MetaDataObject xmlns:v8="http://v8.1c.ru/8.3/data/core">
  <Document uuid="root">
    <Properties><Name>РеализацияТоваровУслуг</Name></Properties>
    <ChildObjects>
      <Attribute uuid="a1"><Properties><Name>Контрагент</Name>
        <Type><v8:Type>cfg:CatalogRef.Контрагенты</v8:Type></Type>
      </Properties></Attribute>
      <Attribute uuid="a2"><Properties><Name>Сумма</Name>
        <Type><v8:Type>xs:decimal</v8:Type></Type>
      </Properties></Attribute>
      <TabularSection uuid="ts1"><Properties><Name>Товары</Name></Properties>
        <ChildObjects>
          <Attribute uuid="a3"><Properties><Name>Номенклатура</Name>
            <Type><v8:Type>cfg:CatalogRef.Номенклатура</v8:Type></Type>
          </Properties></Attribute>
        </ChildObjects>
      </TabularSection>
    </ChildObjects>
  </Document>
</MetaDataObject>"#,
        );

        let mut storage = fresh_storage(&tmp);
        run_index_extras(&repo, &mut storage).unwrap();
        let conn = storage.conn();

        // Контрагент (attr) + Товары.Номенклатура (tabular_attr) = 2 ребра.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM data_links WHERE repo = ?", params![REPO_DEFAULT], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2, "ожидаем 2 ссылочных ребра (примитив Сумма пропущен)");

        let (from_path, to_object, kind): (String, String, String) = conn
            .query_row(
                "SELECT from_path, to_object, link_kind FROM data_links \
                 WHERE repo = ? AND from_object = 'Document.РеализацияТоваровУслуг' AND from_path = 'Контрагент'",
                params![REPO_DEFAULT],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(from_path, "Контрагент");
        assert_eq!(to_object, "Catalog.Контрагенты");
        assert_eq!(kind, "attr");

        // Реквизит табличной части.
        let tab_to: String = conn
            .query_row(
                "SELECT to_object FROM data_links WHERE repo = ? AND from_path = 'Товары.Номенклатура'",
                params![REPO_DEFAULT],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tab_to, "Catalog.Номенклатура");
    }

    #[test]
    fn data_links_idempotent() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        write(
            &repo.join("Configuration.xml"),
            r#"<?xml version="1.0"?>
<MetaDataObject><Configuration><ChildObjects>
  <Catalog>Тест</Catalog>
</ChildObjects></Configuration></MetaDataObject>"#,
        );
        write(
            &repo.join("Catalogs").join("Тест.xml"),
            r#"<?xml version="1.0"?>
<MetaDataObject xmlns:v8="http://v8.1c.ru/8.3/data/core">
  <Catalog uuid="root"><ChildObjects>
    <Attribute uuid="a1"><Properties><Name>Владелец</Name>
      <Type><v8:Type>cfg:CatalogRef.Организации</v8:Type></Type>
    </Properties></Attribute>
  </ChildObjects></Catalog>
</MetaDataObject>"#,
        );

        let mut storage = fresh_storage(&tmp);
        run_index_extras(&repo, &mut storage).unwrap();
        run_index_extras(&repo, &mut storage).unwrap();
        run_index_extras(&repo, &mut storage).unwrap();

        let count: i64 = storage
            .conn()
            .query_row("SELECT COUNT(*) FROM data_links WHERE repo = ?", params![REPO_DEFAULT], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "повторный run не должен плодить дубликаты рёбер");
    }

    // ── Эквивалентность инкрементального обновления полному пересбору ──────
    //
    // Главный приёмочный тест варианта A: после правки одного файла +
    // run_incremental_extras итоговые таблицы должны совпасть с полным
    // run_index_extras на той же конечной версии репо.

    fn snapshot_pcg(conn: &rusqlite::Connection) -> Vec<(String, String, String)> {
        let mut v: Vec<(String, String, String)> = conn
            .prepare(
                "SELECT caller_proc_key, callee_proc_name, call_type \
                 FROM proc_call_graph WHERE repo = ?",
            )
            .unwrap()
            .query_map(params![REPO_DEFAULT], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        v.sort();
        v
    }

    fn snapshot_dl(conn: &rusqlite::Connection) -> Vec<(String, String, String, String)> {
        let mut v: Vec<(String, String, String, String)> = conn
            .prepare(
                "SELECT from_object, from_path, to_object, link_kind \
                 FROM data_links WHERE repo = ?",
            )
            .unwrap()
            .query_map(params![REPO_DEFAULT], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        v.sort();
        v
    }

    fn snapshot_attrs(conn: &rusqlite::Connection) -> Vec<(String, Option<String>)> {
        let mut v: Vec<(String, Option<String>)> = conn
            .prepare("SELECT full_name, attributes_json FROM metadata_objects WHERE repo = ?")
            .unwrap()
            .query_map(params![REPO_DEFAULT], |r| {
                Ok((r.get(0)?, r.get::<_, Option<String>>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        v.sort();
        v
    }

    fn ensure_file(conn: &rusqlite::Connection, path: &str) -> i64 {
        conn.execute(
            "INSERT OR IGNORE INTO files (path, content_hash, language) VALUES (?, 'h', 'bsl')",
            params![path],
        )
        .unwrap();
        conn.query_row("SELECT id FROM files WHERE path = ?", params![path], |r| r.get(0))
            .unwrap()
    }

    fn set_calls(conn: &rusqlite::Connection, file_id: i64, edges: &[(&str, &str)]) {
        conn.execute("DELETE FROM calls WHERE file_id = ?", params![file_id])
            .unwrap();
        for (caller, callee) in edges {
            conn.execute(
                "INSERT INTO calls (file_id, caller, callee, line) VALUES (?, ?, ?, 1)",
                params![file_id, caller, callee],
            )
            .unwrap();
        }
    }

    fn set_func(conn: &rusqlite::Connection, file_id: i64, name: &str, args: &str) {
        conn.execute(
            "INSERT INTO functions (file_id, name, args) VALUES (?, ?, ?)",
            params![file_id, name, args],
        )
        .unwrap();
    }

    #[test]
    fn resolves_callee_keys_local_unique_export_and_null() {
        // Этап 4e: проверяем оба tier'а резолвера и честный NULL.
        let tmp = TempDir::new().unwrap();
        let st = fresh_storage(&tmp);
        let conn = st.conn();

        let p1 = "Documents/Реализация/Ext/ObjectModule.bsl";
        let p2 = "Documents/Поступление/Ext/ObjectModule.bsl";
        let util = "CommonModules/Util/Ext/Module.bsl";
        let a = "CommonModules/A/Ext/Module.bsl";
        let b = "CommonModules/B/Ext/Module.bsl";
        let f1 = ensure_file(conn, p1);
        let f2 = ensure_file(conn, p2);
        let fu = ensure_file(conn, util);
        let fa = ensure_file(conn, a);
        let fb = ensure_file(conn, b);

        // Процедуры: локальные (без Экспорт) + экспортные ('() Экспорт').
        set_func(conn, f1, "ОбработкаПроведения", "()");
        set_func(conn, f1, "МестныйПомощник", "()");
        set_func(conn, f2, "ОбработкаПроведения", "()");
        set_func(conn, fu, "ОбщийУникальный", "() Экспорт");
        set_func(conn, fa, "Дубликат", "() Экспорт");
        set_func(conn, fb, "Дубликат", "() Экспорт");

        set_calls(
            conn,
            f1,
            &[
                ("ОбработкаПроведения", "МестныйПомощник"), // локальный → резолв в p1
                ("ОбработкаПроведения", "ОбщийУникальный"), // уникальный экспорт → util
                ("ОбработкаПроведения", "Дубликат"),       // неоднозначный экспорт → NULL
                ("ОбработкаПроведения", "ВнешнийНеизвестный"), // не резолвится, не балласт → NULL
            ],
        );
        set_calls(conn, f2, &[("ОбработкаПроведения", "ДругойМетод")]);

        build_call_graph(conn).unwrap();

        // 1) Одноимённые caller разведены по файлам (Шаг 1).
        let callers: Vec<String> = conn
            .prepare(
                "SELECT DISTINCT caller_proc_key FROM proc_call_graph \
                 WHERE call_type='direct' ORDER BY caller_proc_key",
            )
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            callers.contains(&format!("{p1}::ОбработкаПроведения")),
            "caller из p1 несёт путь: {callers:?}"
        );
        assert!(
            callers.contains(&format!("{p2}::ОбработкаПроведения")),
            "caller из p2 несёт путь — одноимённые НЕ схлопнуты"
        );

        // callee_proc_key для ребра из p1 по имени callee.
        let key = |callee: &str| -> Option<String> {
            conn.query_row(
                "SELECT callee_proc_key FROM proc_call_graph \
                 WHERE call_type='direct' AND caller_proc_key=?1 AND callee_proc_name=?2",
                params![format!("{p1}::ОбработкаПроведения"), callee],
                |r| r.get::<_, Option<String>>(0),
            )
            .unwrap()
        };

        // 2) Локальный вызов → адрес в файле вызывателя.
        assert_eq!(key("МестныйПомощник"), Some(format!("{p1}::МестныйПомощник")));
        // 3) Уникальный экспорт → адрес единственного носителя.
        assert_eq!(key("ОбщийУникальный"), Some(format!("{util}::ОбщийУникальный")));
        // 4) Неоднозначный экспорт (2 модуля) → честный NULL.
        assert_eq!(key("Дубликат"), None, "неоднозначный экспорт не должен привязываться");
        // 5) Нерезолвимое имя (нет такой процедуры, не балласт) → NULL, ребро на месте.
        assert_eq!(key("ВнешнийНеизвестный"), None, "нерезолвимый вызов не привязывается");
    }

    #[test]
    fn prunes_platform_balast_keeps_real_and_resolved() {
        // Этап 4e-prune: балластное ребро (callee_proc_key NULL) удаляется;
        // реальное локальное ребро остаётся; имя из списка балласта, которое
        // РЕЗОЛВИЛОСЬ в реальную процедуру (callee_proc_key != NULL), сохраняется
        // (защита от коллизий имён по IS NULL).
        let tmp = TempDir::new().unwrap();
        let st = fresh_storage(&tmp);
        let conn = st.conn();

        let p1 = "Documents/Реализация/Ext/ObjectModule.bsl";
        let util = "CommonModules/Util/Ext/Module.bsl";
        let mod_c = "CommonModules/C/Ext/Module.bsl";
        let mod_d = "CommonModules/D/Ext/Module.bsl";
        let f1 = ensure_file(conn, p1);
        let fu = ensure_file(conn, util);
        let fc = ensure_file(conn, mod_c);
        let fd = ensure_file(conn, mod_d);

        set_func(conn, f1, "ОбработкаПроведения", "()");
        set_func(conn, f1, "МестныйПомощник", "()");
        // Экспортная процедура с именем, СОВПАДАЮЩИМ с балластным ("Найти"), уникальна.
        set_func(conn, fu, "Найти", "() Экспорт");
        // Балластное имя "Записать", экспортное НЕОДНОЗНАЧНО (2 модуля) → не резолвится.
        set_func(conn, fc, "Записать", "() Экспорт");
        set_func(conn, fd, "Записать", "() Экспорт");

        set_calls(
            conn,
            f1,
            &[
                ("ОбработкаПроведения", "Добавить"),        // балласт, не экспорт, не резолв → удалить
                ("ОбработкаПроведения", "МестныйПомощник"), // реальное локальное → оставить
                ("ОбработкаПроведения", "Найти"),           // балластное ИМЯ, но резолвится → оставить
                ("ОбработкаПроведения", "Записать"),        // балласт + экспорт-коллизия, NULL → оставить
            ],
        );

        build_call_graph(conn).unwrap();

        let exists = |callee: &str| -> i64 {
            conn.query_row(
                "SELECT COUNT(*) FROM proc_call_graph \
                 WHERE call_type='direct' AND caller_proc_key=?1 AND callee_proc_name=?2",
                params![format!("{p1}::ОбработкаПроведения"), callee],
                |r| r.get(0),
            )
            .unwrap()
        };

        assert_eq!(exists("Добавить"), 0, "балластное ребро (не экспорт, не резолв) удаляется");
        assert_eq!(exists("МестныйПомощник"), 1, "реальное локальное ребро остаётся");
        assert_eq!(
            exists("Найти"),
            1,
            "балластное ИМЯ, резолвленное в реальную процедуру, сохраняется (IS NULL guard)"
        );
        assert_eq!(
            exists("Записать"),
            1,
            "балластное ИМЯ, экспортное в конфиге неоднозначно (NULL), не отсеивается (collision guard)"
        );
    }

    #[test]
    fn resolves_callee_key_by_module_qualifier() {
        // Tier C (CORE B): склеенный вызов `Модуль.Метод` резолвится точно по
        // квалификатору общего модуля — даже если имя метода экспортно в ≥2
        // модулях (что Tier B оставлял бы честным NULL).
        let tmp = TempDir::new().unwrap();
        let st = fresh_storage(&tmp);
        let conn = st.conn();

        let caller = "Documents/Реализация/Ext/ObjectModule.bsl";
        let mod_a = "base/CommonModules/МодульА/Ext/Module.bsl";
        let mod_b = "base/CommonModules/МодульБ/Ext/Module.bsl";
        let fc = ensure_file(conn, caller);
        let fa = ensure_file(conn, mod_a);
        let fb = ensure_file(conn, mod_b);

        set_func(conn, fc, "ОбработкаПроведения", "()");
        // Одно и то же имя метода экспортно в ДВУХ общих модулях.
        set_func(conn, fa, "ОбщийМетод", "() Экспорт");
        set_func(conn, fb, "ОбщийМетод", "() Экспорт");

        set_calls(
            conn,
            fc,
            &[
                ("ОбработкаПроведения", "МодульА.ОбщийМетод"), // → mod_a (квалификатор разводит коллизию)
                ("ОбработкаПроведения", "МодульБ.ОбщийМетод"), // → mod_b
                ("ОбработкаПроведения", "МодульА.НетТакого"),  // метода нет в А → NULL
                ("ОбработкаПроведения", "ЧужойМодуль.Метод"),  // квалификатор не общий модуль → NULL
            ],
        );

        build_call_graph(conn).unwrap();

        let key = |callee: &str| -> Option<String> {
            conn.query_row(
                "SELECT callee_proc_key FROM proc_call_graph \
                 WHERE call_type='direct' AND caller_proc_key=?1 AND callee_proc_name=?2",
                params![format!("{caller}::ОбработкаПроведения"), callee],
                |r| r.get::<_, Option<String>>(0),
            )
            .unwrap()
        };

        // Коллизия имени разрешена квалификатором — точная привязка к нужному модулю.
        assert_eq!(key("МодульА.ОбщийМетод"), Some(format!("{mod_a}::ОбщийМетод")));
        assert_eq!(key("МодульБ.ОбщийМетод"), Some(format!("{mod_b}::ОбщийМетод")));
        // Метода нет в модуле, но квалификатор = реальный модуль → щадим, NULL.
        assert_eq!(key("МодульА.НетТакого"), None, "несуществующий метод модуля не привязывается");
        // Квалификатор — не общий модуль и не коллекция → трактуется как объектный
        // вызов и отсеивается пруном (строки больше нет).
        let exists_chuzhoy: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM proc_call_graph \
                 WHERE call_type='direct' AND caller_proc_key=?1 AND callee_proc_name=?2",
                params![format!("{caller}::ОбработкаПроведения"), "ЧужойМодуль.Метод"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(exists_chuzhoy, 0, "вызов с неизвестным квалификатором отсеян пруном объектных вызовов");
    }

    #[test]
    fn prunes_glued_object_method_but_keeps_resolved_module_call() {
        // CORE B: у склеенных имён балласт отсеивается по методу-ПОСЛЕ-точки
        // (`Объект.Добавить` → `Добавить`); реальный вызов общего модуля,
        // резолвленный Tier C, при этом сохраняется (IS NULL guard).
        let tmp = TempDir::new().unwrap();
        let st = fresh_storage(&tmp);
        let conn = st.conn();

        let caller = "Documents/Реализация/Ext/ObjectModule.bsl";
        let cmod = "base/CommonModules/ОбщегоНазначения/Ext/Module.bsl";
        let fc = ensure_file(conn, caller);
        let fm = ensure_file(conn, cmod);

        set_func(conn, fc, "ОбработкаПроведения", "()");
        set_func(conn, fm, "РеальныйМетод", "() Экспорт");

        set_calls(
            conn,
            fc,
            &[
                ("ОбработкаПроведения", "Объект.Добавить"), // балласт по методу → удалить
                ("ОбработкаПроведения", "ОбщегоНазначения.РеальныйМетод"), // Tier C резолв → оставить
            ],
        );

        build_call_graph(conn).unwrap();

        let exists = |callee: &str| -> i64 {
            conn.query_row(
                "SELECT COUNT(*) FROM proc_call_graph \
                 WHERE call_type='direct' AND caller_proc_key=?1 AND callee_proc_name=?2",
                params![format!("{caller}::ОбработкаПроведения"), callee],
                |r| r.get(0),
            )
            .unwrap()
        };

        assert_eq!(exists("Объект.Добавить"), 0, "склеенный балласт отсеивается по методу-после-точки");
        assert_eq!(
            exists("ОбщегоНазначения.РеальныйМетод"),
            1,
            "резолвленный вызов общего модуля сохраняется"
        );
    }

    #[test]
    fn prunes_object_calls_protects_modules_collections_chains() {
        // CORE B прун по квалификатору: режем `Объект.Метод` (квалификатор —
        // переменная), но щадим общие модули, коллекции метаданных и цепочки.
        let tmp = TempDir::new().unwrap();
        let st = fresh_storage(&tmp);
        let conn = st.conn();

        let caller = "Documents/Реализация/Ext/ObjectModule.bsl";
        let cmod = "base/CommonModules/ОбщегоНазначения/Ext/Module.bsl";
        let fc = ensure_file(conn, caller);
        let fm = ensure_file(conn, cmod);
        set_func(conn, fc, "ОбработкаПроведения", "()");
        set_func(conn, fm, "РеальныйМетод", "() Экспорт");
        // Менеджер-модуль справочника с ЮЗЕР-экспортным методом.
        let mgr = "base/Catalogs/Контрагенты/Ext/ManagerModule.bsl";
        let fmgr = ensure_file(conn, mgr);
        set_func(conn, fmgr, "СоздатьПоНаименованию", "() Экспорт");

        set_calls(
            conn,
            fc,
            &[
                ("ОбработкаПроведения", "Объект.ПроизвольныйМетод"), // объект (1 точка) → удалить
                ("ОбработкаПроведения", "Запрос.ВыполнитьПакет"),   // объект (1 точка) → удалить
                ("ОбработкаПроведения", "Запрос.Поле.Значение"),    // объектная цепочка (2 точки) → удалить
                ("ОбработкаПроведения", "ОбщегоНазначения.РеальныйМетод"), // модуль (Tier C) → оставить
                ("ОбработкаПроведения", "ОбщегоНазначения.НетТакого"),     // модуль, метод не экспортен → NULL, щадим
                ("ОбработкаПроведения", "Справочники.НайтиПоНаименованию"), // коллекция (1 точка) → щадим
                ("ОбработкаПроведения", "Справочники.Контрагенты.СоздатьПоНаименованию"), // менеджер (Tier D) → резолв
                ("ОбработкаПроведения", "Справочники.Контрагенты.ПустаяСсылка"), // платформенный метод менеджера → удалить
            ],
        );

        build_call_graph(conn).unwrap();

        let exists = |callee: &str| -> i64 {
            conn.query_row(
                "SELECT COUNT(*) FROM proc_call_graph \
                 WHERE call_type='direct' AND caller_proc_key=?1 AND callee_proc_name=?2",
                params![format!("{caller}::ОбработкаПроведения"), callee],
                |r| r.get(0),
            )
            .unwrap()
        };
        let key = |callee: &str| -> Option<String> {
            conn.query_row(
                "SELECT callee_proc_key FROM proc_call_graph \
                 WHERE call_type='direct' AND caller_proc_key=?1 AND callee_proc_name=?2",
                params![format!("{caller}::ОбработкаПроведения"), callee],
                |r| r.get::<_, Option<String>>(0),
            )
            .unwrap()
        };

        assert_eq!(exists("Объект.ПроизвольныйМетод"), 0, "объектный вызов (1 точка) отсеян");
        assert_eq!(exists("Запрос.ВыполнитьПакет"), 0, "объектный вызов (1 точка) отсеян");
        assert_eq!(exists("Запрос.Поле.Значение"), 0, "объектная цепочка (2 точки) отсеяна");
        assert_eq!(exists("ОбщегоНазначения.РеальныйМетод"), 1, "общий модуль (резолв) сохранён");
        assert_eq!(exists("ОбщегоНазначения.НетТакого"), 1, "имя общего модуля щадим даже при NULL");
        assert_eq!(exists("Справочники.НайтиПоНаименованию"), 1, "коллекция (1 точка) сохранена");
        assert_eq!(
            key("Справочники.Контрагенты.СоздатьПоНаименованию"),
            Some(format!("{mgr}::СоздатьПоНаименованию")),
            "менеджер-вызов резолвлен в ManagerModule (Tier D)"
        );
        assert_eq!(exists("Справочники.Контрагенты.ПустаяСсылка"), 0, "платформенный метод менеджера отсеян");
    }

    #[test]
    fn incremental_object_xml_matches_full() {
        let cfg = r#"<?xml version="1.0"?>
<MetaDataObject><Configuration><ChildObjects>
  <Document>Реализация</Document>
</ChildObjects></Configuration></MetaDataObject>"#;
        let doc_v1 = r#"<?xml version="1.0"?>
<MetaDataObject xmlns:v8="http://v8.1c.ru/8.3/data/core">
  <Document uuid="root"><Properties><Name>Реализация</Name></Properties>
    <ChildObjects>
      <Attribute uuid="a1"><Properties><Name>Контрагент</Name>
        <Type><v8:Type>cfg:CatalogRef.Контрагенты</v8:Type></Type>
      </Properties></Attribute>
    </ChildObjects>
  </Document>
</MetaDataObject>"#;
        // v2: реквизит переименован + сменил тип ссылки + добавлен второй.
        let doc_v2 = r#"<?xml version="1.0"?>
<MetaDataObject xmlns:v8="http://v8.1c.ru/8.3/data/core">
  <Document uuid="root"><Properties><Name>Реализация</Name></Properties>
    <ChildObjects>
      <Attribute uuid="a1"><Properties><Name>Партнёр</Name>
        <Type><v8:Type>cfg:CatalogRef.Организации</v8:Type></Type>
      </Properties></Attribute>
      <Attribute uuid="a2"><Properties><Name>Склад</Name>
        <Type><v8:Type>cfg:CatalogRef.Склады</v8:Type></Type>
      </Properties></Attribute>
    </ChildObjects>
  </Document>
</MetaDataObject>"#;

        // truth: репо сразу в версии v2, полный пересбор.
        let tmp_t = TempDir::new().unwrap();
        let repo_t = tmp_t.path().join("repo");
        write(&repo_t.join("Configuration.xml"), cfg);
        write(&repo_t.join("Documents").join("Реализация.xml"), doc_v2);
        let mut st_t = fresh_storage(&tmp_t);
        run_index_extras(&repo_t, &mut st_t).unwrap();

        // incr: репо v1 → полный пересбор → правка XML на v2 → инкремент.
        let tmp_i = TempDir::new().unwrap();
        let repo_i = tmp_i.path().join("repo");
        write(&repo_i.join("Configuration.xml"), cfg);
        let doc_path = repo_i.join("Documents").join("Реализация.xml");
        write(&doc_path, doc_v1);
        let mut st_i = fresh_storage(&tmp_i);
        run_index_extras(&repo_i, &mut st_i).unwrap();
        write(&doc_path, doc_v2);
        run_incremental_extras(&repo_i, &mut st_i, &[doc_path.clone()], &[]).unwrap();

        assert_eq!(
            snapshot_dl(st_i.conn()),
            snapshot_dl(st_t.conn()),
            "data_links после инкремента != полному пересбору"
        );
        assert_eq!(
            snapshot_attrs(st_i.conn()),
            snapshot_attrs(st_t.conn()),
            "attributes_json после инкремента != полному пересбору"
        );
    }

    #[test]
    fn incremental_call_graph_direct_matches_full() {
        // Репо с подпиской и формой — проверяем, что инкремент .bsl
        // пересобирает только слой direct и НЕ затирает subscription/form_event.
        let cfg = r#"<?xml version="1.0"?>
<MetaDataObject><Configuration><ChildObjects>
  <Document>Реализация</Document>
</ChildObjects></Configuration></MetaDataObject>"#;
        let sub = r#"<?xml version="1.0"?>
<MetaDataObject>
  <EventSubscription><Properties>
    <Name>Подписка1</Name>
    <Source><Type><v8:Type>cfg:DocumentRef.Реализация</v8:Type></Type></Source>
    <Event>ПриЗаписи</Event>
    <Handler>ОбщийМодуль.Обработчик</Handler>
  </Properties></EventSubscription>
</MetaDataObject>"#;
        let form = r#"<?xml version="1.0"?>
<Form><Events>
  <Event name="ПриОткрытии">ПриОткрытииСервер</Event>
</Events></Form>"#;

        let build = |tmp: &TempDir| -> (std::path::PathBuf, Storage) {
            let repo = tmp.path().join("repo");
            write(&repo.join("Configuration.xml"), cfg);
            write(&repo.join("EventSubscriptions").join("Подписка1.xml"), sub);
            write(
                &repo
                    .join("Documents")
                    .join("Реализация")
                    .join("Forms")
                    .join("ФормаДокумента")
                    .join("Ext")
                    .join("Form.xml"),
                form,
            );
            (repo, fresh_storage(tmp))
        };

        // truth: calls = v2, полный пересбор.
        let tmp_t = TempDir::new().unwrap();
        let (repo_t, mut st_t) = build(&tmp_t);
        let fid_t = ensure_file(st_t.conn(), "Documents/Реализация/Ext/ObjectModule.bsl");
        set_calls(st_t.conn(), fid_t, &[("ПриЗаписи", "ВыполнитьC"), ("ПриЗаписи", "Общее")]);
        run_index_extras(&repo_t, &mut st_t).unwrap();

        // incr: calls = v1 → полный пересбор → правка .bsl (calls → v2) → инкремент.
        let tmp_i = TempDir::new().unwrap();
        let (repo_i, mut st_i) = build(&tmp_i);
        let fid_i = ensure_file(st_i.conn(), "Documents/Реализация/Ext/ObjectModule.bsl");
        set_calls(st_i.conn(), fid_i, &[("ПриЗаписи", "ВыполнитьB"), ("ПриЗаписи", "Общее")]);
        run_index_extras(&repo_i, &mut st_i).unwrap();
        set_calls(st_i.conn(), fid_i, &[("ПриЗаписи", "ВыполнитьC"), ("ПриЗаписи", "Общее")]);
        let bsl_path = repo_i
            .join("Documents")
            .join("Реализация")
            .join("Ext")
            .join("ObjectModule.bsl");
        run_incremental_extras(&repo_i, &mut st_i, &[bsl_path], &[]).unwrap();

        assert_eq!(
            snapshot_pcg(st_i.conn()),
            snapshot_pcg(st_t.conn()),
            "proc_call_graph после инкремента .bsl != полному пересбору"
        );
    }

    #[test]
    fn incremental_direct_shared_edge_survives() {
        // Ключевое свойство per-file при path-привязке ключей: F1 и F2 дают
        // РАЗНЫЕ рёбра — `F1.bsl::A->B` и `F2.bsl::A->B` (caller_proc_key несёт
        // путь файла). F1 дополнительно даёт `F1.bsl::A->C`. Правим F1 → у него
        // остаётся только A->B. Ожидаем: ребро F2 (`F2.bsl::A->B`) не зависит от
        // правки F1 и выживает; `F1.bsl::A->B` остаётся; `F1.bsl::A->C` исчезает.
        // Результат обязан совпасть с полным пересбором.
        fn setup(tmp: &TempDir, f1_edges: &[(&str, &str)]) -> (std::path::PathBuf, Storage, i64) {
            let repo = tmp.path().join("repo");
            std::fs::create_dir_all(&repo).unwrap();
            let st = fresh_storage(tmp);
            let f1 = ensure_file(st.conn(), "F1.bsl");
            let f2 = ensure_file(st.conn(), "F2.bsl");
            set_calls(st.conn(), f1, f1_edges);
            set_calls(st.conn(), f2, &[("A", "B")]);
            (repo, st, f1)
        }

        // truth: конечное состояние сразу (F1={A->B}, F2={A->B}), полный пересбор.
        let tmp_t = TempDir::new().unwrap();
        let (repo_t, mut st_t, _) = setup(&tmp_t, &[("A", "B")]);
        run_index_extras(&repo_t, &mut st_t).unwrap();

        // incr: F1 сперва {A->B, A->C}; полный пересбор; затем F1 -> {A->B}; инкремент F1.
        let tmp_i = TempDir::new().unwrap();
        let (repo_i, mut st_i, f1_i) = setup(&tmp_i, &[("A", "B"), ("A", "C")]);
        run_index_extras(&repo_i, &mut st_i).unwrap();
        set_calls(st_i.conn(), f1_i, &[("A", "B")]);
        run_incremental_extras(&repo_i, &mut st_i, &[repo_i.join("F1.bsl")], &[]).unwrap();

        let s_i = snapshot_pcg(st_i.conn());
        assert_eq!(
            s_i,
            snapshot_pcg(st_t.conn()),
            "after incremental != full rebuild (shared edge)"
        );
        assert!(
            s_i.iter().any(|(c, e, _)| c == "F2.bsl::A" && e == "B"),
            "ребро F2 (F2.bsl::A->B) не зависит от правки F1 и выживает"
        );
        assert!(
            s_i.iter().any(|(c, e, _)| c == "F1.bsl::A" && e == "B"),
            "F1.bsl::A->B остаётся (F1 его по-прежнему даёт)"
        );
        assert!(
            !s_i.iter().any(|(_, e, _)| e == "C"),
            "F1.bsl::A->C должно исчезнуть (F1 его больше не даёт)"
        );
    }

    #[test]
    fn backfill_keys_fill_lowercase_cyrillic() {
        use rusqlite::Connection;
        let conn = Connection::open_in_memory().unwrap();
        for ddl in crate::schema::SCHEMA_EXTENSIONS {
            conn.execute_batch(ddl).unwrap();
        }
        // Ребро без ключа (как сразу после INSERT) → backfill заполняет lower().
        conn.execute(
            "INSERT INTO data_links (repo, from_object, from_path, to_object, link_kind) \
             VALUES ('default','A','p','Document.ЗаказКлиента','attr')",
            [],
        )
        .unwrap();
        backfill_data_link_keys(&conn).unwrap();
        let key: String = conn
            .query_row(
                "SELECT to_object_key FROM data_links WHERE to_object='Document.ЗаказКлиента'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(key, "document.заказклиента");

        conn.execute(
            "INSERT INTO role_rights (repo, role_name, object_name, right_name) \
             VALUES ('default','Менеджер','Document.ЗаказКлиента','Read')",
            [],
        )
        .unwrap();
        backfill_role_right_keys(&conn).unwrap();
        let rk: String = conn
            .query_row(
                "SELECT object_name_key FROM role_rights WHERE right_name='Read'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rk, "document.заказклиента");
    }
}
