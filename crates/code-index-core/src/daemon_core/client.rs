// HTTP-клиент к демону. Все методы асинхронные — CLI-команды вызываются из
// контекста #[tokio::main], а MCP-сервер тоже async.

use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, Result};

use super::ipc::{HealthResponse, PathStatusResponse, ReloadResponse, RuntimeInfo, StopResponse};
use super::runner;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3);

/// Базовый URL запущенного демона. `Err` если runtime-info файл не найден.
pub fn base_url() -> Result<String> {
    let info =
        runner::read_runtime_info().ok_or_else(|| anyhow!("{}", daemon_unavailable_hint()))?;
    Ok(info.base_url())
}

/// Прочитать runtime-info без ошибки (Some если демон запущен).
pub fn runtime_info() -> Option<RuntimeInfo> {
    runner::read_runtime_info()
}

/// Диагностическое сообщение, когда демон не найден через runtime-info.
///
/// Самая частая причина (issue #1, подтверждено воспроизведением) — НЕ «демон не
/// запущен», а РАССИНХРОН `CODE_INDEX_HOME` между процессом `serve` и демоном:
/// они находят друг друга только через `$CODE_INDEX_HOME/daemon.json`, и если у
/// `serve` переменная не задана или указывает в другую папку, runtime-info не
/// читается, хотя демон жив. На Linux/macOS GUI-клиенты (VS Code, Continue, Cline)
/// не наследуют `~/.bashrc`, поэтому `serve`, запущенный клиентом с пустым `env`,
/// не видит `CODE_INDEX_HOME` из шелла. Поэтому сообщение явно называет ожидаемый
/// путь и куда смотреть.
pub fn daemon_unavailable_hint() -> String {
    match super::paths::runtime_info_file() {
        Ok(path) => format!(
            "Демон не найден: отсутствует runtime-info файл {}. \
             CODE_INDEX_HOME = {}. \
             Проверьте: (1) демон запущен — `code-index daemon run`; \
             (2) демон и этот процесс используют ОДИН CODE_INDEX_HOME. \
             Частая причина: GUI-клиент (VS Code/Continue/Cline) на Linux/macOS не читает ~/.bashrc, \
             поэтому serve не видит CODE_INDEX_HOME из шелла — задайте его тем же абсолютным путём, \
             что у демона, в секции \"env\" MCP-конфигурации клиента.",
            path.display(),
            std::env::var(super::paths::HOME_ENV).unwrap_or_else(|_| "<не задана>".to_string()),
        ),
        Err(_) => format!(
            "Демон не найден: переменная окружения {} не задана для этого процесса. \
             Задайте её тем же абсолютным путём, что у демона. Для GUI-клиентов \
             (VS Code/Continue/Cline) на Linux/macOS — в секции \"env\" MCP-конфигурации, \
             т.к. ~/.bashrc к ним не применяется. Пример: \"env\": {{ \"{}\": \"/home/you/code-index\" }}.",
            super::paths::HOME_ENV,
            super::paths::HOME_ENV,
        ),
    }
}

fn async_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(DEFAULT_TIMEOUT)
        .build()
        .map_err(|e| anyhow!("reqwest::Client: {}", e))
}

/// GET /health
pub async fn health() -> Result<HealthResponse> {
    let url = format!("{}/health", base_url()?);
    let resp = async_client()?
        .get(&url)
        .send()
        .await
        .map_err(|e| anyhow!("GET {} → {}", url, e))?
        .error_for_status()?;
    let body: HealthResponse = resp.json().await?;
    Ok(body)
}

/// POST /reload
pub async fn reload() -> Result<ReloadResponse> {
    let url = format!("{}/reload", base_url()?);
    let resp = async_client()?
        .post(&url)
        .send()
        .await
        .map_err(|e| anyhow!("POST {} → {}", url, e))?
        .error_for_status()?;
    let body: ReloadResponse = resp.json().await?;
    Ok(body)
}

/// POST /stop
pub async fn stop() -> Result<StopResponse> {
    let url = format!("{}/stop", base_url()?);
    let resp = async_client()?
        .post(&url)
        .send()
        .await
        .map_err(|e| anyhow!("POST {} → {}", url, e))?
        .error_for_status()?;
    let body: StopResponse = resp.json().await?;
    Ok(body)
}

/// GET /path-status?path=...
pub async fn path_status_async(path: &Path) -> Result<PathStatusResponse> {
    let url = format!(
        "{}/path-status?path={}",
        base_url()?,
        urlencoding(path.to_string_lossy().as_ref())
    );
    let resp = async_client()?
        .get(&url)
        .send()
        .await
        .map_err(|e| anyhow!("GET {} → {}", url, e))?
        .error_for_status()?;
    let body: PathStatusResponse = resp.json().await?;
    Ok(body)
}

/// Минимальная percent-encoding для параметра path в URL.
fn urlencoding(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 3);
    for b in input.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' | b':' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{:02X}", b));
            }
        }
    }
    out
}
