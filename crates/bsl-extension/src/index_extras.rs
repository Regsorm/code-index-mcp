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

mod call_graph;
mod common;
mod dcs;
mod exported;
mod full_scan;
mod harvest;
mod incremental;
mod modules;
mod resume;
mod scan;

pub(crate) use call_graph::*;
pub(crate) use common::*;
pub(crate) use dcs::*;
pub(crate) use exported::*;
pub(crate) use full_scan::*;
pub(crate) use harvest::*;
pub(crate) use incremental::*;
pub(crate) use modules::*;
pub(crate) use resume::*;
pub(crate) use scan::*;

/// Выполнить одну фазу надстройки, отметив её длительность для итоговой
/// строки по папке. Ошибка фазы не фатальна: базовая индексация уже сохранена,
/// поэтому журналируем предупреждение и идём дальше — как было и до отметок.
/// Возвращает `true`, если фаза отработала без ошибки (тогда её можно считать
/// собранной при возобновлении).
///
/// `name` — короткое имя для раскладки в итоге, `label` — прежняя метка
/// предупреждения (по ней ищут в журнале, менять нельзя).
fn phase(name: &'static str, label: &str, f: impl FnOnce() -> Result<()>) -> bool {
    let started = std::time::Instant::now();
    // Отметка «занят этим» — по ней строка состояния демона отвечает, чем
    // папка занята сейчас: слои надстройки идут минутами, и без отметки
    // журнал молчит от начала слоя до его конца.
    code_index_core::logging::stage_begin(name);
    let outcome = f();
    code_index_core::logging::stage_done(name, started.elapsed());
    match outcome {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!("{}: {}", label, e);
            false
        }
    }
}

/// Фаза с поддержкой возобновления: если её результат уже собран для текущего
/// входа (см. `resume.rs`) — пропускается, иначе выполняется, и при успехе
/// отметка сдвигается. Ошибка фазы не фатальна (как и у [`phase`]).
fn resumable_phase(
    resume: Option<&ExtrasResume>,
    conn: &rusqlite::Connection,
    idx: u32,
    name: &'static str,
    label: &str,
    f: impl FnOnce() -> Result<()>,
) {
    if let Some(resume) = resume {
        if resume.is_done(idx) {
            tracing::info!(
                "надстройка: фаза «{}» уже собрана в прошлом проходе — пропускаю",
                name
            );
            return;
        }
    }
    if phase(name, label, f) {
        mark_phase(resume, conn, idx, name);
    }
}

/// Отметить фазу собранной. Провал записи отметки не фатален: это лишь потеря
/// возобновляемости на следующем обрыве.
fn mark_phase(resume: Option<&ExtrasResume>, conn: &rusqlite::Connection, idx: u32, name: &str) {
    if let Some(resume) = resume {
        if let Err(e) = resume.mark(conn, idx + 1) {
            tracing::warn!("надстройка: не сохранена отметка фазы «{}»: {}", name, e);
        }
    }
}

