// Batch-обогащение процедур через `ChatClient`.
//
// Поток:
//   1. Из core::functions выбираем процедуры BSL без записи в
//      `procedure_enrichment` (или все, если `reenrich = true`).
//   2. Берём пачку `batch_size` штук, отправляем параллельно через
//      `JoinSet`.
//   3. По каждому ответу — `parse_response`, UPSERT в `procedure_enrichment`.
//   4. Цикл, пока есть необогащённые процедуры или достигнут лимит.
//
// `proc_key` для записей — `"<file_path>::<function_name>"`. Это согласуется
// с тем, что таблица `procedure_enrichment` уникальна по `(repo, proc_key)`,
// и оставляет дверь для resolution в этап 4e (где формат может смениться
// на `<module>.<proc>` с миграцией).
//
// repo — пока константа `REPO_DEFAULT = "default"` (как в `index_extras`),
// до интеграции с демоном.

use std::sync::Arc;

use anyhow::{Context, Result};
use code_index_core::storage::Storage;
use rusqlite::params;

use super::client::ChatClient;
use super::prompt::{build_messages, parse_response};

/// Repo-key для оффлайн-обогащения. Должен совпадать с REPO_DEFAULT в
/// `index_extras` — иначе search_terms не увидит обогащённых записей.
const REPO_DEFAULT: &str = "default";

/// Результат одного прогона `run`.
#[derive(Debug, Default, Clone)]
pub struct EnrichmentStats {
    /// Сколько процедур мы попытались обогатить.
    pub attempted: usize,
    /// Из них успешно записали в `procedure_enrichment`.
    pub written: usize,
    /// Сколько вернули пустой ответ (parse_response = None).
    pub empty: usize,
    /// Сколько закончились ошибкой (HTTP / парсинг).
    pub failed: usize,
}

/// Параметры одного прогона.
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// Шаблон system-промпта.
    pub prompt_template: String,
    /// Подпись текущей конфигурации (`provider:model`). Записывается
    /// в каждую строку `procedure_enrichment.signature` и в
    /// `embedding_meta.enrichment_signature`.
    pub signature: String,
    /// Максимум процедур за один прогон. `None` — без лимита.
    pub limit: Option<usize>,
    /// `true` — переобогатить даже те процедуры, у которых уже есть terms.
    pub reenrich: bool,
    /// Сколько процедур обрабатывать одной пачкой через JoinSet.
    pub batch_size: usize,
}

/// Описание одной процедуры для обогащения, считанное из core-таблиц.
#[derive(Debug, Clone)]
struct ProcedureRow {
    proc_key: String,
    name: String,
    body: String,
}

