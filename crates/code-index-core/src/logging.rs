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

use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// Предельный размер файла журнала. При превышении текущий файл
/// переименовывается в `.1`, прежний `.1` — в `.2` и так далее.
///
/// Замер на уровне «debug»: пачка из 2 000 файлов пишет около 4 КБ — строк на
/// каждый файл в журнале нет, всё сведено к итогам по пачке, поэтому запас
/// здесь нужен небольшой.
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

// ── Служебный префикс длинных путей Windows ─────────────────────────────────

/// Демон приводит пути к каноническому виду, и на Windows к ним приклеивается
/// префикс длинных путей. В журнале он попадает в каждую строку с путём,
/// занимает четыре знака и читателю не нужен. Вырезаем на выходе — тогда это
/// работает разом для всех строк, включая те, что пишет надстройка, и не
/// приходится трогать полсотни мест вывода.
const WIN_EXT_PREFIX: &[u8] = br"\\?\";

struct StripExtPrefix<W>(W);

impl<'a, W: MakeWriter<'a>> MakeWriter<'a> for StripExtPrefix<W> {
    type Writer = StripExtPrefixWriter<W::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        StripExtPrefixWriter(self.0.make_writer())
    }
}

struct StripExtPrefixWriter<W>(W);

impl<W: Write> Write for StripExtPrefixWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Приёмник отдаёт событие одним куском, поэтому префикс не может
        // оказаться разрезанным между двумя вызовами.
        match without_ext_prefix(buf) {
            Some(clean) => self.0.write_all(&clean)?,
            None => self.0.write_all(buf)?,
        }
        // Отчитываемся за весь исходный буфер: вызывающая сторона считает
        // байты до вырезания, и «записано меньше» она поймёт как сбой.
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// `None` — префикса в буфере нет, значит писать можно как есть, без копии.
fn without_ext_prefix(buf: &[u8]) -> Option<Vec<u8>> {
    if !buf.windows(WIN_EXT_PREFIX.len()).any(|w| w == WIN_EXT_PREFIX) {
        return None;
    }
    let mut out = Vec::with_capacity(buf.len());
    let mut i = 0;
    while i < buf.len() {
        if buf[i..].starts_with(WIN_EXT_PREFIX) {
            i += WIN_EXT_PREFIX.len();
        } else {
            out.push(buf[i]);
            i += 1;
        }
    }
    Some(out)
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
        "warn,code_index_core={lvl},code_index={lvl},bsl_indexer={lvl},bsl_extension={lvl},\
         {summary}=info",
        lvl = lvl,
        summary = SUMMARY_TARGET
    )
}

/// Метка строк короткой сводки по итогам индексации. Своя метка нужна затем,
/// чтобы сводка печаталась при ЛЮБОЙ настройке подробности: она отвечает на
/// главный вопрос «сколько заняло и что получилось», и терять её при
/// `log_level = "error"` нельзя. Остальные строки при этом отсекаются как
/// обычно.
pub const SUMMARY_TARGET: &str = "итог";

/// Печатать ли имя модуля, из которого вышла строка. На обычной работе оно
/// занимает треть строки и читателю журнала не говорит ничего: и так видно,
/// про какую папку и какую работу речь. При отладке — наоборот, по нему сразу
/// открывают нужное место в коде, поэтому там оставляем.
fn show_target(level: &str) -> bool {
    // `RUST_LOG` сильнее конфига — здесь так же, как при сборке фильтра.
    let effective = std::env::var("RUST_LOG")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| level.to_string());
    level_is_verbose(&effective)
}

/// Отладочный ли это уровень. Отдельной функцией, чтобы проверять без оглядки
/// на переменные окружения прогона тестов.
fn level_is_verbose(level: &str) -> bool {
    let lvl = level.to_lowercase();
    lvl.contains("debug") || lvl.contains("trace")
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

// ── Отметка времени ─────────────────────────────────────────────────────────

/// Время в журнале — по часам машины, а не UTC. Приёмник по умолчанию печатает
/// UTC с суффиксом `Z`, и такой журнал невозможно сопоставить ни с временем
/// правки файлов, ни с журналами других служб, ни с тем, что пользователь
/// видит на часах: приходится в уме прибавлять смещение. Смещение печатается
/// рядом со временем — тогда присланный файл читается однозначно и с другой
/// машины.
struct LocalTime;

impl FormatTime for LocalTime {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        write!(w, "{}", local_timestamp())
    }
}

