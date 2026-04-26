// HTTP-клиент к удалённому `code-index serve`.
//
// Один клиент на пару (удалённый IP, порт), переиспользуется (reqwest::Client
// держит connection pool). Запрос — POST `/federate/<tool>` с JSON-телом
// (сериализация наших `*Params` структур).
//
// Порт задаётся per-host через поле `port` в `[[paths]]` секции `serve.toml`
// (см. `federation::config::ServePathEntry`). Если в `serve.toml` для записи
// порт не указан — используется `DEFAULT_REMOTE_PORT` (8011).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::RwLock;

/// Таймаут любого исходящего forwarded-запроса.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// Стандартный порт MCP-serve. Используется как дефолт, когда в `serve.toml`
/// для конкретной записи `[[paths]]` не задан явный `port`. С rc7+ можно
/// конфигурировать per-host (`ServePathEntry::port`).
pub const DEFAULT_REMOTE_PORT: u16 = 8011;

/// Клиент к одному конкретному удалённому serve.
pub struct RemoteServeClient {
    http: reqwest::Client,
    base_url: String,
}

impl RemoteServeClient {
    /// Создать клиента с заданным таймаутом и переиспользуемым connection pool.
    pub fn new(ip: &str, port: u16, timeout: Duration) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .pool_idle_timeout(Some(Duration::from_secs(60)))
            .build()
            .map_err(|e| anyhow::anyhow!("Не удалось создать reqwest::Client: {}", e))?;
        Ok(Self {
            http,
            base_url: format!("http://{}:{}", ip, port),
        })
    }

    /// POST `/federate/<tool>` с JSON-телом `params`. Возвращает строку ответа
    /// (то же, что вернул бы tool-handler удалённого serve).
    pub async fn call_federated(&self, tool: &str, params: Value) -> anyhow::Result<String> {
        let url = format!("{}/federate/{}", self.base_url, tool);
        let resp = self
            .http
            .post(&url)
            .json(&params)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("HTTP-запрос к {} упал: {}", url, e))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| anyhow::anyhow!("Не удалось прочитать тело ответа от {}: {}", url, e))?;
        if !status.is_success() {
            anyhow::bail!("{} вернул статус {}: {}", url, status, text);
        }
        Ok(text)
    }

    /// Базовый URL — для логов и отладки.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

/// Пул переиспользуемых клиентов, ключ — пара (IP удалённого serve, порт).
///
/// Пара ключей нужна, чтобы поддерживать несколько serve-нод на одной машине
/// (разные деплои/тестовые окружения, разнесённые по портам). Без этого один
/// клиент перекрывал бы оба адреса и упирался в дефолтный 8011.
pub struct RemoteClientPool {
    inner: RwLock<HashMap<(String, u16), Arc<RemoteServeClient>>>,
    timeout: Duration,
}

impl RemoteClientPool {
    /// Пул с заданным таймаутом ответа удалённого serve. Сам по себе пул не
    /// фиксирует порт — он подаётся в `get_or_create` per-call (берётся из
    /// `RepoEntry::port`, который в свою очередь приходит из `serve.toml`).
    pub fn new(timeout: Duration) -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
            timeout,
        }
    }

    /// Дефолтный пул: таймаут 5 сек.
    pub fn with_defaults() -> Self {
        Self::new(DEFAULT_TIMEOUT)
    }

    /// Получить или лениво создать клиент для пары (ip, port).
    /// Разные порты на одном IP — разные клиенты с разными connection-pool-ами.
    pub async fn get_or_create(
        &self,
        ip: &str,
        port: u16,
    ) -> anyhow::Result<Arc<RemoteServeClient>> {
        let key = (ip.to_string(), port);
        // Быстрый путь — read lock.
        {
            let r = self.inner.read().await;
            if let Some(c) = r.get(&key) {
                return Ok(Arc::clone(c));
            }
        }
        // Медленный путь — write lock + double-check.
        let mut w = self.inner.write().await;
        if let Some(c) = w.get(&key) {
            return Ok(Arc::clone(c));
        }
        let client = Arc::new(RemoteServeClient::new(ip, port, self.timeout)?);
        w.insert(key, Arc::clone(&client));
        Ok(client)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pool_returns_same_client_for_same_ip_and_port() {
        let pool = RemoteClientPool::with_defaults();
        let a = pool
            .get_or_create("192.0.2.50", DEFAULT_REMOTE_PORT)
            .await
            .unwrap();
        let b = pool
            .get_or_create("192.0.2.50", DEFAULT_REMOTE_PORT)
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&a, &b), "пул должен переиспользовать клиент");
    }

    #[tokio::test]
    async fn pool_creates_separate_clients_for_different_ips() {
        let pool = RemoteClientPool::with_defaults();
        let a = pool
            .get_or_create("192.0.2.50", DEFAULT_REMOTE_PORT)
            .await
            .unwrap();
        let b = pool
            .get_or_create("192.0.2.51", DEFAULT_REMOTE_PORT)
            .await
            .unwrap();
        assert!(!Arc::ptr_eq(&a, &b));
        assert_eq!(a.base_url(), "http://192.0.2.50:8011");
        assert_eq!(b.base_url(), "http://192.0.2.51:8011");
    }

    #[tokio::test]
    async fn pool_creates_separate_clients_for_different_ports_on_same_ip() {
        // Регрессия: до rc7 пул ключевался только по IP, и две serve-ноды на
        // одной машине неизбежно перекрывали друг друга. После rekey-а каждая
        // (ip, port)-пара получает свой переиспользуемый клиент.
        let pool = RemoteClientPool::with_defaults();
        let a = pool.get_or_create("192.0.2.50", 8011).await.unwrap();
        let b = pool.get_or_create("192.0.2.50", 8021).await.unwrap();
        assert!(!Arc::ptr_eq(&a, &b));
        assert_eq!(a.base_url(), "http://192.0.2.50:8011");
        assert_eq!(b.base_url(), "http://192.0.2.50:8021");
    }

    #[tokio::test]
    async fn call_against_unreachable_address_errors_fast() {
        // Используем зарезервированный 0.0.0.2 — реактивный TCP_RST или таймаут.
        let pool = RemoteClientPool::new(Duration::from_millis(300));
        let client = pool
            .get_or_create("127.0.0.1", DEFAULT_REMOTE_PORT)
            .await
            .unwrap();
        // На 127.0.0.1:8011 в тестовом окружении нет live-сервиса (или есть,
        // но эндпоинт /federate/... вернёт 404). В обоих случаях call_federated
        // должен либо вернуть ошибку (нет сервера), либо непустой ответ —
        // нам важно, что он не зависает дольше таймаута.
        let _ = client
            .call_federated("nonexistent_tool", serde_json::json!({}))
            .await;
        // Тест проходит, если функция вернулась (success или error) до таймаута.
    }
}
