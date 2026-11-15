//! Общая инфраструктура ограниченного поиска (#31/#32/#35).
//!
//! Здесь живёт то, что нужно НЕСКОЛЬКИМ поисковым проходам сразу, а не одному
//! инструменту:
//!
//! - [`SearchBudget`] / [`SearchUsage`] — единый счётчик просмотренных файлов,
//!   сжатых и распакованных байт, найденных строк и причина остановки. Бюджет
//!   резервируется ДО параллельной обработки очередной порции, поэтому
//!   перерасход не зависит от того, как планировщик разложил работу;
//! - [`read_indexed_header`] / [`detect_derived_header`] — чтение начального
//!   блока комментариев ТОЛЬКО из индекса (`file_contents`/`text_contents`),
//!   без диска и без миграции. Признак «файл производный» ставится по явной
//!   фразе в шапке, а не по имени файла, и потому не даёт ложных срабатываний
//!   на `derived` в середине пути или на строковом литерале в коде.

use std::collections::BTreeMap;
use std::io::Read;

use rusqlite::OptionalExtension;

// ── Бюджет прохода ──────────────────────────────────────────────────────────

/// Бюджет одного поискового прохода.
///
/// Единица каждого поля — то, что реально измеряется счётчиком, а не оценка:
/// `files` — просмотренные файлы, `compressed_bytes` — сумма длин blob'ов,
/// прочитанных из индекса, `uncompressed_bytes` — сколько из них разжали,
/// `max_matches` — найденные строки.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SearchBudget {
    /// Сколько файлов разрешено просмотреть.
    pub max_files: usize,
    /// Потолок суммы сжатых байт.
    pub max_compressed_bytes: usize,
    /// Потолок суммы распакованных байт.
    pub max_uncompressed_bytes: usize,
    /// Потолок найденных строк.
    pub max_matches: usize,
}

impl SearchBudget {
    /// Без потолков — только для вызовов, где предел задаёт вызывающий слой.
    pub const UNLIMITED: SearchBudget = SearchBudget {
        max_files: usize::MAX,
        max_compressed_bytes: usize::MAX,
        max_uncompressed_bytes: usize::MAX,
        max_matches: usize::MAX,
    };

    pub const fn new(
        max_files: usize,
        max_compressed_bytes: usize,
        max_uncompressed_bytes: usize,
        max_matches: usize,
    ) -> Self {
        Self {
            max_files,
            max_compressed_bytes,
            max_uncompressed_bytes,
            max_matches,
        }
    }

    /// Дополнительный проход по ПРОТИВОПОЛОЖНОЙ категории (#31): 256 файлов,
    /// 8 MiB сжатых, 8 MiB распакованных данных, 20 найденных строк.
    pub const fn other_category() -> Self {
        Self::new(256, 8 * 1024 * 1024, 8 * 1024 * 1024, 20)
    }

    /// Общий бюджет fan-out по репозиториям (#32), включая дополнительный
    /// проход #31: 4096 файлов, 64 MiB сжатых и 64 MiB распакованных данных.
    pub const fn multi_repo() -> Self {
        Self::new(4096, 64 * 1024 * 1024, 64 * 1024 * 1024, usize::MAX)
    }

    /// Проверка шапок на ОДИН запрос (#35): 100 путей, 256 KiB текста.
    pub const fn header_probe() -> Self {
        Self::new(
            HEADER_PROBE_MAX_PATHS,
            HEADER_PROBE_MAX_BYTES,
            HEADER_PROBE_MAX_BYTES,
            usize::MAX,
        )
    }
}

/// Почему проход остановился раньше полного обхода.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SearchStop {
    /// Исчерпан потолок файлов.
    Files,
    /// Исчерпан потолок сжатых байт.
    CompressedBytes,
    /// Исчерпан потолок распакованных байт.
    UncompressedBytes,
    /// Исчерпан потолок найденных строк.
    Matches,
    /// Проход отменён (проверка отмены между порциями).
    Cancelled,
    ResponseBytes,
    RemoteUsageUnknown,
}

