//! Перечень модулей конфигурации: поиск владельца модуля, классификация
//! типа и ведение строк как полным проходом, так и пофайлово.

use std::path::Path;
use anyhow::Result;
use rusqlite::params;
use walkdir::WalkDir;
use crate::module_constants::{module_type_by_filename, property_id_by_type};
use crate::xml::config_dump_info::parse_config_dump_info;
use crate::xml::object_uuid::{
    extract_command_uuid_from_file, extract_form_uuid_any_from_file, extract_object_uuid_from_file,
};

use super::*;


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
pub(crate) fn index_metadata_modules(repo_root: &Path, conn: &rusqlite::Connection) -> Result<()> {
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
    // Миграция: старый ключ UNIQUE(repo, full_name) без extension_name терял
    // модули расширений-доработок (то же имя, что в base) через INSERT OR
    // IGNORE. Обнаружив старую схему — пересоздаём таблицу с новым ключом.
    let old_ddl: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='metadata_modules'",
            [],
            |r| r.get(0),
        )
        .ok();
    if let Some(ddl) = old_ddl {
        if !ddl.contains("extension_name)") {
            conn.execute("DROP TABLE metadata_modules", [])?;
            conn.execute(crate::schema::METADATA_MODULES_DDL, [])?;
            for idx_ddl in crate::schema::METADATA_MODULES_INDEXES {
                conn.execute(idx_ddl, [])?;
            }
            tracing::info!("metadata_modules: миграция схемы — UNIQUE ключ дополнен extension_name");
        }
    }
    conn.execute(
        "DELETE FROM metadata_modules WHERE repo = ?",
        params![REPO_DEFAULT],
    )?;
    let mut total: usize = 0;
    let mut skipped: usize = 0;
    // Кэш описей выгрузки на весь проход: `build_module_row` берёт из него
    // версию модуля, не перечитывая опись для каждого .bsl.
    let mut cfgver_cache: std::collections::HashMap<
        std::path::PathBuf,
        std::collections::HashMap<String, String>,
    > = std::collections::HashMap::new();

    for sub_root in &sub_configs {
        for entry in WalkDir::new(sub_root).into_iter().filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            // Строку собирает тот же хелпер, что и пофайловая ветка инкремента.
            // Раньше здесь лежала своя копия той же логики (классификация типа,
            // поиск владельца, чтение идентификатора), и правка одной стороны не
            // действовала во второй — так модули команд объектов остались бы вне
            // перечня даже после исправления разбора их пути (E-7).
            let row = match build_module_row(repo_root, path, &mut cfgver_cache) {
                Some(r) => r,
                None => {
                    skipped += 1;
                    continue;
                }
            };
            insert_module_row(conn, &row)?;
            total += 1;
        }
    }
    conn.execute("COMMIT", [])?;

    code_index_core::logging::stage_detail(code_index_core::logging::plural(
        total as u64,
        "модуль",
        "модуля",
        "модулей",
    ));
    tracing::info!(
        "metadata_modules: записано {} модулей из {} sub-configs (пропущено файлов: {})",
        total,
        sub_configs.len(),
        skipped,
    );
    Ok(())
}


/// Данные одной строки `metadata_modules`, собранные из `.bsl`-модуля. Отделены
/// от вставки, чтобы одну логику (classify/owner/uuid/config_version) использовать
/// и пофайлово (`update_metadata_module_for_file`), и по-объектно
/// (`update_metadata_modules_for_object`) в рамках одной транзакции.
pub(crate) struct ModuleRow {
    full_name: String,
    object_name: String,
    effective_type: &'static str,
    object_id: String,
    property_id: &'static str,
    config_version: Option<String>,
    code_path: String,
    extension_name: String,
}