/// Запустить полный проход по репо и заполнить специфичные таблицы.
/// Реализация публичная, чтобы её можно было звать из тестов.
pub fn run_index_extras(repo_root: &Path, storage: &mut Storage) -> Result<()> {
    let conn = storage.conn();

    // Отметка «слой дошёл до конца» снимается на время работы: если процесс
    // убьют посередине, демон на следующем старте увидит оборванную надстройку
    // и повторит полный проход, а не отдаст неполные данные как готовые.
    if let Err(e) = storage.set_extras_build_complete(false) {
        tracing::warn!("не снята отметка завершённости надстройки: {}", e);
    }

    // Один обход дерева на весь слой и отпечаток входа для возобновления:
    // прерванный проход продолжается с последней собранной фазы.
    let scan = RepoScan::build(repo_root);
    let resume = ExtrasResume::load(conn, scan_fingerprint(conn, &scan));
    if resume.done() > 0 && resume.done() < EXTRAS_PHASE_COUNT {
        tracing::info!(
            "надстройка: прошлый проход оборвался — продолжаю с фазы {} из {}",
            resume.done() + 1,
            EXTRAS_PHASE_COUNT
        );
    }

    // XML-слой обогащения (перечень, структура, связи, права, формы, подписки,
    // модули) — обход XML выгрузки, дёшево.
    run_index_extras_metadata_layer(repo_root, &scan, conn, Some(&resume))?;

    // КОД-слой (тяжёлый: обратный индекс использований по всему .bsl, термы по
    // сотням тысяч процедур, полный граф вызовов). На инкрементальном пути НЕ
    // вызывается — его держат точечные update_*_for_file по .bsl батча.
    // Обратный индекс использований объектов МД в коде (.bsl) → metadata_code_usages.
    // Если parse-collector уже наполнил слой в этом проходе (полная переиндексация
    // bsl-indexer), повторный disk-rebuild пропускаем — данные идентичны.
    if crate::parse_collector::collector_did(conn, crate::parse_collector::MARK_CODE_USAGES) {
        tracing::info!("metadata_code_usages: наполнено parse-collector'ом, disk-rebuild пропущен");
        mark_phase(Some(&resume), conn, PH_CODE_USAGES, "использования в коде");
    } else {
        resumable_phase(
            Some(&resume),
            conn,
            PH_CODE_USAGES,
            "использования в коде",
            "metadata_code_usages",
            || index_metadata_code_usages(repo_root, conn),
        );
    }
    // Механические термы процедур (имя + объект + синоним + комментарий) —
    // после синонимов (использует metadata_objects.synonym, заполнен в слое).
    // Если parse-collector собрал сырьё в staging (полная переиндексация
    // bsl-indexer) — строим из него, без повторного чтения .bsl с диска;
    // иначе полный disk-rebuild (инкремент / публичный путь).
    if crate::parse_collector::collector_did(conn, crate::parse_collector::MARK_PROC_TERMS) {
        resumable_phase(
            Some(&resume),
            conn,
            PH_PROC_TERMS,
            "термины процедур",
            "procedure_terms (staging)",
            || build_procedure_terms_from_staging(conn),
        );
    } else {
        resumable_phase(
            Some(&resume),
            conn,
            PH_PROC_TERMS,
            "термины процедур",
            "procedure_terms",
            || index_procedure_terms(repo_root, conn),
        );
    }
    // Граф вызовов строится ПОСЛЕ заполнения metadata_forms и event_subscriptions
    // (они в XML-слое выше) — он опирается на их содержимое.
    resumable_phase(
        Some(&resume),
        conn,
        PH_CALL_GRAPH,
        "граф вызовов",
        "proc_call_graph",
        || build_call_graph(conn),
    );
    // ANALYZE: без статистики SQLite в рекурсивном шаге find_path_bsl/
    // find_data_path использует лишь префикс индекса (repo=) и сканирует
    // все рёбра repo на каждой итерации (depth=3 ~240с на КА1.1). После
    // ANALYZE планировщик знает селективность (~5 рёбер на caller_proc_key)
    // и берёт seek по двум столбцам → depth=3 падает до ~0.05с. Хинт
    // INDEXED BY это НЕ чинит — решает только статистика. Графы строятся
    // заново при каждом reindex (DELETE+INSERT), поэтому ANALYZE здесь, в
    // конце прохода, освежает статистику синхронно с ними (~0.6с на 2.4ГБ).
    // Фаза не резюмируется намеренно: она дешёвая, а устаревшая статистика
    // после обрыва стоит секунд поиска.
    let _ = phase("статистика", "ANALYZE", || {
        conn.execute_batch("ANALYZE;").map_err(Into::into)
    });

    if let Err(e) = storage.set_extras_build_complete(true) {
        tracing::warn!("не выставлена отметка завершённости надстройки: {}", e);
    }
    Ok(())
}

