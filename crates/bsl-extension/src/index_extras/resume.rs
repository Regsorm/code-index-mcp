//! Возобновляемость фаз XML-слоя надстройки.
//!
//! Полный пересбор надстройки на большой конфигурации идёт минутами: фазы
//! идут одна за другой и каждая коммитит свой результат. Если процесс убили
//! посередине (перезапуск машины, остановка демона), следующий проход раньше
//! начинал ВСЁ заново. Здесь записывается отпечаток входа слоя и номер
//! последней успешной фазы: при совпадении отпечатка готовые фазы пропускаются.
//!
//! Отпечаток — версия семантики фаз плюс пути и `mtime`/размер всех файлов,
//! которые слой читает. Любое изменение входа делает старые отметки
//! несовместимыми, и слой собирается заново. Отметки живут в `index_state`
//! (та же таблица, что у признака незавершённой массовой загрузки ядра).

use std::hash::{Hash, Hasher};
use std::time::UNIX_EPOCH;

use anyhow::Result;
use rusqlite::{params, Connection};

use super::RepoScan;

/// Версия набора и семантики фаз. Поднимать при изменении состава фаз или
/// смысла их данных: старые отметки перестанут совпадать по отпечатку.
pub(crate) const EXTRAS_PHASES_VERSION: u32 = 1;

// Номера фаз строго по порядку выполнения. `mark` пишет «собрано фаз N»,
// а `is_done` пропускает все фазы с номером меньше N.
pub(crate) const PH_METADATA_OBJECTS: u32 = 0;
pub(crate) const PH_DATA_LINKS: u32 = 1;
pub(crate) const PH_DATA_LINKS_CONFIG: u32 = 2;
pub(crate) const PH_ROLE_RIGHTS: u32 = 3;
pub(crate) const PH_OBJECT_ATTRIBUTES: u32 = 4;
pub(crate) const PH_OBJECT_SYNONYMS: u32 = 5;
pub(crate) const PH_OBJECT_TEMPLATES: u32 = 6;
pub(crate) const PH_METADATA_FORMS: u32 = 7;
pub(crate) const PH_EVENT_SUBSCRIPTIONS: u32 = 8;
pub(crate) const PH_METADATA_MODULES: u32 = 9;
pub(crate) const PH_CONFIG_MANIFEST: u32 = 10;
pub(crate) const PH_DCS: u32 = 11;
pub(crate) const PH_CODE_USAGES: u32 = 12;
pub(crate) const PH_PROC_TERMS: u32 = 13;
pub(crate) const PH_CALL_GRAPH: u32 = 14;

/// Сколько фаз участвует в резюмируемой цепочке (для тестов и журнала).
pub(crate) const EXTRAS_PHASE_COUNT: u32 = PH_CALL_GRAPH + 1;

/// Ключ отпечатка входа слоя в `index_state`.
const FP_KEY: &str = "extras_resume_fp";
/// Ключ числа собранных фаз в `index_state`.
const DONE_KEY: &str = "extras_resume_done";

/// Состояние возобновления на текущий проход.
pub(crate) struct ExtrasResume {
    fingerprint: u64,
    /// `Cell`, а не `u32`: отметку двигают `&ExtrasResume`-ссылки в ходе фаз, и
    /// последующие фазы того же прохода должны видеть уже сдвинутую цепочку.
    done: std::cell::Cell<u32>,
}

impl ExtrasResume {
    /// Прочитать отметки. Отпечаток не совпал — начинаем с нуля.
    pub(crate) fn load(conn: &Connection, fingerprint: u64) -> Self {
        let stored_fp: Option<String> = conn
            .query_row(
                "SELECT value FROM index_state WHERE key = ?1",
                params![FP_KEY],
                |r| r.get(0),
            )
            .ok();
        let stored_done: u32 = conn
            .query_row(
                "SELECT value FROM index_state WHERE key = ?1",
                params![DONE_KEY],
                |r| r.get::<_, String>(0),
            )
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        let same_input = stored_fp
            .as_deref()
            .and_then(|s| s.trim().parse::<u64>().ok())
            == Some(fingerprint);
        Self {
            fingerprint,
            done: std::cell::Cell::new(if same_input { stored_done } else { 0 }),
        }
    }

    /// Собрана ли фаза с номером `phase` (её результат актуален для входа).
    pub(crate) fn is_done(&self, phase: u32) -> bool {
        phase < self.done.get()
    }

    /// Сколько фаз уже собрано (для журнала).
    pub(crate) fn done(&self) -> u32 {
        self.done.get()
    }

    /// Записать «собраны фазы до `done` включительно» одним куском: отпечаток
    /// и число фаз. Обрыв между двумя записями сделал бы отметку неверной,
    /// поэтому обе — в одной транзакции. Отметка монотонна: меньше уже
    /// записанной она не станет.
    pub(crate) fn mark(&self, conn: &Connection, done: u32) -> Result<()> {
        let done = done.max(self.done.get());
        let _ = conn.execute("ROLLBACK", []); // защита от cascade-ошибки
        conn.execute("BEGIN", [])?;
        let write = (|| -> Result<()> {
            conn.execute(
                "INSERT OR REPLACE INTO index_state (key, value) VALUES (?1, ?2)",
                params![FP_KEY, self.fingerprint.to_string()],
            )?;
            conn.execute(
                "INSERT OR REPLACE INTO index_state (key, value) VALUES (?1, ?2)",
                params![DONE_KEY, done.to_string()],
            )?;
            Ok(())
        })();
        match write {
            Ok(()) => {
                conn.execute("COMMIT", [])?;
                self.done.set(done);
                Ok(())
            }
            Err(e) => {
                let _ = conn.execute("ROLLBACK", []);
                Err(e)
            }
        }
    }
}

