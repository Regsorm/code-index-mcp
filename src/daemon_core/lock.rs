// Глобальный PID-lock демона. Один на всю машину, путь из `paths::pid_file()`.
//
// В отличие от старого `src/pidlock.rs` (per-project), этот lock предотвращает
// запуск второго экземпляра демона параллельно с первым.

use std::path::PathBuf;

use anyhow::{bail, Result};

use super::paths;

/// RAII-guard глобального PID-lock. При drop удаляет PID-файл.
pub struct DaemonPidLock {
    path: PathBuf,
}

/// Попытаться захватить PID-lock. Если файл существует и процесс с записанным PID
/// жив — возвращается ошибка с указанием PID.
pub fn acquire() -> Result<DaemonPidLock> {
    let state_dir = paths::ensure_state_dir()?;
    let pid_path = state_dir.join("daemon.pid");

    if pid_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&pid_path) {
            if let Ok(pid) = content.trim().parse::<u32>() {
                if is_process_alive(pid) {
                    bail!(
                        "Демон code-index уже запущен (PID {}). PID-файл: {}",
                        pid,
                        pid_path.display()
                    );
                }
            }
        }
        eprintln!("[daemon] Найден устаревший PID-файл, перезаписываем");
    }

    std::fs::write(&pid_path, std::process::id().to_string())?;
    Ok(DaemonPidLock { path: pid_path })
}

/// Прочитать PID из PID-файла (без проверки живости).
/// Если `CODE_INDEX_HOME` не задана — возвращает `None` (нет папки — нет PID).
pub fn read_pid() -> Option<u32> {
    let pid_path = paths::pid_file().ok()?;
    std::fs::read_to_string(&pid_path)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
}

impl Drop for DaemonPidLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Проверить живость процесса по PID (кроссплатформенно через sysinfo).
fn is_process_alive(pid: u32) -> bool {
    use sysinfo::{Pid, ProcessesToUpdate, System};
    let mut sys = System::new();
    let spid = Pid::from(pid as usize);
    sys.refresh_processes(ProcessesToUpdate::Some(&[spid]), false);
    sys.process(spid).is_some()
}