/// Собрать строку `metadata_modules` из одного `.bsl` (те же хелперы classify/
/// owner/uuid, что у `index_metadata_modules` → полная эквивалентность). Не
/// .bsl-модуль известного типа / без владельца / без UUID → `None` (no-op у
/// вызывающего). `config_version` (только для точности dbgs-breakpoints, не для
/// поиска) берётся из `ConfigDumpInfo.xml` sub-config'а через кэш `cfgver_cache`
/// на батч — чтобы не перечитывать опись для каждого `.bsl` большой пачки.
pub(crate) fn build_module_row(
    repo_root: &Path,
    bsl_path: &Path,
    cfgver_cache: &mut std::collections::HashMap<
        std::path::PathBuf,
        std::collections::HashMap<String, String>,
    >,
) -> Option<ModuleRow> {
    let file_name = bsl_path.file_name().and_then(|n| n.to_str())?;
    let module_type = module_type_by_filename(file_name)?;
    let (effective_type, owner_xml_kind) = classify_module(bsl_path, module_type);
    let property_id = property_id_by_type(effective_type)?;
    // Команда объекта лежит как `<Тип>/<Объект>/Commands/<Команда>/[Ext/]CommandModule.bsl`.
    // Собственного файла у команды нет — она описана внутри XML своего объекта,
    // поэтому общий разбор пути искал несуществующий `Commands/<Команда>.xml`,
    // владелец не находился и модуль пропускался молча (E-7). Владельцем считаем
    // объект, идентификатор берём у самой команды.
    let command_owner = if effective_type == "CommandModule" {
        find_object_command_owner(bsl_path)
    } else {
        None
    };
    let (object_name, uuid_opt) = match command_owner {
        Some((owner_xml_path, object_name, command_name)) => {
            let uuid = extract_command_uuid_from_file(&owner_xml_path, &command_name)
                .ok()
                .flatten();
            (object_name, uuid)
        }
        None => {
            let owner_info = match owner_xml_kind {
                OwnerKind::Form => find_form_owner(bsl_path),
                OwnerKind::Object => find_object_owner(bsl_path),
            };
            let (owner_xml_path, object_name) = owner_info?;
            let uuid = match owner_xml_kind {
                OwnerKind::Form => extract_form_uuid_any_from_file(&owner_xml_path).ok().flatten(),
                OwnerKind::Object => extract_object_uuid_from_file(&owner_xml_path).ok().flatten(),
            };
            (object_name, uuid)
        }
    };
    let object_id = match uuid_opt {
        Some(u) if !u.is_empty() => u,
        _ => return None,
    };
    let sub_root =
        sub_root_for_path(repo_root, bsl_path).unwrap_or_else(|| repo_root.to_path_buf());
    let extension_name = compute_extension_name(repo_root, &sub_root);
    let config_versions = cfgver_cache.entry(sub_root.clone()).or_insert_with(|| {
        parse_config_dump_info(&sub_root).unwrap_or_else(|e| {
            tracing::warn!(
                "ConfigDumpInfo {}: {} — версия модуля не определена",
                sub_root.display(),
                e
            );
            Default::default()
        })
    });
    let config_version = config_versions.get(&object_id).cloned();
    let full_name = format!("{}.{}", object_name, effective_type);
    let code_path = bsl_path
        .strip_prefix(repo_root)
        .unwrap_or(bsl_path)
        .to_string_lossy()
        .replace('\\', "/");
    Some(ModuleRow {
        full_name,
        object_name,
        effective_type,
        object_id,
        property_id,
        config_version,
        code_path,
        extension_name,
    })
}




/// Заполнить `metadata_modules` для выгрузки 1C:EDT.
///
/// Раскладка отличается от формата Конфигуратора: каталога `Ext` нет
/// (`<Тип>/<Объект>/ObjectModule.bsl`), модуль формы лежит в
/// `<Тип>/<Объект>/Forms/<Форма>/Module.bsl`, описание объекта — в
/// `<Объект>/<Объект>.mdo`, а идентификаторы форм и команд записаны
/// вложенными тегами внутри этого же `.mdo`.
///
/// `config_version` для EDT остаётся пустым: служебной описи выгрузки в этом
/// формате не существует, а выдумывать версию нельзя — её сверяет отладчик.
/// Остальные колонки заполняются так же, как у формата Конфигуратора.
pub(crate) fn index_metadata_modules_edt(
    repo_root: &Path,
    src_root: &Path,
    conn: &rusqlite::Connection,
) -> Result<()> {
    let _ = conn.execute("ROLLBACK", []);
    conn.execute("BEGIN", [])?;
    conn.execute(
        "DELETE FROM metadata_modules WHERE repo = ?",
        params![REPO_DEFAULT],
    )?;

    // Один объект даёт до десятков модулей и форм — читаем его `.mdo` один раз.
    let mut mdo_cache: std::collections::HashMap<std::path::PathBuf, String> =
        std::collections::HashMap::new();
    let mut total = 0usize;

    for entry in WalkDir::new(src_root).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("bsl") {
            continue;
        }
        if let Some(row) = build_module_row_edt(repo_root, path, &mut mdo_cache) {
            insert_module_row(conn, &row)?;
            total += 1;
        }
    }
    conn.execute("COMMIT", [])?;

    code_index_core::logging::stage_detail(code_index_core::logging::plural(
        total as u64,
        "модуль",
        "модуля",
        "модулей",
    ));
    tracing::info!(
        "edt metadata_modules: {} модулей (src={})",
        total,
        src_root.display()
    );
    Ok(())
}


