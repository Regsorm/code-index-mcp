use serde::{Deserialize, Serialize};

/// Запись файла в индексе
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRecord {
    pub id: Option<i64>,
    pub path: String,
    pub content_hash: String,
    pub ast_hash: Option<String>,
    pub language: String,
    pub lines_total: usize,
    pub indexed_at: String,
    pub mtime: Option<i64>,      // Unix timestamp секунды (fs::metadata)
    pub file_size: Option<i64>,  // размер файла в байтах
}

/// Запись функции
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FunctionRecord {
    pub id: Option<i64>,
    pub file_id: i64,
    pub name: String,
    pub qualified_name: Option<String>,
    pub line_start: usize,
    pub line_end: usize,
    pub args: Option<String>,
    pub return_type: Option<String>,
    pub docstring: Option<String>,
    pub body: String,
    pub is_async: bool,
    pub node_hash: String,
    /// Тип переопределения: "Перед", "После", "Вместо" (только BSL-расширения)
    pub override_type: Option<String>,
    /// Имя оригинальной процедуры, которую переопределяет аннотация
    pub override_target: Option<String>,
}

/// Запись класса
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassRecord {
    pub id: Option<i64>,
    pub file_id: i64,
    pub name: String,
    pub line_start: usize,
    pub line_end: usize,
    pub bases: Option<String>,
    pub docstring: Option<String>,
    pub body: String,
    pub node_hash: String,
}

/// Запись импорта
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportRecord {
    pub id: Option<i64>,
    pub file_id: i64,
    pub module: Option<String>,
    pub name: Option<String>,
    pub alias: Option<String>,
    pub line: usize,
    pub kind: String,
}

/// Запись вызова
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallRecord {
    pub id: Option<i64>,
    pub file_id: i64,
    pub caller: String,
    pub callee: String,
    pub line: usize,
}

/// Запись переменной
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VariableRecord {
    pub id: Option<i64>,
    pub file_id: i64,
    pub name: String,
    pub value: Option<String>,
    pub line: usize,
}

/// Запись текстового файла
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextFileRecord {
    pub id: Option<i64>,
    pub file_id: i64,
    pub content: String,
}

/// Результат поиска символа (объединённый)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolSearchResult {
    pub functions: Vec<FunctionRecord>,
    pub classes: Vec<ClassRecord>,
    pub variables: Vec<VariableRecord>,
    pub imports: Vec<ImportRecord>,
}

/// Сводка по файлу
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSummary {
    pub file: FileRecord,
    pub functions: Vec<FunctionRecord>,
    pub classes: Vec<ClassRecord>,
    pub imports: Vec<ImportRecord>,
    pub variables: Vec<VariableRecord>,
}

/// Результат grep_body — функция/класс, содержащая паттерн
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepBodyMatch {
    /// Путь к файлу
    pub file_path: String,
    /// Имя функции или класса
    pub name: String,
    /// Тип: "function" или "class"
    pub kind: String,
    /// Начальная строка
    pub line_start: usize,
    /// Конечная строка
    pub line_end: usize,
    /// Номера строк в файле, где найдено совпадение (первые 3)
    pub match_lines: Vec<usize>,
    /// Общее количество совпадений (только если > 3)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub match_count: Option<usize>,
    /// Контекст вокруг каждого совпадения (если запрошен через context_lines).
    /// Ключ — номер строки в файле, значение — текст. Пуст когда context_lines=0.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub context: Vec<ContextLine>,
}

/// Одна строка контекста для grep_body / grep_text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextLine {
    pub line: usize,
    pub content: String,
}

/// Результат `read_file` — содержимое (целиком или по диапазону строк) +
/// метаданные индекса.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadFileResult {
    /// Содержимое (плоский текст с переносами строк).
    /// Для oversize-файлов — пустая строка (см. `oversize`).
    pub content: String,
    /// Сколько строк реально вернулось.
    pub lines_returned: usize,
    /// Всего строк в файле.
    pub lines_total: usize,
    /// Пришлось ли усечь по soft-cap.
    pub truncated: bool,
    /// ISO-время последней индексации (для контроля свежести).
    pub indexed_at: String,
    /// Категория файла: "text" — содержимое из БД доступно;
    /// "code" — content из `file_contents` (Phase 2). Если v0.8.0 ещё не
    /// успел сделать backfill для этого файла — content пуст и `oversize=false`.
    pub category: String,
    /// `true` — файл превышает `max_code_file_size_bytes`, content
    /// намеренно не сохранён в индексе. Используйте `get_function`/
    /// `get_class`/`grep_body` для целевого чтения, либо читайте файл
    /// напрямую с диска.
    #[serde(default, skip_serializing_if = "is_false")]
    pub oversize: bool,
    /// Размер файла в байтах (если известен из таблицы `files`).
    /// Полезно вместе с `oversize=true` для понимания насколько файл велик.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_size: Option<i64>,
    /// Эффективный лимит, по которому был принят `oversize` для этого репо
    /// (per-path > [indexer] > hardcoded 5 МБ). Помогает оператору быстро
    /// понять, нужно ли увеличивать лимит в `daemon.toml`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_limit: Option<i64>,
    /// Человекочитаемая подсказка вызывающей стороне. Заполняется только
    /// для `oversize=true` либо когда content code-файла ещё не наполнен
    /// (backfill в процессе).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

#[inline]
fn is_false(b: &bool) -> bool { !*b }

/// Запись из `list_files` — метаданные файла без полей хеша.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListedFile {
    pub path: String,
    pub language: String,
    pub lines_total: usize,
    pub size: Option<i64>,
    pub mtime: Option<i64>,
}

/// Результат `stat_file` — метаданные одного файла + флаг наличия content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatFileResult {
    pub exists: bool,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtime: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines_total: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexed_at: Option<String>,
    /// Доступен ли content через `read_file`:
    ///   * `"text"` — да, из `text_files`.
    ///   * `"code"` — да, из `file_contents` (Phase 2). См. `oversize`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// `true` — файл превышает лимит и content не сохранён в индексе.
    /// Поле появляется только для code-файлов (Phase 2) — для text всегда отсутствует.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oversize: Option<bool>,
}

/// Один матч `grep_text` — строка в text-файле, удовлетворяющая regex.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepTextMatch {
    pub path: String,
    pub line: usize,
    pub content: String,
    /// Контекст до/после матча, если запрошен. Пуст когда context_lines=0.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub context: Vec<ContextLine>,
}

/// Статус фоновой индексации
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state")]
pub enum IndexingStatus {
    /// БД ещё не открыта — сервер только что запустился
    Initializing,
    /// Индексация не идёт, данные актуальны
    Ready,
    /// Индексация в процессе
    Indexing {
        /// Текущая фаза
        phase: String,
        /// Обработано файлов
        files_done: usize,
        /// Всего файлов
        files_total: usize,
    },
    /// Индексация завершена
    Completed {
        /// Проиндексировано файлов
        files_indexed: usize,
        /// Время в миллисекундах
        elapsed_ms: u64,
    },
    /// Индексация провалилась
    Failed {
        /// Текст ошибки
        error: String,
    },
}

/// Статистика базы данных
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbStats {
    pub total_files: usize,
    pub total_functions: usize,
    pub total_classes: usize,
    pub total_imports: usize,
    pub total_calls: usize,
    pub total_variables: usize,
    pub total_text_files: usize,
    /// Статус фоновой индексации (заполняется MCP-сервером)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexing_status: Option<IndexingStatus>,
}