/// Собственно отметка. Вынесено отдельной функцией ради модульного теста.
fn local_timestamp() -> String {
    chrono::Local::now()
        .format("%Y-%m-%d %H:%M:%S%.3f%:z")
        .to_string()
}

/// Только время, по часам машины: «14:02:10». Для сводки, где дата уже стоит
/// в начале каждой строки журнала и повторять её незачем.
pub fn local_hms() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

// ── Точки инициализации ─────────────────────────────────────────────────────

/// Вывод только в stderr — для коротких команд CLI (`index`, `stats`, `query`).
/// Повторный вызов безвреден: глобальный приёмник ставится один раз.
pub fn init_stderr() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(build_filter(DEFAULT_LEVEL))
        .with_timer(LocalTime)
        .with_target(show_target(DEFAULT_LEVEL))
        .with_writer(StripExtPrefix(io::stderr))
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

    // Тот же файл понадобится для разделителя между операциями: его пишем
    // напрямую, без метки времени и уровня.
    let _ = log_sink().set(writer.clone());

    let with_target = show_target(level);
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_timer(LocalTime)
        .with_target(with_target)
        .with_writer(StripExtPrefix(io::stderr));
    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_timer(LocalTime)
        .with_target(with_target)
        .with_writer(StripExtPrefix(writer));

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

/// Черта перед началом очередной работы с папкой: первичной индексации,
/// проверки при старте, обработки пачки изменений.
///
/// Журнал сплошным списком читается тяжело — по нему не видно, где кончилась
/// прошлая операция и началась следующая.
///
/// Пишется в файл напрямую, минуя приёмник `tracing`: тому положено ставить
/// перед каждой строкой метку времени, уровень и метку цели, а на разделителе
/// они только мешают — черта перестаёт быть чертой. Файл тот же и мьютекс тот
/// же, поэтому порядок строк не нарушается.
pub fn block_separator() {
    let line = format!("{}\n", "─".repeat(78));
    match log_sink().get() {
        Some(sink) => {
            let mut sink = sink.clone();
            let _ = sink.write_all(line.as_bytes());
            let _ = sink.flush();
        }
        // Файл не ведётся (короткие команды CLI) — остаётся поток ошибок.
        None => eprint!("{}", line),
    }
}

/// Файл журнала для прямой записи разделителя. Ставится один раз при запуске
/// демона, вместе с приёмником `tracing`.
fn log_sink() -> &'static std::sync::OnceLock<RotatingFile> {
    static SINK: std::sync::OnceLock<RotatingFile> = std::sync::OnceLock::new();
    &SINK
}

/// Отсечка «пора отчитаться»: не чаще раза в заданный срок.
///
/// Прогресс раньше печатался каждые N файлов, где N — размер транзакции
/// записи. Шаг в файлах для наблюдения не годится: на лёгких файлах строки
/// сыплются без пользы, на тяжёлых между ними проходит сколько угодно
/// времени, а на репозитории меньше N файлов не появляется ни одной. Отсечка
/// по времени отвечает ровно на тот вопрос, ради которого прогресс и читают:
/// движение есть или встало.
pub struct Heartbeat {
    last: std::time::Instant,
    every: std::time::Duration,
}

impl Heartbeat {
    pub fn every_secs(secs: u64) -> Self {
        Self {
            last: std::time::Instant::now(),
            every: std::time::Duration::from_secs(secs),
        }
    }

    /// `true` — с прошлого отчёта прошло достаточно; отсечка сдвигается.
    pub fn due(&mut self) -> bool {
        if self.last.elapsed() >= self.every {
            self.last = std::time::Instant::now();
            true
        } else {
            false
        }
    }
}

