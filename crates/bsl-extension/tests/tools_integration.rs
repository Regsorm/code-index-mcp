// Интеграционные тесты для 1С MCP-tools (этап 6).
//
// Каждый тест:
//  1. Создаёт временную SQLite-БД с базовой схемой core +
//     bsl-extension schema_extensions.
//  2. Заполняет таблицы тестовыми данными (вручную или через
//     run_index_extras).
//  3. Вызывает соответствующий IndexTool::execute и проверяет
//     результат.
//
// Тесты в `tests/` потому что нужен полный API code-index-core
// (Storage, ToolContext) — внутри `bsl-extension` тяжело собрать
// все эти типы из-за приватных полей и Mutex<Storage>.

use std::sync::Arc;

use bsl_extension::{
    schema::SCHEMA_EXTENSIONS,
    tools::{
        FindPathTool, GetEventSubscriptionsTool, GetFormHandlersTool, GetObjectStructureTool,
        SearchTermsTool,
    },
};
use code_index_core::extension::{IndexTool, ToolContext};
use code_index_core::storage::Storage;
use rusqlite::params;
use serde_json::Value;
use tempfile::TempDir;
use tokio::sync::Mutex;

const REPO: &str = "default";

fn fresh_storage() -> (TempDir, Arc<Mutex<Storage>>) {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("test.db");
    let storage = Storage::open_file(&db_path).unwrap();
    storage.apply_schema_extensions(SCHEMA_EXTENSIONS).unwrap();
    (tmp, Arc::new(Mutex::new(storage)))
}

async fn run_tool(
    tool: &dyn IndexTool,
    storage: &Arc<Mutex<Storage>>,
    args: Value,
) -> Value {
    let ctx = ToolContext {
        repo: REPO,
        root_path: None,
        language: Some("bsl"),
        storage,
    };
    tool.execute(args, ctx).await
}

// ── get_object_structure ──────────────────────────────────────────────────

#[tokio::test]
async fn get_object_structure_returns_existing() {
    let (_tmp, storage) = fresh_storage();
    {
        let s = storage.lock().await;
        s.conn()
            .execute(
                "INSERT INTO metadata_objects (repo, full_name, meta_type, name, synonym) \
                 VALUES (?, ?, ?, ?, ?)",
                params![REPO, "Catalog.Контрагенты", "Catalog", "Контрагенты", "Контрагенты"],
            )
            .unwrap();
    }
    let res = run_tool(
        &GetObjectStructureTool,
        &storage,
        serde_json::json!({"repo": REPO, "full_name": "Catalog.Контрагенты"}),
    )
    .await;
    assert_eq!(res["meta_type"].as_str(), Some("Catalog"));
    assert_eq!(res["name"].as_str(), Some("Контрагенты"));
    assert_eq!(res["synonym"].as_str(), Some("Контрагенты"));
}

#[tokio::test]
async fn get_object_structure_reports_missing() {
    let (_tmp, storage) = fresh_storage();
    let res = run_tool(
        &GetObjectStructureTool,
        &storage,
        serde_json::json!({"repo": REPO, "full_name": "Catalog.НетТакого"}),
    )
    .await;
    assert!(res["error"].is_string());
}

#[tokio::test]
async fn get_object_structure_validates_full_name() {
    let (_tmp, storage) = fresh_storage();
    let res = run_tool(
        &GetObjectStructureTool,
        &storage,
        serde_json::json!({"repo": REPO}), // нет full_name
    )
    .await;
    assert!(
        res["error"].as_str().unwrap_or("").contains("full_name"),
        "ошибка должна упоминать missing full_name: {:?}",
        res
    );
}

// ── get_form_handlers ─────────────────────────────────────────────────────

#[tokio::test]
async fn get_form_handlers_returns_array() {
    let (_tmp, storage) = fresh_storage();
    let handlers_json = serde_json::json!([
        {"event": "ПриОткрытии", "handler": "ПриОткрытии"},
        {"event": "ПередЗакрытием", "handler": "ОбработатьЗакрытие"}
    ])
    .to_string();
    {
        let s = storage.lock().await;
        s.conn()
            .execute(
                "INSERT INTO metadata_forms (repo, owner_full_name, form_name, handlers_json) \
                 VALUES (?, ?, ?, ?)",
                params![REPO, "Document.Реализация", "ФормаДокумента", handlers_json],
            )
            .unwrap();
    }
    let res = run_tool(
        &GetFormHandlersTool,
        &storage,
        serde_json::json!({
            "repo": REPO,
            "owner_full_name": "Document.Реализация",
            "form_name": "ФормаДокумента"
        }),
    )
    .await;
    let arr = res["handlers"].as_array().expect("handlers — массив");
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["event"].as_str(), Some("ПриОткрытии"));
    assert_eq!(arr[1]["handler"].as_str(), Some("ОбработатьЗакрытие"));
}

// ── get_event_subscriptions ───────────────────────────────────────────────

