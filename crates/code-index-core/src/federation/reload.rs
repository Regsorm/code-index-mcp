//! Атомарное перечитывание пары `serve.toml` + `daemon.toml` для serve.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use serde::Serialize;
use tokio::sync::Mutex;

use crate::daemon_core::config as daemon_config;
use crate::mcp::{build_federated_repo_map, CodeIndexServer, RepoEntry};
use crate::storage::PoolConfig;

use super::{config as serve_config, repos, whitelist};

/// Сериализуемый итог одной попытки перечитки.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ReloadResult {
    pub reloaded: bool,
    pub generation: u64,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
    pub restart_required: Vec<String>,
    pub error: Option<String>,
    pub finished_at_unix: u64,
}

/// Общий объект для file-watch и ручного `POST /reload`.
#[derive(Clone)]
pub struct ServeConfigReloader {
    server: CodeIndexServer,
    allowed: Arc<ArcSwap<HashSet<IpAddr>>>,
    serve_path: PathBuf,
    daemon_path: PathBuf,
    applied_me_ip: String,
    applied_pool: PoolConfig,
    lock: Arc<Mutex<()>>,
    generation: Arc<AtomicU64>,
}

impl ServeConfigReloader {
    pub fn new(
        server: CodeIndexServer,
        allowed: Arc<ArcSwap<HashSet<IpAddr>>>,
        serve_path: PathBuf,
        daemon_path: PathBuf,
        applied_me_ip: String,
        applied_pool: PoolConfig,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            server,
            allowed,
            serve_path: absolute_path(&serve_path)?,
            daemon_path: absolute_path(&daemon_path)?,
            applied_me_ip,
            applied_pool,
            lock: Arc::new(Mutex::new(())),
            generation: Arc::new(AtomicU64::new(0)),
        })
    }

    pub fn serve_path(&self) -> &Path {
        &self.serve_path
    }

    pub fn daemon_path(&self) -> &Path {
        &self.daemon_path
    }

    pub async fn reload(&self) -> ReloadResult {
        let _guard = self.lock.lock().await;
        let empty = || ReloadResult {
            reloaded: false,
            generation: self.generation.load(Ordering::SeqCst),
            added: Vec::new(),
            removed: Vec::new(),
            changed: Vec::new(),
            restart_required: Vec::new(),
            error: None,
            finished_at_unix: finished_at_unix(),
        };

        let serve_cfg = match serve_config::load_from(&self.serve_path) {
            Ok(cfg) => cfg,
            Err(error) => return self.failed(empty(), error.to_string()),
        };
        let daemon_cfg = match daemon_config::load_from(&self.daemon_path) {
            Ok(cfg) => cfg,
            Err(error) => return self.failed(empty(), error.to_string()),
        };

        if serve_cfg.me.ip.trim() != self.applied_me_ip.trim() {
            let mut result = empty();
            result.restart_required.push("[me].ip".to_string());
            return self.failed(
                result,
                "Изменение [me].ip требует перезапуска serve".to_string(),
            );
        }

        let daemon_aliases: BTreeSet<String> = daemon_cfg
            .paths
            .iter()
            .map(|entry| entry.effective_alias())
            .collect();
        let missing: Vec<String> = serve_cfg
            .paths
            .iter()
            .filter(|entry| entry.ip.trim() == serve_cfg.me.ip.trim())
            .filter(|entry| !daemon_aliases.contains(&entry.alias))
            .map(|entry| entry.alias.clone())
            .collect();
        if !missing.is_empty() {
            return self.failed(
                empty(),
                format!(
                    "Локальные алиасы serve.toml без пары в daemon.toml: {}",
                    missing.join(", ")
                ),
            );
        }

        let mut restart_required = Vec::new();
        if pool_differs(serve_cfg.pool.resolve(), self.applied_pool) {
            restart_required.push("[pool]".to_string());
        }

        let merged = match repos::merge(&serve_cfg, &daemon_cfg) {
            Ok(repos) => repos,
            Err(error) => {
                let mut result = empty();
                result.restart_required = restart_required;
                return self.failed(result, error.to_string());
            }
        };
        let local_languages = crate::mcp::config_watch::path_languages(&daemon_cfg);
        let old = self.server.repos.load_full();
        let new_map = match build_federated_repo_map(
            merged,
            self.server.registry.as_ref().as_ref(),
            &local_languages,
            self.applied_pool,
            Some(old.as_ref()),
            true,
        ) {
            Ok(map) => map,
            Err(error) => {
                let mut result = empty();
                result.restart_required = restart_required;
                return self.failed(result, error.to_string());
            }
        };

        let (added, removed, changed) = diff_repos(old.as_ref(), &new_map);
        if added.is_empty() && removed.is_empty() && changed.is_empty() {
            let active_languages = crate::mcp::config_watch::active_languages(&daemon_cfg);
            self.server.reload_extensions(active_languages).await;
            let result = ReloadResult {
                reloaded: true,
                generation: self.generation.load(Ordering::SeqCst),
                added,
                removed,
                changed,
                restart_required,
                error: None,
                finished_at_unix: finished_at_unix(),
            };
            self.store_result(&result);
            tracing::info!(
                added = ?result.added,
                removed = ?result.removed,
                changed = ?result.changed,
                restart_required = ?result.restart_required,
                "Конфигурация serve перечитана без изменений"
            );
            return result;
        }

        let affected = removed.iter().chain(changed.iter());
        for alias in affected.clone() {
            self.server.cache.invalidate_scope(alias);
        }
        let new_allowed = whitelist::build(&serve_cfg);
        let mut transition_allowed = self.allowed.load_full().as_ref().clone();
        transition_allowed.extend(new_allowed.iter().copied());
        self.allowed.store(Arc::new(transition_allowed));
        self.server.repos.store(Arc::new(new_map));
        self.allowed.store(Arc::new(new_allowed));
        for alias in affected {
            self.server.cache.invalidate_scope(alias);
        }
        if !removed.is_empty() || !changed.is_empty() {
            self.server.dedup.reset_all();
        }

        let active_languages = crate::mcp::config_watch::active_languages(&daemon_cfg);
        self.server.reload_extensions(active_languages).await;

        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let result = ReloadResult {
            reloaded: true,
            generation,
            added,
            removed,
            changed,
            restart_required,
            error: None,
            finished_at_unix: finished_at_unix(),
        };
        self.store_result(&result);
        tracing::info!(
            added = ?result.added,
            removed = ?result.removed,
            changed = ?result.changed,
            restart_required = ?result.restart_required,
            "Конфигурация serve успешно перечитана"
        );
        result
    }

    fn failed(&self, mut result: ReloadResult, error: String) -> ReloadResult {
        result.error = Some(error.clone());
        result.finished_at_unix = finished_at_unix();
        self.store_result(&result);
        tracing::warn!("Конфигурация serve не перечитана: {}", error);
        result
    }

    fn store_result(&self, result: &ReloadResult) {
        self.server
            .config_reload
            .store(Some(Arc::new(result.clone())));
    }
}