impl SearchStop {
    pub fn as_str(self) -> &'static str {
        match self {
            SearchStop::Files => "files",
            SearchStop::CompressedBytes => "compressed_bytes",
            SearchStop::UncompressedBytes => "uncompressed_bytes",
            SearchStop::Matches => "matches",
            SearchStop::Cancelled => "cancelled",
            SearchStop::ResponseBytes => "response_bytes",
            SearchStop::RemoteUsageUnknown => "remote_usage_unknown",
        }
    }
}

/// Фактически израсходованный бюджет прохода.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct SearchUsage {
    /// Сколько файлов просмотрено.
    pub files: usize,
    /// Сколько сжатых байт прочитано.
    pub compressed_bytes: usize,
    /// Сколько байт распаковано.
    pub uncompressed_bytes: usize,
    /// Сколько строк найдено.
    pub matches: usize,
    /// Причина досрочной остановки (`None` — обход дошёл до конца).
    pub stopped: Option<SearchStop>,
    pub unreadable: usize,
    pub oversize: usize,
    pub dependent_files: Vec<String>,
}

impl SearchUsage {
    pub fn new() -> Self {
        Self::default()
    }

    /// Обход состоялся полностью (никакого досрочного выхода не было)?
    pub fn complete(&self) -> bool {
        self.stopped.is_none() && self.unreadable == 0 && self.oversize == 0
    }

    /// Причина неполноты для ответа инструмента.
    pub fn reason(&self) -> Option<&'static str> {
        self.stopped.map(SearchStop::as_str)
    }

    /// Есть ли запас на очередной файл.
    ///
    /// Проверяется ДО порции: резерв идёт по худшему случаю (файл целиком),
    /// поэтому параллельная обработка порции не выводит счётчики за предел.
    pub fn allows_file(&self, budget: &SearchBudget) -> bool {
        self.stopped.is_none()
            && self.files < budget.max_files
            && self.compressed_bytes < budget.max_compressed_bytes
            && self.uncompressed_bytes < budget.max_uncompressed_bytes
    }

    /// Учесть просмотренный файл. `uncompressed` — сколько реально разжали
    /// (для шапки это префикс, а не весь файл).
    ///
    /// `false` — потолок достигнут, проход обязан остановиться.
    pub fn charge_file(
        &mut self,
        budget: &SearchBudget,
        compressed: usize,
        uncompressed: usize,
    ) -> bool {
        let stop = if self.files >= budget.max_files {
            Some(SearchStop::Files)
        } else if compressed
            > budget
                .max_compressed_bytes
                .saturating_sub(self.compressed_bytes)
        {
            Some(SearchStop::CompressedBytes)
        } else if uncompressed
            > budget
                .max_uncompressed_bytes
                .saturating_sub(self.uncompressed_bytes)
        {
            Some(SearchStop::UncompressedBytes)
        } else {
            None
        };
        if let Some(stop) = stop {
            self.stopped = Some(stop);
            return false;
        }
        self.files += 1;
        self.compressed_bytes += compressed;
        self.uncompressed_bytes += uncompressed;
        self.stopped.is_none()
    }

    /// Учесть найденные строки. `false` — потолок достигнут.
    pub fn charge_matches(&mut self, budget: &SearchBudget, count: usize) -> bool {
        self.matches = self.matches.saturating_add(count);
        if self.matches > budget.max_matches {
            self.matches = budget.max_matches;
            self.stopped = Some(SearchStop::Matches);
        }
        self.complete()
    }

    /// Сколько строк ещё можно добрать до потолка.
    pub fn remaining_matches(&self, budget: &SearchBudget) -> usize {
        budget.max_matches.saturating_sub(self.matches)
    }

    /// Отметить отмену: проход прекращается между порциями, уже собранное
    /// остаётся в ответе, но полным он не объявляется.
    pub fn cancel(&mut self) {
        self.stopped = Some(SearchStop::Cancelled);
    }

    /// JSON-вид для ответа инструмента.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "files": self.files,
            "compressed_bytes": self.compressed_bytes,
            "uncompressed_bytes": self.uncompressed_bytes,
            "matches": self.matches,
            "complete": self.complete(),
            "stop_reason": self.reason(),
            "unreadable": self.unreadable,
            "oversize": self.oversize,
        })
    }
}

struct SearchScope {
    budget: SearchBudget,
    usage: SearchUsage,
    header_usage: SearchUsage,
    header_budget: SearchBudget,
    deadline: std::time::Instant,
    limit: Option<usize>,
    response_bytes: usize,
}