/// Отпечаток входа XML-слоя: версия фаз плюс содержимое файлов, которые слой
/// читает. Для файлов, попавших в индекс ядра, берётся `content_hash` из базы
/// (изменение содержимого видно без `stat` и не зависит от `mtime`), а для
/// неиндексированных (крупные XML сверх предела текстового индекса) —
/// `(mtime, размер)` с диска. Порядок — как в списках `scan`.
pub(crate) fn scan_fingerprint(conn: &Connection, scan: &RepoScan) -> u64 {
    // path → content_hash для всего, что ядро записало в базу.
    let tracked: std::collections::HashMap<String, String> = {
        let mut map = std::collections::HashMap::new();
        if let Ok(mut stmt) = conn.prepare("SELECT path, content_hash FROM files") {
            if let Ok(rows) =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            {
                for row in rows.flatten() {
                    map.insert(row.0, row.1);
                }
            }
        }
        map
    };

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    EXTRAS_PHASES_VERSION.hash(&mut hasher);
    for list in [
        &scan.config_paths,
        &scan.root_xmls,
        &scan.nested_subsystems,
        &scan.exchange_content,
        &scan.defined_types,
        &scan.functional_options,
        &scan.rights_files,
        &scan.form_xmls,
        &scan.form_descriptors,
        &scan.event_sub_xmls,
        &scan.template_descriptors,
        &scan.dump_info_files,
        &scan.bsl_files,
        &scan.content_files,
    ] {
        for path in list {
            path.hash(&mut hasher);
            let rel = super::common::rel_path(&scan.repo_root, path);
            match tracked.get(&rel) {
                Some(hash) => hash.hash(&mut hasher),
                None => {
                    // Файл ядром не индексирован (oversize/бинарный) — берём
                    // метаданные диска, иначе его правку отпечаток не заметит.
                    if let Ok(meta) = std::fs::metadata(path) {
                        meta.len().hash(&mut hasher);
                        if let Ok(modified) = meta.modified() {
                            if let Ok(age) = modified.duration_since(UNIX_EPOCH) {
                                age.as_nanos().hash(&mut hasher);
                            }
                        }
                    }
                }
            }
        }
    }
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn storage_with_state() -> (tempfile::TempDir, rusqlite::Connection) {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("index.db");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE index_state (key TEXT PRIMARY KEY, value TEXT NOT NULL);")
            .unwrap();
        (tmp, conn)
    }

    #[test]
    fn чужой_отпечаток_обнуляет_прогресс() {
        let (_tmp, conn) = storage_with_state();
        let resume = ExtrasResume {
            fingerprint: 42,
            done: std::cell::Cell::new(0),
        };
        resume.mark(&conn, 5).unwrap();

        assert_eq!(ExtrasResume::load(&conn, 42).done(), 5);
        assert!(ExtrasResume::load(&conn, 42).is_done(4));
        assert!(!ExtrasResume::load(&conn, 42).is_done(5));

        // Вход изменился — отметки недействительны.
        assert_eq!(ExtrasResume::load(&conn, 43).done(), 0);
    }

    #[test]
    fn без_таблицы_читается_как_нулевой_прогресс() {
        let tmp = tempfile::tempdir().unwrap();
        let conn = rusqlite::Connection::open(tmp.path().join("db.sqlite")).unwrap();
        let resume = ExtrasResume::load(&conn, 1);
        assert_eq!(resume.done(), 0);
    }

    #[test]
    fn отметка_монотонна_и_видна_в_памяти_сразу() {
        let (_tmp, conn) = storage_with_state();
        let resume = ExtrasResume {
            fingerprint: 7,
            done: std::cell::Cell::new(0),
        };
        resume.mark(&conn, 5).unwrap();
        assert_eq!(resume.done(), 5, "отметка обновилась в памяти");
        resume.mark(&conn, 3).unwrap();
        assert_eq!(resume.done(), 5, "понизить отметку нельзя");
        assert_eq!(
            ExtrasResume::load(&conn, 7).done(),
            5,
            "в базе отметка тоже не понизилась"
        );
    }

    #[test]
    fn отпечаток_видит_правку_содержимого_макета() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let write = |path: &std::path::Path, text: &str| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write(&repo.join("base").join("Configuration.xml"), "<x/>");
        let dcs = repo
            .join("base")
            .join("Reports")
            .join("Отчёт")
            .join("Templates")
            .join("Схема")
            .join("Ext")
            .join("Template.dcs");
        write(&dcs, "first");
        let (_tmp2, conn) = storage_with_state();
        let fp1 = scan_fingerprint(&conn, &crate::index_extras::RepoScan::build(&repo));
        write(&dcs, "second-longer");
        let fp2 = scan_fingerprint(&conn, &crate::index_extras::RepoScan::build(&repo));
        assert_ne!(fp1, fp2, "правка Template.dcs обязана менять отпечаток");
    }
}
