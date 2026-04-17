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
    let info = runner::read_runtime_info()
        .ok_or_else(|| anyhow!("Демон не запущен (runtime-info файл отсутствует)"))?;
    Ok(info.base_url())
}

/// Прочитать runtime-info без ошибки (Some если демон запущен).
pub fn runtime_info() -> Option<RuntimeInfo> {
    runner::read_runtime_info()
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
