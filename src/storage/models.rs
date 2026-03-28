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
}

/// Запись функции
#[derive(Debug, Clone, Serialize, Deserialize)]
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
}