/// XML-слой обогащения: перечень объектов, связи данных, конфиг-уровневые
/// рёбра, права ролей, структура объектов (attributes_json), синонимы, формы,
/// подписки, модули. Всё это — обход XML выгрузки (дёшево, секунды даже на УТ),
/// без тяжёлого КОД-слоя (code_usages / procedure_terms / call_graph).
///
/// Идемпотентен (каждая фаза DELETE+INSERT либо UPDATE по full_name). Каждая
/// фаза независима: ошибка → warning, идём дальше. При возобновлении
/// (`resume`) уже собранные фазы пропускаются; для раскладки 1C:EDT отпечаток
/// входа не строится, поэтому там возобновление выключено.
fn run_index_extras_metadata_layer(
    repo_root: &Path,
    scan: &RepoScan,
    conn: &rusqlite::Connection,
    resume: Option<&ExtrasResume>,
) -> Result<()> {
    // Формат 1C:EDT (`.mdo`) — отдельный путь разбора. Заполняет ТЕ ЖЕ таблицы
    // (metadata_objects / data_links), поэтому downstream-инструменты не меняются.
    let resume = if let Some(src_root) = crate::xml::edt_mdo::detect_edt_src(repo_root) {
        phase("объекты (EDT)", "edt metadata layer", || {
            run_edt_metadata_layer(repo_root, &src_root, conn)
        });
        // Права ролей EDT лежат отдельными файлами и в общий проход по `.mdo`
        // не попадают — своя фаза, как у формата Конфигуратора (E-1).
        phase("права ролей (EDT)", "edt role_rights", || {
            run_edt_role_rights(&src_root, conn)
        });
        // Перечень модулей: своя раскладка путей и свои источники
        // идентификаторов, поэтому отдельная фаза (E-1).
        phase("модули (EDT)", "edt metadata_modules", || {
            index_metadata_modules_edt(repo_root, &src_root, conn)
        });
        None
    } else {
        run_metadata_layer_configurator(scan, conn, resume)?;
        resume
    };
    // Схемы компоновки данных — ПОСЛЕДНЯЯ фаза слоя, и это обязательно: и
    // `index_data_links` (Конфигуратор), и `run_edt_metadata_layer` (EDT) сносят
    // ВСЕ рёбра репо целиком, а паспорта макетов появляются в
    // `index_object_templates` / `run_edt_metadata_layer`. Рёбра `dcs_query` и
    // строки `dcs_*` обязаны лечь поверх уже собранного.
    resumable_phase(
        resume,
        conn,
        PH_DCS,
        "схемы компоновки",
        "dcs_schemas",
        || index_dcs_schemas(repo_root, conn),
    );
    Ok(())
}

