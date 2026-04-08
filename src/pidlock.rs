/// PID-lock для защиты от запуска нескольких экземпляров демона.
use std::path::{Path, PathBuf};
use anyhow::{Result, bail};

/// Попытаться захватить PID-lock. Если файл существует и процесс жив → ошибка.
pub fn acquire(lock_dir: &Path) -> Result<PidLock> {
    let pid_path = lock_dir.join("serve.pid");

    if pid_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&pid_path) {
            if let Ok(pid) = content.trim().parse::<u32>() {
                if is_process_alive(pid) {
                    bail!(
                        "Другой экземпляр code-index serve уже запущен (PID {}). \
                         Файл блокировки: {}",
                        pid,
                        pid_path.display()
                    );
                }
            }
        }
        eprintln!("[pidlock] Найден устаревший PID-файл, перезаписываем");
    }

    std::fs::create_dir_all(lock_dir)?;
    std::fs::write(&pid_path, std::process::id().to_string())?;

    Ok(PidLock { path: pid_path })
}

/// RAII guard — удаляет PID-файл при drop
pub struct PidLock {
    path: PathBuf,
}

impl Drop for PidLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Проверить, жив ли процесс с данным PID.
/// Использует sysinfo 0.32: Pid::from(usize), ProcessesToUpdate::Some(&[Pid]).
fn is_process_alive(pid: u32) -> bool {
    use sysinfo::{System, Pid, ProcessesToUpdate};
    let mut sys = System::new();
    let spid = Pid::from(pid as usize);
    sys.refresh_processes(ProcessesToUpdate::Some(&[spid]), false);
    sys.process(spid).is_some()
}
