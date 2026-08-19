//! Инкрементальное обновление по событиям наблюдателя: пофайловые ветки
//! и сверка состава области по описи выгрузки.

use std::path::Path;
use anyhow::Result;
use code_index_core::storage::Storage;
use rusqlite::params;
use crate::xml::config_dump_info::parse_config_dump_info_rows;
use crate::xml::event_subscriptions::parse_event_subscription_file;
use crate::xml::forms::parse_form_file;
use crate::code_usages::extract_code_usages;
use crate::xml::object_attributes::{
    parse_object_attributes_file, parse_object_belonging, parse_object_header_xml,
};

use super::*;


/// Per-object обновление `data_links` для одного объекта: удалить его прежние
/// рёбра (`from_object = X`) и переразобрать только его XML. Покрывает и
/// recorder-рёбра (движения документа), т.к. они тоже имеют `from_object`
/// = документ. Если файл удалён — рёбра просто исчезают.
pub(crate) fn update_data_links_for_object(
    conn: &rusqlite::Connection,
    roots: &[std::path::PathBuf],
    xml_path: &Path,
) -> Result<()> {
    let owner_full = match object_full_name_from_path(xml_path) {
        Some((_mt, full)) => full,
        None => return Ok(()),
    };
    // Папка (plural) и имя объекта — из пути; ищем копии объекта во ВСЕХ sub-config
    // и объединяем рёбра (симметрично bulk index_data_links), а не разбираем один
    // пришедший файл. Удалённая/ушедшая копия отсеивается сама (файла нет).
    let folder = match xml_path.parent().and_then(|d| d.file_name()).and_then(|s| s.to_str()) {
        Some(s) => s.to_string(),
        None => return Ok(()),
    };
    let stem = match xml_path.file_stem().and_then(|s| s.to_str()) {
        Some(s) => s.to_string(),
        None => return Ok(()),
    };
    let _ = conn.execute("ROLLBACK", []);
    conn.execute("BEGIN", [])?;
    conn.execute(
        "DELETE FROM data_links WHERE repo = ? AND from_object = ?",
        params![REPO_DEFAULT, &owner_full],
    )?;
    {
        let mut stmt = conn.prepare(
            "INSERT OR IGNORE INTO data_links \
             (repo, from_object, from_path, to_object, link_kind, is_composite, is_universal) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )?;
        for root in roots {
            let cand = root.join(&folder).join(format!("{}.xml", stem));
            if !cand.is_file() {
                continue;
            }
            match parse_object_attributes_file(&cand, &owner_full) {
                Ok(edges) => {
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
                Err(e) => {
                    tracing::warn!("update_data_links_for_object {}: {}", cand.display(), e)
                }
            }
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
pub(crate) fn update_object_attributes_for_object(
    conn: &rusqlite::Connection,
    roots: &[std::path::PathBuf],
    xml_path: &Path,
) -> Result<()> {
    let owner_full = match object_full_name_from_path(xml_path) {
        Some((_mt, full)) => full,
        None => return Ok(()),
    };
    // Папка (plural) и имя объекта — из пути изменённого XML; ищем копии этого
    // объекта во всех sub-config (`roots`, посчитаны один раз на пачку) и мерджим
    // (base-first).
    let folder = match xml_path.parent().and_then(|d| d.file_name()).and_then(|s| s.to_str()) {
        Some(s) => s.to_string(),
        None => return Ok(()),
    };
    let stem = match xml_path.file_stem().and_then(|s| s.to_str()) {
        Some(s) => s.to_string(),
        None => return Ok(()),
    };
    let json_opt =
        merged_object_structure(roots, &folder, &stem).map(|s| s.to_json().to_string());
    conn.execute(
        "UPDATE metadata_objects SET attributes_json = ? WHERE repo = ? AND full_name = ?",
        params![json_opt, REPO_DEFAULT, &owner_full],
    )?;
    Ok(())
}


/// Per-object upsert строки `metadata_objects` (перечень + синоним + владелец
/// `sub_config`) из шапки объектного XML. В отличие от `index_metadata_objects`
/// (полный DELETE repo + reinsert из Configuration.xml на триггере
/// `config_changed`), ведёт ОДИН объект точечно на объектном событии — закрывает
/// дыру «синоним/перечень изменённого объекта не обновляются, если в батче нет
/// Configuration.xml».
///
/// Синоним и владелец вычисляются ПЕРЕСБОРОМ по всем sub-config'ам объекта
/// (base-first), а не по одному пришедшему файлу — результат детерминирован при
/// любом порядке прихода событий (как в `update_object_attributes_for_object`).
/// `attributes_json` НЕ трогаем: его отдельно ведёт
/// `update_object_attributes_for_object`, `ON CONFLICT` его сохраняет.
///
/// Владелец: копия в base ИЛИ `Adopted` ИЛИ без тега `ObjectBelonging` → база
/// (`''`); только `Native` в расширении → путь расширения. Строку из воздуха не
/// создаём — если на диске нет ни одной копии (объект удалён), выходим (удаление
/// ведёт отдельная ветка Фазы 2).
pub(crate) fn upsert_metadata_object(
    repo_root: &Path,
    conn: &rusqlite::Connection,
    roots: &[std::path::PathBuf],
    xml_path: &Path,
) -> Result<()> {
    let (meta_type, full_name) = match object_full_name_any(xml_path) {
        Some(x) => x,
        None => return Ok(()),
    };
    let folder = match xml_path.parent().and_then(|d| d.file_name()).and_then(|s| s.to_str()) {
        Some(s) => s.to_string(),
        None => return Ok(()),
    };
    let stem = match xml_path.file_stem().and_then(|s| s.to_str()) {
        Some(s) => s.to_string(),
        None => return Ok(()),
    };

    let mut synonym: Option<String> = None;
    let mut owner = String::new();
    let mut owner_is_base = false;
    let mut any_copy = false;
    for sub_root in roots {
        let cand = sub_root.join(&folder).join(format!("{}.xml", stem));
        if !cand.is_file() {
            continue;
        }
        let content = match std::fs::read_to_string(&cand) {
            Ok(c) => c,
            Err(_) => continue,
        };
        any_copy = true;
        // Синоним base-first: первый непустой (roots идут base-first).
        if synonym.is_none() {
            if let Some((_mt, _nm, Some(s))) = parse_object_header_xml(&content) {
                if !s.is_empty() {
                    synonym = Some(s);
                }
            }
        }
        if owner_is_base {
            continue;
        }
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

    if !any_copy {
        return Ok(());
    }

    let _ = conn.execute("ROLLBACK", []); // защита от cascade-ошибки
    conn.execute("BEGIN", [])?;
    conn.execute(
        "INSERT INTO metadata_objects (repo, full_name, meta_type, name, synonym, sub_config) \
         VALUES (?, ?, ?, ?, ?, ?) \
         ON CONFLICT(repo, full_name) DO UPDATE SET \
             meta_type = excluded.meta_type, \
             name = excluded.name, \
             synonym = excluded.synonym, \
             sub_config = excluded.sub_config",
        params![REPO_DEFAULT, &full_name, meta_type, &stem, synonym, &owner],
    )?;
    conn.execute("COMMIT", [])?;
    Ok(())
}


/// Per-file обновление строки `metadata_forms` для одной формы по её Form.xml.
/// Слой `form_event` графа пересобирается отдельно (после всех форм батча).
pub(crate) fn update_metadata_forms_for_file(
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
                let handlers_json = crate::xml::forms::handlers_to_json(&handlers)?;
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
pub(crate) fn update_event_subscription_for_file(conn: &rusqlite::Connection, xml_path: &Path) -> Result<()> {
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
/// Изменение `ConfigDumpInfo.xml` (опись выгрузки) = триггер сверки состава
/// объектов области (Фаза 3): `reconcile_area` точечно diff-ит реестр
/// `config_manifest` и удаляет из индекса ТОЛЬКО пропавшие объекты (каскад по
/// дому / пере-сборка заимствователя). Добавленные и изменённые объекты
/// индексируются своим ходом пофайловыми ветками — квадратичный полный пересбор
/// метаданного слоя на каждую область больше не нужен.
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
    // Выгрузка 1C:EDT — своя раскладка и свои имена файлов. Разбор путей ниже
    // рассчитан на формат Конфигуратора (расширение `xml`, объект прямо внутри
    // папки типа, имена `Rights.xml` / `Form.xml` / `ConfigDumpInfo.xml`), и для
    // EDT не срабатывала НИ ОДНА ветка: слой метаданных не обновлялся вовсе,
    // до полной переиндексации (E-4).
    if crate::xml::edt_mdo::detect_edt_src(repo_root).is_some() {
        return run_incremental_extras_edt(repo_root, storage, changed, deleted);
    }
    let mut bsl_paths: Vec<&std::path::PathBuf> = Vec::new();
    let mut dump_info_areas: Vec<std::path::PathBuf> = Vec::new();
    let mut object_xmls: Vec<&std::path::PathBuf> = Vec::new();
    // Корневые объектные XML ВСЕХ типов верхнего уровня (надмножество object_xmls)
    // для точечного upsert перечня/синонима/владельца. Только changed (см. ниже).
    let mut all_object_xmls: Vec<&std::path::PathBuf> = Vec::new();
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
        } else if fname == "ConfigDumpInfo.xml" {
            // Опись выгрузки области — триггер сверки реестра config_manifest
            // (Фаза 3). Область = каталог описи (рядом с Configuration.xml).
            // Configuration.xml триггером БОЛЬШЕ не служит: источник истины о
            // составе объектов — опись, а не манифест. Дедуп областей батча.
            if let Some(area_root) = p.parent() {
                let ar = area_root.to_path_buf();
                if !dump_info_areas.contains(&ar) {
                    dump_info_areas.push(ar);
                }
            }
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

    // Перечень + синоним + владелец точечно на объектных событиях (ВСЕ типы
    // верхнего уровня, а не только object_xmls со структурой): закрывает дыру,
    // когда в батче нет Configuration.xml. Только changed — существующие файлы;
    // удаление объектов ведёт отдельная ветка (Фаза 2), а upsert по несуществующим
    // копиям и так самозащищён (any_copy).
    for p in changed.iter() {
        if object_full_name_any(p).is_some() {
            all_object_xmls.push(p);
        }
    }

    // Структурное изменение состава объектов (Configuration.xml в батче): мог
    // добавиться/удалиться/переименоваться объект. Пересобираем ТОЛЬКО лёгкий
    // XML-слой (перечень + структура + связи + права + формы + подписки + модули,
    // обход XML — секунды), а НЕ тяжёлый код-слой (термы ~260K / граф / usages).
    // Код-слой держат точечные update_*_for_file по .bsl этого батча (ниже),
    // поэтому здесь НЕ делаем return и НЕ зовём полный run_index_extras — это
    // убирает многоминутный re-enrichment на ходу (зависание daemon на bulk git).
    let conn = storage.conn();
    // sub_config_roots (обход дерева repo) считаем ОДИН раз на пачку и прокидываем
    // в пофайловые функции — иначе каждая пере-сборка объекта делала бы свой обход
    // (до трёх на объект). Ленивое: только когда есть объектные/описные события,
    // иначе пачка «только .bsl» не платила бы за лишний обход дерева.
    let need_roots =
        !dump_info_areas.is_empty() || !all_object_xmls.is_empty() || !object_xmls.is_empty();
    let roots: Vec<std::path::PathBuf> =
        if need_roots { sub_config_roots(repo_root) } else { Vec::new() };
    // Фаза 3: сверка затронутых областей по свежей описи вместо квадратичного
    // полного пересбора метаданного слоя. Каждая область → точечный diff реестра
    // config_manifest; индексные действия — ТОЛЬКО на удалении объектов (каскад
    // по дому / пере-сборка заимствователя). Добавление и изменение объектов
    // приезжает своим ходом через пофайловые ветки ниже (upsert_metadata_object
    // / update_*_for_*). reconcile_area ведёт собственные транзакции, поэтому
    // безопасен до пофайловых веток; home объекта читается ДО их правок.
    for area_root in &dump_info_areas {
        match reconcile_area(repo_root, conn, &roots, area_root) {
            Ok(s) => tracing::info!(
                "reconcile_area {}: +{} ~{} -{} (объектов удалено {}, пере-собрано {})",
                area_root.display(), s.added, s.updated, s.removed,
                s.deleted_objects, s.remerged_objects,
            ),
            Err(e) => tracing::warn!("reconcile_area {}: {}", area_root.display(), e),
        }
    }
    // Точечный upsert перечня/синонима/владельца по каждому изменённому объекту.
    // Идёт ПОСЛЕ config_changed-блока: если тот сделал полный reinsert (sub_config
    // там всегда '' — Configuration.xml не знает ObjectBelonging), upsert поверх
    // выставит корректного владельца Native-объектам.
    for p in &all_object_xmls {
        if let Err(e) = upsert_metadata_object(repo_root, conn, &roots, p) {
            tracing::warn!("upsert_metadata_object {}: {}", p.display(), e);
        }
    }
    for p in &object_xmls {
        update_data_links_for_object(conn, &roots, p)?;
        update_object_attributes_for_object(conn, &roots, p)?;
    }
    for p in &form_xmls {
        update_metadata_forms_for_file(repo_root, conn, p)?;
    }
    for p in &sub_xmls {
        update_event_subscription_for_file(conn, p)?;
    }
    // .bsl — точечный per-file апдейт слоя direct (O(рёбер файла)) + обратного
    // индекса использований объектов МД в коде (metadata_code_usages).
    // Кэш configVersion по sub-config на время батча — ConfigDumpInfo.xml один
    // на sub-config, парсить его на каждый .bsl большой пачки расточительно.
    let mut cfgver_cache: std::collections::HashMap<
        std::path::PathBuf,
        std::collections::HashMap<String, String>,
    > = std::collections::HashMap::new();
    for p in &bsl_paths {
        update_call_graph_direct_for_file(repo_root, conn, p)?;
        update_code_usages_for_file(repo_root, conn, p)?;
        update_procedure_terms_for_file(repo_root, conn, p)?;
        // Точечное ведение metadata_modules (dbgs): завести/обновить строку
        // модуля этого .bsl. Superset при живом config_changed-триггере.
        if let Err(e) = update_metadata_module_for_file(repo_root, conn, p, &mut cfgver_cache) {
            tracing::warn!("update_metadata_module_for_file {}: {}", p.display(), e);
        }
    }
    // Этап 4e ОДИН раз на батч: резолв callee_proc_key новых direct-рёбер +
    // отсев балласта (тот же хелпер, что в полном пересборе). Пофайловый цикл
    // выше кладёт лишь сырые рёбра с NULL-адресом (дёшево, по индексам); тяжёлый
    // глобальный резолв (build_common_module_methods + resolve_* + prune_*) идёт
    // здесь единожды — вместо N-кратного повтора на каждый файл (была
    // квадратичная деградация на bulk-батче в тысячи файлов).
    if !bsl_paths.is_empty() {
        let _ = conn.execute("ROLLBACK", []);
        conn.execute("BEGIN", [])?;
        resolve_and_prune_direct_edges(conn)?;
        conn.execute("COMMIT", [])?;
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
    // Освежить статистику планировщика, если графовые таблицы (data_links /
    // proc_call_graph) разъехались со статистикой в ≥1.5× (например, bulk-залив
    // расширений). Только когда рёбра реально могли измениться в этом батче.
    if !dump_info_areas.is_empty() || !bsl_paths.is_empty() || !object_xmls.is_empty() || refs_dirty {
        if let Err(e) = maybe_analyze_graph_tables(conn) {
            tracing::warn!("maybe_analyze_graph_tables: {}", e);
        }
    }
    Ok(())
}


/// Итог сверки одной области: сколько строк реестра добавлено/обновлено/убрано и
/// сколько объектов реально удалено каскадом / пере-собрано после ухода
/// заимствователя. Для логов и тестов.
#[derive(Default, Debug)]
pub(crate) struct ReconcileStats {
    pub(crate) added: usize,
    pub(crate) updated: usize,
    pub(crate) removed: usize,
    pub(crate) deleted_objects: usize,
    pub(crate) remerged_objects: usize,
}


/// Метаданные объекта по его singular full_name (`Catalog.X`) из
/// `metadata_objects`: (meta_type, name-stem, дом = `sub_config`). `None` — если
/// строки нет (значит full_name — под-элемент/модуль, а не самостоятельный объект).
pub(crate) fn lookup_object_meta(
    conn: &rusqlite::Connection,
    full_name: &str,
) -> Option<(String, String, String)> {
    match conn.query_row(
        "SELECT meta_type, name, sub_config FROM metadata_objects WHERE repo = ? AND full_name = ?",
        params![REPO_DEFAULT, full_name],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        },
    ) {
        Ok(v) => Some(v),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(e) => {
            tracing::warn!("lookup_object_meta {}: {}", full_name, e);
            None
        }
    }
}


/// Каскадное удаление объекта — когда его уронила ДОМАШНЯЯ область (реальное
/// удаление). Сносит: строку объекта, связи данных в обе стороны, формы, модули
/// и ВСЕ строки объекта (сам + под-элементы) во ВСЕХ областях реестра. Роли и
/// конфиг-связи не трогаем — их ведут отдельные проходы. Граф вызовов модулей
/// чистится по удалению .bsl-файлов (direct_edge_files).
pub(crate) fn delete_object_cascade(
    conn: &rusqlite::Connection,
    full_name: &str,
    meta_type: &str,
    name: &str,
) -> Result<()> {
    let _ = conn.execute("ROLLBACK", []);
    conn.execute("BEGIN", [])?;
    conn.execute(
        "DELETE FROM metadata_objects WHERE repo = ? AND full_name = ?",
        params![REPO_DEFAULT, full_name],
    )?;
    conn.execute(
        "DELETE FROM data_links WHERE repo = ? AND (from_object = ? OR to_object = ?)",
        params![REPO_DEFAULT, full_name, full_name],
    )?;
    if let Some(folder) = plural_folder(meta_type) {
        let plural = format!("{}.{}", folder, name);
        conn.execute(
            "DELETE FROM metadata_forms WHERE repo = ? AND owner_full_name = ?",
            params![REPO_DEFAULT, &plural],
        )?;
        conn.execute(
            "DELETE FROM metadata_modules WHERE repo = ? AND object_name = ?",
            params![REPO_DEFAULT, &plural],
        )?;
    }
    // Сам объект + его под-элементы/модули (`X.*`) во ВСЕХ областях реестра.
    conn.execute(
        "DELETE FROM config_manifest WHERE repo = ? AND (full_name = ? OR full_name LIKE ? ESCAPE '\\')",
        params![REPO_DEFAULT, full_name, format!("{}.%", like_escape(full_name))],
    )?;
    conn.execute("COMMIT", [])?;
    Ok(())
}


/// Пере-сборка объекта после ухода ЗАИМСТВОВАТЕЛЯ (дом жив): объект не удаляем,
/// перечитываем перечень/синоним/владельца и структуру реквизитов по ОСТАВШИМСЯ
/// на диске областям (упавшая копия расширения уже исчезла с диска, мердж её не
/// подхватит). Переиспользует пофайловые `upsert_metadata_object` +
/// `update_object_attributes_for_object` (каждая ведёт свою транзакцию).
pub(crate) fn remerge_object(
    repo_root: &Path,
    conn: &rusqlite::Connection,
    roots: &[std::path::PathBuf],
    meta_type: &str,
    name: &str,
    cfgver_cache: &mut std::collections::HashMap<
        std::path::PathBuf,
        std::collections::HashMap<String, String>,
    >,
) -> Result<()> {
    let folder = match plural_folder(meta_type) {
        Some(f) => f,
        None => return Ok(()),
    };
    let rel = format!("{}/{}.xml", folder, name);
    let xml_path = roots
        .iter()
        .map(|root| root.join(&rel))
        .find(|p| p.is_file());
    let xml_path = match xml_path {
        Some(p) => p,
        None => return Ok(()), // корневого XML нет (объект без структуры) — пере-сливать нечего
    };
    upsert_metadata_object(repo_root, conn, roots, &xml_path)?;
    update_object_attributes_for_object(conn, roots, &xml_path)?;
    // data_links симметрично attributes_json: пересобрать по оставшимся копиям.
    // Первично для ухода заимствователя, когда delete объектного XML watcher-ом
    // не доставлен (надёжный сигнал — MODIFY описи → сюда). Идемпотентно при
    // повторной пересборке в этой же пачке.
    update_data_links_for_object(conn, roots, &xml_path)?;
    // metadata_modules симметрично: пересобрать модули объекта по оставшимся
    // копиям (DELETE + обход .bsl во всех roots). Без этого config_version
    // модулей формы заимствователя устаревал бы при уходе — расхождение с полным
    // reindex, пойман федеративным smoke на типовой торговой конфигурации.
    update_metadata_modules_for_object(repo_root, conn, roots, &xml_path, cfgver_cache)?;
    Ok(())
}


/// Сверка ОДНОЙ области: свежая опись `ConfigDumpInfo.xml` ↔ строки этой области
/// в реестре. Появились/изменились → правим ТОЛЬКО реестр (объектные файлы
/// приедут своим ходом через пофайловую обработку). Пропали → приводим реестр к
/// свежему виду, а по индексу действуем ТОЛЬКО для пропавших ОБЪЕКТОВ по правилу
/// дома (`metadata_objects.sub_config`): дом уронил → каскадное удаление;
/// заимствователь уронил → пере-сборка. Пропавшие под-элементы — только реестр
/// (их объект-владелец чинит своя пофайловая обработка, Вариант А).
pub(crate) fn reconcile_area(
    repo_root: &Path,
    conn: &rusqlite::Connection,
    roots: &[std::path::PathBuf],
    area_root: &Path,
) -> Result<ReconcileStats> {
    let area = compute_extension_name(repo_root, area_root);
    // Опись могла исчезнуть (область целиком удалена в этом батче) → трактуем как
    // пустую: все прежние строки области — пропавшие, объекты чистятся по дому.
    let fresh: std::collections::HashMap<String, String> =
        if area_root.join("ConfigDumpInfo.xml").is_file() {
            parse_config_dump_info_rows(area_root)?.into_iter().collect()
        } else {
            std::collections::HashMap::new()
        };

    let mut old: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT full_name, config_version FROM config_manifest WHERE repo = ? AND area = ?",
        )?;
        let rows = stmt.query_map(params![REPO_DEFAULT, &area], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (fnm, cv) = row?;
            old.insert(fnm, cv);
        }
    }

    let mut stats = ReconcileStats::default();
    // Кэш описей sub-config'ов на всю область — общий для пере-сборки
    // metadata_modules всех объектов (parse_config_dump_info дорогой, не читаем
    // одну опись повторно для каждого объекта/модуля).
    let mut cfgver_cache: std::collections::HashMap<
        std::path::PathBuf,
        std::collections::HashMap<String, String>,
    > = std::collections::HashMap::new();

    // 1) Пропавшие строки — индексные действия только для объектов (по дому).
    //    Каскад/пере-сборка ведут собственные транзакции, поэтому делаем их ДО
    //    транзакции синхронизации реестра (шаг 2).
    for full_name in old.keys() {
        if fresh.contains_key(full_name) {
            continue;
        }
        stats.removed += 1;
        if let Some((meta_type, name, home)) = lookup_object_meta(conn, full_name) {
            if area == home {
                delete_object_cascade(conn, full_name, &meta_type, &name)?;
                stats.deleted_objects += 1;
            } else {
                remerge_object(repo_root, conn, roots, &meta_type, &name, &mut cfgver_cache)?;
                stats.remerged_objects += 1;
            }
        }
        // под-элемент/модуль → никаких индексных действий (Вариант А), только реестр ниже
    }

    // 2) Синхронизация реестра ЭТОЙ области под свежую опись (точечный diff).
    let _ = conn.execute("ROLLBACK", []);
    conn.execute("BEGIN", [])?;
    {
        let mut del = conn.prepare(
            "DELETE FROM config_manifest WHERE repo = ? AND area = ? AND full_name = ?",
        )?;
        for full_name in old.keys() {
            if !fresh.contains_key(full_name) {
                del.execute(params![REPO_DEFAULT, &area, full_name])?;
            }
        }
    }
    {
        let mut ins = conn.prepare(
            "INSERT INTO config_manifest (repo, area, full_name, config_version) \
             VALUES (?, ?, ?, ?) \
             ON CONFLICT(repo, area, full_name) DO UPDATE SET config_version = excluded.config_version",
        )?;
        for (full_name, cv) in &fresh {
            match old.get(full_name) {
                Some(oldcv) if oldcv == cv => {}
                Some(_) => {
                    ins.execute(params![REPO_DEFAULT, &area, full_name, cv])?;
                    stats.updated += 1;
                }
                None => {
                    ins.execute(params![REPO_DEFAULT, &area, full_name, cv])?;
                    stats.added += 1;
                }
            }
        }
    }
    conn.execute("COMMIT", [])?;

    Ok(stats)
}


/// Per-file обновление `metadata_code_usages` для одного `.bsl`: снести прежние
/// строки файла и переразобрать (или просто снести, если файл удалён).
pub(crate) fn update_code_usages_for_file(
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


/// Per-file обновление механических термов для одного `.bsl`: снести свои
/// (`mech:%`) строки файла и пересобрать по текущему состоянию `functions`
/// (или просто снести, если файл удалён). LLM-строки не трогаются.
pub(crate) fn update_procedure_terms_for_file(
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
    // proc_key имеет вид '<rel>::<proc>'. Прежнее `proc_key LIKE ?||'::%'` делало
    // SCAN всей procedure_enrichment (LIKE в SQLite case-insensitive → индекс
    // idx_pe_proc_key НЕ используется). На массовом deleted-батче это квадратично:
    // каждый удалённый .bsl сканировал ~260K строк (1446 файлов git-pull → минуты).
    // Range по префиксу '<rel>::' идёт через индекс (SEARCH вместо SCAN): нижняя
    // граница '<rel>::', верхняя '<rel>:;' (последний ':' префикса +1 = ';').
    // signature LIKE 'mech:%' остаётся доп-фильтром уже на строках одного файла.
    let pk_lo = format!("{}::", rel);
    let pk_hi = format!("{}:;", rel);
    conn.execute(
        "DELETE FROM procedure_enrichment \
         WHERE repo = ?1 AND proc_key >= ?2 AND proc_key < ?3 AND signature LIKE 'mech:%'",
        params![REPO_DEFAULT, &pk_lo, &pk_hi],
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

// ── Инкрементальное обновление для выгрузки 1C:EDT ───────────────────────────
//
// Роль служебной описи выгрузки здесь играет само дерево файлов: объект
// считается удалённым, когда исчезает его `<Имя>.mdo`. Отдельного реестра
// областей в этом формате не существует, поэтому сверять нечего и не с чем.

/// Тип метаданных по имени папки выгрузки (`Catalogs` → `Catalog`).
fn edt_meta_type_by_folder(folder: &str) -> Option<&'static str> {
    ALL_OBJECT_FOLDERS
        .iter()
        .find(|(f, _)| *f == folder)
        .map(|(_, t)| *t)
}

/// Разобрать путь описания объекта `<Папка типа>/<Имя>/<Имя>.mdo`.
/// Возвращает `(папка типа, имя объекта)`. Описание самой конфигурации и
/// прочие `.mdo` вне папок типов отсеиваются.
fn edt_object_from_mdo(path: &Path) -> Option<(String, String)> {
    let stem = path.file_stem()?.to_str()?;
    let obj_dir = path.parent()?;
    if obj_dir.file_name()?.to_str()? != stem {
        return None;
    }
    let folder = obj_dir.parent()?.file_name()?.to_str()?;
    edt_meta_type_by_folder(folder)?;
    Some((folder.to_string(), stem.to_string()))
}

/// Владелец и имя формы по пути `Form.form`: форма объекта лежит в
/// `<Папка типа>/<Объект>/Forms/<Форма>/Form.form`, общая форма — сама объект
/// (`CommonForms/<Имя>/Form.form`).
fn edt_form_owner(path: &Path) -> Option<(String, String)> {
    let segs: Vec<&str> = path
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();
    if let Some(i) = segs.iter().rposition(|s| *s == "Forms") {
        if i < 2 {
            return None;
        }
        let form_name = segs.get(i + 1)?;
        Some((
            format!("{}.{}", segs[i - 2], segs[i - 1]),
            form_name.to_string(),
        ))
    } else {
        let i = segs.iter().rposition(|s| *s == "CommonForms")?;
        let name = segs.get(i + 1)?;
        Some((format!("CommonForms.{}", name), name.to_string()))
    }
}

/// Имя роли по пути `Roles/<Имя>/Rights.rights`.
fn edt_role_name(path: &Path) -> Option<String> {
    let role_dir = path.parent()?;
    let role = role_dir.file_name()?.to_str()?;
    if role_dir.parent()?.file_name()?.to_str()? != "Roles" {
        return None;
    }
    Some(role.to_string())
}

/// Завести/обновить один объект EDT по его `.mdo`: строка перечня, структура,
/// связи данных (объектные и конфигурационные), подписка на событие.
fn upsert_edt_object(
    conn: &rusqlite::Connection,
    mdo_path: &Path,
    obj_name: &str,
) -> Result<()> {
    use crate::xml::edt_mdo;

    let content = match std::fs::read_to_string(mdo_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("edt upsert read {}: {}", mdo_path.display(), e);
            return Ok(());
        }
    };
    let (meta_type, synonym) = match edt_mdo::parse_mdo_header(&content) {
        Some((mt, _name, syn)) => (mt, syn),
        None => return Ok(()),
    };
    let full_name = format!("{}.{}", meta_type, obj_name);
    let attributes_json = match edt_mdo::parse_mdo_structure_xml(&content) {
        Ok(s) if !s.is_empty() => Some(s.to_json().to_string()),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!("edt structure {}: {}", mdo_path.display(), e);
            None
        }
    };

    let _ = conn.execute("ROLLBACK", []);
    conn.execute("BEGIN", [])?;
    conn.execute(
        "INSERT INTO metadata_objects \
         (repo, full_name, meta_type, name, synonym, attributes_json) \
         VALUES (?, ?, ?, ?, ?, ?) \
         ON CONFLICT(repo, full_name) DO UPDATE SET \
             meta_type = excluded.meta_type, \
             name = excluded.name, \
             synonym = excluded.synonym, \
             attributes_json = excluded.attributes_json",
        params![
            REPO_DEFAULT,
            &full_name,
            &meta_type,
            obj_name,
            synonym,
            attributes_json
        ],
    )?;

    // Связи данных объекта пересобираются целиком: и объектные, и
    // конфигурационные лежат в этом же файле.
    conn.execute(
        "DELETE FROM data_links WHERE repo = ? AND from_object = ?",
        params![REPO_DEFAULT, &full_name],
    )?;
    {
        let mut ins_link = conn.prepare(
            "INSERT OR IGNORE INTO data_links \
             (repo, from_object, from_path, to_object, link_kind, is_composite, is_universal) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )?;
        match edt_mdo::parse_mdo_datalinks_xml(&content) {
            Ok(edges) => {
                for edge in edges {
                    ins_link.execute(params![
                        REPO_DEFAULT,
                        &full_name,
                        &edge.from_path,
                        &edge.to_object,
                        edge.link_kind,
                        edge.is_composite as i64,
                        edge.is_universal as i64,
                    ])?;
                }
            }
            Err(e) => tracing::warn!("edt data_links {}: {}", mdo_path.display(), e),
        }
        for (kind, to_object, from_path, is_composite, is_universal) in
            edt_mdo::parse_mdo_config_refs(&meta_type, &content)
        {
            ins_link.execute(params![
                REPO_DEFAULT,
                &full_name,
                &from_path,
                &to_object,
                kind,
                is_composite as i64,
                is_universal as i64,
            ])?;
        }
    }

    if meta_type == "EventSubscription" {
        conn.execute(
            "DELETE FROM event_subscriptions WHERE repo = ? AND name = ?",
            params![REPO_DEFAULT, obj_name],
        )?;
        if let Some((nm, ev, module, proc_, sources)) =
            edt_mdo::parse_mdo_event_subscription(&content)
        {
            let sources_json = serde_json::to_string(&sources)?;
            conn.execute(
                "INSERT OR IGNORE INTO event_subscriptions \
                 (repo, name, event, handler_module, handler_proc, sources_json) \
                 VALUES (?, ?, ?, ?, ?, ?)",
                params![REPO_DEFAULT, &nm, &ev, &module, &proc_, &sources_json],
            )?;
        }
    }
    backfill_data_link_keys(conn)?;
    conn.execute("COMMIT", [])?;
    Ok(())
}

/// Убрать объект EDT, чей `.mdo` исчез: строку перечня, его связи, формы,
/// модули и подписку.
fn delete_edt_object(
    conn: &rusqlite::Connection,
    folder: &str,
    obj_name: &str,
) -> Result<()> {
    let meta_type = match edt_meta_type_by_folder(folder) {
        Some(t) => t,
        None => return Ok(()),
    };
    let full_name = format!("{}.{}", meta_type, obj_name);
    let owner_key = format!("{}.{}", folder, obj_name);

    let _ = conn.execute("ROLLBACK", []);
    conn.execute("BEGIN", [])?;
    conn.execute(
        "DELETE FROM metadata_objects WHERE repo = ? AND full_name = ?",
        params![REPO_DEFAULT, &full_name],
    )?;
    conn.execute(
        "DELETE FROM data_links WHERE repo = ? AND from_object = ?",
        params![REPO_DEFAULT, &full_name],
    )?;
    conn.execute(
        "DELETE FROM metadata_forms WHERE repo = ? AND owner_full_name = ?",
        params![REPO_DEFAULT, &owner_key],
    )?;
    // Модули объекта: и собственные (`Catalogs.X.ObjectModule`), и модули его
    // форм и команд (`Catalogs.X.Form.Y.FormModule`).
    conn.execute(
        "DELETE FROM metadata_modules WHERE repo = ? AND (object_name = ? OR object_name LIKE ?)",
        params![REPO_DEFAULT, &owner_key, format!("{}.%", owner_key)],
    )?;
    if meta_type == "EventSubscription" {
        conn.execute(
            "DELETE FROM event_subscriptions WHERE repo = ? AND name = ?",
            params![REPO_DEFAULT, obj_name],
        )?;
    }
    if meta_type == "Role" {
        conn.execute(
            "DELETE FROM role_rights WHERE repo = ? AND role_name = ?",
            params![REPO_DEFAULT, obj_name],
        )?;
    }
    conn.execute("COMMIT", [])?;
    Ok(())
}

/// Обновить одну форму EDT по её `Form.form` (или убрать, если файл исчез).
fn update_edt_form(conn: &rusqlite::Connection, form_path: &Path) -> Result<()> {
    let (owner, form_name) = match edt_form_owner(form_path) {
        Some(v) => v,
        None => return Ok(()),
    };
    let _ = conn.execute("ROLLBACK", []);
    conn.execute("BEGIN", [])?;
    conn.execute(
        "DELETE FROM metadata_forms WHERE repo = ? AND owner_full_name = ? AND form_name = ?",
        params![REPO_DEFAULT, &owner, &form_name],
    )?;
    if let Ok(content) = std::fs::read_to_string(form_path) {
        let handlers = crate::xml::edt_mdo::parse_mdo_form_handlers(&content);
        let handlers_json = crate::xml::forms::handlers_to_json(&handlers)?;
        conn.execute(
            "INSERT OR IGNORE INTO metadata_forms (repo, owner_full_name, form_name, handlers_json) \
             VALUES (?, ?, ?, ?)",
            params![REPO_DEFAULT, &owner, &form_name, &handlers_json],
        )?;
    }
    conn.execute("COMMIT", [])?;
    Ok(())
}

/// Обновить права одной роли EDT по её `Rights.rights` (или убрать, если файл
/// исчез). Точечно: пересборка всей таблицы прав тут не нужна.
fn update_edt_role_rights(conn: &rusqlite::Connection, rights_path: &Path) -> Result<()> {
    let role_name = match edt_role_name(rights_path) {
        Some(v) => v,
        None => return Ok(()),
    };
    let _ = conn.execute("ROLLBACK", []);
    conn.execute("BEGIN", [])?;
    conn.execute(
        "DELETE FROM role_rights WHERE repo = ? AND role_name = ?",
        params![REPO_DEFAULT, &role_name],
    )?;
    if rights_path.is_file() {
        match crate::xml::metadata_refs::parse_role_rights_file(rights_path) {
            Ok(rights) => {
                let mut stmt = conn.prepare(
                    "INSERT OR IGNORE INTO role_rights (repo, role_name, object_name, right_name) \
                     VALUES (?, ?, ?, ?)",
                )?;
                for r in rights {
                    stmt.execute(params![
                        REPO_DEFAULT,
                        &role_name,
                        &r.object_name,
                        &r.right_name
                    ])?;
                }
            }
            Err(e) => tracing::warn!("edt role_rights {}: {}", rights_path.display(), e),
        }
    }
    backfill_role_right_keys(conn)?;
    conn.execute("COMMIT", [])?;
    Ok(())
}

/// Инкрементальное обновление extras для батча выгрузки 1C:EDT.
pub(crate) fn run_incremental_extras_edt(
    repo_root: &Path,
    storage: &mut Storage,
    changed: &[std::path::PathBuf],
    deleted: &[std::path::PathBuf],
) -> Result<()> {
    let mut bsl_changed: Vec<&std::path::PathBuf> = Vec::new();
    let mut bsl_deleted: Vec<&std::path::PathBuf> = Vec::new();
    let mut objects: Vec<(&std::path::PathBuf, String, String)> = Vec::new();
    let mut removed_objects: Vec<(String, String)> = Vec::new();
    let mut forms: Vec<&std::path::PathBuf> = Vec::new();
    let mut roles: Vec<&std::path::PathBuf> = Vec::new();

    let classify = |p: &std::path::PathBuf| -> (&'static str, Option<(String, String)>) {
        let fname = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext.eq_ignore_ascii_case("bsl") {
            ("bsl", None)
        } else if fname == "Form.form" {
            ("form", None)
        } else if fname == "Rights.rights" {
            ("rights", None)
        } else if ext == "mdo" {
            match edt_object_from_mdo(p) {
                Some(v) => ("object", Some(v)),
                None => ("skip", None),
            }
        } else {
            ("skip", None)
        }
    };

    for p in changed {
        match classify(p) {
            ("bsl", _) => bsl_changed.push(p),
            ("form", _) => forms.push(p),
            ("rights", _) => roles.push(p),
            ("object", Some((folder, name))) => objects.push((p, folder, name)),
            _ => {}
        }
    }
    for p in deleted {
        match classify(p) {
            ("bsl", _) => bsl_deleted.push(p),
            ("form", _) => forms.push(p),
            ("rights", _) => roles.push(p),
            ("object", Some((folder, name))) => removed_objects.push((folder, name)),
            _ => {}
        }
    }

    let conn = storage.conn();

    for (folder, name) in &removed_objects {
        if let Err(e) = delete_edt_object(conn, folder, name) {
            tracing::warn!("edt delete {}.{}: {}", folder, name, e);
        }
    }
    for (path, _folder, name) in &objects {
        if let Err(e) = upsert_edt_object(conn, path, name) {
            tracing::warn!("edt upsert {}: {}", path.display(), e);
        }
    }
    for p in &forms {
        if let Err(e) = update_edt_form(conn, p) {
            tracing::warn!("edt form {}: {}", p.display(), e);
        }
    }
    for p in &roles {
        if let Err(e) = update_edt_role_rights(conn, p) {
            tracing::warn!("edt rights {}: {}", p.display(), e);
        }
    }

    // Код-слой: те же точечные обновления, что и у формата Конфигуратора —
    // они разбирают содержимое `.bsl`, а не раскладку выгрузки.
    for p in &bsl_changed {
        update_call_graph_direct_for_file(repo_root, conn, p)?;
        update_code_usages_for_file(repo_root, conn, p)?;
        update_procedure_terms_for_file(repo_root, conn, p)?;
        if let Err(e) = update_metadata_module_for_file_edt(repo_root, conn, p) {
            tracing::warn!("edt module {}: {}", p.display(), e);
        }
    }
    for p in &bsl_deleted {
        let code_path = p
            .strip_prefix(repo_root)
            .unwrap_or(p)
            .to_string_lossy()
            .replace('\\', "/");
        conn.execute(
            "DELETE FROM metadata_modules WHERE repo = ? AND code_path = ?",
            params![REPO_DEFAULT, &code_path],
        )?;
    }

    if !bsl_changed.is_empty() {
        let _ = conn.execute("ROLLBACK", []);
        conn.execute("BEGIN", [])?;
        resolve_and_prune_direct_edges(conn)?;
        conn.execute("COMMIT", [])?;
        rebuild_call_graph_extension_override(conn)?;
    }
    if !forms.is_empty() {
        rebuild_call_graph_form_event(conn)?;
    }
    if !objects.is_empty() || !removed_objects.is_empty() {
        rebuild_call_graph_subscription(conn)?;
    }
    if !bsl_changed.is_empty() || !objects.is_empty() || !removed_objects.is_empty() {
        if let Err(e) = maybe_analyze_graph_tables(conn) {
            tracing::warn!("maybe_analyze_graph_tables: {}", e);
        }
    }
    Ok(())
}