/// Собрать строку `metadata_modules` из одного `.bsl` выгрузки EDT.
/// Не модуль известного типа / без владельца / без идентификатора → `None`.
pub(crate) fn build_module_row_edt(
    repo_root: &Path,
    bsl_path: &Path,
    mdo_cache: &mut std::collections::HashMap<std::path::PathBuf, String>,
) -> Option<ModuleRow> {
    let file_name = bsl_path.file_name().and_then(|n| n.to_str())?;
    let module_type = module_type_by_filename(file_name)?;
    let (effective_type, _) = classify_module(bsl_path, module_type);
    let property_id = property_id_by_type(effective_type)?;

    let segments: Vec<&str> = bsl_path
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();
    if segments.len() < 3 {
        return None;
    }

    // Папка объекта = каталог, в котором лежит `<Имя>.mdo`. Поднимаемся от
    // модуля вверх, пока такой не найдётся: так одинаково разбираются и
    // `<Объект>/ObjectModule.bsl`, и `<Объект>/Forms/<Ф>/Module.bsl`,
    // и `<Объект>/Commands/<К>/CommandModule.bsl`.
    let mut obj_dir = bsl_path.parent()?.to_path_buf();
    let mdo_path = loop {
        let name = obj_dir.file_name().and_then(|s| s.to_str())?;
        let candidate = obj_dir.join(format!("{}.mdo", name));
        if candidate.is_file() {
            break candidate;
        }
        obj_dir = obj_dir.parent()?.to_path_buf();
        // Модули самой конфигурации (`Configuration/SessionModule.bsl` и
        // соседние) владельца-объекта не имеют — их пропускает и формат
        // Конфигуратора.
        if obj_dir.file_name().and_then(|s| s.to_str()) == Some("src") {
            return None;
        }
    };
    let owner_name = obj_dir.file_name().and_then(|s| s.to_str())?;
    let meta_folder = obj_dir.parent()?.file_name().and_then(|s| s.to_str())?;
    if meta_folder == "src" || owner_name == "Configuration" {
        return None;
    }

    let content = match mdo_cache.get(&mdo_path) {
        Some(c) => c,
        None => {
            let c = std::fs::read_to_string(&mdo_path).ok()?;
            mdo_cache.entry(mdo_path.clone()).or_insert(c)
        }
    };

    // Имя владельца и источник идентификатора зависят от вида модуля:
    // форма и команда объекта описаны внутри `.mdo` владельца, остальные
    // модули принадлежат самому объекту.
    let (object_name, uuid) = if let Some(idx) = segments.iter().rposition(|s| *s == "Forms") {
        let form_name = segments.get(idx + 1)?;
        (
            format!("{}.{}.Form.{}", meta_folder, owner_name, form_name),
            crate::xml::edt_mdo::parse_mdo_child_uuid(content, "forms", form_name),
        )
    } else if let Some(idx) = segments.iter().rposition(|s| *s == "Commands") {
        let command_name = segments.get(idx + 1)?;
        (
            format!("{}.{}.Command.{}", meta_folder, owner_name, command_name),
            crate::xml::edt_mdo::parse_mdo_child_uuid(content, "commands", command_name),
        )
    } else {
        (
            format!("{}.{}", meta_folder, owner_name),
            crate::xml::edt_mdo::parse_mdo_root_uuid(content),
        )
    };

    let object_id = match uuid {
        Some(u) if !u.is_empty() => u,
        _ => return None,
    };
    let code_path = bsl_path
        .strip_prefix(repo_root)
        .unwrap_or(bsl_path)
        .to_string_lossy()
        .replace('\\', "/");

    Some(ModuleRow {
        full_name: format!("{}.{}", object_name, effective_type),
        object_name,
        effective_type,
        object_id,
        property_id,
        // Описи выгрузки в EDT нет — версия объекта неизвестна.
        config_version: None,
        code_path,
        extension_name: String::new(),
    })
}


