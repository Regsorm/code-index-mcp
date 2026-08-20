// Журнал долгоживущих процессов: демона индексации и сервера выдачи.
//
// Задача модуля — чтобы у пользователя оставался файл, который можно
// приложить к обращению: «индексация встала, вот журнал». До этого весь
// вывод шёл только в stderr, а на Windows демон отвязывается от консоли
// (DETACHED_PROCESS) — то есть вывод терялся полностью.
//
// Устройство:
//   * запись идёт одновременно в stderr и в файл (в контейнере stderr
//     собирает `docker logs`, на рабочей станции файл — единственный след);
//   * файл ротируется по размеру: `daemon.log` → `daemon.log.1` → ... ;
//   * уровень берётся из `RUST_LOG`, иначе из `[daemon] log_level`
//     в `daemon.toml`, иначе `info`;
//   * чужие крейты (hyper, reqwest, notify) держатся на `warn` — иначе
//     `log_level = "debug"` тонет в их собственном потоке сообщений.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// Предельный размер файла журнала. При превышении текущий файл
/// переименовывается в `.1`, прежний `.1` — в `.2` и так далее.
pub const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;

/// Сколько прежних файлов журнала хранить помимо текущего.
pub const KEEP_ROTATED: usize = 3;

/// Уровень по умолчанию, если не задан ни `RUST_LOG`, ни `log_level`.
pub const DEFAULT_LEVEL: &str = "info";

// ── Файл журнала с ротацией по размеру ──────────────────────────────────────

struct Inner {
    path: PathBuf,
    file: Option<File>,
    /// Сколько байт уже в текущем файле (учитывая то, что было до открытия).
    written: u64,
    max_bytes: u64,
    keep: usize,
}

impl Inner {
    fn open(path: PathBuf, max_bytes: u64, keep: usize) -> io::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            path,
            file: Some(file),
            written,
            max_bytes,
            keep,
        })
    }

    /// Сдвинуть файлы: `.{keep}` удаляется, `.{n}` → `.{n+1}`, текущий → `.1`.
    /// Ошибки переименования не фатальны — журнал не имеет права ронять процесс,
    /// в худшем случае текущий файл продолжит расти.
    fn rotate(&mut self) {
        // Закрыть текущий дескриптор до переименования — Windows не даёт
        // переименовать файл, открытый на запись.
        self.file = None;

        let numbered = |n: usize| -> PathBuf {
            let mut s = self.path.clone().into_os_string();
            s.push(format!(".{}", n));
            PathBuf::from(s)
        };

        let _ = std::fs::remove_file(numbered(self.keep));
        for n in (1..self.keep).rev() {
            let from = numbered(n);
            if from.exists() {
                let _ = std::fs::rename(&from, numbered(n + 1));
            }
        }
        let _ = std::fs::rename(&self.path, numbered(1));

        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .ok();
        self.written = 0;
    }
}

impl Write for Inner {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.written + buf.len() as u64 > self.max_bytes {
            self.rotate();
        }
        match self.file.as_mut() {
            Some(f) => {
                let n = f.write(buf)?;
                self.written += n as u64;
                Ok(n)
            }
            // Файл переоткрыть не удалось — притворяемся, что записали.
            // Потеря строки журнала лучше паники в рабочем процессе.
            None => Ok(buf.len()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.file.as_mut() {
            Some(f) => f.flush(),
            None => Ok(()),
        }
    }
}

/// Обёртка над файлом журнала, пригодная как приёмник записи для tracing.
/// Клонируется дёшево: внутри общий `Arc<Mutex<_>>`, поэтому строки из разных
/// потоков не перемешиваются внутри одной записи.
#[derive(Clone)]
pub struct RotatingFile(Arc<Mutex<Inner>>);

impl RotatingFile {
    /// Открыть файл журнала. `Err` — если каталог недоступен на запись;
    /// вызывающий в этом случае остаётся с одним stderr.
    pub fn open(path: impl AsRef<Path>, max_bytes: u64, keep: usize) -> io::Result<Self> {
        let inner = Inner::open(path.as_ref().to_path_buf(), max_bytes, keep)?;
        Ok(Self(Arc::new(Mutex::new(inner))))
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Отравленный мьютекс (паника в другом потоке во время записи) не должен
        // валить ещё и этот поток — забираем данные и продолжаем.
        let mut guard = self.0.lock().unwrap_or_else(|e| e.into_inner());
        guard.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut guard = self.0.lock().unwrap_or_else(|e| e.into_inner());
        guard.flush()
    }
}

impl<'a> MakeWriter<'a> for RotatingFile {
    type Writer = RotatingFile;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

// ── Сборка фильтра уровней ──────────────────────────────────────────────────

/// Собрать строку директив для `EnvFilter` по запрошенному уровню.
///
/// Наши крейты идут на запрошенном уровне, всё остальное — на `warn`.
/// Без этого `log_level = "debug"` заливает журнал сообщениями hyper/reqwest,
/// в которых разбирать нечего.
pub fn filter_directives(level: &str) -> String {
    let lvl = level.trim().to_lowercase();
    let lvl = if lvl.is_empty() { DEFAULT_LEVEL } else { &lvl };
    format!(
        "warn,code_index_core={lvl},code_index={lvl},bsl_indexer={lvl},bsl_extension={lvl}",
        lvl = lvl
    )
}

fn build_filter(level: &str) -> EnvFilter {
    // `RUST_LOG` сильнее конфига — это привычный способ разово поднять
    // подробность, не трогая daemon.toml.
    if let Ok(env) = std::env::var("RUST_LOG") {
        if !env.trim().is_empty() {
            if let Ok(f) = EnvFilter::try_new(&env) {
                return f;
            }
        }
    }
    EnvFilter::try_new(filter_directives(level))
        .unwrap_or_else(|_| EnvFilter::new(filter_directives(DEFAULT_LEVEL)))
}

// ── Точки инициализации ─────────────────────────────────────────────────────

/// Вывод только в stderr — для коротких команд CLI (`index`, `stats`, `query`).
/// Повторный вызов безвреден: глобальный приёмник ставится один раз.
pub fn init_stderr() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(build_filter(DEFAULT_LEVEL))
        .with_writer(io::stderr)
        .try_init();
}

/// Вывод в stderr и в файл. Возвращает путь файла, если его удалось открыть;
/// `None` — журнал ведётся только в stderr (например, каталог только на чтение).
///
/// Ошибку открытия намеренно не поднимаем вверх: невозможность вести файл —
/// не повод не запускать демон.
pub fn init_with_file(path: impl AsRef<Path>, level: &str) -> Option<PathBuf> {
    let path = path.as_ref().to_path_buf();
    let writer = match RotatingFile::open(&path, MAX_LOG_BYTES, KEEP_ROTATED) {
        Ok(w) => w,
        Err(e) => {
            init_stderr();
            tracing::warn!(
                "не удалось открыть файл журнала {}: {} — вывод только в stderr",
                path.display(),
                e
            );
            return None;
        }
    };

    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(io::stderr);
    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(writer);

    let ok = tracing_subscriber::registry()
        .with(build_filter(level))
        .with(stderr_layer)
        .with(file_layer)
        .try_init()
        .is_ok();

    if ok {
        Some(path)
    } else {
        // Приёмник уже стоял (тесты, повторный вызов) — файл в этом случае
        // не подключён, и врать об этом нельзя.
        None
    }
}

// ── Память процесса — для журнала ───────────────────────────────────────────

/// Сколько оперативной памяти занимает текущий процесс, в байтах.
/// `None` — система не отдала сведений о процессе.
///
/// Замер разовый и недешёвый (опрос процесса у ОС), поэтому зовётся в
/// считаных местах: пульс раз в минуту, итог первичной индексации, итог
/// пакета изменений.
pub fn process_memory_bytes() -> Option<u64> {
    use sysinfo::{Pid, ProcessesToUpdate, System};
    let mut sys = System::new();
    let pid = Pid::from(std::process::id() as usize);
    sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), false);
    sys.process(pid).map(|p| p.memory())
}

