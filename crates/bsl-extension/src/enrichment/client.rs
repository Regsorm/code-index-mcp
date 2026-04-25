// HTTP-клиент к OpenAI-compatible /v1/chat/completions.
//
// Под feature `enrichment` — без неё bsl-extension не тащит reqwest, и
// сам клиент не компилируется. Вызывающий код (`batch.rs`, `cli.rs`)
// тоже целиком gated на эту фичу.
//
// Тестируемость через trait `ChatClient`: основная реализация
// `ReqwestChatClient` ходит в живой endpoint, тесты подставляют свой
// (см. `MockChatClient` в `tests/`). Поэтому signature trait'а
// async-friendly, без захвата self в return type.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

use super::prompt::Message;

/// Абстракция над «отправь user+system → получи строку ответа».
/// Trait async, чтобы дать тестам подменить `execute` мок-реализацией.
pub trait ChatClient: Send + Sync {
    /// Один запрос к chat-completions. Возвращает текст из
    /// `choices[0].message.content`.
    fn complete<'a>(
        &'a self,
        messages: Vec<Message>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>>;
}

/// Боевая реализация поверх reqwest. Конструктор резолвит API-ключ из
/// переменной окружения один раз — если переменной нет, создание клиента
/// падает. Это намеренно: тихое отсутствие ключа = `401 Unauthorized` на
/// каждый запрос, а это путает диагностику.
pub struct ReqwestChatClient {
    http: reqwest::Client,
    url: String,
    model: String,
    api_key: Option<String>,
    /// Сколько раз повторить при сетевой ошибке / 5xx.
    max_retries: u32,
}

impl ReqwestChatClient {
    /// Собрать клиент. `api_key_env` — имя env-переменной (не сама
    /// переменная); None — без авторизации (Ollama локально).
    pub fn new(
        url: impl Into<String>,
        model: impl Into<String>,
        api_key_env: Option<&str>,
        max_retries: u32,
    ) -> Result<Self> {
        let api_key = match api_key_env {
            Some(name) => Some(
                std::env::var(name)
                    .with_context(|| format!("env-переменная {} не задана (api_key_env)", name))?,
            ),
            None => None,
        };
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .context("не удалось собрать reqwest::Client")?;
        Ok(Self { http, url: url.into(), model: model.into(), api_key, max_retries })
    }
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<RequestMessage<'a>>,
    /// Низкий temperature — для FTS-обогащения нам важна стабильность
    /// формулировок, не креативность. 0.2 — устойчивый компромисс.
    temperature: f32,
}

#[derive(Serialize)]
struct RequestMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ResponseMessage,
}

#[derive(Deserialize)]
struct ResponseMessage {
    content: Option<String>,
}

impl ChatClient for ReqwestChatClient {
    fn complete<'a>(
        &'a self,
        messages: Vec<Message>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>> {
        Box::pin(async move {
            let req_body = ChatRequest {
                model: &self.model,
                messages: messages
                    .iter()
                    .map(|m| RequestMessage { role: m.role, content: &m.content })
                    .collect(),
                temperature: 0.2,
            };

            // Простой backoff: 0мс, 500мс, 1500мс между попытками. Для
            // batch-обогащения на десятки тысяч процедур этого достаточно;
            // модель внешних API выдерживает.
            let max_attempts = self.max_retries.max(1);
            let mut last_err: Option<anyhow::Error> = None;
            for attempt in 0..max_attempts {
                if attempt > 0 {
                    let delay_ms = 500u64 * (1 << (attempt - 1).min(4));
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }

                let mut req = self.http.post(&self.url).json(&req_body);
                if let Some(key) = &self.api_key {
                    req = req.bearer_auth(key);
                }

                match req.send().await {
                    Ok(resp) => {
                        let status = resp.status();
                        if status.is_success() {
                            let parsed: ChatResponse = resp
                                .json()
                                .await
                                .context("не удалось распарсить chat-completions ответ")?;
                            let content = parsed
                                .choices
                                .first()
                                .and_then(|c| c.message.content.clone())
                                .ok_or_else(|| {
                                    anyhow!("chat-completions ответ без choices[0].message.content")
                                })?;
                            return Ok(content);
                        }
                        // 4xx — не ретраим (обычно auth или bad model name)
                        let body = resp.text().await.unwrap_or_default();
                        if status.is_client_error() {
                            return Err(anyhow!(
                                "chat-completions {} (без retry): {}",
                                status,
                                body.chars().take(500).collect::<String>()
                            ));
                        }
                        last_err = Some(anyhow!(
                            "chat-completions {}: {}",
                            status,
                            body.chars().take(500).collect::<String>()
                        ));
                    }
                    Err(e) => {
                        last_err = Some(anyhow::Error::new(e).context("send chat-completions"));
                    }
                }
            }
            Err(last_err.unwrap_or_else(|| anyhow!("chat-completions: исчерпаны попытки")))
        })
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Mock-реализация `ChatClient` для unit-тестов batch/cli без сети.
    use super::*;
    use std::sync::Mutex;

    /// Моковый клиент: отдаёт заранее заданные ответы по очереди.
    /// Если ответов больше нет — отдаёт ошибку.
    pub struct MockChatClient {
        pub responses: Mutex<Vec<Result<String>>>,
        pub calls: Mutex<Vec<Vec<Message>>>,
    }

    impl MockChatClient {
        pub fn with_responses<I: IntoIterator<Item = Result<String>>>(items: I) -> Self {
            Self {
                responses: Mutex::new(items.into_iter().collect()),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl ChatClient for MockChatClient {
        fn complete<'a>(
            &'a self,
            messages: Vec<Message>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>>
        {
            Box::pin(async move {
                self.calls.lock().unwrap().push(messages);
                let mut q = self.responses.lock().unwrap();
                if q.is_empty() {
                    return Err(anyhow!("MockChatClient: ответы исчерпаны"));
                }
                q.remove(0)
            })
        }
    }
}
