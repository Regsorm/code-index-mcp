/// Модуль файлового наблюдателя — отслеживает изменения в проекте
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Событие файловой системы
#[derive(Debug, Clone)]
pub enum FileEvent {
    /// Файл создан
    Created(PathBuf),
    /// Файл изменён
    Modified(PathBuf),
    /// Файл удалён
    Deleted(PathBuf),
}

/// Конфигурация watcher
pub struct WatcherConfig {
    /// Задержка debounce в миллисекундах (по умолчанию 1500 мс)
    pub debounce_ms: u64,
    /// Максимальное время ожидания батча в миллисекундах (по умолчанию 2000 мс)
    pub batch_ms: u64,
    /// Дополнительные директории для исключения
    pub exclude_dirs: Vec<String>,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            debounce_ms: 1500,
            batch_ms: 2000,
            exclude_dirs: vec![],
        }
    }
}

/// Создать watcher и вернуть receiver для событий файловой системы.
///
/// Watcher работает в фоновом потоке notify. Возвращает кортеж
/// (watcher, receiver). Watcher нужно хранить — при дропе останавливается.
pub fn create_watcher(
    root: &Path,
    config: &WatcherConfig,
) -> anyhow::Result<(RecommendedWatcher, mpsc::Receiver<FileEvent>)> {
    let (tx, rx) = mpsc::channel();
    let exclude_dirs = config.exclude_dirs.clone();
    let root_path = root.to_path_buf();

    let mut watcher =
        notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
            if let Ok(event) = res {
                for path in &event.paths {
                    // Вычисляем относительный путь для проверки исключений
                    let rel = path.strip_prefix(&root_path).unwrap_or(path);

                    // Проверяем, не попадает ли файл в исключённую директорию
                    let is_excluded = rel.components().any(|c| {
                        let s = c.as_os_str().to_string_lossy();
                        exclude_dirs.contains(&s.to_string())
                            || crate::indexer::file_types::EXCLUDE_DIRS
                                .contains(&s.as_ref())
                    });
                    if is_excluded {
                        continue;
                    }

                    // Для событий не-удаления — проверяем, что это файл, а не директория
                    if !matches!(event.kind, notify::EventKind::Remove(_)) && !path.is_file() {
                        continue;
                    }

                    let file_event = match event.kind {
                        notify::EventKind::Create(_) => FileEvent::Created(path.clone()),
                        notify::EventKind::Modify(_) => FileEvent::Modified(path.clone()),
                        notify::EventKind::Remove(_) => FileEvent::Deleted(path.clone()),
                        _ => continue,
                    };

                    let _ = tx.send(file_event);
                }
            }
        })?;

    watcher.watch(root, RecursiveMode::Recursive)?;

    Ok((watcher, rx))
}

