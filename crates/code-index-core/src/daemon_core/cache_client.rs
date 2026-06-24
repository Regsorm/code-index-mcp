//! HTTP-клиент для отправки событий точечной инвалидации в `mcp-cache-ci`
//! после переиндексации файлов (этап 3 event-based cache invalidation).
//!
//! Daemon вызывает [`CacheClient::invalidate_files`] **после**
//! `storage.commit_batch()` SQLite-транзакции — таким образом cache-ci узнаёт
//! об изменении файла РОВНО ТОГДА, когда новые данные уже доступны в индексе.
//! Порядок критичен: invalidate ДО commit → cache-ci примет следующий запрос,
//! форварднет в daemon и получит старые данные (race-условие в окне между
//! invalidate и commit). Invalidate ПОСЛЕ commit → корректно.
//!
//! Список target-эндпоинтов — из `daemon.toml` секции `[[cache_targets]]`.
//! Если она пуста — `CacheClient::is_empty()` → true, worker такое
//! `invalidate_files` не вызывает (нет потребителя — нет работы).
//!
//! Failure (network, 5xx, timeout) → лог-предупреждение, не падать. TTL
//! на стороне cache-ci подстрахует.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

/// Endpoint cache-ci для отправки `POST /invalidate`. URL — корень cache-ci
/// (например `http://127.0.0.1:8011`); `/invalidate` дописывается автоматически.
#[derive(Debug, Clone)]
pub struct CacheTarget {
    pub url: String,
}

/// Клиент с пулом соединений + список targets. Дешёвый клон (`Arc` под капотом
/// у `reqwest::Client`).
#[derive(Clone)]
pub struct CacheClient {
    client: reqwest::Client,
    targets: Arc<Vec<CacheTarget>>,
}