/// Паспорт машины строкой для журнала: ядра, оперативная память, система.
///
/// Без него присланный журнал нечитаем: времена есть, а соотнести их не с чем
/// — непонятно, медленно из-за слабой машины или из-за нашей недоработки.
pub fn machine_note() -> String {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get().to_string())
        .unwrap_or_else(|_| "?".to_string());
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    let total_gb = sys.total_memory() as f64 / (1024.0 * 1024.0 * 1024.0);
    let avail_gb = sys.available_memory() as f64 / (1024.0 * 1024.0 * 1024.0);
    format!(
        "{} на {}, ядер {}, оперативной памяти {:.1} ГБ (свободно {:.1} ГБ)",
        sysinfo::System::name().unwrap_or_else(|| "система неизвестна".to_string()),
        sysinfo::System::kernel_version().unwrap_or_else(|| "?".to_string()),
        cores,
        total_gb,
        avail_gb
    )
}

/// Объём в байтах человеку: «812 МБ», «4,0 ГБ». Гигабайты с одним знаком —
/// в решениях о памяти важен порядок, а не точность до мегабайта.
pub fn human_bytes(bytes: u64) -> String {
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * MB;
    if bytes >= GB {
        let gb = bytes as f64 / GB as f64;
        format!("{:.1} ГБ", gb).replace('.', ",")
    } else if bytes >= MB {
        format!("{} МБ", bytes / MB)
    } else {
        format!("{} КБ", bytes / 1024)
    }
}

/// Память процесса строкой для журнала: «812 МБ» либо «сведений нет».
pub fn memory_note() -> String {
    match process_memory_bytes() {
        Some(bytes) => format!("{} МБ", bytes / (1024 * 1024)),
        None => "сведений нет".to_string(),
    }
}

// ── Длительности ────────────────────────────────────────────────────────────

/// Человекочитаемая длительность в секундах: журнал читает человек, а не
/// машина — «3 ч 12 мин» разбирается с одного взгляда, 11532 секунды нет.
pub fn human_duration(sec: u64) -> String {
    if sec < 60 {
        format!("{} с", sec)
    } else if sec < 3600 {
        format!("{} мин {} с", sec / 60, sec % 60)
    } else {
        format!("{} ч {} мин", sec / 3600, (sec % 3600) / 60)
    }
}

/// То же для миллисекунд. Этапы различаются на три порядка — от долей секунды
/// до минут, — поэтому короткие показываем подробнее: «832 мс», «5,3 с»,
/// «1 мин 54 с».
pub fn human_ms(ms: u128) -> String {
    if ms < 1000 {
        format!("{} мс", ms)
    } else if ms < 60_000 {
        format!("{},{} с", ms / 1000, (ms % 1000) / 100)
    } else {
        human_duration((ms / 1000) as u64)
    }
}

/// Длительность этапа. Отдельно от `human_ms` разбирается случай меньше
/// миллисекунды: при частичной индексации такие этапы обычны, и «0 мс» не
/// отличалось бы от «этап не выполнялся».
pub fn human_dur(dur: std::time::Duration) -> String {
    let us = dur.as_micros();
    if us < 1000 {
        format!("0,{} мс", us / 100)
    } else {
        human_ms(dur.as_millis())
    }
}

/// Число со словом в нужном падеже: «1 файл», «3 файла», «57072 файла»,
/// «17869 объектов». Без этого в журнале получаются «23623 рёбер» и «2 файлов»,
/// на которых глаз спотыкается.
pub fn plural(n: u64, one: &str, few: &str, many: &str) -> String {
    let last_two = n % 100;
    let last = n % 10;
    let form = if (11..=14).contains(&last_two) {
        many
    } else if last == 1 {
        one
    } else if (2..=4).contains(&last) {
        few
    } else {
        many
    };
    format!("{} {}", n, form)
}

// ── Времена этапов для итоговой строки ──────────────────────────────────────
//
// Этапы отмечаются там, где выполняются (ядро индексатора, надстройка
// процессора), а печатает их одной короткой строкой тот, кто подводит итог по
// папке. Прокидывать времена через возвращаемые значения пришлось бы через
// границы трёх крейтов и трёх сигнатур — накопитель дешевле.
//
// Накопитель привязан к потоку: воркер одной папки работает в своём потоке
// (`spawn_blocking`), поэтому этапы соседних папок, индексируемых
// одновременно, не смешиваются.

