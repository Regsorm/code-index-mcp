//! Граф вызовов процедур 1С: сборка слоёв, резолв адресов вызываемых
//! процедур и отсев платформенного балласта.

use std::path::Path;
use anyhow::Result;
use rusqlite::params;

use super::*;


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
pub(crate) fn update_call_graph_direct_for_file(
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

    // callee_proc_key здесь НЕ резолвим: новые рёбра остаются с NULL-адресом,
    // а резолв всего графа (resolve_and_prune_direct_edges) вызывающий делает
    // ОДИН раз после пофайлового цикла батча. Пофайловый резолв раньше
    // (build_common_module_methods + resolve_* + prune_* на КАЖДЫЙ .bsl) давал
    // квадратичную деградацию на bulk-батче: каждый файл заново сканировал весь
    // proc_call_graph и пересобирал tmp-карты общих модулей.
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
pub(crate) fn rebuild_call_graph_subscription(conn: &rusqlite::Connection) -> Result<()> {
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
pub(crate) fn rebuild_call_graph_form_event(conn: &rusqlite::Connection) -> Result<()> {
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


/// Построить граф вызовов из заполненных metadata_forms,
/// event_subscriptions и core-таблицы `calls`. Удаляет старые ребра
/// этого репо и вставляет свежие — идемпотентно.
/// Полный пересбор слоя `extension_override` из `functions.override_*`.
/// Идентично subscription-/form_event-частям `build_call_graph`. Вызывается
/// инкрементально при изменении `.bsl` — override-данные живут в `functions`,
/// которую core-индексатор обновляет на правку модуля расширения.
pub(crate) fn rebuild_call_graph_extension_override(conn: &rusqlite::Connection) -> Result<()> {
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


pub(crate) fn build_call_graph(conn: &rusqlite::Connection) -> Result<()> {
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
    // ── direct + direct_edge_files: материализуем calls⋈files ОДИН раз ──
    // Дорогой JOIN+DISTINCT по calls⋈files раньше гонялся дважды (для
    // proc_call_graph и для direct_edge_files) и в паре с построчной вставкой
    // в индексируемую таблицу деградировал сильнее суммы частей. Собираем
    // распарсенное множество рёбер во временную таблицу один раз и наполняем
    // из неё обе таблицы простыми вставками без повторного JOIN/DISTINCT.
    conn.execute_batch("DROP TABLE IF EXISTS tmp_direct_raw; CREATE TEMP TABLE tmp_direct_raw AS SELECT DISTINCT f.path AS path, c.caller AS caller, c.callee AS callee FROM calls c JOIN files f ON f.id = c.file_id WHERE c.caller IS NOT NULL AND c.callee IS NOT NULL;")?;

    let direct_count = conn.execute(
        "INSERT OR IGNORE INTO proc_call_graph (repo, caller_proc_key, callee_proc_name, call_type) SELECT ?, path || '::' || caller, callee, 'direct' FROM tmp_direct_raw",
        params![REPO_DEFAULT],
    )?;

    conn.execute("DELETE FROM direct_edge_files WHERE repo = ?", params![REPO_DEFAULT])?;
    conn.execute(
        "INSERT OR IGNORE INTO direct_edge_files (repo, caller, callee, source_file) SELECT ?, caller, callee, path FROM tmp_direct_raw",
        params![REPO_DEFAULT],
    )?;
    conn.execute_batch("DROP TABLE IF EXISTS tmp_direct_raw;")?;

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

    // ── этап 4e + 4e-D + 4e-prune: резолв callee_proc_key + отсев балласта ──
    // Общий с инкрементом хелпер: run_incremental_extras зовёт его же ОДИН раз
    // после пофайловой вставки рёбер батча → идентичность full↔incremental.
    resolve_and_prune_direct_edges(conn)?;

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


/// Этап 4e (общий для полного пересбора и инкремента): заполнить
/// `callee_proc_key` всем direct-рёбрам с NULL-адресом и отсеять
/// платформенный/объектный балласт. Транзакцией управляет вызывающий.
/// Трогает ТОЛЬКО рёбра с `callee_proc_key IS NULL` → идемпотентен: результат
/// идентичен при вызове из `build_call_graph` (после полной вставки рёбер) и из
/// `run_incremental_extras` (после пофайловой вставки рёбер батча).
pub(crate) fn resolve_and_prune_direct_edges(conn: &rusqlite::Connection) -> Result<()> {
    resolve_direct_callee_keys(conn)?;
    resolve_callee_keys_by_manager(conn, None)?;
    prune_platform_balast(conn, None)?;
    prune_object_method_calls(conn, None)?;
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
pub(crate) fn resolve_direct_callee_keys(conn: &rusqlite::Connection) -> Result<()> {
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
pub(crate) fn build_common_module_methods(conn: &rusqlite::Connection) -> Result<()> {
    conn.execute_batch(
        "DROP TABLE IF EXISTS tmp_pcg_cmeth;\n\
         CREATE TEMP TABLE tmp_pcg_cmeth AS\n\
           SELECT substr(s.seg, 1, instr(s.seg, '/') - 1) AS mname,\n\
                  fn.name AS method,\n\
                  s.path  AS path\n\
           FROM (SELECT id, path,\n\
                        substr(path, instr(path,'CommonModules/')+length('CommonModules/')) AS seg\n\
                 FROM files\n\
                 WHERE path LIKE '%CommonModules/%/Module.bsl') s\n\
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
pub(crate) fn resolve_callee_keys_by_qualifier(
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
pub(crate) const PLATFORM_BALAST: &[&str] = &[
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
/// в конфигурации, не трогается вовсе (адаптивно к размеру конфигурации). `file_scope=
/// Some(rel)` ограничивает удаление рёбрами одного файла (инкремент), `None` —
/// весь граф (полный пересбор).
pub(crate) fn prune_platform_balast(conn: &rusqlite::Connection, file_scope: Option<&str>) -> Result<()> {
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
pub(crate) const METADATA_COLLECTIONS: &[&str] = &[
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
pub(crate) fn prune_object_method_calls(conn: &rusqlite::Connection, file_scope: Option<&str>) -> Result<()> {
    // tmp_pmods — имена общих модулей (сегмент пути после CommonModules/).
    conn.execute_batch(
        "DROP TABLE IF EXISTS tmp_pmods;\n\
         CREATE TEMP TABLE tmp_pmods AS\n\
           SELECT DISTINCT substr(seg,1,instr(seg,'/')-1) AS q FROM (\n\
             SELECT substr(path, instr(path,'CommonModules/')+length('CommonModules/')) AS seg\n\
             FROM files WHERE path LIKE '%CommonModules/%/Module.bsl') WHERE instr(seg,'/')>0;\n\
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
/// `<...>/<Folder>/<Object>/[Ext/]ManagerModule.bsl` в Rust (в SQLite нет «последнего
/// вхождения» для надёжного разбора двух хвостовых сегментов).
pub(crate) fn build_manager_module_methods(conn: &rusqlite::Connection) -> Result<()> {
    conn.execute_batch(
        "DROP TABLE IF EXISTS tmp_pcg_mmeth; \
         CREATE TEMP TABLE tmp_pcg_mmeth(folder TEXT, object TEXT, method TEXT, path TEXT);",
    )?;
    let rows: Vec<(String, String)> = {
        let mut st = conn.prepare(
            "SELECT fl.path, fn.name FROM functions fn JOIN files fl ON fl.id = fn.file_id \
             WHERE fl.path LIKE '%ManagerModule.bsl' \
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
            // Формат Конфигуратора кладёт модуль в каталог Ext, выгрузка 1C:EDT —
            // прямо в каталог объекта. Принимаем обе раскладки.
            let prefix = path
                .strip_suffix("/Ext/ManagerModule.bsl")
                .or_else(|| path.strip_suffix("/ManagerModule.bsl"));
            if let Some(prefix) = prefix {
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
pub(crate) fn build_collection_folder_map(conn: &rusqlite::Connection) -> Result<()> {
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
/// `<Папка>/<Объект>/[Ext/]ManagerModule.bsl`. Платформенные методы менеджера
/// (`ПустаяСсылка`, `НайтиПоКоду`) не экспортны в модуле → остаются NULL.
/// `file_scope=Some(rel)` — в области одного файла (инкремент).
pub(crate) fn resolve_callee_keys_by_manager(conn: &rusqlite::Connection, file_scope: Option<&str>) -> Result<()> {
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
