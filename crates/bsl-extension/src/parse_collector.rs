// Сборщик BSL-«сырья» в фазе ПАРАЛЛЕЛЬНОГО парсинга ядра (только полный путь
// индексации: `--force` или свежая БД).
//
// Зачем: раньше `index_extras` после парсинга ПОВТОРНО читал все `.bsl` с диска,
// чтобы построить `metadata_code_usages` (и другие слои). Здесь то же сырьё
// вытаскивается прямо из горячих в RAM `parse_results` во время параллельного
// парсинга ядра — диск не перечитывается. Общий примитив извлечения
// (`code_usages::extract_code_usages`) один и тот же, что у инкрементального
// пути (`update_code_usages_for_file`), поэтому результат идентичен.
//
// Корректность: сборщик задействуется ядром ТОЛЬКО при полном парсинге
// (`full_reindex_with_collector` гейтит по `force || is_fresh_db`). Тогда БД
// пустая, все файлы распарсены, и полный `DELETE+INSERT` строит слой с нуля.
// При частичном mtime-fast-path (демон с изменениями) сборщик выключен, и
// `index_extras` делает полный disk-rebuild как раньше. Watcher-инкремент
// (`index_extras_for_files`) сборщик не использует.

use std::sync::Mutex;

use anyhow::Result;
use rusqlite::params;

use code_index_core::extension::{ParseExtrasCollector, ParsedFileCtx};
use code_index_core::storage::Storage;

use crate::code_usages::{extract_code_usages, CodeUsage};

/// repo-ключ в специфичных таблицах: в каждой БД ровно один репо = "default".
const REPO_DEFAULT: &str = "default";

/// Ключ слоя `metadata_code_usages` в temp-маркере «сделано сборщиком».
pub const MARK_CODE_USAGES: &str = "code_usages";

/// Ключ слоя термов процедур в temp-маркере «сделано сборщиком».
pub const MARK_PROC_TERMS: &str = "proc_terms";

/// Одна строка сырья термов процедуры, собранная в фазе парсинга. Синоним
/// объекта тут отсутствует намеренно — он ещё не заполнен (XML-слой идёт
/// позже); подставляется по metadata_objects при сборке из staging.
struct StagedProcTerm {
    proc_key: String,
    proc_name: String,
    object_meta_type: Option<&'static str>,
    object_name: Option<String>,
    comment: Option<String>,
}

/// Сборщик extras BSL для параллельного парсинга. Копит сырьё потоко-безопасно
/// в `on_parsed` (rayon), сбрасывает в БД в `write` (серийно, после фазы записи
/// ядра).
#[derive(Default)]
pub struct BslParseCollector {
    /// Накопленные обращения к объектам МД: (file_path, обращения этого файла).
    code_usages: Mutex<Vec<(String, Vec<CodeUsage>)>>,
    /// Сырьё термов процедур (имя + объект + комментарий), собранное в
    /// парсинге. Синоним объекта подставляется позже, при сборке из staging.
    proc_terms: Mutex<Vec<StagedProcTerm>>,
}

impl BslParseCollector {
    pub fn new() -> Self {
        Self::default()
    }
}

impl ParseExtrasCollector for BslParseCollector {
    fn on_parsed(&self, ctx: ParsedFileCtx) {
        // Все слои сборщика извлекаются только из .bsl-модулей.
        if !ctx.rel_path.to_ascii_lowercase().ends_with(".bsl") {
            return;
        }

        // Слой 1: обращения к объектам МД → metadata_code_usages.
        let usages = extract_code_usages(ctx.content);
        if !usages.is_empty() {
            self.code_usages
                .lock()
                .expect("BslParseCollector.code_usages mutex")
                .push((ctx.rel_path.to_string(), usages));
        }

        // Слой 2: сырьё термов процедур (имя + объект + комментарий). Синоним
        // объекта НЕ берём — он ещё не заполнен; подставится позже.
        if let Some(pr) = ctx.parse_result {
            if !pr.functions.is_empty() {
                use crate::terms::{extract_leading_comment, object_from_module_path};
                let object = object_from_module_path(ctx.rel_path);
                let lines: Vec<&str> = ctx.content.lines().collect();
                let mut staged: Vec<StagedProcTerm> = Vec::with_capacity(pr.functions.len());
                for f in &pr.functions {
                    let comment = extract_leading_comment(&lines, f.line_start);
                    staged.push(StagedProcTerm {
                        proc_key: format!("{}::{}", ctx.rel_path, f.name),
                        proc_name: f.name.clone(),
                        object_meta_type: object.as_ref().map(|(mt, _)| *mt),
                        object_name: object.as_ref().map(|(_, nm)| nm.clone()),
                        comment,
                    });
                }
                self.proc_terms
                    .lock()
                    .expect("BslParseCollector.proc_terms mutex")
                    .extend(staged);
            }
        }
    }

