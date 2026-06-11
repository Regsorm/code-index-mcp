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
        "ПЕРВЫЙ выбор для поиска «где в конфигурации реализован функционал X»: ищет \
         процедуры 1С по словам, а не по точному написанию идентификатора. Термы \
         заполняются механически при индексации: слова имени процедуры \
         (CamelCase-сплит: УточнитьДанныеПоШтрихкоду → уточнить данные по штрихкоду), \
         имя и СИНОНИМ объекта-владельца (русское представление ↔ английский \
         идентификатор), комментарий над процедурой. КАК СПРАШИВАТЬ: 1-3 ключевых \
         слова или корень слова — 'штрихкод', 'резерв склад', 'расчет цен'. Слова \
         объединяются по ИЛИ, лучшие совпадения (больше слов совпало) — сверху; НЕ \
         нужно угадывать точную фразу. Словоформы и подстроки от 3 символов работают \
         (триграммы: 'штрихкод' найдёт 'ПоШтрихкоду'), регистр и ё/е не важны. Явный \
         FTS-синтаксис (AND/OR/NOT/\"фраза\") тоже поддержан. Возвращает {proc_key, \
         terms, score}; proc_key = '<путь>::<имя процедуры>' — тело дальше брать \
         через get_function. Точное написание символа известно → \
         get_function/find_symbol; regex по коду → grep_body/grep_code. \
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
                    return crate::tools::wrap_error(json!({
                        "error": "missing or empty parameter 'query' (string)"
                    }));
                }
            };
            // Серверное переписывание запроса (поймано бенчем 2026-06-10):
            // многословный запрос без явных операторов в FTS5 — неявный AND,
            // на коротких термах почти всегда 0 совпадений. Переписываем в OR
            // по словам (BM25 поднимет строки с бОльшим числом совпадений) +
            // свёртка ё→е (термы нормализованы так же, см. terms::fold_text).
            // Явный FTS-синтаксис (AND/OR/NOT/кавычки/скобки/префикс*)
            // пропускаем как есть, только с ё→е.
            let has_ops = query.contains(" AND ")
                || query.contains(" OR ")
                || query.contains(" NOT ")
                || query.contains('"')
                || query.contains('(')
                || query.contains('*');
            let fts_query = if has_ops {
                query.replace('ё', "е").replace('Ё', "Е")
            } else {
                // Слова короче 3 символов триграммами не ищутся — отбрасываем.
                let words: Vec<String> = crate::terms::fold_text(&query)
                    .split_whitespace()
                    .filter(|w| w.chars().count() >= 3)
                    .map(|w| format!("\"{}\"", w))
                    .collect();
                if words.is_empty() {
                    crate::terms::fold_text(&query)
                } else {
                    words.join(" OR ")
                }
            };
            let limit: i64 = args
                .get("limit")
                .and_then(|v| v.as_i64())
                .unwrap_or(20)
                .clamp(1, 200);

            let storage = match ctx.storage.get().await {
                Ok(s) => s,
                Err(e) => {
                    return crate::tools::wrap_error(serde_json::json!({
                        "error": format!("storage pool: {}", e)
                    }));
                }
            };
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
                Err(e) => {
                    return crate::tools::wrap_error(json!({
                        "error": format!("prepare: {}", e)
                    }))
                }
            };
            // Репо-колонка в per-repo БД всегда 'default' (см. index_extras::REPO_DEFAULT);
            // ctx.repo — алиас маршрутизации, в данных его нет (как во всех BSL-tools).
            let rows_iter = stmt.query_map(params!["default", &fts_query, limit], |r| {
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
                    return crate::tools::wrap_error(json!({
                        "error": format!("FTS-запрос '{}' отвергнут: {}", query, e)
                    }));
                }
            };

            // E1: пустой результат + пустая таблица обогащения → подсказка,
            // что enrich не запускался (иначе пусто читается как «нет совпадений»,
            // и агент зря тратит вызов вместо grep_body/search_function).
            // fts_query показываем для прозрачности: агент видит, как его
            // запрос был переписан (OR-семантика), и может скорректироваться.
            let mut result = json!({ "query": query, "fts_query": fts_query, "results": rows });
            if result["results"]
                .as_array()
                .map(|a| a.is_empty())
                .unwrap_or(false)
            {
                let enriched: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM procedure_enrichment WHERE repo = ?1",
                        params!["default"],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                if enriched == 0 {
                    result["hint"] = json!(
                        "Таблица термов пуста — репо ещё не переиндексировано версией с \
                         механическим обогащением (0.30.0+). Нужна полная переиндексация \
                         (`bsl-indexer index <path> --force`); до неё используйте \
                         grep_body/grep_code/search_function/get_function."
                    );
                } else {
                    result["hint"] = json!(
                        "0 совпадений. Триграммный поиск: запрос должен быть ≥3 символов; \
                         попробуйте другую словоформу/корень слова ('провед' вместо \
                         'проведение'), синоним понятия или OR-комбинацию. Точное имя \
                         символа → find_symbol/get_function."
                    );
                }
            }
            crate::tools::wrap_with_meta(result, Vec::new())
        })
    }
}