fn pool_differs(left: PoolConfig, right: PoolConfig) -> bool {
    left.max_size != right.max_size
        || left.cache_kib != right.cache_kib
        || left.busy_timeout_ms != right.busy_timeout_ms
}

fn repo_changed(left: &RepoEntry, right: &RepoEntry) -> bool {
    left.is_local != right.is_local
        || left.root_path != right.root_path
        || left.ip != right.ip
        || left.port != right.port
        || left.language != right.language
}

fn diff_repos(
    old: &BTreeMap<String, RepoEntry>,
    new: &BTreeMap<String, RepoEntry>,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let added = new
        .keys()
        .filter(|alias| !old.contains_key(*alias))
        .cloned()
        .collect();
    let removed = old
        .keys()
        .filter(|alias| !new.contains_key(*alias))
        .cloned()
        .collect();
    let changed = old
        .iter()
        .filter_map(|(alias, old_entry)| {
            new.get(alias)
                .filter(|new_entry| repo_changed(old_entry, new_entry))
                .map(|_| alias.clone())
        })
        .collect();
    (added, removed, changed)
}

pub(crate) fn absolute_path(path: &Path) -> anyhow::Result<PathBuf> {
    std::path::absolute(path).map_err(Into::into)
}

fn finished_at_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use tempfile::TempDir;

    use crate::mcp::CodeIndexServer;
    use crate::serve_cache::ServeCache;
    use crate::storage::Storage;

    struct Fixture {
        _tmp: TempDir,
        serve_path: PathBuf,
        daemon_path: PathBuf,
        server: CodeIndexServer,
        allowed: Arc<ArcSwap<HashSet<IpAddr>>>,
        reloader: ServeConfigReloader,
    }

    fn escaped(path: &Path) -> String {
        path.display().to_string().replace('\\', "\\\\")
    }

    fn remote(alias: &str, ip: &str) -> String {
        format!("[[paths]]\nalias = \"{alias}\"\nip = \"{ip}\"\n")
    }

    fn local(alias: &str, root: &Path) -> (String, String) {
        (
            format!("[[paths]]\nalias = \"{alias}\"\nip = \"127.0.0.1\"\n"),
            format!(
                "[[paths]]\nalias = \"{alias}\"\npath = \"{}\"\nlanguage = \"rust\"\n",
                escaped(root)
            ),
        )
    }

    fn serve_text(paths: &str) -> String {
        format!("[me]\nip = \"127.0.0.1\"\n\n{paths}")
    }

    fn fixture(serve: String, daemon: String, tmp: TempDir) -> Fixture {
        let serve_path = tmp.path().join("serve.toml");
        let daemon_path = tmp.path().join("daemon.toml");
        fs::write(&serve_path, serve).unwrap();
        fs::write(&daemon_path, daemon).unwrap();
        let serve_cfg = serve_config::load_from(&serve_path).unwrap();
        let daemon_cfg = daemon_config::load_from(&daemon_path).unwrap();
        let merged = repos::merge(&serve_cfg, &daemon_cfg).unwrap();
        for repo in &merged {
            if repo.is_local {
                let db = repo.db_path.as_ref().unwrap();
                fs::create_dir_all(db.parent().unwrap()).unwrap();
                drop(Storage::open_file(db).unwrap());
            }
        }
        let server = CodeIndexServer::from_federated(
            merged,
            serve_cfg.me.ip.clone(),
            None,
            crate::mcp::config_watch::path_languages(&daemon_cfg),
            serve_cfg.pool.resolve(),
        )
        .unwrap();
        let allowed = Arc::new(ArcSwap::from_pointee(whitelist::build(&serve_cfg)));
        let reloader = ServeConfigReloader::new(
            server.clone(),
            allowed.clone(),
            serve_path.clone(),
            daemon_path.clone(),
            serve_cfg.me.ip,
            serve_cfg.pool.resolve(),
        )
        .unwrap();
        Fixture {
            _tmp: tmp,
            serve_path,
            daemon_path,
            server,
            allowed,
            reloader,
        }
    }

    fn remote_fixture() -> Fixture {
        fixture(
            serve_text(&remote("old", "192.0.2.10")),
            String::new(),
            TempDir::new().unwrap(),
        )
    }

    #[tokio::test]
    async fn adds_remote_alias_for_existing_server_clone_and_whitelist() {
        let f = remote_fixture();
        let old_clone = f.server.clone();
        fs::write(
            &f.serve_path,
            serve_text(&(remote("old", "192.0.2.10") + &remote("new", "192.0.2.20"))),
        )
        .unwrap();

        let result = f.reloader.reload().await;
        assert!(result.reloaded);
        assert_eq!(result.added, vec!["new"]);
        assert!(old_clone.resolve_repo("new").is_ok());
        assert!(f.allowed.load().contains(&"192.0.2.20".parse().unwrap()));
    }

    #[tokio::test]
    async fn adds_local_alias_and_creates_database() {
        let f = remote_fixture();
        let root = f._tmp.path().join("local");
        fs::create_dir(&root).unwrap();
        let (serve_local, daemon_local) = local("local", &root);
        fs::write(
            &f.serve_path,
            serve_text(&(remote("old", "192.0.2.10") + &serve_local)),
        )
        .unwrap();
        fs::write(&f.daemon_path, daemon_local).unwrap();

        let result = f.reloader.reload().await;
        let entry = f.server.resolve_repo("local").unwrap();
        assert!(result.reloaded);
        assert_eq!(result.added, vec!["local"]);
        assert!(entry.is_local && entry.storage.is_some());
        assert!(root.join(".code-index/index.db").exists());
    }

    #[tokio::test]
    async fn missing_local_pair_preserves_repos_and_whitelist() {
        let f = remote_fixture();
        let before_repos = f.server.repo_aliases();
        let before_allowed = f.allowed.load_full();
        fs::write(
            &f.serve_path,
            serve_text(
                &(remote("old", "192.0.2.10")
                    + "[[paths]]\nalias = \"local\"\nip = \"127.0.0.1\"\n"),
            ),
        )
        .unwrap();

        let result = f.reloader.reload().await;
        assert!(!result.reloaded);
        assert!(result.error.unwrap().contains("local"));
        assert_eq!(f.server.repo_aliases(), before_repos);
        assert_eq!(*f.allowed.load_full(), *before_allowed);
    }

    #[tokio::test]
    async fn parse_errors_in_either_file_preserve_generation_and_repos() {
        let f = remote_fixture();
        let before = f.server.repo_aliases();
        fs::write(&f.serve_path, "not = [toml").unwrap();
        let first = f.reloader.reload().await;
        assert!(!first.reloaded);
        assert_eq!(first.generation, 0);
        fs::write(&f.serve_path, serve_text(&remote("old", "192.0.2.10"))).unwrap();
        fs::write(&f.daemon_path, "not = [toml").unwrap();
        let second = f.reloader.reload().await;
        assert!(!second.reloaded);
        assert_eq!(second.generation, 0);
        assert_eq!(f.server.repo_aliases(), before);
    }

    #[tokio::test]
    async fn missing_either_file_preserves_state() {
        let f = remote_fixture();
        let before = f.server.repo_aliases();
        fs::remove_file(&f.serve_path).unwrap();
        assert!(!f.reloader.reload().await.reloaded);
        fs::write(&f.serve_path, serve_text(&remote("old", "192.0.2.10"))).unwrap();
        fs::remove_file(&f.daemon_path).unwrap();
        assert!(!f.reloader.reload().await.reloaded);
        assert_eq!(f.server.repo_aliases(), before);
    }

    #[tokio::test]
    async fn reuses_unchanged_pool_and_reopens_changed_path() {
        let tmp = TempDir::new().unwrap();
        let one = tmp.path().join("one");
        let two = tmp.path().join("two");
        let moved = tmp.path().join("moved");
        fs::create_dir(&one).unwrap();
        fs::create_dir(&two).unwrap();
        fs::create_dir(&moved).unwrap();
        let (s1, d1) = local("one", &one);
        let (s2, d2) = local("two", &two);
        let f = fixture(serve_text(&(s1 + &s2)), d1 + &d2, tmp);
        let old_one = f.server.resolve_repo("one").unwrap().storage.unwrap();
        let old_two = f.server.resolve_repo("two").unwrap().storage.unwrap();
        let (_, moved_daemon) = local("two", &moved);
        let (_, one_daemon) = local("one", &one);
        fs::write(&f.daemon_path, one_daemon + &moved_daemon).unwrap();

        let result = f.reloader.reload().await;
        let new_one = f.server.resolve_repo("one").unwrap().storage.unwrap();
        let new_two = f.server.resolve_repo("two").unwrap().storage.unwrap();
        assert_eq!(result.changed, vec!["two"]);
        assert!(Arc::ptr_eq(&old_one, &new_one));
        assert!(!Arc::ptr_eq(&old_two, &new_two));
    }

    #[tokio::test]
    async fn removal_keeps_previously_resolved_entry_alive() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("local");
        fs::create_dir(&root).unwrap();
        let (serve_local, daemon_local) = local("old", &root);
        let f = fixture(serve_text(&serve_local), daemon_local, tmp);
        let old_entry = f.server.resolve_repo("old").unwrap();
        fs::write(&f.serve_path, serve_text("")).unwrap();

        let result = f.reloader.reload().await;
        assert_eq!(result.removed, vec!["old"]);
        assert!(f.server.resolve_repo("old").is_err());
        old_entry
            .storage_pool()
            .get()
            .await
            .unwrap()
            .conn()
            .execute_batch("SELECT 1")
            .unwrap();
    }

    #[tokio::test]
    async fn me_ip_change_requires_restart_and_preserves_state() {
        let f = remote_fixture();
        fs::write(&f.serve_path, "[me]\nip = \"127.0.0.2\"\n").unwrap();
        let result = f.reloader.reload().await;
        assert!(!result.reloaded);
        assert_eq!(result.restart_required, vec!["[me].ip"]);
        assert_eq!(f.server.repo_aliases(), vec!["old"]);
    }

    #[tokio::test]
    async fn pool_change_is_reported_while_repo_change_is_applied() {
        let f = remote_fixture();
        fs::write(
            &f.serve_path,
            format!(
                "{}\n[pool]\npool_size = 9\n",
                serve_text(&(remote("old", "192.0.2.10") + &remote("new", "192.0.2.20")))
            ),
        )
        .unwrap();
        let result = f.reloader.reload().await;
        assert!(result.reloaded);
        assert_eq!(result.restart_required, vec!["[pool]"]);
        assert_eq!(result.added, vec!["new"]);
    }

    #[tokio::test]
    async fn nonexistent_local_root_is_not_created() {
        let f = remote_fixture();
        let root = f._tmp.path().join("missing");
        let (serve_local, daemon_local) = local("missing", &root);
        fs::write(&f.serve_path, serve_text(&serve_local)).unwrap();
        fs::write(&f.daemon_path, daemon_local).unwrap();
        let result = f.reloader.reload().await;
        assert!(!result.reloaded);
        assert!(!root.exists());
        assert_eq!(f.server.repo_aliases(), vec!["old"]);
    }

    #[tokio::test]
    async fn changed_path_invalidates_only_its_cache_scope() {
        let tmp = TempDir::new().unwrap();
        let one = tmp.path().join("one");
        let two = tmp.path().join("two");
        let moved = tmp.path().join("moved");
        fs::create_dir(&one).unwrap();
        fs::create_dir(&two).unwrap();
        fs::create_dir(&moved).unwrap();
        let (s1, d1) = local("one", &one);
        let (s2, d2) = local("two", &two);
        let f = fixture(serve_text(&(s1 + &s2)), d1 + &d2, tmp);
        let one_key = ServeCache::key("one", "tool", &serde_json::json!({}));
        let two_key = ServeCache::key("two", "tool", &serde_json::json!({}));
        f.server
            .cache
            .insert(one_key.clone(), Arc::new("one".into()), "one", &[]);
        f.server
            .cache
            .insert(two_key.clone(), Arc::new("two".into()), "two", &[]);
        let (_, d1) = local("one", &one);
        let (_, moved_d2) = local("two", &moved);
        fs::write(&f.daemon_path, d1 + &moved_d2).unwrap();

        f.reloader.reload().await;
        assert!(f.server.cache.get(&two_key).is_none());
        assert!(f.server.cache.get(&one_key).is_some());
    }

    #[tokio::test]
    async fn unchanged_reload_does_not_advance_generation() {
        let f = remote_fixture();
        let result = f.reloader.reload().await;
        assert!(result.reloaded);
        assert_eq!(result.generation, 0);
        assert!(result.added.is_empty() && result.removed.is_empty() && result.changed.is_empty());
    }

    #[tokio::test]
    async fn concurrent_reloads_are_serialized() {
        let f = remote_fixture();
        fs::write(
            &f.serve_path,
            serve_text(&(remote("old", "192.0.2.10") + &remote("new", "192.0.2.20"))),
        )
        .unwrap();
        let (first, second) = tokio::join!(f.reloader.reload(), f.reloader.reload());
        assert!(first.reloaded && second.reloaded);
        assert_eq!(first.generation, 1);
        assert_eq!(second.generation, 1);
        assert_eq!(f.server.repo_aliases(), vec!["new", "old"]);
    }
}