/// Один завершённый этап: как называется, сколько занял и что после себя
/// оставил («57072 файла», «23623 ребра»). Итог этапа заполняет тот, кто его
/// выполняет, — только он знает, что считать.
///
/// Длительность хранится как есть, а не в целых миллисекундах: при частичной
/// индексации этапы идут доли миллисекунды, и округление превратило бы всю
/// раскладку в нули.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stage {
    pub name: &'static str,
    pub dur: std::time::Duration,
    pub detail: Option<String>,
}

thread_local! {
    static STAGES: std::cell::RefCell<Vec<Stage>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// Итог текущего этапа: его сообщают по ходу работы, а имя и длительность
    /// становятся известны только при завершении этапа.
    static PENDING: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// Чем заняты рабочие потоки прямо сейчас: поток → (имя этапа, когда начат).
///
/// Накопитель этапов выше — `thread_local`, и увидеть его из чужого потока
/// нельзя. А строку состояния демона печатает отдельная задача, которой нужно
/// ответить на вопрос «чем занята папка прямо сейчас». Отсюда общий на процесс
/// реестр: рабочий поток отмечает в нём начатый этап, задача состояния читает.
fn running_stages() -> &'static Mutex<std::collections::HashMap<std::thread::ThreadId, (String, std::time::Instant)>> {
    static RUNNING: std::sync::OnceLock<
        Mutex<std::collections::HashMap<std::thread::ThreadId, (String, std::time::Instant)>>,
    > = std::sync::OnceLock::new();
    RUNNING.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Отметить, что текущий поток начал этап с таким названием. Парная к
/// `stage_done`/`stage_idle`, которые отметку снимают.
pub fn stage_begin(name: impl Into<String>) {
    if let Ok(mut map) = running_stages().lock() {
        map.insert(
            std::thread::current().id(),
            (name.into(), std::time::Instant::now()),
        );
    }
}

/// Снять отметку о текущем этапе потока — работа закончена, показывать нечего.
pub fn stage_idle() {
    if let Ok(mut map) = running_stages().lock() {
        map.remove(&std::thread::current().id());
    }
}

/// Чем занят поток и сколько секунд он этим занят. `None` — между этапами или
/// когда поток отработал. Для строки состояния демона.
pub fn stage_running(thread: std::thread::ThreadId) -> Option<(String, u64)> {
    running_stages()
        .lock()
        .ok()
        .and_then(|map| map.get(&thread).map(|(name, at)| (name.clone(), at.elapsed().as_secs())))
}

/// Забыть накопленное — перед началом очередного набора этапов.
pub fn stages_reset() {
    STAGES.with(|s| s.borrow_mut().clear());
    PENDING.with(|p| *p.borrow_mut() = None);
    stage_idle();
}

/// Сообщить, что сделал текущий этап: «57072 файла», «17869 объектов».
/// Прицепится к ближайшему `stage_done`.
pub fn stage_detail(detail: impl Into<String>) {
    PENDING.with(|p| *p.borrow_mut() = Some(detail.into()));
}

/// Отметить завершённый этап и сколько он занял.
pub fn stage_done(name: &'static str, dur: std::time::Duration) {
    let detail = PENDING.with(|p| p.borrow_mut().take());
    STAGES.with(|s| s.borrow_mut().push(Stage { name, dur, detail }));
    // Этап кончился: до начала следующего показывать в состоянии демона нечего.
    stage_idle();
}

/// Прибавить время к этапу с таким именем, заведя его, если ещё не было.
/// Нужен там, где этап складывается из множества мелких шагов: при частичной
/// индексации те же слои обновляются пофайлово, и без накопления получилось бы
/// по этапу на каждый файл.
pub fn stage_add(name: &'static str, dur: std::time::Duration) {
    STAGES.with(|s| {
        let mut stages = s.borrow_mut();
        match stages.iter_mut().find(|st| st.name == name) {
            Some(st) => st.dur += dur,
            None => stages.push(Stage { name, dur, detail: None }),
        }
    });
}

/// Проставить итог этапу, набранному по частям: сколько всего получилось.
pub fn stage_set_detail(name: &'static str, detail: impl Into<String>) {
    STAGES.with(|s| {
        if let Some(st) = s.borrow_mut().iter_mut().find(|st| st.name == name) {
            st.detail = Some(detail.into());
        }
    });
}

/// Забрать накопленное, очистив накопитель под следующий набор.
pub fn stages_take() -> Vec<Stage> {
    PENDING.with(|p| *p.borrow_mut() = None);
    STAGES.with(|s| std::mem::take(&mut *s.borrow_mut()))
}

/// Раскладка по этапам — по строке на этап, столбцами и с нумерацией:
///
/// ```text
/// этап 1  обход дерева и отбор изменившихся  57072 файла     3,0 с
/// этап 2  разбор в несколько потоков         57072 файла     5,8 с
/// ```
///
/// Порядок — как выполнялись: блок читают сверху вниз как ход работы, а самый
/// дорогой этап и так виден по столбцу времени. Столбцы выравниваются по
/// самому длинному значению, иначе числа не сопоставить глазом.
pub fn stages_block(stages: &[Stage]) -> Vec<String> {
    let name_w = stages.iter().map(|s| s.name.chars().count()).max().unwrap_or(0);
    let detail_w = stages
        .iter()
        .map(|s| s.detail.as_deref().unwrap_or("").chars().count())
        .max()
        .unwrap_or(0);
    stages
        .iter()
        .enumerate()
        .map(|(i, s)| {
            format!(
                "  этап {:<2} {:<name_w$}   {:<detail_w$}   {}",
                i + 1,
                s.name,
                s.detail.as_deref().unwrap_or(""),
                human_dur(s.dur),
                name_w = name_w,
                detail_w = detail_w,
            )
        })
        .collect()
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
    fn длительность_переводится_в_человеческий_вид() {
        assert_eq!(human_duration(42), "42 с");
        assert_eq!(human_duration(125), "2 мин 5 с");
        assert_eq!(human_duration(7300), "2 ч 1 мин");
        // Этапы бывают короче секунды — тогда миллисекунды, иначе «0 с»
        // не отличить от пропущенного этапа.
        assert_eq!(human_ms(832), "832 мс");
        assert_eq!(human_ms(5319), "5,3 с");
        assert_eq!(human_ms(46986), "46,9 с");
        assert_eq!(human_ms(114351), "1 мин 54 с");
    }

    #[test]
    fn этапы_копятся_с_итогами_и_печатаются_блоком() {
        stages_reset();
        assert!(stages_block(&stages_take()).is_empty(), "пусто — печатать нечего");

        let ms = std::time::Duration::from_millis;
        stage_detail("57072 файла");
        stage_done("обход", ms(5319));
        stage_done("индексы", ms(14226)); // свой итог не сообщал
        stage_detail("23623 ребра");
        stage_done("связи данных", ms(1800));

        let stages = stages_take();
        assert_eq!(stages.len(), 3);
        // Итог прицепляется к своему этапу и не протекает на следующий.
        assert_eq!(stages[0].detail.as_deref(), Some("57072 файла"));
        assert_eq!(stages[1].detail, None);
        assert_eq!(stages[2].detail.as_deref(), Some("23623 ребра"));

        let block = stages_block(&stages);
        assert_eq!(block.len(), 3);
        assert!(block[0].starts_with("  этап 1  обход"), "{}", block[0]);
        assert!(block[0].ends_with("57072 файла   5,3 с"), "{}", block[0]);
        assert!(block[2].starts_with("  этап 3  связи данных"), "{}", block[2]);

        // Забрали — накопитель пуст, следующий набор не смешается с прошлым.
        assert!(stages_take().is_empty());
    }

    #[test]
    fn этап_из_множества_мелких_шагов_копится_по_имени() {
        stages_reset();
        // Пофайловый цикл частичной индексации: доли миллисекунды на файл.
        for _ in 0..3 {
            stage_add("граф вызовов", std::time::Duration::from_micros(400));
        }
        stage_set_detail("граф вызовов", "3 файла");

        let stages = stages_take();
        assert_eq!(stages.len(), 1, "три шага дают ОДИН этап");
        assert_eq!(stages[0].dur, std::time::Duration::from_micros(1200));
        assert_eq!(stages[0].detail.as_deref(), Some("3 файла"));

        // Меньше миллисекунды показываем долями, иначе не отличить от
        // «этап не выполнялся».
        assert_eq!(human_dur(std::time::Duration::from_micros(400)), "0,4 мс");
        assert_eq!(human_dur(std::time::Duration::from_millis(24)), "24 мс");
    }

    #[test]
    fn число_со_словом_склоняется() {
        assert_eq!(plural(1, "файл", "файла", "файлов"), "1 файл");
        assert_eq!(plural(3, "файл", "файла", "файлов"), "3 файла");
        assert_eq!(plural(7, "файл", "файла", "файлов"), "7 файлов");
        // Одиннадцать-четырнадцать — исключение, у них форма как у многих.
        assert_eq!(plural(11, "файл", "файла", "файлов"), "11 файлов");
        assert_eq!(plural(13, "файл", "файла", "файлов"), "13 файлов");
        assert_eq!(plural(21, "файл", "файла", "файлов"), "21 файл");
        assert_eq!(plural(57072, "файл", "файла", "файлов"), "57072 файла");
        assert_eq!(plural(23623, "ребро", "ребра", "рёбер"), "23623 ребра");
        assert_eq!(plural(17869, "объект", "объекта", "объектов"), "17869 объектов");
    }

    #[test]
    fn служебный_префикс_путей_вырезается_из_вывода() {
        let буфер = r"[\\?\C:\Repo\ut-a] начата первичная индексация".as_bytes();
        let чисто = without_ext_prefix(буфер).expect("префикс есть — нужна чистка");
        assert_eq!(
            String::from_utf8(чисто).unwrap(),
            r"[C:\Repo\ut-a] начата первичная индексация"
        );
        // Строка без префикса копироваться не должна — это обычный случай.
        assert!(without_ext_prefix("[stand] обычная строка".as_bytes()).is_none());
    }

    #[test]
    fn имя_модуля_печатается_только_при_отладке() {
        assert!(!level_is_verbose("info"), "на обычной работе адрес в коде не нужен");
        assert!(!level_is_verbose("warn"));
        assert!(level_is_verbose("debug"), "при отладке по нему открывают код");
        assert!(level_is_verbose("trace"));
        // Так уровень приходит из переменной окружения — набором директив.
        assert!(level_is_verbose("warn,code_index_core=debug"));
    }

    #[test]
    fn отметка_времени_по_часам_машины_а_не_utc() {
        let stamp = local_timestamp();
        // Формат «2026-08-20 13:06:45.902+03:00»: без суффикса Z и без
        // разделителя T — это и отличает местное время от UTC в журнале.
        assert!(!stamp.ends_with('Z'), "отметка не должна быть в UTC: {}", stamp);
        assert!(!stamp.contains('T'), "разделитель T не нужен: {}", stamp);

        // Сверяем с местным временем до и после замера — иначе тест мигал бы
        // раз в минуту, попадая на смену минуты между двумя вызовами часов.
        let now = chrono::Local::now();
        let minute_now = now.format("%Y-%m-%d %H:%M").to_string();
        let minute_before = (now - chrono::Duration::minutes(1))
            .format("%Y-%m-%d %H:%M")
            .to_string();
        assert!(
            stamp.starts_with(&minute_now) || stamp.starts_with(&minute_before),
            "отметка {} должна совпадать с местным временем {}",
            stamp,
            now
        );
        // Смещение печатается рядом, иначе присланный файл не прочитать
        // с другой машины.
        assert!(
            stamp.ends_with(&now.format("%:z").to_string()),
            "в отметке должно быть смещение часового пояса: {}",
            stamp
        );
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
