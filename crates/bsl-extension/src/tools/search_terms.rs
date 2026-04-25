// MCP-tool `search_terms` — поиск процедур 1С по бизнес-терминам через
// FTS5 на колонке `procedure_enrichment.terms`.
//
// Это «оффлайновый семантический канал» из карточки 261:
//   * не требует embedder и интернета;
//   * работает по уже накопленным termам (заполняются командой
//     `bsl-indexer enrich`);
//   * NULL/отсутствующие записи просто не находятся — это ожидаемое
//     поведение progressive enhancement, а не баг.
//
// Под feature `enrichment` НЕ помещается. Сама таблица
// `procedure_enrichment` создаётся schema_extensions всегда, и tool
// просто ничего не находит, если она пуста (returns `{"results": []}`).
// Зачем держать tool вне feature: search_terms — read-only, без
// HTTP-клиента; полезен и в публичных сборках bsl-indexer без enrichment
// (на VM RAG, где enrichment мог отрабатывать на другой машине, а
// здесь только индекс с готовыми termами).

use std::future::Future;
use std::pin::Pin;

use code_index_core::extension::{IndexTool, ToolContext};
use rusqlite::params;
use serde_json::{json, Value};

pub struct SearchTermsTool;

impl IndexTool for SearchTermsTool {
    fn name(&self) -> &str {
        "search_terms"
    }

    fn description(&self) -> &str {
        "Ищет процедуры 1С по бизнес-терминам, ранее извлечённым LLM-обогащением \
         (см. `bsl-indexer enrich`). Использует FTS5 на колонке terms, поддерживает \
         FTS-синтаксис: AND, OR, NOT, \"точная фраза\", префикс*. Возвращает массив \
         {proc_key, terms, signature, score} с наилучшим совпадением сверху. Если в \
         репо ещё ни одна процедура не обогащена — возвращает пустой массив. \
         For BSL/1C repositories only."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo": {
                    "type": "string",
                    "description": "Алиас репозитория (из --path alias=dir или daemon.toml)"
                },
                "query": {
                    "type": "string",
                    "description": "FTS5-запрос: 'скидки' / 'товары AND склад' / '\"приём заказа\"' / 'провед*'"
                },
                "limit": {
                    "type": "integer",
                    "description": "Максимум результатов. По умолчанию 20.",
                    "default": 20,
                    "minimum": 1,
                    "maximum": 200
                }
            },
            "required": ["repo", "query"]
        })
    }

    fn applicable_languages(&self) -> Option<&'static [&'static str]> {
        Some(&["bsl"])
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        ctx: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = Value> + Send + 'a>> {
        Box::pin(async move {
            let query = match args.get("query").and_then(|v| v.as_str()) {
                Some(s) if !s.trim().is_empty() => s.to_string(),
                _ => {
                    return json!({"error": "missing or empty parameter 'query' (string)"});
                }
            };
            let limit: i64 = args
                .get("limit")
                .and_then(|v| v.as_i64())
                .unwrap_or(20)
                .clamp(1, 200);

            let storage = ctx.storage.lock().await;
            let conn = storage.conn();

            // FTS5 поиск по terms + JOIN с procedure_enrichment для proc_key,
            // signature. Фильтрация по repo идёт ПОСЛЕ FTS-матча (FTS-индекс
            // не разделён по repo — это компромисс: один FTS на всю БД проще
            // в обслуживании, на масштабе УТ ~313к процедур latency
            // ~единицы мс).
            //
            // ORDER BY rank — стандартное FTS5-ранжирование (BM25). Меньше
            // — лучше; в выводе отдаём как `score` для прозрачности LLM.
            let sql = "
                SELECT pe.proc_key, pe.terms, pe.signature, fts.rank
                FROM fts_procedure_enrichment fts
                JOIN procedure_enrichment pe ON pe.id = fts.rowid
                WHERE pe.repo = ?1 AND fts.terms MATCH ?2
                ORDER BY fts.rank
                LIMIT ?3
            ";

            let mut stmt = match conn.prepare(sql) {
                Ok(s) => s,
                Err(e) => return json!({"error": format!("prepare: {}", e)}),
            };
            let rows_iter = stmt.query_map(params![ctx.repo, &query, limit], |r| {
                Ok(json!({
                    "proc_key": r.get::<_, String>(0)?,
                    "terms": r.get::<_, Option<String>>(1)?,
                    "signature": r.get::<_, Option<String>>(2)?,
                    "score": r.get::<_, f64>(3)?,
                }))
            });

            let rows: Vec<Value> = match rows_iter {
                Ok(iter) => iter
                    .filter_map(|r| r.ok())
                    .collect(),
                Err(e) => {
                    // Типичная причина — невалидный FTS5 синтаксис в query.
                    // Возвращаем структурированную ошибку, чтобы LLM
                    // подкорректировала запрос.
                    return json!({
                        "error": format!("FTS-запрос '{}' отвергнут: {}", query, e)
                    });
                }
            };

            json!({
                "query": query,
                "results": rows,
            })
        })
    }
}