/// Собрать батч событий с debounce.
///
/// Блокирует поток до первого события, затем ждёт `debounce_ms` тишины.
/// Максимальное время накопления батча ограничено `batch_ms`.
/// Возвращает дедуплицированный список событий (одно событие на файл).
pub fn collect_batch(
    rx: &mpsc::Receiver<FileEvent>,
    debounce_ms: u64,
    batch_ms: u64,
) -> Vec<FileEvent> {
    // Дедупликация: для каждого пути — только последнее событие
    let mut pending: HashMap<PathBuf, FileEvent> = HashMap::new();
    let debounce = Duration::from_millis(debounce_ms);
    let batch_timeout = Duration::from_millis(batch_ms);

    // Ждём первое событие (блокирующе)
    match rx.recv() {
        Ok(event) => {
            let path = event_path(&event).clone();
            pending.insert(path, event);
        }
        Err(_) => return vec![], // канал закрыт
    }

    let batch_start = Instant::now();
    let mut last_event = Instant::now();

    // Собираем дополнительные события пока есть «тишина» < debounce_ms
    loop {
        // Прерываем если суммарное время накопления превысило batch_ms
        if batch_start.elapsed() >= batch_timeout {
            break;
        }

        let elapsed_since_last = last_event.elapsed();
        if elapsed_since_last >= debounce {
            // Тишина — батч готов
            break;
        }

        let wait = debounce.saturating_sub(elapsed_since_last);
        match rx.recv_timeout(wait) {
            Ok(event) => {
                let path = event_path(&event).clone();
                pending.insert(path, event);
                last_event = Instant::now();
            }
            Err(mpsc::RecvTimeoutError::Timeout) => break, // тишина — батч готов
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    pending.into_values().collect()
}

/// Вспомогательная функция: извлечь путь из события
fn event_path(event: &FileEvent) -> &PathBuf {
    match event {
        FileEvent::Created(p) | FileEvent::Modified(p) | FileEvent::Deleted(p) => p,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_watcher_detects_file_creation() {
        let tmp = TempDir::new().unwrap();
        let config = WatcherConfig::default();
        let (_watcher, rx) = create_watcher(tmp.path(), &config).unwrap();

        // Небольшая пауза для инициализации watcher
        std::thread::sleep(Duration::from_millis(100));
        fs::write(tmp.path().join("test.py"), "def foo(): pass").unwrap();

        // Ждём события создания файла
        let event = rx.recv_timeout(Duration::from_secs(3));
        assert!(event.is_ok(), "Должно быть событие создания файла");
    }

    #[test]
    fn test_watcher_excludes_dirs() {
        let tmp = TempDir::new().unwrap();
        let git_dir = tmp.path().join(".git");
        fs::create_dir_all(&git_dir).unwrap();

        let config = WatcherConfig::default();
        let (_watcher, rx) = create_watcher(tmp.path(), &config).unwrap();

        std::thread::sleep(Duration::from_millis(100));

        // Файл в .git — НЕ должен генерировать событие (стандартная исключённая директория)
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main").unwrap();

        // Файл в корне — должен генерировать событие
        fs::write(tmp.path().join("main.py"), "print('hello')").unwrap();

        let mut events = vec![];
        // Собираем все события в течение 2 секунд
        while let Ok(e) = rx.recv_timeout(Duration::from_secs(2)) {
            events.push(e);
        }

        // Должно быть событие только для main.py, не для .git/HEAD
        let has_main = events.iter().any(|e| match e {
            FileEvent::Created(p) | FileEvent::Modified(p) => p.ends_with("main.py"),
            _ => false,
        });
        let has_git = events.iter().any(|e| match e {
            FileEvent::Created(p) | FileEvent::Modified(p) => {
                p.to_str().unwrap_or("").contains(".git")
            }
            _ => false,
        });

        assert!(has_main, "Должно быть событие для main.py");
        assert!(!has_git, "НЕ должно быть событий для .git");
    }

    #[test]
    fn test_collect_batch_deduplication() {
        let (tx, rx) = mpsc::channel();

        // Отправляем два события для одного файла — должно прийти одно
        let path = PathBuf::from("/tmp/test.py");
        tx.send(FileEvent::Created(path.clone())).unwrap();
        tx.send(FileEvent::Modified(path.clone())).unwrap();

        // Закрываем отправитель
        drop(tx);

        // Собираем с минимальным debounce
        let batch = collect_batch(&rx, 50, 200);

        // Дедупликация: одно событие на файл (последнее — Modified)
        assert_eq!(batch.len(), 1, "Должно быть одно событие (дедупликация)");
        assert!(
            matches!(&batch[0], FileEvent::Modified(p) if p == &path),
            "Событие должно быть Modified"
        );
    }

    #[test]
    fn test_collect_batch_empty_on_closed_channel() {
        let (tx, rx) = mpsc::channel::<FileEvent>();
        drop(tx); // Закрываем канал немедленно

        let batch = collect_batch(&rx, 100, 200);
        assert!(batch.is_empty(), "Пустой батч при закрытом канале");
    }
}
