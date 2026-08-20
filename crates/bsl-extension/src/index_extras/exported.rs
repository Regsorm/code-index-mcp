//! Справочник экспортных процедур конфигурации (`exported_procs`).
//!
//! Чтобы понять, куда ведёт вызов из изменившегося модуля, мало самого
//! модуля: в коде 1С вызов записан как `Модуль.Метод` либо просто `Метод`, и
//! адрес выводится только знанием всей конфигурации — в каком модуле имя
//! объявлено экспортным и не объявлено ли оно ещё где-то.
//!
//! Раньше это знание собиралось заново на КАЖДЫЙ пакет изменений: временные
//! таблицы строились полным перебором всех процедур с условием
//! `args LIKE '%) Экспорт%'` — экспортность нигде не хранилась, а поиск
//! подстроки индексом не ускорить. На типовой торговой конфигурации это 9,4
//! секунды на пакет независимо от того, изменился один файл или тысяча.
//!
//! Теперь справочник постоянный: собирается целиком при полном пересборе
//! графа и ведётся пофайлово при точечных обновлениях.

use anyhow::Result;
use rusqlite::params;

use super::*;

/// Условие экспортности в тексте сигнатуры. Отдельного признака у процедуры
/// нет, поэтому экспортность определяется по ключевому слову после списка
/// параметров — так же, как это делалось во временных таблицах до появления
/// справочника.
const EXPORT_MARK: &str = "%) Экспорт%";

/// Вид модуля по его пути и данные для адресации вызова.
///
/// * `common`  — общий модуль: `.../CommonModules/<Имя>/Ext/Module.bsl`,
///   вызов вида `ОбщегоНазначения.Метод`; `owner` = имя общего модуля.
/// * `manager` — модуль менеджера: `.../<Папка>/<Объект>/[Ext/]ManagerModule.bsl`,
///   вызов вида `Справочники.Контрагенты.Метод`; `owner` = объект,
///   `folder` = папка типа. Принимаются обе раскладки — Конфигуратора (с `Ext`)
///   и 1C:EDT (без него).
/// * `other`   — прочие модули; участвуют только в проверке «это имя
///   где-нибудь экспортно» (отсев платформенного балласта).
pub(crate) fn classify_module(path: &str) -> (&'static str, Option<String>, Option<String>) {
    const CM: &str = "CommonModules/";
    if path.ends_with("/Module.bsl") {
        if let Some(idx) = path.find(CM) {
            let seg = &path[idx + CM.len()..];
            if let Some(slash) = seg.find('/') {
                return ("common", Some(seg[..slash].to_string()), None);
            }
        }
    }
    let manager = path
        .strip_suffix("/Ext/ManagerModule.bsl")
        .or_else(|| path.strip_suffix("/ManagerModule.bsl"));
    if let Some(prefix) = manager {
        let mut segs = prefix.rsplit('/');
        if let (Some(object), Some(folder)) = (segs.next(), segs.next()) {
            return ("manager", Some(object.to_string()), Some(folder.to_string()));
        }
    }
    ("other", None, None)
}

/// Собрать справочник заново по всей конфигурации. Зовётся при полном
/// пересборе графа — там всё равно читается вся таблица процедур.
///
/// Транзакцией НЕ управляет: вызывающий уже ведёт свою, а вложенные
/// `BEGIN`/`COMMIT` в SQLite рвут её (внешний `COMMIT` потом падает с
/// «нет активной транзакции»).
pub(crate) fn rebuild_exported_procs(conn: &rusqlite::Connection) -> Result<usize> {
    let rows: Vec<(String, String)> = {
        let mut st = conn.prepare(
            "SELECT fl.path, fn.name FROM functions fn JOIN files fl ON fl.id = fn.file_id \
             WHERE fn.name IS NOT NULL AND fn.name != '' AND fn.args LIKE ?1",
        )?;
        let v = st
            .query_map(params![EXPORT_MARK], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        v
    };

    conn.execute(
        "DELETE FROM exported_procs WHERE repo = ?1",
        params![REPO_DEFAULT],
    )?;
    {
        let mut ins = conn.prepare(
            "INSERT OR IGNORE INTO exported_procs (repo, path, name, kind, owner, folder) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for (path, name) in &rows {
            let (kind, owner, folder) = classify_module(path);
            ins.execute(params![REPO_DEFAULT, path, name, kind, owner, folder])?;
        }
    }
    Ok(rows.len())
}

/// Переписать строки справочника для одного модуля: удалить прежние и внести
/// его текущие экспортные процедуры. Для удалённого файла строк в `functions`
/// уже нет — остаётся только удаление. Стоимость — по числу процедур модуля.
pub(crate) fn update_exported_procs_for_file(
    conn: &rusqlite::Connection,
    rel_path: &str,
) -> Result<()> {
    let names: Vec<String> = {
        let mut st = conn.prepare(
            "SELECT fn.name FROM functions fn JOIN files fl ON fl.id = fn.file_id \
             WHERE fl.path = ?1 AND fn.name IS NOT NULL AND fn.name != '' AND fn.args LIKE ?2",
        )?;
        let v = st
            .query_map(params![rel_path, EXPORT_MARK], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        v
    };

    conn.execute(
        "DELETE FROM exported_procs WHERE repo = ?1 AND path = ?2",
        params![REPO_DEFAULT, rel_path],
    )?;
    if names.is_empty() {
        return Ok(());
    }
    let (kind, owner, folder) = classify_module(rel_path);
    let mut ins = conn.prepare(
        "INSERT OR IGNORE INTO exported_procs (repo, path, name, kind, owner, folder) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    for name in &names {
        ins.execute(params![REPO_DEFAULT, rel_path, name, kind, owner, folder])?;
    }
    Ok(())
}

/// Пуст ли справочник. База, проиндексированная прежней версией, справочника
/// не имеет — тогда точечный резолв не сработает и нужен полный пересбор.
pub(crate) fn exported_procs_empty(conn: &rusqlite::Connection) -> bool {
    conn.query_row(
        "SELECT 1 FROM exported_procs WHERE repo = ?1 LIMIT 1",
        params![REPO_DEFAULT],
        |_| Ok(()),
    )
    .is_err()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn вид_модуля_определяется_по_пути() {
        assert_eq!(
            classify_module("base/CommonModules/ОбщегоНазначения/Ext/Module.bsl"),
            ("common", Some("ОбщегоНазначения".to_string()), None)
        );
        // Раскладка 1C:EDT — без каталога Ext.
        assert_eq!(
            classify_module("src/CommonModules/ОбщегоНазначения/Module.bsl"),
            ("common", Some("ОбщегоНазначения".to_string()), None)
        );
        assert_eq!(
            classify_module("base/Catalogs/Контрагенты/Ext/ManagerModule.bsl"),
            (
                "manager",
                Some("Контрагенты".to_string()),
                Some("Catalogs".to_string())
            )
        );
        assert_eq!(
            classify_module("src/Documents/ЗаказКлиента/ManagerModule.bsl"),
            (
                "manager",
                Some("ЗаказКлиента".to_string()),
                Some("Documents".to_string())
            )
        );
        // Модуль формы — ни общий, ни менеджера.
        assert_eq!(
            classify_module("base/Catalogs/Контрагенты/Forms/ФормаЭлемента/Ext/Form/Module.bsl"),
            ("other", None, None)
        );
    }
}
