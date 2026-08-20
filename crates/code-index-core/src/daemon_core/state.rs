// Разделяемое состояние демона: статусы отслеживаемых папок, прогресс, ошибки.
//
// Все изменения статуса идут через методы `DaemonState`, чтобы watcher,
// indexer и HTTP-сервер видели одни и те же данные.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::RwLock;

use super::ipc::{PathHealth, PathStatus, Progress};

/// Разделяемое состояние, которое демон держит в памяти.
#[derive(Clone)]
pub struct DaemonState {
    inner: Arc<RwLock<DaemonStateInner>>,
}

struct DaemonStateInner {
    /// Время старта демона (unix seconds).
    started_at_unix: u64,
    /// Время старта в RFC 3339 — кешируем, чтобы не форматировать каждый раз.
    started_at_rfc3339: String,
    /// Статусы папок.
    paths: HashMap<PathBuf, PathRuntime>,
}

/// Runtime-данные по одной папке.
#[derive(Debug, Clone)]
pub struct PathRuntime {
    pub status: PathStatus,
    pub progress: Option<Progress>,
    pub error: Option<String>,
    /// Когда папка последний раз приходила в `Ready`.
    pub last_ready_at: Option<String>,
    /// Момент последнего изменения статуса или прогресса. По нему строка
    /// состояния демона считает, сколько папка стоит без движения — главный
    /// признак «встало» при разборе обращения.
    pub changed_at: Instant,
    /// Поток, который ведёт эту папку. По нему берётся текущий этап работы:
    /// сам поток занят долгой синхронной работой и состояние обновлять не
    /// может, поэтому этап читается из общего реестра `logging`.
    pub worker_thread: Option<std::thread::ThreadId>,
}

impl Default for PathRuntime {
    fn default() -> Self {
        Self {
            status: PathStatus::NotStarted,
            progress: None,
            error: None,
            last_ready_at: None,
            changed_at: Instant::now(),
            worker_thread: None,
        }
    }
}

/// Срез одной папки для строки состояния демона в журнале.
#[derive(Debug, Clone)]
pub struct PathPulse {
    pub path: PathBuf,
    pub status: PathStatus,
    pub progress: Option<Progress>,
    /// Секунд без изменения статуса и прогресса.
    pub still_sec: u64,
    /// Чем папка занята прямо сейчас и сколько секунд: («граф вызовов», 40).
    /// `None` — между этапами или когда работа закончена.
    pub stage: Option<(String, u64)>,
}

impl DaemonState {
    pub fn new() -> Self {
        let now = SystemTime::now();
        let started_at_unix = now.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let started_at_rfc3339 = chrono::DateTime::<chrono::Utc>::from(now).to_rfc3339();
        Self {
            inner: Arc::new(RwLock::new(DaemonStateInner {
                started_at_unix,
                started_at_rfc3339,
                paths: HashMap::new(),
            })),
        }
    }

    /// Список отслеживаемых путей.
    pub async fn tracked_paths(&self) -> Vec<PathBuf> {
        let guard = self.inner.read().await;
        guard.paths.keys().cloned().collect()
    }

    /// Зарегистрировать набор путей в состоянии. Новые пути добавляются
    /// со статусом `NotStarted`; убранные — удаляются; существующие не трогаются.
    /// Возвращает `(added, removed, unchanged)`.
    pub async fn apply_config(&self, paths: &[PathBuf]) -> (Vec<PathBuf>, Vec<PathBuf>, Vec<PathBuf>) {
        let mut guard = self.inner.write().await;
        let mut added = Vec::new();
        let mut unchanged = Vec::new();

        let wanted: std::collections::HashSet<_> = paths.iter().cloned().collect();
        let existing: std::collections::HashSet<_> = guard.paths.keys().cloned().collect();

        for p in &wanted {
            if !existing.contains(p) {
                guard.paths.insert(p.clone(), PathRuntime::default());
                added.push(p.clone());
            } else {
                unchanged.push(p.clone());
            }
        }

        let removed: Vec<PathBuf> = existing.difference(&wanted).cloned().collect();
        for p in &removed {
            guard.paths.remove(p);
        }

        (added, removed, unchanged)
    }

    /// Запомнить поток, который ведёт эту папку. Вызывается самим рабочим
    /// потоком в начале работы: по этой отметке строка состояния демона
    /// узнаёт, каким этапом папка занята прямо сейчас.
    pub async fn note_worker_thread(&self, path: &PathBuf) {
        let mut guard = self.inner.write().await;
        let entry = guard.paths.entry(path.clone()).or_default();
        entry.worker_thread = Some(std::thread::current().id());
    }

