//! Вызовы с квалификатором для `get_callers` (1С).
//!
//! Разборщик BSL хранит вызываемое имя вместе с приёмником
//! (`ОбщийМодуль.Метод`, `Справочники.Объект.Метод`, `Переменная.Метод`),
//! поэтому точный поиск по голому имени такие вызовы не находит. Здесь мы
//! добираем их двумя источниками: точными написаниями по месту объявления имени
//! (`exported_procs`) и рёбрами графа, привязанными к объявлениям имени (сюда
//! попадают вызовы через переменную из формы, привязанные к модулю своего
//! объекта — правило (д) в `index_extras::call_graph`).

use std::collections::{BTreeSet, HashSet};

use anyhow::Result;
use code_index_core::storage::models::CallRecord;
use code_index_core::storage::Storage;
use rusqlite::params;

use crate::index_extras::REPO_DEFAULT;

/// Вызовы процедуры `function_name`, записанные в коде с квалификатором.
///
/// Ошибки SQL не пробрасываются: при сбое пишем `tracing::warn!` и отдаём то,
/// что успели собрать (в худшем случае — пусто).
pub fn qualified_callers(storage: &Storage, function_name: &str) -> Vec<CallRecord> {
    // Имя с точкой — уже квалификатор, а не искомое имя процедуры.
    if function_name.is_empty() || function_name.contains('.') {
        return Vec::new();
    }
    let mut out: Vec<CallRecord> = Vec::new();
    if let Err(e) = collect(storage.conn(), function_name, &mut out) {
        tracing::warn!("qualified_callers({}): {}", function_name, e);
    }
    out
}

/// Собрать вызовы с квалификатором в `out`, отбрасывая дубли по `id` и сохраняя
/// порядок: сначала точные написания (шаг 1), затем рёбра графа (шаг 2).
fn collect(
    conn: &rusqlite::Connection,
    function_name: &str,
    out: &mut Vec<CallRecord>,
) -> Result<()> {
    let mut seen: HashSet<i64> = HashSet::new();

    // ── 1. Точные написания по месту объявления имени ─────────────────────
    let decls: Vec<(String, Option<String>, Option<String>)> = {
        let mut st = conn.prepare(
            "SELECT kind, owner, folder FROM exported_procs WHERE repo = ?1 AND name = ?2",
        )?;
        let v = st
            .query_map(params![REPO_DEFAULT, function_name], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        v
    };
    let pairs = crate::code_usages::collection_folder_pairs();
    let mut spellings: BTreeSet<String> = BTreeSet::new();
    for (kind, owner, folder) in &decls {
        if kind == "common" {
            // Общий модуль: вызов `Модуль.Метод`.
            if let Some(owner) = owner {
                spellings.insert(format!("{owner}.{function_name}"));
            }
        } else if kind == "manager" {
            // Модуль менеджера: вызов `Коллекция.Объект.Метод`, обе формы
            // обращения (RU `Справочники` и EN `Catalogs`) ведут в одну папку.
            if let (Some(owner), Some(folder)) = (owner, folder) {
                for (coll, f) in &pairs {
                    if *f == folder.as_str() {
                        spellings.insert(format!("{coll}.{owner}.{function_name}"));
                    }
                }
            }
        }
    }
    for spelling in &spellings {
        let mut st =
            conn.prepare("SELECT id, file_id, caller, callee, line FROM calls WHERE callee = ?1")?;
        let rows = st
            .query_map(params![spelling], read_call)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for c in rows {
            if c.id.map_or(true, |id| seen.insert(id)) {
                out.push(c);
            }
        }
    }

    // ── 2. Рёбра графа, привязанные к объявлениям этого имени ─────────────
    // Сюда попадают вызовы через переменную (`Объект.Метод`), которым адрес
    // проставило правило привязки к модулю объекта формы.
    let edges: Vec<(String, String)> = {
        let mut st = conn.prepare(
            "SELECT caller_proc_key, callee_proc_name FROM proc_call_graph \
             WHERE repo = ?1 AND call_type = 'direct' \
               AND callee_proc_key IN (SELECT path || '::' || name FROM exported_procs \
                                       WHERE repo = ?1 AND name = ?2) \
               AND instr(callee_proc_name, '.') > 0",
        )?;
        let v = st
            .query_map(params![REPO_DEFAULT, function_name], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        v
    };
    for (caller_key, callee_name) in &edges {
        // Вызовы, уже взятые точным написанием, повторно не берём.
        if spellings.contains(callee_name) {
            continue;
        }
        // `caller_proc_key` = `<path>::<caller>` — режем по ПЕРВОМУ `::`.
        let (path, caller) = match caller_key.split_once("::") {
            Some(v) => v,
            None => continue,
        };
        let mut st = conn.prepare(
            "SELECT c.id, c.file_id, c.caller, c.callee, c.line \
             FROM calls c JOIN files f ON f.id = c.file_id \
             WHERE c.callee = ?1 AND c.caller = ?2 AND f.path = ?3",
        )?;
        let rows = st
            .query_map(params![callee_name, caller, path], read_call)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for c in rows {
            if c.id.map_or(true, |id| seen.insert(id)) {
                out.push(c);
            }
        }
    }
    Ok(())
}

/// Прочитать строку `calls` в `CallRecord` (`line` в БД — `INTEGER`).
fn read_call(r: &rusqlite::Row<'_>) -> rusqlite::Result<CallRecord> {
    Ok(CallRecord {
        id: Some(r.get(0)?),
        file_id: r.get(1)?,
        caller: r.get(2)?,
        callee: r.get(3)?,
        line: r.get::<_, i64>(4)? as usize,
    })
}