tokio::task_local! {
    static SEARCH_SCOPE: std::cell::RefCell<SearchScope>;
}

pub async fn search_scope<F: std::future::Future>(
    budget: SearchBudget,
    timeout: std::time::Duration,
    limit: usize,
    future: F,
) -> (F::Output, SearchUsage) {
    SEARCH_SCOPE
        .scope(
            std::cell::RefCell::new(SearchScope {
                budget,
                usage: SearchUsage::default(),
                header_usage: SearchUsage::default(),
                header_budget: SearchBudget::header_probe(),
                deadline: std::time::Instant::now() + timeout,
                limit: Some(limit),
                response_bytes: usize::MAX,
            }),
            async {
                let result = future.await;
                let usage = SEARCH_SCOPE.with(|s| s.borrow().usage.clone());
                (result, usage)
            },
        )
        .await
}

pub async fn single_search_scope<F: std::future::Future>(
    timeout: Option<std::time::Duration>,
    future: F,
) -> F::Output {
    let timeout = timeout.unwrap_or(std::time::Duration::from_secs(365 * 24 * 3600));
    let (result, _) = search_scope(SearchBudget::UNLIMITED, timeout, usize::MAX, async {
        let _ = SEARCH_SCOPE.try_with(|s| s.borrow_mut().limit = None);
        future.await
    })
    .await;
    result
}

pub fn set_response_bytes(bytes: usize) {
    let _ = SEARCH_SCOPE.try_with(|s| s.borrow_mut().response_bytes = bytes);
}

pub fn search_response_bytes(default: usize) -> usize {
    SEARCH_SCOPE
        .try_with(|s| s.borrow().response_bytes.min(default))
        .unwrap_or(default)
}

pub fn set_search_limit(limit: usize) {
    let _ = SEARCH_SCOPE.try_with(|s| s.borrow_mut().limit = Some(limit));
}

pub fn limit_header_budget(budget: SearchBudget) {
    let _ = SEARCH_SCOPE.try_with(|s| s.borrow_mut().header_budget = budget);
}

pub fn remaining_header_budget() -> SearchBudget {
    SEARCH_SCOPE
        .try_with(|s| {
            let s = s.borrow();
            SearchBudget::new(
                s.header_budget
                    .max_files
                    .saturating_sub(s.header_usage.files),
                s.header_budget
                    .max_compressed_bytes
                    .saturating_sub(s.header_usage.compressed_bytes),
                s.header_budget
                    .max_uncompressed_bytes
                    .saturating_sub(s.header_usage.uncompressed_bytes),
                usize::MAX,
            )
        })
        .unwrap_or_else(|_| SearchBudget::header_probe())
}

pub fn record_header_usage(files: usize, compressed: usize, decoded: usize) {
    let _ = SEARCH_SCOPE.try_with(|s| {
        let mut s = s.borrow_mut();
        s.header_usage.files += files;
        s.header_usage.compressed_bytes += compressed;
        s.header_usage.uncompressed_bytes += decoded;
    });
}

pub fn search_limit() -> Option<usize> {
    SEARCH_SCOPE.try_with(|s| s.borrow().limit).ok().flatten()
}

pub fn remaining_timeout() -> std::time::Duration {
    SEARCH_SCOPE
        .try_with(|s| {
            s.borrow()
                .deadline
                .saturating_duration_since(std::time::Instant::now())
        })
        .unwrap_or(std::time::Duration::from_secs(30))
}

#[cfg(test)]
pub(crate) fn expire_search_deadline_for_test() {
    let _ = SEARCH_SCOPE.try_with(|scope| {
        scope.borrow_mut().deadline = std::time::Instant::now();
    });
}

pub fn deadline_expired() -> bool {
    SEARCH_SCOPE
        .try_with(|s| std::time::Instant::now() >= s.borrow().deadline)
        .unwrap_or(false)
}