/// Память процесса строкой для журнала: «812 МБ» либо «сведений нет».
pub fn memory_note() -> String {
    match process_memory_bytes() {
        Some(bytes) => format!("{} МБ", bytes / (1024 * 1024)),
        None => "сведений нет".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ротация_сдвигает_файлы_и_удаляет_лишние() {
        let tmp = tempfile::TempDir::new().unwrap();
        let log = tmp.path().join("daemon.log");

        // max_bytes=100, keep=2 — три записи по 60 байт дают две ротации.
        let mut w = RotatingFile::open(&log, 100, 2).unwrap();
        for _ in 0..3 {
            w.write_all(&[b'x'; 60]).unwrap();
            w.flush().unwrap();
        }

        assert!(log.exists(), "текущий файл журнала должен существовать");
        assert!(
            log.with_extension("log.1").exists(),
            "первый прежний файл должен быть создан"
        );
        assert!(
            !log.with_extension("log.3").exists(),
            "файлов сверх keep=2 быть не должно"
        );
    }

    #[test]
    fn запись_продолжается_после_ротации() {
        let tmp = tempfile::TempDir::new().unwrap();
        let log = tmp.path().join("serve.log");

        let mut w = RotatingFile::open(&log, 50, 1).unwrap();
        w.write_all(&[b'a'; 40]).unwrap();
        w.write_all("после ротации".as_bytes()).unwrap();
        w.flush().unwrap();

        let text = std::fs::read_to_string(&log).unwrap();
        assert!(
            text.contains("после ротации"),
            "новая строка должна попасть в свежий файл: {:?}",
            text
        );
    }

    #[test]
    fn открытие_дописывает_в_существующий_файл() {
        let tmp = tempfile::TempDir::new().unwrap();
        let log = tmp.path().join("daemon.log");
        std::fs::write(&log, "прежняя строка\n").unwrap();

        let mut w = RotatingFile::open(&log, MAX_LOG_BYTES, KEEP_ROTATED).unwrap();
        w.write_all("новая строка\n".as_bytes()).unwrap();
        w.flush().unwrap();

        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains("прежняя строка"), "прежнее содержимое стёрто");
        assert!(text.contains("новая строка"));
    }

    #[test]
    fn память_процесса_измеряется_и_ненулевая() {
        // Собственный процесс всегда занимает хоть сколько-то памяти —
        // если замер вернул None или ноль, сведения в журнал пойдут ложные.
        let bytes = process_memory_bytes();
        assert!(
            bytes.map(|b| b > 0).unwrap_or(false),
            "не удалось измерить память процесса: {:?}",
            bytes
        );
        assert!(memory_note().ends_with("МБ"), "{}", memory_note());
    }

    #[test]
    fn наши_крейты_на_запрошенном_уровне_чужие_на_warn() {
        let d = filter_directives("debug");
        assert!(d.starts_with("warn,"), "чужие крейты должны быть на warn: {d}");
        assert!(d.contains("code_index_core=debug"));
        assert!(d.contains("bsl_extension=debug"));

        // Пустое значение не должно давать битую директиву `code_index_core=`.
        let empty = filter_directives("  ");
        assert!(empty.contains(&format!("code_index_core={}", DEFAULT_LEVEL)));
        assert!(EnvFilter::try_new(empty).is_ok());
    }
}