/// Прогон обогащения. На каждую процедуру делается один HTTP-запрос
/// через `client`. По окончанию подпись пишется в `embedding_meta`.
pub async fn run(
    storage: &mut Storage,
    client: Arc<dyn ChatClient>,
    opts: &RunOptions,
) -> Result<EnrichmentStats> {
    // Состояние подписи ДО прогона: от него зависит, можно ли записать новую
    // (см. решение в конце функции).
    let signature_before = super::signature::check_signature(storage.conn(), &opts.signature)?;
    let candidates = collect_candidates(storage, opts.reenrich, opts.limit)?;
    let total = candidates.len();
    if total == 0 {
        tracing::info!(
            "enrichment: нечего обогащать (reenrich={}, limit={:?})",
            opts.reenrich,
            opts.limit
        );
        return Ok(EnrichmentStats::default());
    }

    tracing::info!(
        "enrichment: candidates={}, batch_size={}, signature={}",
        total,
        opts.batch_size,
        opts.signature
    );

    let mut stats = EnrichmentStats::default();
    let template = Arc::new(opts.prompt_template.clone());

    for chunk in candidates.chunks(opts.batch_size.max(1)) {
        let mut tasks: tokio::task::JoinSet<(ProcedureRow, Result<Option<String>>)> =
            tokio::task::JoinSet::new();
        for proc in chunk {
            let proc = proc.clone();
            let client = Arc::clone(&client);
            let template = Arc::clone(&template);
            tasks.spawn(async move {
                let messages = build_messages(&template, &proc.name, &proc.body);
                let response = client.complete(messages).await;
                match response {
                    Ok(raw) => (proc, Ok(parse_response(&raw))),
                    Err(e) => (proc, Err(e)),
                }
            });
        }

        while let Some(joined) = tasks.join_next().await {
            stats.attempted += 1;
            match joined {
                Ok((proc, Ok(Some(terms)))) => {
                    if let Err(e) = upsert_terms(storage, &proc.proc_key, &terms, &opts.signature) {
                        tracing::warn!(
                            "enrichment: не удалось записать proc_key={}: {}",
                            proc.proc_key,
                            e
                        );
                        stats.failed += 1;
                    } else {
                        stats.written += 1;
                    }
                }
                Ok((proc, Ok(None))) => {
                    tracing::debug!("enrichment: пустой ответ для {}", proc.proc_key);
                    stats.empty += 1;
                }
                Ok((proc, Err(e))) => {
                    tracing::warn!("enrichment: ошибка для {}: {}", proc.proc_key, e);
                    stats.failed += 1;
                }
                Err(join_err) => {
                    tracing::error!("enrichment: JoinSet panicked: {}", join_err);
                    stats.failed += 1;
                }
            }
        }
    }

    // Подпись означает «вся база обогащена ЭТОЙ моделью», поэтому пишем её
    // только когда это правда. Прежде она записывалась в конце любого прогона:
    // достаточно было пробного обогащения сотни процедур новой моделью, чтобы
    // признак рассинхронизации исчез, и следующая сверка сказала «совпадает»
    // при смеси двух моделей в базе (C-3).
    //
    // Условия записи:
    //   * прогон не урезан (`limit` не задан) — иначе охвачены не все;
    //   * не было отказов — иначе часть процедур осталась без термов этой модели;
    //   * прежняя подпись отсутствовала либо совпадала, а при расхождении —
    //     только с полным переобогащением (`reenrich`), как и требует описание
    //     сверки: при расхождении решает человек.
    // Пустые ответы модели записи не мешают: это состоявшийся результат, смеси
    // моделей он не создаёт.
    let mismatch_without_reenrich = matches!(
        signature_before,
        super::signature::SignatureCheck::Mismatch { .. }
    ) && !opts.reenrich;
    let full_run = opts.limit.is_none() && stats.failed == 0;
    if full_run && !mismatch_without_reenrich {
        super::signature::write_signature(storage.conn(), &opts.signature)
            .context("не удалось обновить enrichment_signature")?;
    } else {
        let reason = if mismatch_without_reenrich {
            "в базе термы другой модели, а прогон без --reenrich"
        } else if opts.limit.is_some() {
            "прогон был ограничен по числу процедур"
        } else {
            "часть процедур завершилась отказом"
        };
        tracing::warn!(
            "enrichment: подпись НЕ обновлена ({}). Она означает «вся база обогащена \
             этой моделью» — записывать её сейчас значило бы скрыть рассинхронизацию.",
            reason
        );
    }

    tracing::info!(
        "enrichment: готово — written={}, empty={}, failed={}",
        stats.written,
        stats.empty,
        stats.failed
    );
    Ok(stats)
}