pub fn remaining_budget(requested: SearchBudget) -> SearchBudget {
    SEARCH_SCOPE
        .try_with(|s| {
            let s = s.borrow();
            if s.usage.stopped == Some(SearchStop::RemoteUsageUnknown) {
                return SearchBudget::new(0, 0, 0, requested.max_matches);
            }
            SearchBudget::new(
                requested
                    .max_files
                    .min(s.budget.max_files.saturating_sub(s.usage.files)),
                requested.max_compressed_bytes.min(
                    s.budget
                        .max_compressed_bytes
                        .saturating_sub(s.usage.compressed_bytes),
                ),
                requested.max_uncompressed_bytes.min(
                    s.budget
                        .max_uncompressed_bytes
                        .saturating_sub(s.usage.uncompressed_bytes),
                ),
                requested.max_matches,
            )
        })
        .unwrap_or(requested)
}

pub fn mark_remote_usage_unknown() {
    let _ = SEARCH_SCOPE
        .try_with(|s| s.borrow_mut().usage.stopped = Some(SearchStop::RemoteUsageUnknown));
}

pub fn current_usage() -> SearchUsage {
    SEARCH_SCOPE
        .try_with(|s| s.borrow().usage.clone())
        .unwrap_or_default()
}

pub fn current_header_usage() -> SearchUsage {
    SEARCH_SCOPE
        .try_with(|s| s.borrow().header_usage.clone())
        .unwrap_or_default()
}

pub fn record_usage(usage: &SearchUsage) {
    let _ = SEARCH_SCOPE.try_with(|s| {
        let mut s = s.borrow_mut();
        s.usage.files += usage.files;
        s.usage.compressed_bytes += usage.compressed_bytes;
        s.usage.uncompressed_bytes += usage.uncompressed_bytes;
        s.usage.matches += usage.matches;
        s.usage.unreadable += usage.unreadable;
        s.usage.oversize += usage.oversize;
        s.usage.stopped = usage.stopped.or(s.usage.stopped);
        s.usage
            .dependent_files
            .extend(usage.dependent_files.iter().cloned());
    });
}

// ── Чтение шапки из индекса (#35) ───────────────────────────────────────────

/// Предел шапки: не больше 20 физических строк.
pub const HEADER_MAX_LINES: usize = 20;
/// Предел шапки: не больше 4 KiB распакованного текста.
pub const HEADER_MAX_BYTES: usize = 4 * 1024;
/// Предел сжатого префикса на файл — больше не читаем даже из индекса.
pub const HEADER_MAX_COMPRESSED_BYTES: usize = 64 * 1024;
/// Предел числа путей, проверяемых шапками на один запрос.
pub const HEADER_PROBE_MAX_PATHS: usize = 100;
/// Предел сжатого/распакованного текста на один запрос проверки шапок.
pub const HEADER_PROBE_MAX_BYTES: usize = 256 * 1024;

/// Явные фразы, которыми файл объявляет себя производным. Сравнение —
/// Unicode-регистронезависимое (строка шапки приводится к нижнему регистру).
///
/// Одиночного слова `derived` здесь нет намеренно: оно встречается в
/// обыденных именах (`derived_kind`) и признаком производности не является.
const DERIVED_PHRASES: &[&str] = &[
    "не редактировать",
    "do not edit",
    "auto-generated",
    "generated file",
    "derived file",
    "автоизвлечение",
];

/// Категория индексированного содержимого: код (`file_contents`) либо текст
/// (`text_contents`). Других источников у проверки шапки нет — диск не читается.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexedCategory {
    Code,
    Text,
}

impl IndexedCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            IndexedCategory::Code => "code",
            IndexedCategory::Text => "text",
        }
    }

    /// Таблица сжатого содержимого этой категории.
    fn table(self) -> &'static str {
        match self {
            IndexedCategory::Code => "file_contents",
            IndexedCategory::Text => "text_contents",
        }
    }
}

/// Подтверждённый признак производного файла.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedHeader {
    /// Физическая строка (1-based) шапки, где стоит фраза.
    pub header_line: usize,
    /// Распознанная фраза в нижнем регистре.
    pub phrase: String,
}

/// Результат проверки шапки файла.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderStatus {
    /// В шапке стоит явная фраза — файл производный.
    Derived(DerivedHeader),
    /// Шапка прочитана, явных фраз нет.
    Clean,
    /// Проверить не удалось: нет содержимого, oversize, битый blob, бюджет.
    /// Ложного `derived = false` в этом случае быть не должно.
    Unknown(String),
}

