// Диспатч remote tool-call — обёртка над `RemoteClientPool::get_or_create`
// и `RemoteServeClient::call_federated` с единым форматом ошибок.
//
// На failure возвращаем JSON-string с `status: "federation_error"` —
// клиент MCP-протокола получит её как обычный tool-result, без RPC-ошибки.

use serde::Serialize;
use serde_json::Value;

use super::client::RemoteClientPool;

/// Сериализовать `params` (наша `*Params` структура) в JSON и форвардить
/// удалённому serve. Возвращает строку — успех (тот же ответ, что вернул бы
/// удалённый tool-handler) либо federation-error JSON.
pub async fn dispatch_remote<P: Serialize>(
    pool: &RemoteClientPool,
    ip: &str,
    tool: &str,
    params: &P,
) -> String {
    let params_json = match serde_json::to_value(params) {
        Ok(v) => v,
        Err(e) => return federation_error(tool, ip, format!("Сериализация params: {}", e)),
    };
    dispatch_remote_value(pool, ip, tool, params_json).await
}

/// То же что `dispatch_remote`, но `params` — уже готовый `serde_json::Value`.
/// Используется приёмной стороной в `get_stats(repo=Some)` для проброса.
pub async fn dispatch_remote_value(
    pool: &RemoteClientPool,
    ip: &str,
    tool: &str,
    params: Value,
) -> String {
    let client = match pool.get_or_create(ip).await {
        Ok(c) => c,
        Err(e) => return federation_error(tool, ip, format!("Не удалось создать клиент: {}", e)),
    };
    match client.call_federated(tool, params).await {
        Ok(body) => body,
        Err(e) => federation_error(tool, ip, e.to_string()),
    }
}

/// Стандартная JSON-обёртка для federation-error.
pub fn federation_error(tool: &str, ip: &str, message: impl Into<String>) -> String {
    let v = serde_json::json!({
        "status": "federation_error",
        "tool": tool,
        "ip": ip,
        "message": message.into(),
    });
    serde_json::to_string_pretty(&v)
        .unwrap_or_else(|_| format!("{{\"status\":\"federation_error\",\"tool\":\"{}\"}}", tool))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn unreachable_remote_returns_federation_error_json() {
        // Несуществующий порт на 127.0.0.1 — connect быстро упадёт с RST.
        let pool = super::super::client::RemoteClientPool::new(1, Duration::from_millis(500));
        let body = dispatch_remote(
            &pool,
            "127.0.0.1",
            "search_function",
            &serde_json::json!({"repo": "x", "query": "y"}),
        )
        .await;
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["status"], "federation_error");
        assert_eq!(parsed["tool"], "search_function");
        assert_eq!(parsed["ip"], "127.0.0.1");
        assert!(!parsed["message"].as_str().unwrap().is_empty());
    }
}
