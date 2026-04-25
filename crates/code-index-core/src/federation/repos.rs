// Слияние `serve.toml` (глобальный реестр) и `daemon.toml` (локальные пути)
// в реестр репозиториев `Vec<FederatedRepo>`, который потребляет `CodeIndexServer`.
//
// Логика:
//   * `serve.toml` — список (alias, ip) глобально для всей федерации;
//   * `daemon.toml` — пути на этой машине, у каждого `effective_alias()`;
//   * для записи `serve.paths[i]`:
//       - если `entry.ip == serve.me.ip` → ищем PathEntry с тем же алиасом
//         в `daemon.toml` → `is_local = true`, `root_path/db_path = Some(...)`;
//         если в daemon.toml записи нет → пропускаем с warning (репо лежит
//         не на этой машине, хотя адрес в serve.toml указывает на нас);
//       - иначе → `is_local = false`, `root_path/db_path = None`.

use std::path::PathBuf;

use crate::daemon_core::config::DaemonFileConfig;

use super::config::ServeFileConfig;

/// Запись реестра, потребляемая `CodeIndexServer`. Для удалённых репо
/// `root_path`/`db_path` отсутствуют — обращаться к локальному SQLite нельзя,
/// все запросы форвардятся.
#[derive(Debug, Clone)]
pub struct FederatedRepo {
    /// Глобально уникальный алиас.
    pub alias: String,
    /// IP машины, на которой репо физически находится.
    pub ip: String,
    /// Канонический путь к корню (только для local).
    pub root_path: Option<PathBuf>,
    /// Путь к `.code-index/index.db` (только для local).
    pub db_path: Option<PathBuf>,
    /// Признак «репо обслуживается этой машиной».
    pub is_local: bool,
}

/// Свести реестры воедино. Ошибки в этой функции — только если входные
/// конфиги уже невалидны (не должно произойти после `config::validate`).
/// Отсутствие локальной записи для local-алиаса — warning + skip, не ошибка.
pub fn merge(
    serve: &ServeFileConfig,
    daemon: &DaemonFileConfig,
) -> anyhow::Result<Vec<FederatedRepo>> {
    let me_ip = serve.me.ip.trim();

    // Карта alias → PathBuf из daemon.toml (используется только для local-репо).
    let mut local_paths: std::collections::HashMap<String, PathBuf> =
        std::collections::HashMap::new();
    for entry in &daemon.paths {
        local_paths.insert(entry.effective_alias(), entry.path.clone());
    }

    let mut out = Vec::with_capacity(serve.paths.len());
    for entry in &serve.paths {
        let is_local = entry.ip.trim() == me_ip;
        if is_local {
            match local_paths.get(&entry.alias) {
                Some(raw_path) => {
                    let root = raw_path
                        .canonicalize()
                        .unwrap_or_else(|_| raw_path.clone());
                    let db = root.join(".code-index").join("index.db");
                    out.push(FederatedRepo {
                        alias: entry.alias.clone(),
                        ip: entry.ip.clone(),
                        root_path: Some(root),
                        db_path: Some(db),
                        is_local: true,
                    });
                }
                None => {
                    tracing::warn!(
                        "serve.toml: репо '{}' помечен как локальный (ip={}), \
                         но в daemon.toml нет PathEntry с этим алиасом. \
                         Репо пропущен.",
                        entry.alias,
                        entry.ip
                    );
                }
            }
        } else {
            out.push(FederatedRepo {
                alias: entry.alias.clone(),
                ip: entry.ip.clone(),
                root_path: None,
                db_path: None,
                is_local: false,
            });
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon_core::config::{DaemonSection, PathEntry};
    use crate::federation::config::{MeSection, ServePathEntry};

    fn serve(me_ip: &str, paths: Vec<(&str, &str)>) -> ServeFileConfig {
        ServeFileConfig {
            me: MeSection { ip: me_ip.to_string(), token: None },
            paths: paths
                .into_iter()
                .map(|(alias, ip)| ServePathEntry {
                    alias: alias.to_string(),
                    ip: ip.to_string(),
                })
                .collect(),
        }
    }

    fn daemon(paths: Vec<(&str, &str)>) -> DaemonFileConfig {
        DaemonFileConfig {
            daemon: DaemonSection::default(),
            paths: paths
                .into_iter()
                .map(|(path, alias)| PathEntry {
                    path: PathBuf::from(path),
                    debounce_ms: None,
                    batch_ms: None,
                    alias: if alias.is_empty() { None } else { Some(alias.to_string()) },
                    language: None,
                })
                .collect(),
            enrichment: None,
        }
    }

    #[test]
    fn local_alias_with_matching_daemon_entry_is_kept() {
        let s = serve("192.0.2.10", vec![("ut", "192.0.2.10")]);
        let d = daemon(vec![("/tmp/repo_ut", "ut")]);
        let merged = merge(&s, &d).unwrap();

        assert_eq!(merged.len(), 1);
        let r = &merged[0];
        assert_eq!(r.alias, "ut");
        assert_eq!(r.ip, "192.0.2.10");
        assert!(r.is_local);
        // canonicalize может не сработать на временных путях — тогда remember путь как есть.
        assert!(r.root_path.is_some());
        assert!(r.db_path.is_some());
        let db = r.db_path.as_ref().unwrap();
        assert!(db.ends_with(".code-index/index.db") || db.ends_with(".code-index\\index.db"));
    }

    #[test]
    fn local_alias_without_daemon_entry_is_skipped() {
        let s = serve("192.0.2.10", vec![("ut", "192.0.2.10")]);
        let d = daemon(vec![]);
        let merged = merge(&s, &d).unwrap();
        assert!(merged.is_empty());
    }

    #[test]
    fn remote_alias_passes_through_without_daemon_lookup() {
        let s = serve("192.0.2.10", vec![("ut_vm", "192.0.2.50")]);
        let d = daemon(vec![]); // даже если в daemon.toml пусто
        let merged = merge(&s, &d).unwrap();

        assert_eq!(merged.len(), 1);
        let r = &merged[0];
        assert_eq!(r.alias, "ut_vm");
        assert_eq!(r.ip, "192.0.2.50");
        assert!(!r.is_local);
        assert!(r.root_path.is_none());
        assert!(r.db_path.is_none());
    }

    #[test]
    fn mixed_local_and_remote() {
        let s = serve(
            "192.0.2.10",
            vec![
                ("ut", "192.0.2.50"),         // remote
                ("dev", "192.0.2.10"),       // local (есть в daemon.toml)
                ("missing", "192.0.2.10"),   // local, но нет в daemon.toml — skip
            ],
        );
        let d = daemon(vec![("/tmp/dev_repo", "dev")]);
        let merged = merge(&s, &d).unwrap();

        assert_eq!(merged.len(), 2, "ut и dev — да, missing — skip");
        assert_eq!(merged[0].alias, "ut");
        assert!(!merged[0].is_local);
        assert_eq!(merged[1].alias, "dev");
        assert!(merged[1].is_local);
    }

    #[test]
    fn daemon_alias_resolved_via_effective_alias() {
        // PathEntry без явного alias — `effective_alias` берёт последний сегмент пути.
        let s = serve("127.0.0.1", vec![("repout", "127.0.0.1")]);
        let d = daemon(vec![("/tmp/RepoUT", "")]);
        let merged = merge(&s, &d).unwrap();
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].alias, "repout");
        assert!(merged[0].is_local);
    }
}
