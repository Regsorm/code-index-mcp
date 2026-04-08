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
