// IPC-контракт между демоном и его клиентами (MCP-сервер, CLI-команды управления).
//
// Транспорт — HTTP на loopback, тело — JSON. Все структуры сериализуемы и
// десериализуемы через serde, так что сервер и клиент используют одни и те же типы.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ── Runtime-info, записываемый демоном при старте ────────────────────────────

/// Содержимое файла `daemon.json` — MCP-клиент читает его, чтобы подключиться к демону.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeInfo {
    /// PID процесса демона.
    pub pid: u32,
    /// Версия демона (CARGO_PKG_VERSION).
    pub version: String,
    /// Фактический хост HTTP-сервера.
    pub http_host: String,
    /// Фактический порт HTTP-сервера (после возможного автовыбора).
    pub http_port: u16,
    /// Время старта в формате RFC 3339.
    pub started_at: String,
}

impl RuntimeInfo {
    /// Базовый URL для HTTP-запросов клиента.
    pub fn base_url(&self) -> String {
        format!("http://{}:{}", self.http_host, self.http_port)
    }
}

// ── Ответ GET /health ────────────────────────────────────────────────────────

/// Ответ корневого health-эндпоинта демона.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    /// Общий статус демона. Всегда `"running"` пока HTTP-сервер отвечает.
    pub status: String,
    /// Версия бинарника.
    pub version: String,
    /// PID процесса.
    pub pid: u32,
    /// Сколько секунд демон работает с момента старта.
    pub uptime_sec: u64,
    /// Время старта (RFC 3339).
    pub started_at: String,
    /// Per-папка статусы.
    pub paths: Vec<PathHealth>,
}

/// Статус одной отслеживаемой папки в ответе health.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathHealth {
    /// Абсолютный путь папки (как указан в конфиге, приведённый к канонической форме).
    pub path: PathBuf,
    /// Текущий статус.
    pub status: PathStatus,
    /// Прогресс индексации — `Some` только для `InitialIndexing` и `ReindexingBatch`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<Progress>,
    /// Краткое сообщение об ошибке — `Some` только для `Error`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Когда папка последний раз приходила в статус `Ready` (RFC 3339).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_ready_at: Option<String>,
}

/// Жизненный цикл отдельной папки, зафиксированный в брифе.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathStatus {
    /// Папки нет в конфиге демона либо демон её ещё не заметил.
    NotStarted,
    /// Идёт первичная индексация (пустая БД либо `.code-index/` отсутствовал).
    InitialIndexing,
    /// Индекс актуален, watcher активен.
    Ready,
    /// Watcher обрабатывает батч изменений файлов.
    ReindexingBatch,
    /// Ошибка — детали в поле `error`.
    Error,
}

/// Прогресс индексации папки.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Progress {
    pub files_done: usize,
    pub files_total: usize,
    /// Процент, округлённый до одного знака. `None` если `files_total == 0`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percent: Option<f32>,
}

impl Progress {
    pub fn new(done: usize, total: usize) -> Self {
        let percent = if total == 0 {
            None
        } else {
            Some(((done as f32) / (total as f32) * 100.0 * 10.0).round() / 10.0)
        };
        Self {
            files_done: done,
            files_total: total,
            percent,
        }
    }
}

// ── GET /path-status?path=... — оптимизированный эндпоинт для MCP ────────────

/// Ответ `GET /path-status?path=...`. MCP дёргает его при каждом tool-call,
/// чтобы не тащить полный health со всеми папками.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathStatusResponse {
    pub path: PathBuf,
    pub status: PathStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<Progress>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ── POST /reload ─────────────────────────────────────────────────────────────

/// Ответ `POST /reload`: диагностика — какие папки добавились/убрались.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReloadResponse {
    pub reloaded: bool,
    /// Пути, которых не было до reload.
    pub added: Vec<PathBuf>,
    /// Пути, которые были но убраны из конфига.
    pub removed: Vec<PathBuf>,
    /// Пути, которые остались без изменений.
    pub unchanged: Vec<PathBuf>,
    /// Сообщение об ошибке чтения конфига, если reload провалился.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ── POST /stop ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StopResponse {
    pub stopping: bool,
}

// ── Универсальный JSON-ответ инструмента MCP, когда папка не `Ready` ─────────

/// Структура, которую MCP-tool возвращает клиенту вместо «нормального» результата,
/// когда индекс ещё не готов или демон недоступен. Клиенту (агенту) остаётся
/// распознать поле `status` и поступить соответственно: подождать, запустить
/// демон, добавить путь в конфиг.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ToolUnavailable {
    /// Первичная или массовая переиндексация в процессе.
    Indexing {
        progress: Progress,
        message: String,
    },
    /// Запрошенная папка не в конфиге демона.
    NotStarted { message: String },
    /// Демон не отвечает на health-IPC.
    DaemonOffline { message: String },
    /// Ошибка на стороне демона — прокинута в поле message.
    Error { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_percent_rounded_to_one_decimal() {
        let p = Progress::new(1, 3);
        assert_eq!(p.files_done, 1);
        assert_eq!(p.files_total, 3);
        assert_eq!(p.percent, Some(33.3));
    }

    #[test]
    fn progress_percent_none_when_total_zero() {
        let p = Progress::new(0, 0);
        assert_eq!(p.percent, None);
    }

    #[test]
    fn path_status_serializes_as_snake_case() {
        let s = serde_json::to_string(&PathStatus::InitialIndexing).unwrap();
        assert_eq!(s, "\"initial_indexing\"");
    }

    #[test]
    fn tool_unavailable_tag_on_status_field() {
        let t = ToolUnavailable::DaemonOffline {
            message: "offline".into(),
        };
        let s = serde_json::to_string(&t).unwrap();
        assert!(s.contains("\"status\":\"daemon_offline\""));
        assert!(s.contains("\"message\":\"offline\""));
    }
}