/// Подобрать процедуры BSL, которые ещё не обогащены (или все, если
/// `reenrich = true`). Возвращает `Vec<ProcedureRow>` с уже готовым
/// `proc_key`.
fn collect_candidates(
    storage: &Storage,
    reenrich: bool,
    limit: Option<usize>,
) -> Result<Vec<ProcedureRow>> {
    let conn = storage.conn();
    // Базовый запрос: BSL-процедуры из functions + path файла. proc_key —
    // `<file_path>::<function_name>`.
    let base_sql = "\
        SELECT files.path AS file_path, functions.name, functions.body \
        FROM functions \
        JOIN files ON files.id = functions.file_id \
        WHERE files.language = 'bsl'";
    let filter_sql = if reenrich {
        ""
    } else {
        // LEFT JOIN на procedure_enrichment по proc_key — оставляем только
        // те, у кого записи нет либо terms IS NULL.
        " AND NOT EXISTS (\
              SELECT 1 FROM procedure_enrichment pe \
              WHERE pe.repo = ? \
                AND pe.proc_key = files.path || '::' || functions.name \
                AND pe.terms IS NOT NULL\
            )"
    };
    let limit_sql = match limit {
        Some(n) => format!(" LIMIT {}", n),
        None => String::new(),
    };
    let sql = format!("{}{}{}", base_sql, filter_sql, limit_sql);

    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<ProcedureRow> = if reenrich {
        stmt.query_map([], |r| {
            Ok(ProcedureRow {
                proc_key: format!(
                    "{}::{}",
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?
                ),
                name: r.get::<_, String>(1)?,
                body: r.get::<_, String>(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    } else {
        stmt.query_map(params![REPO_DEFAULT], |r| {
            Ok(ProcedureRow {
                proc_key: format!(
                    "{}::{}",
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?
                ),
                name: r.get::<_, String>(1)?,
                body: r.get::<_, String>(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    Ok(rows)
}

/// UPSERT terms+signature для одной процедуры.
fn upsert_terms(
    storage: &mut Storage,
    proc_key: &str,
    terms: &str,
    signature: &str,
) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    storage.conn().execute(
        "INSERT INTO procedure_enrichment (repo, proc_key, terms, signature, updated_at) \
         VALUES (?, ?, ?, ?, ?) \
         ON CONFLICT(repo, proc_key) DO UPDATE SET \
             terms = excluded.terms, \
             signature = excluded.signature, \
             updated_at = excluded.updated_at",
        params![REPO_DEFAULT, proc_key, terms, signature, now],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enrichment::client::test_support::MockChatClient;
    use code_index_core::storage::Storage;
    use tempfile::TempDir;

    fn open_storage_with_bsl_function(tmp: &TempDir, file_path: &str, name: &str, body: &str) -> Storage {
        let db_path = tmp.path().join("index.db");
        let storage = Storage::open_file(&db_path).unwrap();
        storage.apply_schema_extensions(crate::schema::SCHEMA_EXTENSIONS).unwrap();

        let conn = storage.conn();
        conn.execute(
            "INSERT INTO files (path, content_hash, language) VALUES (?, ?, 'bsl')",
            params![file_path, "h"],
        )
        .unwrap();
        let file_id: i64 = conn
            .query_row("SELECT id FROM files WHERE path = ?", params![file_path], |r| {
                r.get(0)
            })
            .unwrap();
        conn.execute(
            "INSERT INTO functions (file_id, name, body, node_hash) VALUES (?, ?, ?, ?)",
            params![file_id, name, body, "h"],
        )
        .unwrap();
        storage
    }

    #[tokio::test]
    async fn run_writes_terms_and_updates_signature() {
        let tmp = TempDir::new().unwrap();
        let mut storage = open_storage_with_bsl_function(
            &tmp,
            "CommonModules/Расчёт/Ext/Module.bsl",
            "Старт",
            "// тело процедуры",
        );

        let client = Arc::new(MockChatClient::with_responses([Ok(
            "1. товары\n2. склад\n3. проведение".to_string()
        )]));
        let opts = RunOptions {
            prompt_template: "sys".to_string(),
            signature: "openai_compatible:claude-haiku-4.5".to_string(),
            limit: None,
            reenrich: false,
            batch_size: 4,
        };
        let stats = run(&mut storage, client.clone(), &opts).await.unwrap();
        assert_eq!(stats.attempted, 1);
        assert_eq!(stats.written, 1);
        assert_eq!(stats.empty, 0);
        assert_eq!(stats.failed, 0);

        let terms: String = storage
            .conn()
            .query_row(
                "SELECT terms FROM procedure_enrichment WHERE repo = ? AND proc_key = ?",
                params!["default", "CommonModules/Расчёт/Ext/Module.bsl::Старт"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(terms, "товары, склад, проведение");

        let sig = super::super::signature::read_signature(storage.conn()).unwrap();
        assert_eq!(sig.as_deref(), Some("openai_compatible:claude-haiku-4.5"));

        // Mock-клиент должен был быть вызван ровно один раз.
        assert_eq!(client.calls.lock().unwrap().len(), 1);
    }

    /// C-3: подпись означает «вся база обогащена этой моделью». Урезанный по
    /// числу процедур прогон такого не даёт — записать подпись значило бы
    /// скрыть рассинхронизацию от следующей сверки.
    #[tokio::test]
    async fn limited_run_does_not_write_signature() {
        let tmp = TempDir::new().unwrap();
        let mut storage = open_storage_with_bsl_function(&tmp, "A.bsl", "П", "// тело");

        let client = Arc::new(MockChatClient::with_responses([Ok("термы".to_string())]));
        let opts = RunOptions {
            prompt_template: "sys".to_string(),
            signature: "новая-модель".to_string(),
            limit: Some(1), // <-- прогон урезан
            reenrich: false,
            batch_size: 4,
        };
        let stats = run(&mut storage, client, &opts).await.unwrap();
        assert_eq!(stats.written, 1, "термы записаны как обычно");
        assert_eq!(
            super::super::signature::read_signature(storage.conn()).unwrap(),
            None,
            "подпись после урезанного прогона не пишется"
        );
    }

    /// C-3: прогон с отказами тоже не даёт права записать подпись — часть
    /// процедур осталась без термов этой модели.
    #[tokio::test]
    async fn failed_run_does_not_write_signature() {
        let tmp = TempDir::new().unwrap();
        let mut storage = open_storage_with_bsl_function(&tmp, "A.bsl", "П", "// тело");

        let client = Arc::new(MockChatClient::with_responses([Err(anyhow::anyhow!("сеть"))]));
        let opts = RunOptions {
            prompt_template: "sys".to_string(),
            signature: "новая-модель".to_string(),
            limit: None,
            reenrich: false,
            batch_size: 4,
        };
        let stats = run(&mut storage, client, &opts).await.unwrap();
        assert_eq!(stats.failed, 1);
        assert_eq!(
            super::super::signature::read_signature(storage.conn()).unwrap(),
            None,
            "после отказов подпись не пишется"
        );
    }

    /// C-3: в базе термы прежней модели. Без полного переобогащения запись
    /// новой подписи стёрла бы признак расхождения — решать должен человек.
    #[tokio::test]
    async fn mismatch_without_reenrich_keeps_stored_signature() {
        let tmp = TempDir::new().unwrap();
        let mut storage = open_storage_with_bsl_function(&tmp, "A.bsl", "П", "// тело");
        super::super::signature::write_signature(storage.conn(), "старая-модель").unwrap();

        let client = Arc::new(MockChatClient::with_responses([Ok("термы".to_string())]));
        let opts = RunOptions {
            prompt_template: "sys".to_string(),
            signature: "новая-модель".to_string(),
            limit: None,
            reenrich: false, // <-- расхождение без переобогащения
            batch_size: 4,
        };
        run(&mut storage, client, &opts).await.unwrap();
        assert_eq!(
            super::super::signature::read_signature(storage.conn())
                .unwrap()
                .as_deref(),
            Some("старая-модель"),
            "прежняя подпись сохраняется, расхождение остаётся видимым"
        );

        // С полным переобогащением — подпись обновляется.
        let client = Arc::new(MockChatClient::with_responses([Ok("термы".to_string())]));
        let opts = RunOptions { reenrich: true, ..opts };
        run(&mut storage, client, &opts).await.unwrap();
        assert_eq!(
            super::super::signature::read_signature(storage.conn())
                .unwrap()
                .as_deref(),
            Some("новая-модель")
        );
    }

    #[tokio::test]
    async fn run_skips_already_enriched_unless_reenrich() {
        let tmp = TempDir::new().unwrap();
        let mut storage = open_storage_with_bsl_function(
            &tmp,
            "X.bsl",
            "P",
            "// b",
        );
        // Уже есть запись с непустыми terms.
        storage
            .conn()
            .execute(
                "INSERT INTO procedure_enrichment (repo, proc_key, terms, signature, updated_at) \
                 VALUES ('default', 'X.bsl::P', 'старое', 'old-sig', 0)",
                [],
            )
            .unwrap();

        let client = Arc::new(MockChatClient::with_responses([Ok("новое".to_string())]));
        let opts = RunOptions {
            prompt_template: "sys".to_string(),
            signature: "new-sig".to_string(),
            limit: None,
            reenrich: false, // <-- ключевое: skip-режим
            batch_size: 4,
        };
        let stats = run(&mut storage, client.clone(), &opts).await.unwrap();
        // candidates пуст — ничего не делалось.
        assert_eq!(stats.attempted, 0);
        assert_eq!(stats.written, 0);
        assert_eq!(client.calls.lock().unwrap().len(), 0);

        // Старые terms нетронуты.
        let terms: String = storage
            .conn()
            .query_row(
                "SELECT terms FROM procedure_enrichment WHERE proc_key = 'X.bsl::P'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(terms, "старое");
    }

    #[tokio::test]
    async fn reenrich_overwrites_existing_terms() {
        let tmp = TempDir::new().unwrap();
        let mut storage = open_storage_with_bsl_function(&tmp, "X.bsl", "P", "// b");
        storage
            .conn()
            .execute(
                "INSERT INTO procedure_enrichment (repo, proc_key, terms, signature, updated_at) \
                 VALUES ('default', 'X.bsl::P', 'старое', 'old-sig', 0)",
                [],
            )
            .unwrap();

        let client = Arc::new(MockChatClient::with_responses([Ok("новое".to_string())]));
        let opts = RunOptions {
            prompt_template: "sys".to_string(),
            signature: "new-sig".to_string(),
            limit: None,
            reenrich: true,
            batch_size: 4,
        };
        let stats = run(&mut storage, client, &opts).await.unwrap();
        assert_eq!(stats.attempted, 1);
        assert_eq!(stats.written, 1);

        let row: (String, String) = storage
            .conn()
            .query_row(
                "SELECT terms, signature FROM procedure_enrichment WHERE proc_key = 'X.bsl::P'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(row.0, "новое");
        assert_eq!(row.1, "new-sig");
    }

    #[tokio::test]
    async fn limit_is_respected() {
        let tmp = TempDir::new().unwrap();
        let mut storage = open_storage_with_bsl_function(&tmp, "A.bsl", "P1", "//1");
        // Добавим вторую процедуру для того же файла.
        let conn = storage.conn();
        let file_id: i64 = conn
            .query_row("SELECT id FROM files WHERE path = 'A.bsl'", [], |r| r.get(0))
            .unwrap();
        conn.execute(
            "INSERT INTO functions (file_id, name, body, node_hash) VALUES (?, ?, ?, ?)",
            params![file_id, "P2", "//2", "h"],
        )
        .unwrap();

        let client = Arc::new(MockChatClient::with_responses([Ok("a".to_string())]));
        let opts = RunOptions {
            prompt_template: "sys".to_string(),
            signature: "sig".to_string(),
            limit: Some(1),
            reenrich: false,
            batch_size: 4,
        };
        let stats = run(&mut storage, client.clone(), &opts).await.unwrap();
        assert_eq!(stats.attempted, 1, "limit=1 должен ограничить");
        assert_eq!(client.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn empty_response_does_not_write_record() {
        let tmp = TempDir::new().unwrap();
        let mut storage = open_storage_with_bsl_function(&tmp, "E.bsl", "P", "//b");

        let client = Arc::new(MockChatClient::with_responses([Ok("".to_string())]));
        let opts = RunOptions {
            prompt_template: "sys".to_string(),
            signature: "sig".to_string(),
            limit: None,
            reenrich: false,
            batch_size: 4,
        };
        let stats = run(&mut storage, client, &opts).await.unwrap();
        assert_eq!(stats.attempted, 1);
        assert_eq!(stats.empty, 1);
        assert_eq!(stats.written, 0);

        let exists: i64 = storage
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM procedure_enrichment WHERE proc_key = 'E.bsl::P'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(exists, 0, "пустой ответ не должен создавать запись");
    }

    #[tokio::test]
    async fn errors_increment_failed_counter() {
        let tmp = TempDir::new().unwrap();
        let mut storage = open_storage_with_bsl_function(&tmp, "F.bsl", "P", "//b");

        let client = Arc::new(MockChatClient::with_responses([Err(anyhow::anyhow!("boom"))]));
        let opts = RunOptions {
            prompt_template: "sys".to_string(),
            signature: "sig".to_string(),
            limit: None,
            reenrich: false,
            batch_size: 4,
        };
        let stats = run(&mut storage, client, &opts).await.unwrap();
        assert_eq!(stats.failed, 1);
        assert_eq!(stats.written, 0);
    }
}