impl HeaderStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            HeaderStatus::Derived(_) => "derived",
            HeaderStatus::Clean => "clean",
            HeaderStatus::Unknown(_) => "unknown",
        }
    }

    /// Запись для `file_annotations` по одному пути.
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            HeaderStatus::Derived(d) => serde_json::json!({
                "derived": true,
                "header_line": d.header_line,
            }),
            HeaderStatus::Clean => serde_json::json!({ "derived": false }),
            HeaderStatus::Unknown(reason) => serde_json::json!({
                "derived_status": "unknown",
                "derived_reason": reason,
            }),
        }
    }
}

/// Распознать производность по тексту шапки.
///
/// Смотрим только начальный блок комментариев и останавливаемся на первом
/// исполняемом тексте: маркер в коде, в строковом литерале или в глубоком
/// комментарии (ниже шапки) признаком не является. BOM, CRLF, отступы и
/// пропускаемый shebang учитываются; `_`/`%`-подстановок здесь нет.
pub fn detect_derived_header(header: &str, language: &str) -> Option<DerivedHeader> {
    static MATCHERS: std::sync::LazyLock<Vec<regex::Regex>> = std::sync::LazyLock::new(|| {
        DERIVED_PHRASES
            .iter()
            .map(|phrase| {
                regex::RegexBuilder::new(&regex::escape(phrase))
                    .case_insensitive(true)
                    .build()
                    .expect("буквальная фраза шапки")
            })
            .collect()
    });
    for (line_no, text) in header_comment_lines(header, language) {
        for (phrase, matcher) in DERIVED_PHRASES.iter().zip(MATCHERS.iter()) {
            if matcher.is_match(&text) {
                return Some(DerivedHeader {
                    header_line: line_no,
                    phrase: (*phrase).to_string(),
                });
            }
        }
    }
    None
}

/// Строки начального блока комментариев: `(номер физической строки, текст)`.
fn header_comment_lines(header: &str, language: &str) -> Vec<(usize, String)> {
    let (line_comments, block_comments): (&[&str], &[(&str, &str)]) = match language {
        "rust" | "c" | "cpp" | "c++" | "csharp" | "c#" | "cs" => (&["//"], &[("/*", "*/")]),
        "python" | "ruby" | "bash" | "sh" | "shell" | "powershell" | "perl" => (&["#"], &[]),
        "php" => (&["//", "#"], &[("/*", "*/")]),
        "text" => (
            &["//", "#", "--"],
            &[("/*", "*/"), ("<!--", "-->"), ("(*", "*)")],
        ),
        "sql" | "lua" => (&["--"], &[("/*", "*/")]),
        _ => (&["//"], &[("/*", "*/"), ("<!--", "-->"), ("(*", "*)")]),
    };
    let end = (0..=header.len().min(HEADER_MAX_BYTES))
        .rev()
        .find(|&n| header.is_char_boundary(n))
        .unwrap_or(0);
    let header = &header[..end];
    let text = header.strip_prefix('\u{feff}').unwrap_or(header);
    let mut out = Vec::new();
    let mut block: Option<&str> = None;
    'lines: for (i, raw) in text.lines().take(HEADER_MAX_LINES).enumerate() {
        let line_no = i + 1;
        let mut rest = raw.trim();
        if line_no == 1 && rest.starts_with("#!") && !rest.starts_with("#![") {
            continue;
        }
        loop {
            if let Some(close) = block {
                if let Some((comment, tail)) = rest.split_once(close) {
                    out.push((line_no, comment.to_string()));
                    block = None;
                    rest = tail.trim_start();
                } else {
                    out.push((line_no, rest.to_string()));
                    continue 'lines;
                }
            }
            if rest.is_empty() {
                continue 'lines;
            }
            if matches!(language, "rust" | "php" | "text")
                && (rest.starts_with("#[") || rest.starts_with("#!["))
            {
                break 'lines;
            }
            if line_comments.iter().any(|prefix| rest.starts_with(*prefix)) {
                out.push((line_no, rest.to_string()));
                continue 'lines;
            }
            if let Some((open, close)) = block_comments
                .iter()
                .copied()
                .find(|(open, _)| rest.starts_with(*open))
            {
                rest = &rest[open.len()..];
                block = Some(close);
                continue;
            }
            break 'lines;
        }
    }
    out
}