    /// Выставить статус папки. Используется фоновыми задачами демона.
    pub async fn set_status(&self, path: &PathBuf, status: PathStatus) {
        let mut guard = self.inner.write().await;
        let entry = guard.paths.entry(path.clone()).or_default();
        entry.status = status;
        entry.error = None;
        entry.changed_at = Instant::now();
        if status == PathStatus::Ready {
            entry.progress = None;
            entry.last_ready_at = Some(chrono::Utc::now().to_rfc3339());
        }
    }

    /// Обновить прогресс индексации папки. Статус должен быть `InitialIndexing`
    /// или `ReindexingBatch`, иначе вызов игнорируется.
    pub async fn set_progress(&self, path: &PathBuf, progress: Progress) {
        let mut guard = self.inner.write().await;
        if let Some(entry) = guard.paths.get_mut(path) {
            if matches!(
                entry.status,
                PathStatus::InitialIndexing | PathStatus::ReindexingBatch
            ) {
                entry.progress = Some(progress);
                entry.changed_at = Instant::now();
            }
        }
    }

    /// Зафиксировать ошибку индексации папки.
    pub async fn set_error(&self, path: &PathBuf, message: impl Into<String>) {
        let mut guard = self.inner.write().await;
        let entry = guard.paths.entry(path.clone()).or_default();
        entry.status = PathStatus::Error;
        entry.progress = None;
        entry.error = Some(message.into());
        entry.changed_at = Instant::now();
    }

    /// Получить текущий runtime одной папки.
    pub async fn get(&self, path: &PathBuf) -> Option<PathRuntime> {
        let guard = self.inner.read().await;
        guard.paths.get(path).cloned()
    }

    /// Время старта демона в секундах UNIX.
    pub async fn started_at_unix(&self) -> u64 {
        self.inner.read().await.started_at_unix
    }

    /// Время старта демона в RFC 3339.
    pub async fn started_at_rfc3339(&self) -> String {
        self.inner.read().await.started_at_rfc3339.clone()
    }

    /// Срез всех папок для пульса в журнале: статус, прогресс и сколько
    /// секунд не было движения. Порядок — по пути, чтобы строки в журнале
    /// от снимка к снимку шли одинаково и разница читалась глазами.
    pub async fn pulse_snapshot(&self) -> Vec<PathPulse> {
        let guard = self.inner.read().await;
        let now = Instant::now();
        let mut out: Vec<PathPulse> = guard
            .paths
            .iter()
            .map(|(path, rt)| PathPulse {
                path: path.clone(),
                status: rt.status,
                progress: rt.progress.clone(),
                still_sec: now.duration_since(rt.changed_at).as_secs(),
                stage: rt.worker_thread.and_then(crate::logging::stage_running),
            })
            .collect();
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
    }

    /// Сформировать срез состояния для ответа GET /health.
    pub async fn to_health_paths(&self) -> Vec<PathHealth> {
        let guard = self.inner.read().await;
        guard
            .paths
            .iter()
            .map(|(path, rt)| PathHealth {
                path: path.clone(),
                status: rt.status,
                progress: rt.progress.clone(),
                error: rt.error.clone(),
                last_ready_at: rt.last_ready_at.clone(),
            })
            .collect()
    }
}

impl Default for DaemonState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn apply_config_tracks_diff() {
        let st = DaemonState::new();

        let (added, removed, unchanged) = st
            .apply_config(&[PathBuf::from("/a"), PathBuf::from("/b")])
            .await;
        assert_eq!(added.len(), 2);
        assert_eq!(removed.len(), 0);
        assert_eq!(unchanged.len(), 0);

        let (added, removed, unchanged) = st
            .apply_config(&[PathBuf::from("/b"), PathBuf::from("/c")])
            .await;
        assert_eq!(added, vec![PathBuf::from("/c")]);
        assert_eq!(removed, vec![PathBuf::from("/a")]);
        assert_eq!(unchanged, vec![PathBuf::from("/b")]);
    }

    #[tokio::test]
    async fn set_ready_clears_progress() {
        let st = DaemonState::new();
        let path = PathBuf::from("/a");
        st.apply_config(&[path.clone()]).await;
        st.set_status(&path, PathStatus::InitialIndexing).await;
        st.set_progress(&path, Progress::new(10, 100)).await;

        st.set_status(&path, PathStatus::Ready).await;

        let rt = st.get(&path).await.unwrap();
        assert_eq!(rt.status, PathStatus::Ready);
        assert!(rt.progress.is_none());
        assert!(rt.last_ready_at.is_some());
    }
}
