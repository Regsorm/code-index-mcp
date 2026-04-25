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
use crate::xml::object_uuid::{extract_form_uuid_from_file, extract_object_uuid_from_file};

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
    Ok(())
}

/// Построить граф вызовов из заполненных metadata_forms,
/// event_subscriptions и core-таблицы `calls`. Удаляет старые ребра
/// этого репо и вставляет свежие — идемпотентно.
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
    // `direct`. caller_proc_key — это callee (имя процедуры) из calls
    // — увы, в core нет module-context'а у вызовов, поэтому используем
    // голое имя. На этапе 4e (resolution) добавим попытку найти
    // module по functions.qualified_name.
    let direct_count = conn.execute(
        "INSERT OR IGNORE INTO proc_call_graph \
         (repo, caller_proc_key, callee_proc_name, call_type) \
         SELECT ?, caller, callee, 'direct' \
         FROM calls \
         WHERE caller IS NOT NULL AND callee IS NOT NULL",
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

    conn.execute("COMMIT", [])?;

    tracing::info!(
        "proc_call_graph: {} direct + {} subscription + {} form_event ребер",
        direct_count,
        subscription_count,
        form_count
    );

    // TODO(этап 4e): resolution callee_proc_key через functions.qualified_name —
    // пробуем сопоставить `callee_proc_name` с реальной процедурой по
    // имени модуля и заполнить `callee_proc_key`.
    // TODO(этап 4f): extension_override — нужен парсер расширения (CFE).
    // TODO(этап 4g): external_assignment — runtime-анализ переменных
    // неопределённого типа. Опционально, очень дорогая фича.

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
}