impl CacheClient {
    /// Сконструировать из списка URL-ов cache-ci. Пустой список → клиент
    /// безопасен в вызове, но `is_empty()=true` и `invalidate_files` ничего
    /// не делает.
    pub fn new(target_urls: Vec<String>) -> Self {
        let targets: Vec<CacheTarget> = target_urls
            .into_iter()
            .filter(|s| !s.trim().is_empty())
            .map(|url| CacheTarget {
                url: url.trim_end_matches('/').to_string(),
            })
            .collect();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .pool_idle_timeout(Duration::from_secs(60))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            client,
            targets: Arc::new(targets),
        }
    }

    /// Нет targets — нет необходимости вызывать `invalidate_files`.
    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    /// Число настроенных endpoints (для диагностики).
    pub fn target_count(&self) -> usize {
        self.targets.len()
    }

    /// Послать `POST /invalidate {file_paths: [...]}` всем target'ам параллельно.
    /// Возвращает только число успешных HTTP-ответов (2xx). Любые ошибки
    /// (соединение, таймаут, 5xx) пишутся в stderr и не пробрасываются вверх —
    /// событийный канал «best-effort», TTL остаётся safety net на стороне cache-ci.
    pub async fn invalidate_files(&self, repo: &str, file_paths: &[String]) -> usize {
        if file_paths.is_empty() || self.targets.is_empty() {
            return 0;
        }
        // `repo` нужен serve (кэш repo-scope: снять flux + снести ключи репо).
        // Прокси mcp-cache-ci его игнорирует — у него приоритет `file_paths`
        // (точная инвалидация по reverse_index). Один payload на оба target'а.
        let payload = json!({ "repo": repo, "file_paths": file_paths });
        let mut joins = Vec::with_capacity(self.targets.len());
        for target in self.targets.iter() {
            let url = format!("{}/invalidate", target.url);
            let body = payload.clone();
            let client = self.client.clone();
            joins.push(tokio::spawn(async move {
                match client.post(&url).json(&body).send().await {
                    Ok(resp) => {
                        let status = resp.status();
                        if status.is_success() {
                            true
                        } else {
                            let body_text =
                                resp.text().await.unwrap_or_else(|_| "<no body>".into());
                            eprintln!(
                                "[cache_client] {} non-2xx ({}): {}",
                                url, status, body_text
                            );
                            false
                        }
                    }
                    Err(e) => {
                        eprintln!("[cache_client] {} send error: {}", url, e);
                        false
                    }
                }
            }));
        }
        let mut ok_count = 0usize;
        for j in joins {
            if let Ok(true) = j.await {
                ok_count += 1;
            }
        }
        ok_count
    }

    /// Послать ранний сигнal `POST /mark-dirty {repo, files:[{path, mtime}]}`
    /// всем target'ам параллельно (write-triggered ленивая ревалидация, #1471).
    ///
    /// Вызывается worker'ом в начале обработки батча — ДО переразбора и commit,
    /// в дополнение к [`invalidate_files`] после commit. `mtime` — observed
    /// (наблюдённый на диске на момент события) unix-секунды; cache-ci хранит
    /// max observed на путь и сверяет с индексным mtime из ответа serve, чтобы
    /// не закэшировать старьё в окне переразбора.
    ///
    /// `repo` — `effective_alias()` пути (тот же scope, что клиент шлёт в `repo`
    /// аргументе tool-call). Best-effort: ошибки логируются, не пробрасываются.
    pub async fn mark_dirty_files(&self, repo: &str, files: &[(String, i64)]) -> usize {
        if files.is_empty() || self.targets.is_empty() {
            return 0;
        }
        let files_json: Vec<serde_json::Value> = files
            .iter()
            .map(|(path, mtime)| json!({ "path": path, "mtime": mtime }))
            .collect();
        let payload = json!({ "repo": repo, "files": files_json });
        let mut joins = Vec::with_capacity(self.targets.len());
        for target in self.targets.iter() {
            let url = format!("{}/mark-dirty", target.url);
            let body = payload.clone();
            let client = self.client.clone();
            joins.push(tokio::spawn(async move {
                match client.post(&url).json(&body).send().await {
                    Ok(resp) => {
                        let status = resp.status();
                        if status.is_success() {
                            true
                        } else {
                            // 404 ожидаем на cache-ci старее 0.4.0 (нет роута) —
                            // best-effort, invalidate после commit подстрахует.
                            let body_text =
                                resp.text().await.unwrap_or_else(|_| "<no body>".into());
                            eprintln!(
                                "[cache_client] {} non-2xx ({}): {}",
                                url, status, body_text
                            );
                            false
                        }
                    }
                    Err(e) => {
                        eprintln!("[cache_client] {} send error: {}", url, e);
                        false
                    }
                }
            }));
        }
        let mut ok_count = 0usize;
        for j in joins {
            if let Ok(true) = j.await {
                ok_count += 1;
            }
        }
        ok_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_targets_means_is_empty() {
        let c = CacheClient::new(Vec::new());
        assert!(c.is_empty());
        assert_eq!(c.target_count(), 0);
    }

    #[test]
    fn whitespace_only_urls_are_filtered_out() {
        let c = CacheClient::new(vec!["   ".into(), "".into()]);
        assert!(c.is_empty());
    }

    #[test]
    fn trailing_slashes_are_stripped() {
        let c = CacheClient::new(vec!["http://127.0.0.1:8011/".into()]);
        assert_eq!(c.target_count(), 1);
        // Не должен быть с trailing slash — иначе POST /invalidate станет
        // POST //invalidate.
        assert!(!c.targets[0].url.ends_with('/'));
    }

    #[tokio::test]
    async fn invalidate_files_noop_on_empty_paths() {
        let c = CacheClient::new(vec!["http://127.0.0.1:8011".into()]);
        let ok = c.invalidate_files("ut", &[]).await;
        assert_eq!(ok, 0);
    }

    #[tokio::test]
    async fn invalidate_files_noop_on_no_targets() {
        let c = CacheClient::new(Vec::new());
        let ok = c
            .invalidate_files("ut", &["src/X.bsl".to_string()])
            .await;
        assert_eq!(ok, 0);
    }

    #[tokio::test]
    async fn invalidate_files_handles_unreachable_target() {
        // Несуществующий порт, чтобы получить connection refused / timeout.
        // Главное проверить что вызов НЕ паникует и возвращает 0 успехов.
        let c = CacheClient::new(vec!["http://127.0.0.1:1".into()]);
        let ok = c
            .invalidate_files("ut", &["src/X.bsl".to_string()])
            .await;
        assert_eq!(ok, 0);
    }
}