#[tokio::test]
async fn get_event_subscriptions_lists_all_when_no_filters() {
    let (_tmp, storage) = fresh_storage();
    {
        let s = storage.lock().await;
        for (name, ev, m, p) in &[
            ("Sub1", "ПриЗаписи", "Логирование", "Записать"),
            ("Sub2", "ПередЗаписью", "Аудит", "Проверить"),
        ] {
            s.conn()
                .execute(
                    "INSERT INTO event_subscriptions (repo, name, event, handler_module, handler_proc, sources_json) \
                     VALUES (?, ?, ?, ?, ?, '[]')",
                    params![REPO, name, ev, m, p],
                )
                .unwrap();
        }
    }
    let res = run_tool(
        &GetEventSubscriptionsTool,
        &storage,
        serde_json::json!({"repo": REPO}),
    )
    .await;
    assert_eq!(res["count"].as_u64(), Some(2));
    assert_eq!(res["subscriptions"].as_array().map(|a| a.len()), Some(2));
}

#[tokio::test]
async fn get_event_subscriptions_filters_by_handler_module() {
    let (_tmp, storage) = fresh_storage();
    {
        let s = storage.lock().await;
        for (name, m) in &[("A", "ModX"), ("B", "ModY"), ("C", "ModX")] {
            s.conn()
                .execute(
                    "INSERT INTO event_subscriptions (repo, name, event, handler_module, handler_proc, sources_json) \
                     VALUES (?, ?, 'E', ?, 'P', '[]')",
                    params![REPO, name, m],
                )
                .unwrap();
        }
    }
    let res = run_tool(
        &GetEventSubscriptionsTool,
        &storage,
        serde_json::json!({"repo": REPO, "handler_module": "ModX"}),
    )
    .await;
    assert_eq!(res["count"].as_u64(), Some(2), "filter ModX → 2 совпадения");
}

// ── find_path ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn find_path_returns_direct_edge_when_exists() {
    let (_tmp, storage) = fresh_storage();
    {
        let s = storage.lock().await;
        s.conn()
            .execute(
                "INSERT INTO proc_call_graph (repo, caller_proc_key, callee_proc_name, call_type) \
                 VALUES (?, 'A', 'B', 'direct')",
                params![REPO],
            )
            .unwrap();
    }
    let res = run_tool(
        &FindPathTool,
        &storage,
        serde_json::json!({"repo": REPO, "from": "A", "to": "B", "max_depth": 3}),
    )
    .await;
    assert_eq!(res["found"].as_bool(), Some(true));
    let path = res["path"].as_array().expect("path — массив");
    assert_eq!(path.len(), 1);
    assert_eq!(path[0]["caller"].as_str(), Some("A"));
    assert_eq!(path[0]["callee"].as_str(), Some("B"));
}

#[tokio::test]
async fn find_path_walks_two_hops() {
    let (_tmp, storage) = fresh_storage();
    {
        let s = storage.lock().await;
        s.conn()
            .execute(
                "INSERT INTO proc_call_graph (repo, caller_proc_key, callee_proc_name, call_type) \
                 VALUES (?, 'A', 'B', 'direct'), (?, 'B', 'C', 'direct')",
                params![REPO, REPO],
            )
            .unwrap();
    }
    let res = run_tool(
        &FindPathTool,
        &storage,
        serde_json::json!({"repo": REPO, "from": "A", "to": "C", "max_depth": 3}),
    )
    .await;
    assert_eq!(res["found"].as_bool(), Some(true), "путь A→B→C должен находиться");
    let path = res["path"].as_array().unwrap();
    assert_eq!(path.len(), 2);
}

