use bsl_extension::{
    index_extras::run_index_extras, schema::SCHEMA_EXTENSIONS, tools::GetEventSubscriptionsTool,
};
use code_index_core::{
    extension::{IndexTool, ToolContext},
    storage::{Storage, StoragePool},
};
use serde_json::{json, Value};
use std::{path::Path, sync::Arc};

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn fixture() -> (tempfile::TempDir, Arc<StoragePool>) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    for owner in ["src/cf", "src/cfe/Feature"] {
        write(
            &root.join(owner).join("Configuration.xml"),
            r#"<MetaDataObject><Configuration><ChildObjects><Document>Заказ</Document></ChildObjects></Configuration></MetaDataObject>"#,
        );
    }
    for (owner, name, source) in [
        (
            "src/cf",
            "Exact",
            "<v8:Type>cfg:DocumentObject.Заказ</v8:Type>",
        ),
        (
            "src/cfe/Feature",
            "Exact",
            "<v8:Type>cfg:DocumentObject.Заказ</v8:Type>",
        ),
        (
            "src/cf",
            "Manager",
            "<v8:Type>cfg:DocumentManager.Заказ</v8:Type>",
        ),
        (
            "src/cf",
            "Set",
            "<v8:TypeSet>cfg:DocumentObject</v8:TypeSet>",
        ),
        (
            "src/cf",
            "UniversalManager",
            "<v8:Type>cfg:DocumentManager</v8:Type>",
        ),
        (
            "src/cf",
            "Constant",
            "<v8:TypeSet>cfg:ConstantValueManager</v8:TypeSet>",
        ),
        (
            "src/cf",
            "Recalculation",
            "<v8:TypeSet>cfg:RecalculationRecordSet</v8:TypeSet>",
        ),
        (
            "src/cf",
            "Reference",
            "<v8:Type>cfg:DocumentRef.Заказ</v8:Type>",
        ),
        (
            "src/cf",
            "RecordSet",
            "<v8:Type>cfg:InformationRegisterRecordSet.Регистр</v8:Type>",
        ),
        (
            "src/cf",
            "Defined",
            "<v8:TypeSet>cfg:DefinedType.Outer</v8:TypeSet>",
        ),
        (
            "src/cfe/Feature",
            "Scoped",
            "<v8:TypeSet>cfg:DefinedType.Inner</v8:TypeSet>",
        ),
        (
            "src/cf",
            "WrongKind",
            "<v8:TypeSet>cfg:CatalogObject</v8:TypeSet>",
        ),
    ] {
        write(
            &root
                .join(owner)
                .join(format!("EventSubscriptions/{name}.xml")),
            &format!(
                r#"<MetaDataObject xmlns:v8="urn:v8"><EventSubscription><Properties>
            <Name>{name}</Name><Source>{source}</Source><Event>BeforeWrite</Event><Handler>M.{name}</Handler>
            </Properties></EventSubscription></MetaDataObject>"#
            ),
        );
    }
    for (owner, name, types) in [
        (
            "src/cf",
            "Outer",
            "<v8:TypeSet>cfg:DefinedType.Inner</v8:TypeSet>",
        ),
        (
            "src/cf",
            "Inner",
            "<v8:Type>cfg:DocumentObject.Заказ</v8:Type>",
        ),
        (
            "src/cfe/Feature",
            "Inner",
            "<v8:Type>cfg:DocumentObject.Другой</v8:Type>",
        ),
    ] {
        write(
            &root.join(owner).join(format!("DefinedTypes/{name}.xml")),
            &format!(
                r#"<MetaDataObject xmlns:v8="urn:v8"><DefinedType><Properties><Name>{name}</Name><Type>{types}</Type></Properties></DefinedType></MetaDataObject>"#
            ),
        );
    }
    let mut storage = Storage::open_file(&tmp.path().join("test.db")).unwrap();
    storage.apply_schema_extensions(SCHEMA_EXTENSIONS).unwrap();
    run_index_extras(&root, &mut storage).unwrap();
    storage
        .conn()
        .execute(
            "INSERT OR REPLACE INTO index_state(key,value) VALUES('index_data_version',?1)",
            [code_index_core::storage::schema::INDEX_DATA_VERSION.to_string()],
        )
        .unwrap();
    (tmp, StoragePool::single(storage))
}

async fn query(storage: &Arc<StoragePool>, args: Value) -> Value {
    GetEventSubscriptionsTool
        .execute(
            args,
            ToolContext {
                repo: "default",
                root_path: None,
                language: Some("bsl"),
                storage,
            },
        )
        .await["result"]
        .clone()
}