/// XML-слой метаданных формата Конфигуратора: перечень объектов, связи данных,
/// конфиг-уровневые рёбра, права ролей, структура объектов (attributes_json),
/// синонимы, макеты, формы, подписки, модули, опись состава. Вынесен из
/// `run_index_extras_metadata_layer` отдельной функцией, чтобы ветки EDT и
/// Конфигуратора читались как два равноправных пути одного слоя.
fn run_metadata_layer_configurator(
    scan: &RepoScan,
    conn: &rusqlite::Connection,
    resume: Option<&ExtrasResume>,
) -> Result<()> {
    // Разбор корневых XML нужен только фазам надстройки, которые ещё не
    // собраны: на полностью готовом слое он не строится (иначе холостой
    // повторный запуск `index` читал бы десятки тысяч XML впустую).
    let need_harvest = resume.is_none_or(|r| {
        !r.is_done(PH_DATA_LINKS)
            || !r.is_done(PH_OBJECT_ATTRIBUTES)
            || !r.is_done(PH_OBJECT_SYNONYMS)
            || !r.is_done(PH_METADATA_MODULES)
    });
    let harvest = need_harvest.then(|| XmlHarvest::build(scan));
    let harvest_of = || {
        harvest
            .as_ref()
            .expect("harvest строится, когда хоть одна фаза, его использующая, не собрана")
    };
    resumable_phase(
        resume,
        conn,
        PH_METADATA_OBJECTS,
        "объекты",
        "metadata_objects",
        || index_metadata_objects(scan, conn),
    );
    // Граф связей данных: ссылочные реквизиты/измерения → рёбра data_links.
    resumable_phase(
        resume,
        conn,
        PH_DATA_LINKS,
        "связи данных",
        "data_links",
        || index_data_links(scan, harvest_of(), conn),
    );
    // Рёбра data_links КОНФИГУРАЦИОННОГО уровня (подсистемы, планы обмена,
    // определяемые типы, расположение ФО). Строго ПОСЛЕ index_data_links —
    // та wipe-ит все рёбра repo и пишет объектные; эта добавляет свои link_kind.
    resumable_phase(
        resume,
        conn,
        PH_DATA_LINKS_CONFIG,
        "связи конфигурации",
        "data_links(config-level)",
        || index_metadata_refs(scan, conn),
    );
    // Права ролей → отдельная таблица role_rights.
    resumable_phase(
        resume,
        conn,
        PH_ROLE_RIGHTS,
        "права ролей",
        "role_rights",
        || index_role_rights(scan, conn),
    );
    // Полная структура объектов (реквизиты+типы, ТЧ, измерения, ресурсы)
    // → metadata_objects.attributes_json. Зависит от строк, созданных
    // index_metadata_objects (выше), — делает UPDATE по full_name.
    resumable_phase(
        resume,
        conn,
        PH_OBJECT_ATTRIBUTES,
        "структура объектов",
        "object_attributes",
        || index_object_attributes(scan, harvest_of(), conn),
    );
    // Синонимы (русские представления) ВСЕХ объектов — отдельный лёгкий проход
    // по корневым XML всех папок типов. Покрывает и объекты без структуры
    // реквизитов (CommonModule/Constant/CommonPicture/FunctionalOption/…),
    // которых нет в OBJECT_FOLDERS. UPDATE по full_name; зависит от строк,
    // созданных index_metadata_objects.
    resumable_phase(
        resume,
        conn,
        PH_OBJECT_SYNONYMS,
        "синонимы",
        "object_synonyms",
        || index_object_synonyms(scan, harvest_of(), conn),
    );
    // Макеты объектов — своими строками перечня. Строго ПОСЛЕ
    // index_metadata_objects (та сносит перечень репо целиком) и после
    // синонимов (те делают UPDATE по перечню и макетов не касаются).
    resumable_phase(
        resume,
        conn,
        PH_OBJECT_TEMPLATES,
        "макеты",
        "object_templates",
        || index_object_templates(scan, conn),
    );
    resumable_phase(
        resume,
        conn,
        PH_METADATA_FORMS,
        "формы",
        "metadata_forms",
        || index_metadata_forms(scan, conn),
    );
    resumable_phase(
        resume,
        conn,
        PH_EVENT_SUBSCRIPTIONS,
        "подписки",
        "event_subscriptions",
        || index_event_subscriptions(scan, conn),
    );
    // metadata_modules зависят от UUID объектов (читают XML-файлы напрямую)
    // и от ConfigDumpInfo.xml каждой sub-config. Не зависят от других
    // *_index_extras-функций; порядок не критичен. После `DumpConfigToFiles`
    // платформа 1С перезаписывает всю выгрузку, поэтому полный пересбор оправдан.
    resumable_phase(
        resume,
        conn,
        PH_METADATA_MODULES,
        "модули",
        "metadata_modules",
        || index_metadata_modules(scan, harvest_of(), conn),
    );
    // Реестр строк ConfigDumpInfo.xml всех областей (base + расширения) —
    // плоский снимок состава для diff-сверки Фазы 2. Только текст описей,
    // объектные XML не читает. Идемпотентно (DELETE repo + reinsert).
    resumable_phase(
        resume,
        conn,
        PH_CONFIG_MANIFEST,
        "опись состава",
        "config_manifest",
        || index_config_manifest(scan, conn),
    );
    Ok(())
}