    fn write(&self, storage: &mut Storage) -> Result<()> {
        let conn = storage.conn();
        let files = self
            .code_usages
            .lock()
            .expect("BslParseCollector.code_usages mutex");

        // Полный пересбор metadata_code_usages для repo — как в
        // index_metadata_code_usages (DELETE по repo + INSERT всех обращений).
        let _ = conn.execute("ROLLBACK", []); // защита от cascade-ошибки
        conn.execute("BEGIN", [])?;
        conn.execute(
            "DELETE FROM metadata_code_usages WHERE repo = ?",
            params![REPO_DEFAULT],
        )?;
        let mut total: usize = 0;
        {
            let mut stmt = conn.prepare(
                "INSERT INTO metadata_code_usages \
                 (repo, object_ref, object_ref_key, member_path, usage_kind, file_path, line) \
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )?;
            for (file_path, usages) in files.iter() {
                for u in usages {
                    stmt.execute(params![
                        REPO_DEFAULT,
                        &u.object_ref,
                        &u.object_ref_key,
                        &u.member_path,
                        u.usage_kind,
                        file_path,
                        u.line as i64,
                    ])?;
                    total += 1;
                }
            }
        }
        conn.execute("COMMIT", [])?;

        // Пометить, что этот слой наполнен сборщиком в текущем проходе —
        // чтобы run_index_extras не делал повторный disk-rebuild.
        mark_done(conn, MARK_CODE_USAGES)?;

        tracing::info!(
            "metadata_code_usages (parse-collector): {} обращений из {} .bsl",
            total,
            files.len()
        );

        // Слой 2: сырьё термов процедур → temp-таблица _proc_terms_staging.
        // Финальная сборка термов (build_terms с синонимом объекта) — позже,
        // в build_procedure_terms_from_staging, ПОСЛЕ заполнения синонимов
        // XML-слоем. Здесь только сброс накопленного сырья.
        {
            let terms = self
                .proc_terms
                .lock()
                .expect("BslParseCollector.proc_terms mutex");
            conn.execute_batch("DROP TABLE IF EXISTS _proc_terms_staging; CREATE TEMP TABLE _proc_terms_staging (proc_key TEXT, proc_name TEXT, object_meta_type TEXT, object_name TEXT, comment TEXT);")?;
            conn.execute("BEGIN", [])?;
            {
                let mut stmt = conn.prepare("INSERT INTO _proc_terms_staging (proc_key, proc_name, object_meta_type, object_name, comment) VALUES (?1, ?2, ?3, ?4, ?5)")?;
                for t in terms.iter() {
                    stmt.execute(params![&t.proc_key, &t.proc_name, t.object_meta_type, &t.object_name, &t.comment])?;
                }
            }
            conn.execute("COMMIT", [])?;
            mark_done(conn, MARK_PROC_TERMS)?;
            tracing::info!(
                "procedure_terms (parse-collector): {} процедур в staging",
                terms.len()
            );
        }

        Ok(())
    }
}

/// Пометить в temp-таблице (на текущем соединении), что parse-collector уже
/// наполнил extras-слой `what` в этом проходе индексации. Temp-таблица живёт
/// до reopen БД и самоочищается — маркер не переживает конец полной индексации.
fn mark_done(conn: &rusqlite::Connection, what: &str) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS _parse_collector_done (what TEXT PRIMARY KEY);",
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO _parse_collector_done(what) VALUES (?1)",
        params![what],
    )?;
    Ok(())
}

/// Наполнил ли parse-collector слой `what` в ТЕКУЩЕМ проходе (по temp-маркеру
/// на этом соединении). Используется `run_index_extras`, чтобы пропустить
/// повторный disk-rebuild слоя, который сборщик уже построил в парсинге.
pub fn collector_did(conn: &rusqlite::Connection, what: &str) -> bool {
    // Сначала проверяем существование temp-таблицы, иначе прямой SELECT из
    // отсутствующей таблицы вернёт ошибку.
    let exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_temp_master \
             WHERE type = 'table' AND name = '_parse_collector_done'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if exists == 0 {
        return false;
    }
    conn.query_row(
        "SELECT COUNT(*) FROM _parse_collector_done WHERE what = ?1",
        params![what],
        |r| r.get::<_, i64>(0),
    )
    .unwrap_or(0)
        > 0
}