/// Вставка/обновление одной строки `metadata_modules`. Транзакцией управляет вызывающий.
pub(crate) fn insert_module_row(conn: &rusqlite::Connection, row: &ModuleRow) -> Result<()> {
    conn.execute(
        "INSERT INTO metadata_modules \
         (repo, full_name, object_name, module_type, object_id, property_id, \
          config_version, code_path, extension_name) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(repo, full_name, extension_name) DO UPDATE SET \
             object_name = excluded.object_name, \
             module_type = excluded.module_type, \
             object_id = excluded.object_id, \
             property_id = excluded.property_id, \
             config_version = excluded.config_version, \
             code_path = excluded.code_path",
        params![
            REPO_DEFAULT,
            &row.full_name,
            &row.object_name,
            row.effective_type,
            &row.object_id,
            row.property_id,
            row.config_version.as_deref(),
            &row.code_path,
            &row.extension_name,
        ],
    )?;
    Ok(())
}


/// Per-file точечное обновление `metadata_modules` для одного изменённого `.bsl`.
/// Закрывает дыру «строка нового модуля не заводится без Configuration.xml в
/// батче». Не .bsl-модуль известного типа / без владельца / без UUID → no-op.
pub(crate) fn update_metadata_module_for_file(
    repo_root: &Path,
    conn: &rusqlite::Connection,
    bsl_path: &Path,
    cfgver_cache: &mut std::collections::HashMap<
        std::path::PathBuf,
        std::collections::HashMap<String, String>,
    >,
) -> Result<()> {
    let rel = rel_path(repo_root, bsl_path);
    let _ = conn.execute("ROLLBACK", []); // защита от cascade-ошибки
    conn.execute("BEGIN", [])?;
    // Сначала БЕЗУСЛОВНО снимаем прежнюю строку этого файла — как это делают
    // соседние пофайловые ветки (использования в коде, механические термы).
    // Без этого удалённый модуль остаётся в перечне навсегда (E-5): тип берётся
    // из имени файла, а владелец и его идентификатор — из XML объекта, который
    // на диске остался, поэтому строка успешно перезаписывалась уже после
    // исчезновения модуля, и инкремент расходился с полным пересбором.
    conn.execute(
        "DELETE FROM metadata_modules WHERE repo = ?1 AND code_path = ?2",
        params![REPO_DEFAULT, &rel],
    )?;
    if bsl_path.is_file() {
        if let Some(row) = build_module_row(repo_root, bsl_path, cfgver_cache) {
            insert_module_row(conn, &row)?;
        }
    }
    conn.execute("COMMIT", [])?;
    Ok(())
}