/// EDT-аналог metadata-слоя: обходит `src/<Тип>/<Имя>/<Имя>.mdo` и заполняет
/// `metadata_objects` (состав + синоним + `attributes_json` + паспорта макетов)
/// и `data_links` (ссылочные реквизиты/измерения + движения документов). Один
/// проход по объектам вместо серии раздельных (в формате EDT весь объект — в
/// одном `.mdo`, читать файл повторно незачем; описание макетов — тоже внутри
/// `.mdo`, элементами `<templates>`). Идемпотентно: DELETE+INSERT всего
/// репо. Формы/подписки/права/модули EDT — отдельными проходами (этап 2).
/// `repo_root` нужен паспортам макетов: путь их содержимого пишется
/// относительно корня репозитория.
fn run_edt_metadata_layer(
    repo_root: &Path,
    src_root: &Path,
    conn: &rusqlite::Connection,
) -> Result<()> {
    use crate::xml::edt_mdo;

    let _ = conn.execute("ROLLBACK", []);
    conn.execute("BEGIN", [])?;
    conn.execute(
        "DELETE FROM metadata_objects WHERE repo = ?",
        params![REPO_DEFAULT],
    )?;
    conn.execute(
        "DELETE FROM data_links WHERE repo = ?",
        params![REPO_DEFAULT],
    )?;
    conn.execute(
        "DELETE FROM metadata_forms WHERE repo = ?",
        params![REPO_DEFAULT],
    )?;
    conn.execute(
        "DELETE FROM event_subscriptions WHERE repo = ?",
        params![REPO_DEFAULT],
    )?;

    let mut ins_obj = conn.prepare(
        "INSERT OR IGNORE INTO metadata_objects \
         (repo, full_name, meta_type, name, synonym, attributes_json) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )?;
    let mut ins_link = conn.prepare(
        "INSERT OR IGNORE INTO data_links \
         (repo, from_object, from_path, to_object, link_kind, is_composite, is_universal) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )?;
    let mut ins_form = conn.prepare(
        "INSERT OR IGNORE INTO metadata_forms (repo, owner_full_name, form_name, handlers_json) \
         VALUES (?, ?, ?, ?)",
    )?;
    let mut ins_sub = conn.prepare(
        "INSERT OR IGNORE INTO event_subscriptions \
         (repo, name, event, handler_module, handler_proc, sources_json) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )?;

    let mut objects = 0usize;
    let mut links = 0usize;
    let mut cfg_links = 0usize;
    let mut forms = 0usize;
    let mut subs = 0usize;
    let mut templates = 0usize;
    // Обходим ВСЕ папки типов в src/ (не только OBJECT_FOLDERS): meta_type берём
    // из корневого тега `.mdo` (parse_mdo_header) — как index_object_synonyms для
    // формата Конфигуратора. Так в metadata_objects попадают и объекты без
    // структуры реквизитов (CommonModule/Constant/Report/Role/CommonPicture/...) —
    // с синонимом. Структуру/связи парсим для всех; пустые — отбрасываем.
    let type_dirs = match std::fs::read_dir(src_root) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("edt: read_dir({}): {}", src_root.display(), e);
            conn.execute("COMMIT", [])?;
            return Ok(());
        }
    };
    for td in type_dirs.filter_map(|e| e.ok()) {
        let type_dir = td.path();
        if !type_dir.is_dir() {
            continue;
        }
        // Configuration — сама конфигурация, не папка объектов; пропускаем.
        if type_dir.file_name().and_then(|s| s.to_str()) == Some("Configuration") {
            continue;
        }
        let objs = match std::fs::read_dir(&type_dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        // Очередь каталогов объектов. У всех типов, кроме подсистем, объекты
        // лежат строго на втором уровне; вложенные подсистемы —
        // `<Родитель>/Subsystems/<Ребёнок>/<Ребёнок>.mdo` — докладываются в
        // очередь по ходу обхода. Раньше до них дело не доходило вовсе:
        // из 755 подсистем живой выгрузки в реестр попадали 73 верхнеуровневые.
        let mut obj_queue: Vec<std::path::PathBuf> = objs
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        while let Some(obj_dir) = obj_queue.pop() {
            // Докладываем вложенные до любых `continue`: ветка дерева не должна
            // теряться из-за того, что у самой подсистемы не прочитался `.mdo`.
            let nested_dir = obj_dir.join("Subsystems");
            if nested_dir.is_dir() {
                if let Ok(nested) = std::fs::read_dir(&nested_dir) {
                    obj_queue.extend(
                        nested
                            .filter_map(|e| e.ok())
                            .map(|e| e.path())
                            .filter(|p| p.is_dir()),
                    );
                }
            }
            let obj_name = match obj_dir.file_name().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let mdo = obj_dir.join(format!("{}.mdo", obj_name));
            if !mdo.is_file() {
                continue;
            }
            let content = match std::fs::read_to_string(&mdo) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("edt: read {}: {}", mdo.display(), e);
                    continue;
                }
            };
            // meta_type из корневого тега `mdclass:<Тип>`; синоним из шапки.
            let (meta_type, synonym) = match edt_mdo::parse_mdo_header(&content) {
                Some((mt, _name, syn)) => (mt, syn),
                None => continue,
            };
            let full_name = format!("{}.{}", meta_type, obj_name);
            let attributes_json = match edt_mdo::parse_mdo_structure_xml(&content) {
                Ok(s) if !s.is_empty() => Some(s.to_json().to_string()),
                Ok(_) => None,
                Err(e) => {
                    tracing::warn!("edt structure {}: {}", mdo.display(), e);
                    None
                }
            };
            ins_obj.execute(params![
                REPO_DEFAULT,
                &full_name,
                &meta_type,
                &obj_name,
                synonym,
                attributes_json,
            ])?;
            objects += 1;

            // Макеты объекта: описание лежит здесь же, элементами
            // `<templates>` (вид, имя, синоним), содержимое — отдельным
            // файлом `Templates/<Имя>/Template.<расширение>`. Паспорт макета
            // пишется в тот же перечень; транзакция уже открыта.
            for t in edt_mdo::parse_mdo_templates(&content) {
                let row = template_row_from_edt(repo_root, &full_name, &obj_dir, &t);
                insert_template_row(conn, &row)?;
                templates += 1;
            }

            // Подписка на событие: помимо строки в metadata_objects пишем в
            // event_subscriptions (источник get_event_subscriptions).
            if meta_type == "EventSubscription" {
                if let Some((nm, ev, module, proc_, sources)) =
                    edt_mdo::parse_mdo_event_subscription(&content)
                {
                    let sources_json = serde_json::to_string(&sources)?;
                    ins_sub.execute(params![
                        REPO_DEFAULT,
                        &nm,
                        &ev,
                        &module,
                        &proc_,
                        &sources_json,
                    ])?;
                    subs += 1;
                }
            }

            // Конфигурационные рёбра (состав подсистемы и плана обмена, цели
            // определяемого типа, расположение и состав функциональной опции)
            // лежат в этом же `.mdo` — пишем их тем же проходом (E-1).
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
                cfg_links += 1;
            }

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
                        links += 1;
                    }
                }
                Err(e) => tracing::warn!("edt data_links {}: {}", mdo.display(), e),
            }

            // Общая форма — сама себе объект: `CommonForms/<Имя>/Form.form`,
            // каталога `Forms` у неё нет (E-3).
            if meta_type == "CommonForm" {
                let form_file = obj_dir.join("Form.form");
                if form_file.is_file() {
                    match std::fs::read_to_string(&form_file) {
                        Ok(fcontent) => {
                            let handlers = edt_mdo::parse_mdo_form_handlers(&fcontent);
                            let handlers_json = crate::xml::forms::handlers_to_json(&handlers)?;
                            ins_form.execute(params![
                                REPO_DEFAULT,
                                &format!("CommonForms.{}", obj_name),
                                &obj_name,
                                &handlers_json,
                            ])?;
                            forms += 1;
                        }
                        Err(e) => tracing::warn!("edt common form {}: {}", form_file.display(), e),
                    }
                }
            }

            // Формы объекта: <obj>/Forms/<ФормаИмя>/Form.form. owner_full_name —
            // в формате папки выгрузки '<PluralFolder>.<Имя>' (Documents.X), как у
            // metadata_forms формата Конфигуратора.
            let forms_dir = obj_dir.join("Forms");
            if forms_dir.is_dir() {
                let type_folder = type_dir
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default();
                let owner = format!("{}.{}", type_folder, obj_name);
                if let Ok(fread) = std::fs::read_dir(&forms_dir) {
                    for fe in fread.filter_map(|e| e.ok()) {
                        let fdir = fe.path();
                        if !fdir.is_dir() {
                            continue;
                        }
                        let form_name = match fdir.file_name().and_then(|s| s.to_str()) {
                            Some(s) => s.to_string(),
                            None => continue,
                        };
                        let form_file = fdir.join("Form.form");
                        if !form_file.is_file() {
                            continue;
                        }
                        let fcontent = match std::fs::read_to_string(&form_file) {
                            Ok(c) => c,
                            Err(_) => continue,
                        };
                        let handlers = edt_mdo::parse_mdo_form_handlers(&fcontent);
                        let handlers_json = crate::xml::forms::handlers_to_json(&handlers)?;
                        ins_form.execute(params![
                            REPO_DEFAULT,
                            &owner,
                            &form_name,
                            &handlers_json,
                        ])?;
                        forms += 1;
                    }
                }
            }
        }
    }
    drop(ins_obj);
    drop(ins_link);
    drop(ins_form);
    drop(ins_sub);
    backfill_data_link_keys(conn)?;
    crate::schema::backfill_metadata_object_keys(conn)?;
    conn.execute("COMMIT", [])?;

    tracing::info!(
        "edt metadata: {} объектов, {} рёбер data_links ({} конфигурационных), \
         {} форм, {} подписок, {} макетов (src={})",
        objects,
        links,
        cfg_links,
        forms,
        subs,
        templates,
        src_root.display()
    );
    Ok(())
}