/// Прочитать шапку файла ТОЛЬКО из индекса и сразу распознать производность.
///
/// Источник — сжатое содержимое `file_contents` (код) либо `text_contents`
/// (текст). Диск не читается, миграции не запускаются.
pub fn read_indexed_header(
    conn: &rusqlite::Connection,
    category: IndexedCategory,
    path: &str,
) -> HeaderStatus {
    read_header_limited(
        conn,
        category,
        path,
        HEADER_MAX_COMPRESSED_BYTES,
        HEADER_MAX_BYTES,
    )
    .0
}

fn read_header_limited(
    conn: &rusqlite::Connection,
    category: IndexedCategory,
    path: &str,
    compressed_limit: usize,
    decoded_limit: usize,
) -> (HeaderStatus, usize, usize) {
    let sql = format!(
        "SELECT substr(tc.content_blob, 1, ?2), tc.oversize, fi.language FROM {} tc \
         JOIN files fi ON fi.id = tc.file_id WHERE fi.path = ?1 LIMIT 1",
        category.table()
    );
    let row = conn
        .query_row(
            &sql,
            rusqlite::params![path, compressed_limit as i64],
            |r| {
                Ok((
                    r.get::<_, Option<Vec<u8>>>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .optional();
    let (blob, language) = match row {
        Ok(Some((_, oversize, _))) if oversize != 0 => {
            return (HeaderStatus::Unknown("oversize".into()), 0, 0)
        }
        Ok(Some((Some(blob), _, language))) => (blob, language),
        Ok(_) => {
            return (
                HeaderStatus::Unknown("нет индексированного содержимого".into()),
                0,
                0,
            )
        }
        Err(e) => return (HeaderStatus::Unknown(format!("чтение индекса: {e}")), 0, 0),
    };
    let mut out = Vec::new();
    let decoded = match zstd::stream::read::Decoder::new(blob.as_slice()) {
        Ok(decoder) => decoder.take(decoded_limit as u64).read_to_end(&mut out),
        Err(e) => {
            return (
                HeaderStatus::Unknown(format!("битый zstd: {e}")),
                blob.len(),
                0,
            )
        }
    };
    let size = out.len();
    let status = if let Err(e) = decoded {
        HeaderStatus::Unknown(format!("распаковка: {e}"))
    } else {
        let text = match std::str::from_utf8(&out) {
            Ok(text) => Some(text),
            Err(e) if e.error_len().is_none() => std::str::from_utf8(&out[..e.valid_up_to()]).ok(),
            Err(_) => None,
        };
        match text {
            Some(text) => match detect_derived_header(text, &language) {
                Some(derived) => HeaderStatus::Derived(derived),
                None if decoded_limit < HEADER_MAX_BYTES && size >= decoded_limit => {
                    HeaderStatus::Unknown("исчерпан бюджет распакованных шапок".into())
                }
                None => HeaderStatus::Clean,
            },
            None => HeaderStatus::Unknown("содержимое не UTF-8".into()),
        }
    };
    (status, blob.len(), size)
}

/// Проверка шапок для набора путей ОДНОГО ответа.
///
/// Один путь проверяется один раз (уникальные пути), суммарно — не больше
/// [`HEADER_PROBE_MAX_PATHS`] путей и [`HEADER_PROBE_MAX_BYTES`] текста.
/// В multi-repo бюджет общий через SEARCH_SCOPE. Сам кэш шапок создаётся
/// отдельно для каждого repo: одинаковый путь может иметь разное содержимое.
#[derive(Debug)]
pub struct HeaderProbe {
    seen: BTreeMap<String, HeaderStatus>,
    usage: SearchUsage,
    budget: SearchBudget,
}

impl Default for HeaderProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl HeaderProbe {
    pub fn new() -> Self {
        Self::with_budget(SearchBudget::header_probe())
    }

    pub fn with_budget(budget: SearchBudget) -> Self {
        let ceiling = SearchBudget::header_probe();
        let budget = SearchBudget::new(
            budget.max_files.min(ceiling.max_files),
            budget
                .max_compressed_bytes
                .min(ceiling.max_compressed_bytes),
            budget
                .max_uncompressed_bytes
                .min(ceiling.max_uncompressed_bytes),
            usize::MAX,
        );
        Self {
            seen: BTreeMap::new(),
            usage: SearchUsage::new(),
            budget,
        }
    }

    /// Повторный путь внутри одного repo берётся из кэша запроса. Между
    /// репозиториями кэш шапок не переиспользуется.
    pub fn probe(
        &mut self,
        conn: &rusqlite::Connection,
        category: IndexedCategory,
        path: &str,
    ) -> HeaderStatus {
        if let Some(status) = self.seen.get(path) {
            return status.clone();
        }
        let shared = remaining_header_budget();
        let compressed = self
            .budget
            .max_compressed_bytes
            .saturating_sub(self.usage.compressed_bytes)
            .min(shared.max_compressed_bytes);
        let decoded = self
            .budget
            .max_uncompressed_bytes
            .saturating_sub(self.usage.uncompressed_bytes)
            .min(shared.max_uncompressed_bytes);
        let status = if self.usage.files >= self.budget.max_files
            || shared.max_files == 0
            || compressed == 0
            || decoded == 0
            || deadline_expired()
        {
            HeaderStatus::Unknown("исчерпан бюджет проверки шапок".into())
        } else {
            let (status, compressed, decoded) = read_header_limited(
                conn,
                category,
                path,
                compressed.min(HEADER_MAX_COMPRESSED_BYTES),
                decoded.min(HEADER_MAX_BYTES),
            );
            self.usage.files += 1;
            self.usage.compressed_bytes += compressed;
            self.usage.uncompressed_bytes += decoded;
            record_header_usage(1, compressed, decoded);
            status
        };
        self.seen.insert(path.to_string(), status.clone());
        status
    }

    /// Проверить шапку, не зная заранее категорию файла (list_files).
    ///
    /// Сначала `file_contents`, при `unknown` — `text_contents`: у text-файла
    /// содержимое лежит в другой таблице, и без второй попытки пометка была бы
    /// ложной неполнотой.
    pub fn probe_any(&mut self, conn: &rusqlite::Connection, path: &str) -> HeaderStatus {
        if let Some(status) = self.seen.get(path) {
            return status.clone();
        }
        let category = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM file_contents fc JOIN files fi ON fi.id=fc.file_id WHERE fi.path=?1)",
            rusqlite::params![path], |r| r.get::<_, bool>(0),
        ).unwrap_or(false);
        self.probe(
            conn,
            if category {
                IndexedCategory::Code
            } else {
                IndexedCategory::Text
            },
            path,
        )
    }

    /// Пометки по исходным путям для `file_annotations`.
    pub fn annotations(&self) -> serde_json::Map<String, serde_json::Value> {
        self.seen
            .iter()
            .map(|(path, status)| (path.clone(), status.to_json()))
            .collect()
    }

    /// Хоть одна шапка осталась непроверенной?
    pub fn incomplete(&self) -> bool {
        self.seen
            .values()
            .any(|s| matches!(s, HeaderStatus::Unknown(_)))
    }

    /// Сводка неполноты: сколько путей проверено и почему часть — нет.
    pub fn incomplete_summary(&self) -> serde_json::Value {
        let unknown: Vec<&String> = self
            .seen
            .iter()
            .filter(|(_, s)| matches!(s, HeaderStatus::Unknown(_)))
            .map(|(p, _)| p)
            .collect();
        serde_json::json!({
            "checked": self.seen.len().saturating_sub(unknown.len()),
            "attempted": self.usage.files,
            "unknown": unknown.len(),
            "unknown_paths": unknown.into_iter().take(8).collect::<Vec<_>>(),
            "complete": !self.incomplete(),
            "usage": self.usage.to_json(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn language_header_cases() -> Vec<(&'static str, &'static str, Option<usize>)> {
        vec![
            ("rust", "#[doc = \"DO NOT EDIT\"]\nfn main() {}", None),
            ("rust", "#![doc = \"DO NOT EDIT\"]\nfn main() {}", None),
            ("rust", "#[cfg(test)]\n// DO NOT EDIT", None),
            ("php", "#[Attribute(\"DO NOT EDIT\")]", None),
            ("c", "#define TEXT \"DO NOT EDIT\"", None),
            ("cpp", "#pragma message(\"DO NOT EDIT\")", None),
            ("csharp", "#region DO NOT EDIT", None),
            ("rust", "/// DO NOT EDIT\nfn main() {}", Some(1)),
            ("rust", "//! Не редактировать\nfn main() {}", Some(1)),
            ("rust", "/*\n * AUTO-GENERATED\n */\nfn main() {}", Some(2)),
            (
                "rust",
                "\u{feff}#!/usr/bin/env rust-script\r\n\r\n  // DO NOT EDIT\r\nfn main() {}",
                Some(3),
            ),
            ("python", "# НЕ РЕДАКТИРОВАТЬ\npass", Some(1)),
            ("ruby", "# generated file\nputs 1", Some(1)),
            ("python", "#[ DO NOT EDIT ]", Some(1)),
            ("ruby", "#[ DO NOT EDIT ]", Some(1)),
            ("php", "# DO NOT EDIT\n$x = 1;", Some(1)),
            ("php", "// DO NOT EDIT", Some(1)),
            ("text", "\u{feff}#!/bin/sh\r\n  # DO NOT EDIT\r\nx", Some(2)),
            ("text", "<!-- DO NOT EDIT -->\ntext", Some(1)),
            ("text", "(* Не редактировать *)", Some(1)),
            ("text", "-- derived file", Some(1)),
            ("text", "/* auto-generated */", Some(1)),
            ("text", "#[doc = \"DO NOT EDIT\"]", None),
            ("text", "#![doc = \"DO NOT EDIT\"]", None),
            ("rust", "const X: &str = \"DO NOT EDIT\";", None),
            ("python", "x = 'DO NOT EDIT'", None),
            ("rust", "fn main() {}\n// DO NOT EDIT", None),
            ("python", "print(1)\n# DO NOT EDIT", None),
            ("php", "#[Attribute]\n# DO NOT EDIT", None),
            ("c", "#include <x>\n// DO NOT EDIT", None),
            ("c", "// DO NOT EDIT\nint x;", Some(1)),
            ("cpp", "/* Не редактировать */\nint x;", Some(1)),
            ("csharp", "/// DO NOT EDIT\nclass X {}", Some(1)),
        ]
    }

    #[test]
    fn derived_headers_follow_indexed_language_syntax() {
        for (language, header, line) in language_header_cases() {
            let actual = detect_derived_header(header, language).map(|d| d.header_line);
            assert_eq!(actual, line, "{language}: {header}");
        }
    }

    #[test]
    fn indexed_header_blobs_use_files_language_for_both_categories() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE files(id INTEGER PRIMARY KEY, path TEXT, language TEXT);
             CREATE TABLE file_contents(file_id INTEGER, content_blob BLOB, oversize INTEGER);
             CREATE TABLE text_contents(file_id INTEGER, content_blob BLOB, oversize INTEGER);
             INSERT INTO files VALUES(1, 'unrelated-name.bin', 'text');",
        )
        .unwrap();
        for category in [IndexedCategory::Code, IndexedCategory::Text] {
            for (language, header, line) in language_header_cases() {
                conn.execute("UPDATE files SET language=?1", [language])
                    .unwrap();
                conn.execute(&format!("DELETE FROM {}", category.table()), [])
                    .unwrap();
                let blob = zstd::stream::encode_all(header.as_bytes(), 1).unwrap();
                conn.execute(
                    &format!("INSERT INTO {} VALUES(1, ?1, 0)", category.table()),
                    rusqlite::params![blob],
                )
                .unwrap();
                let actual = read_indexed_header(&conn, category, "unrelated-name.bin");
                match (actual, line) {
                    (HeaderStatus::Derived(derived), Some(expected)) => {
                        assert_eq!(derived.header_line, expected);
                    }
                    (HeaderStatus::Clean, None) => {}
                    (actual, expected) => panic!("{language}: {header}: {actual:?}, {expected:?}"),
                }
            }
        }
    }

    #[test]
    fn header_limits_and_first_code_remain_effective() {
        let below_lines = format!("{}// DO NOT EDIT", "\n".repeat(HEADER_MAX_LINES));
        assert!(detect_derived_header(&below_lines, "rust").is_none());
        let below_bytes = format!("// {}DO NOT EDIT", " ".repeat(HEADER_MAX_BYTES));
        assert!(detect_derived_header(&below_bytes, "rust").is_none());
        assert!(
            detect_derived_header("/* clean */ const X: &str = \"DO NOT EDIT\";", "rust").is_none()
        );
    }
}