#[tokio::test]
async fn subscriptions_match_sets_nested_defined_types_managers_and_scoped_extensions() {
    let (_tmp, pool) = fixture();
    let result = query(
        &pool,
        json!({"repo":"default","source":"Document.Заказ","limit":100}),
    )
    .await;
    assert_eq!(result["total"], 6, "{result}");
    assert_eq!(result["complete"], true, "{result}");
    let rows = result["subscriptions"].as_array().unwrap();
    assert_eq!(
        rows.iter().filter(|r| r["matched_via"] == "exact").count(),
        3
    );
    assert_eq!(
        rows.iter()
            .filter(|r| r["matched_via"] == "type_set")
            .count(),
        2
    );
    assert_eq!(
        rows.iter()
            .filter(|r| r["matched_via"] == "defined_type")
            .count(),
        1
    );
    assert_eq!(
        rows.iter().filter(|r| r.get("extension").is_some()).count(),
        1
    );
    assert!(rows
        .iter()
        .all(|r| r["name"] != "Scoped" && r["name"] != "WrongKind"));
    let short = query(&pool, json!({"repo":"default","source":"заказ"})).await;
    assert_eq!(short["total"], 6, "{short}");
    let ru = query(
        &pool,
        json!({"repo":"default","source":"Документ.заказ","limit":2}),
    )
    .await;
    assert_eq!(ru["total"], 6);
    assert_eq!(ru["count"], 2);
    assert_eq!(ru["truncated"], true);
    let exact = query(
        &pool,
        json!({"repo":"default","source":"Document.Заказ","exact_only":true}),
    )
    .await;
    assert_eq!(exact["total"], 3);
    let manager = query(
        &pool,
        json!({"repo":"default","source":"DocumentManager.Заказ"}),
    )
    .await;
    assert_eq!(manager["total"], 2);
    let recordset = query(
        &pool,
        json!({"repo":"default","source":"InformationRegister.Регистр"}),
    )
    .await;
    assert_eq!(recordset["total"], 1);
    assert_eq!(recordset["subscriptions"][0]["name"], "RecordSet");
    let constant = query(
        &pool,
        json!({"repo":"default","source":"Constant.Параметр"}),
    )
    .await;
    assert_eq!(constant["total"], 1);
    assert_eq!(constant["complete"], true);
    let object = query(
        &pool,
        json!({"repo":"default","source":"DocumentObject.Заказ"}),
    )
    .await;
    assert_eq!(object["total"], 4);
}

#[tokio::test]
async fn unresolved_cyclic_and_unknown_sources_warn_even_when_omitted() {
    let (_tmp, pool) = fixture();
    {
        let storage = pool.get().await.unwrap();
        storage.conn().execute("INSERT INTO event_subscriptions(repo,name,event,handler_module,handler_proc,type_sets_json,warnings_json)
            VALUES('default','Unknown','ПередЗаписью','M','P','[\"cfg:DefinedType.Missing\"]','[\"unknown source element: Future\"]')",[]).unwrap();
        storage
            .conn()
            .execute(
                "INSERT INTO subscription_defined_types VALUES('default','src/cf','Loop',?1)",
                [
                    json!({"sources":[],"type_sets":["cfg:DefinedType.Loop"],"warnings":[]})
                        .to_string(),
                ],
            )
            .unwrap();
        storage.conn().execute("INSERT INTO event_subscriptions(repo,name,event,handler_module,handler_proc,type_sets_json,owner)
            VALUES('default','Cycle','ПередЗаписью','M','P','[\"cfg:DefinedType.Loop\"]','src/cf')",[]).unwrap();
    }
    let result = query(
        &pool,
        json!({"repo":"default","source":"Document.Заказ","limit":1}),
    )
    .await;
    assert_eq!(result["total"], 6);
    assert_eq!(result["complete"], false);
    let warnings = result["warnings"].to_string();
    assert!(warnings.contains("Future"));
    assert!(warnings.contains("Missing"));
    assert!(warnings.contains("cyclic"));
    {
        let storage = pool.get().await.unwrap();
        storage
            .conn()
            .execute(
                "UPDATE index_state SET value='4' WHERE key='index_data_version'",
                [],
            )
            .unwrap();
    }
    let old = query(&pool, json!({"repo":"default","source":"Document.Заказ"})).await;
    assert!(old["warnings"].to_string().contains("reindex_required"));
}