/// Права ролей выгрузки 1C:EDT: `Roles/<Имя>/Rights.rights`.
///
/// Содержимое файла совпадает с `Roles/<Имя>/Ext/Rights.xml` формата
/// Конфигуратора (то же пространство имён `v8.1c.ru/8.2/roles`, те же элементы
/// `<object>/<right>`), поэтому разбор берётся общий. Отличаются только имя
/// файла и глубина: имя роли — каталог-родитель, а не «через Ext».
///
/// Идемпотентно: DELETE всего репо + INSERT, как одноимённая фаза формата
/// Конфигуратора. Хранятся только выданные права (`<value>true</value>`).
fn run_edt_role_rights(src_root: &Path, conn: &rusqlite::Connection) -> Result<()> {
    let roles_dir = src_root.join("Roles");
    if !roles_dir.is_dir() {
        return Ok(());
    }

    let _ = conn.execute("ROLLBACK", []);
    conn.execute("BEGIN", [])?;
    conn.execute(
        "DELETE FROM role_rights WHERE repo = ?",
        params![REPO_DEFAULT],
    )?;
    let mut stmt = conn.prepare(
        "INSERT OR IGNORE INTO role_rights (repo, role_name, object_name, right_name) \
         VALUES (?, ?, ?, ?)",
    )?;

    let mut total = 0usize;
    let mut roles = 0usize;
    let entries = match std::fs::read_dir(&roles_dir) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("edt role_rights read_dir({}): {}", roles_dir.display(), e);
            conn.execute("COMMIT", [])?;
            return Ok(());
        }
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let role_dir = entry.path();
        if !role_dir.is_dir() {
            continue;
        }
        let role_name = match role_dir.file_name().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let rights_file = role_dir.join("Rights.rights");
        if !rights_file.is_file() {
            continue;
        }
        match crate::xml::metadata_refs::parse_role_rights_file(&rights_file) {
            Ok(rights) => {
                roles += 1;
                for r in rights {
                    stmt.execute(params![
                        REPO_DEFAULT,
                        &role_name,
                        &r.object_name,
                        &r.right_name
                    ])?;
                    total += 1;
                }
            }
            Err(e) => tracing::warn!("edt role_rights {}: {}", rights_file.display(), e),
        }
    }
    drop(stmt);
    backfill_role_right_keys(conn)?;
    conn.execute("COMMIT", [])?;

    tracing::info!(
        "edt role_rights: {} прав из {} ролей (src={})",
        total,
        roles,
        src_root.display()
    );
    Ok(())
}

#[cfg(test)]
#[path = "index_extras/tests.rs"]
mod tests;