/// Per-object пересборка `metadata_modules` объекта: DELETE всех его модулей (по
/// всем sub-config'ам, ключ `object_name`) + обход каталогов объекта во ВСЕХ
/// `roots` с повторной вставкой по существующим `.bsl`. Симметрично
/// `update_data_links_for_object`: при уходе заимствователя опись расширения
/// обнуляется, но `.bsl`-модули форм физически на месте — пофайловый путь их не
/// трогает, и `config_version` устаревал бы (расхождение с полным reindex,
/// пойман федеративным smoke на типовой торговой конфигурации). Здесь модули приводятся к свежей описи.
pub(crate) fn update_metadata_modules_for_object(
    repo_root: &Path,
    conn: &rusqlite::Connection,
    roots: &[std::path::PathBuf],
    xml_path: &Path,
    cfgver_cache: &mut std::collections::HashMap<
        std::path::PathBuf,
        std::collections::HashMap<String, String>,
    >,
) -> Result<()> {
    // Папка (plural) и имя объекта — из пути корневого XML; `object_name` в
    // metadata_modules хранится как '<PluralFolder>.<Name>'.
    let folder = match xml_path.parent().and_then(|d| d.file_name()).and_then(|s| s.to_str()) {
        Some(s) => s.to_string(),
        None => return Ok(()),
    };
    let stem = match xml_path.file_stem().and_then(|s| s.to_str()) {
        Some(s) => s.to_string(),
        None => return Ok(()),
    };
    let object_name = format!("{}.{}", folder, stem);
    let like = format!("{}.%", object_name);

    let _ = conn.execute("ROLLBACK", []);
    conn.execute("BEGIN", [])?;
    // Модули самого объекта (ObjectModule/ManagerModule: object_name точно) и его
    // форм/команд (object_name '<obj>.Form.X' / '<obj>.Command.Y': по LIKE).
    conn.execute(
        "DELETE FROM metadata_modules \
         WHERE repo = ? AND (object_name = ? OR object_name LIKE ?)",
        params![REPO_DEFAULT, &object_name, &like],
    )?;
    for root in roots {
        let obj_dir = root.join(&folder).join(&stem);
        if !obj_dir.is_dir() {
            continue;
        }
        for entry in WalkDir::new(&obj_dir).into_iter().filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() {
                continue;
            }
            if let Some(row) = build_module_row(repo_root, entry.path(), cfgver_cache) {
                insert_module_row(conn, &row)?;
            }
        }
    }
    conn.execute("COMMIT", [])?;
    Ok(())
}


/// Что искать как XML-владелец .bsl-файла модуля.
#[derive(Debug, Clone, Copy)]
pub(crate) enum OwnerKind {
    /// Форма: рядом с .bsl лежит Form.xml (его uuid — атрибут корня <Form>).
    Form,
    /// Обычный объект: на 1 уровень выше Ext-папки модуль/в самой папке
    /// объекта лежит `<Имя>.xml` с дочерним <Document/Catalog/.../> uuid="…".
    Object,
}


/// Уточнить тип модуля и определить как искать владельца.
/// Особый случай: Module.bsl внутри `Forms/<X>/Ext/Form/Module.bsl` — это
/// FormModule, а не CommonModule.Module.
pub(crate) fn classify_module(bsl_path: &Path, raw_type: &'static str) -> (&'static str, OwnerKind) {
    if raw_type == "Module"
        && (path_has_segment(bsl_path, "Forms") || path_has_segment(bsl_path, "CommonForms"))
    {
        return ("FormModule", OwnerKind::Form);
    }
    // CommandModule в `<Object>/Commands/<CmdName>/Ext/CommandModule.bsl` —
    // владелец = Commands/<CmdName>.xml. Не реализуем сейчас, фолбэк ниже —
    // owner = ближайший XML «вверху». Большинство CommandModule всё равно
    // отработают через find_object_owner.
    (raw_type, OwnerKind::Object)
}


pub(crate) fn path_has_segment(p: &Path, segment: &str) -> bool {
    p.components().any(|c| match c {
        std::path::Component::Normal(s) => s.to_str() == Some(segment),
        _ => false,
    })
}


/// Найти XML-владельца для модуля формы.
/// Обычные формы: `<...>/<MetaType>/<Owner>/Forms/<FormName>/[Ext/Form/]Module.bsl`;
/// общие формы:   `<...>/CommonForms/<FormName>/[Ext/Form/]Module.bsl`.
/// UUID формы живёт в `Forms/<FormName>.xml` (сосед папки формы,
/// иерархическая выгрузка DumpConfigToFiles, стиль MetaDataObject/Form) —
/// это основной случай; `<FormDir>/[Ext/]Form.xml` (uuid атрибутом корня) —
/// запасной layout-вариант.
/// Возвращает (путь к XML, owner_full_name).
pub(crate) fn find_form_owner(bsl_path: &Path) -> Option<(std::path::PathBuf, String)> {
    let segments: Vec<&str> = bsl_path
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();
    let (form_name, owner_full) = if let Some(idx) = segments.iter().rposition(|s| *s == "Forms") {
        if idx + 1 >= segments.len() || idx < 2 {
            return None;
        }
        let form_name = segments[idx + 1];
        let owner_name = segments[idx - 1];
        let meta_type = segments[idx - 2];
        (form_name, format!("{}.{}.Form.{}", meta_type, owner_name, form_name))
    } else if let Some(idx) = segments.iter().rposition(|s| *s == "CommonForms") {
        if idx + 1 >= segments.len() {
            return None;
        }
        let form_name = segments[idx + 1];
        (form_name, format!("CommonForms.{}", form_name))
    } else {
        return None;
    };
    // Поднимаемся до папки с именем формы.
    let mut form_dir = bsl_path.to_path_buf();
    while let Some(parent) = form_dir.parent() {
        form_dir = parent.to_path_buf();
        if form_dir
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s == form_name)
            .unwrap_or(false)
        {
            break;
        }
    }
    // Кандидаты владельца по приоритету (см. doc-комментарий).
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Some(forms_dir) = form_dir.parent() {
        candidates.push(forms_dir.join(format!("{form_name}.xml")));
    }
    candidates.push(form_dir.join("Ext").join("Form.xml"));
    candidates.push(form_dir.join("Form.xml"));
    let xml_path = candidates.into_iter().find(|p| p.is_file())?;
    Some((xml_path, owner_full))
}