#[tokio::test]
async fn find_path_returns_not_found_when_no_path() {
    let (_tmp, storage) = fresh_storage();
    let res = run_tool(
        &FindPathTool,
        &storage,
        serde_json::json!({"repo": REPO, "from": "A", "to": "B"}),
    )
    .await;
    assert_eq!(res["found"].as_bool(), Some(false));
    assert!(res["path"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn find_path_respects_max_depth() {
    let (_tmp, storage) = fresh_storage();
    {
        // Цепочка A → B → C → D, max_depth=2 → не должен найти D (длина 3).
        let s = storage.lock().await;
        s.conn()
            .execute(
                "INSERT INTO proc_call_graph (repo, caller_proc_key, callee_proc_name, call_type) VALUES \
                 (?, 'A', 'B', 'direct'), (?, 'B', 'C', 'direct'), (?, 'C', 'D', 'direct')",
                params![REPO, REPO, REPO],
            )
            .unwrap();
    }
    let res_short = run_tool(
        &FindPathTool,
        &storage,
        serde_json::json!({"repo": REPO, "from": "A", "to": "D", "max_depth": 2}),
    )
    .await;
    assert_eq!(res_short["found"].as_bool(), Some(false));

    let res_full = run_tool(
        &FindPathTool,
        &storage,
        serde_json::json!({"repo": REPO, "from": "A", "to": "D", "max_depth": 3}),
    )
    .await;
    assert_eq!(res_full["found"].as_bool(), Some(true));
}

// ── search_terms (этап 5a) ────────────────────────────────────────────────

/// Хелпер: засеять `procedure_enrichment` тремя записями для тестов.
async fn seed_enrichment(storage: &Arc<Mutex<Storage>>) {
    let s = storage.lock().await;
    let conn = s.conn();
    for (proc_key, terms) in &[
        ("Расчёт.Старт",         "запуск, инициализация, проведение"),
        ("Продажи.СоздатьЗаказ", "товары, склад, заказ клиента, скидки"),
        ("Логирование.Записать", "журнал, аудит, ошибка, отладка"),
    ] {
        conn.execute(
            "INSERT INTO procedure_enrichment (repo, proc_key, terms, signature, updated_at) \
             VALUES (?, ?, ?, ?, ?)",
            params![REPO, proc_key, terms, "openai_compatible:claude-haiku-4.5", 0i64],
        )
        .unwrap();
    }
}

#[tokio::test]
async fn search_terms_finds_by_simple_word() {
    let (_tmp, storage) = fresh_storage();
    seed_enrichment(&storage).await;

    let res = run_tool(
        &SearchTermsTool,
        &storage,
        serde_json::json!({"repo": REPO, "query": "склад"}),
    )
    .await;
    let results = res["results"].as_array().expect("results — массив");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["proc_key"].as_str(), Some("Продажи.СоздатьЗаказ"));
    assert!(results[0]["terms"].as_str().unwrap().contains("склад"));
    assert!(results[0]["signature"].as_str().is_some());
    // BM25 ранжирование возвращает отрицательные числа (меньше = лучше).
    assert!(results[0]["score"].as_f64().is_some());
}

#[tokio::test]
async fn search_terms_supports_and_or() {
    let (_tmp, storage) = fresh_storage();
    seed_enrichment(&storage).await;

    // OR: «склад OR журнал» должно найти и Продажи, и Логирование.
    let res_or = run_tool(
        &SearchTermsTool,
        &storage,
        serde_json::json!({"repo": REPO, "query": "склад OR журнал"}),
    )
    .await;
    let or_keys: Vec<String> = res_or["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["proc_key"].as_str().map(String::from))
        .collect();
    assert_eq!(or_keys.len(), 2);
    assert!(or_keys.contains(&"Продажи.СоздатьЗаказ".to_string()));
    assert!(or_keys.contains(&"Логирование.Записать".to_string()));

    // AND: «товары AND склад» — только Продажи.
    let res_and = run_tool(
        &SearchTermsTool,
        &storage,
        serde_json::json!({"repo": REPO, "query": "товары AND склад"}),
    )
    .await;
    let and_results = res_and["results"].as_array().unwrap();
    assert_eq!(and_results.len(), 1);
    assert_eq!(and_results[0]["proc_key"].as_str(), Some("Продажи.СоздатьЗаказ"));
}

#[tokio::test]
async fn search_terms_returns_empty_for_unknown_word() {
    let (_tmp, storage) = fresh_storage();
    seed_enrichment(&storage).await;

    let res = run_tool(
        &SearchTermsTool,
        &storage,
        serde_json::json!({"repo": REPO, "query": "квантовая_телепортация"}),
    )
    .await;
    let results = res["results"].as_array().unwrap();
    assert!(results.is_empty(), "слово, которого нет в termах, не должно совпадать");
}

#[tokio::test]
async fn search_terms_filters_by_repo() {
    // Запись в другой repo не должна находиться по нашему alias.
    let (_tmp, storage) = fresh_storage();
    {
        let s = storage.lock().await;
        s.conn()
            .execute(
                "INSERT INTO procedure_enrichment (repo, proc_key, terms, signature, updated_at) \
                 VALUES (?, ?, ?, ?, ?)",
                params!["other-repo", "X.Y", "складские, операции", "sig", 0i64],
            )
            .unwrap();
    }
    let res = run_tool(
        &SearchTermsTool,
        &storage,
        serde_json::json!({"repo": REPO, "query": "складские"}),
    )
    .await;
    let results = res["results"].as_array().unwrap();
    assert!(results.is_empty(), "запись из другого repo не должна находиться");
}

#[tokio::test]
async fn search_terms_empty_query_returns_error() {
    let (_tmp, storage) = fresh_storage();
    let res = run_tool(
        &SearchTermsTool,
        &storage,
        serde_json::json!({"repo": REPO, "query": "   "}),
    )
    .await;
    assert!(res["error"].as_str().is_some(), "пустой query должен возвращать error");
}

#[tokio::test]
async fn search_terms_respects_limit() {
    let (_tmp, storage) = fresh_storage();
    seed_enrichment(&storage).await;
    // Все три записи содержат «и» в окончаниях, но точнее найдём по «проведение/заказ/журнал»:
    // «OR» через всё → 3 записи; ставим limit=2.
    let res = run_tool(
        &SearchTermsTool,
        &storage,
        serde_json::json!({"repo": REPO, "query": "проведение OR заказ OR журнал", "limit": 2}),
    )
    .await;
    let results = res["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
}
