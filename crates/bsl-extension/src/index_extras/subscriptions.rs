//! Shared writers for subscription sources and scoped defined types.
use std::path::Path;

use anyhow::Result;
use rusqlite::{params, Connection};

use super::{rel_path, RepoScan, REPO_DEFAULT};
use crate::xml::event_subscriptions::{parse_source_types, EventSubscription};

pub(crate) fn subscription_owner(repo_root: &Path, path: &Path) -> String {
    path.parent()
        .and_then(Path::parent)
        .map(|root| rel_path(repo_root, root))
        .unwrap_or_default()
}

pub(crate) fn insert_event_subscription(
    repo_root: &Path,
    conn: &Connection,
    path: &Path,
    sub: &EventSubscription,
) -> Result<()> {
    let owner = subscription_owner(repo_root, path);
    let source_path = rel_path(repo_root, path);
    conn.execute(
        "INSERT INTO event_subscriptions
         (repo,name,event,handler_module,handler_proc,sources_json,owner,source_path,type_sets_json,warnings_json)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
         ON CONFLICT(repo,owner,name) DO UPDATE SET event=excluded.event,
         handler_module=excluded.handler_module,handler_proc=excluded.handler_proc,
         sources_json=excluded.sources_json,source_path=excluded.source_path,
         type_sets_json=excluded.type_sets_json,warnings_json=excluded.warnings_json",
        params![REPO_DEFAULT, sub.name,sub.event,sub.handler_module,sub.handler_proc,
            serde_json::to_string(&sub.sources)?,owner,source_path,
            serde_json::to_string(&sub.type_sets)?,serde_json::to_string(&sub.source_warnings)?],
    )?;
    Ok(())
}

pub(crate) fn index_subscription_defined_types(scan: &RepoScan, conn: &Connection) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "DELETE FROM subscription_defined_types WHERE repo=?1",
        [REPO_DEFAULT],
    )?;
    for path in &scan.defined_types {
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let content = std::fs::read_to_string(path)?;
        let types = parse_source_types(&content, "Type")?;
        tx.execute("INSERT INTO subscription_defined_types(repo,owner,name,types_json) VALUES (?1,?2,?3,?4)",
            params![REPO_DEFAULT,subscription_owner(&scan.repo_root,path),name,serde_json::to_string(&types)?])?;
    }
    tx.commit()?;
    Ok(())
}