/// Найти владельца для модуля команды ОБЪЕКТА:
/// `<...>/<MetaType>/<OwnerName>/Commands/<CommandName>/[Ext/]CommandModule.bsl`.
/// Возвращает (путь к XML объекта, `<MetaType>.<OwnerName>.Command.<CommandName>`,
/// имя команды). Общие команды (`CommonCommands/<Имя>/Ext/CommandModule.bsl`)
/// сюда не попадают — у них нет сегмента `Commands`, их ведёт общий разбор пути.
pub(crate) fn find_object_command_owner(bsl_path: &Path) -> Option<(std::path::PathBuf, String, String)> {
    let segments: Vec<&str> = bsl_path
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();
    let idx = segments.iter().rposition(|s| *s == "Commands")?;
    if idx < 2 || idx + 1 >= segments.len() {
        return None;
    }
    let meta_type = segments[idx - 2];
    let owner_name = segments[idx - 1];
    let command_name = segments[idx + 1];

    // Поднимаемся до каталога `Commands`; папка объекта — его родитель, а XML
    // объекта — сосед этой папки. Раньше подъём шёл до папки С ИМЕНЕМ ОБЪЕКТА,
    // и у команды, названной так же, как сам объект (обычное дело у отчётов и
    // обработок), первой снизу встречалась папка КОМАНДЫ: владелец искался как
    // `Commands/<Имя>.xml`, не находился, и модуль пропускался молча — 91 из
    // 587 модулей команд типовой бухгалтерии (E-12).
    let mut dir = bsl_path.to_path_buf();
    loop {
        let parent = dir.parent()?;
        if parent.file_name().and_then(|s| s.to_str()) == Some("Commands") {
            dir = parent.parent()?.to_path_buf();
            break;
        }
        dir = parent.to_path_buf();
    }
    let owner_xml = dir
        .parent()?
        .join(format!("{}.xml", owner_name));
    if !owner_xml.is_file() {
        return None;
    }
    let full = format!("{}.{}.Command.{}", meta_type, owner_name, command_name);
    Some((owner_xml, full, command_name.to_string()))
}


/// Найти XML-файл владельца для не-form модуля.
/// Layout: `<...>/<MetaType>/<OwnerName>/[Ext/]<ModuleFile>.bsl`
/// → искать `<...>/<MetaType>/<OwnerName>.xml`.
/// Возвращает (путь к XML, owner_full_name = "<MetaType>.<OwnerName>").
pub(crate) fn find_object_owner(bsl_path: &Path) -> Option<(std::path::PathBuf, String)> {
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

/// Точечное обновление `metadata_modules` для одного изменённого `.bsl`
/// выгрузки EDT. Кэш описаний объектов здесь не нужен: файл в батче один,
/// объект читается один раз.
pub(crate) fn update_metadata_module_for_file_edt(
    repo_root: &Path,
    conn: &rusqlite::Connection,
    bsl_path: &Path,
) -> Result<()> {
    let mut cache = std::collections::HashMap::new();
    match build_module_row_edt(repo_root, bsl_path, &mut cache) {
        Some(row) => insert_module_row(conn, &row),
        None => Ok(()),
    }
}
