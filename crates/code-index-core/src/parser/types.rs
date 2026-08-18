use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Вычислить SHA-256 хеш строки → hex
pub fn sha256_hex(data: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data.as_bytes());
    hex::encode(hasher.finalize())
}

/// Извлечённая функция из AST
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ParsedFunction {
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

/// Извлечённый класс
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedClass {
    pub name: String,
    pub line_start: usize,
    pub line_end: usize,
    pub bases: Option<String>,
    pub docstring: Option<String>,
    pub body: String,
    pub node_hash: String,
}

/// Извлечённый импорт
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedImport {
    pub module: Option<String>,
    pub name: Option<String>,
    pub alias: Option<String>,
    pub line: usize,
    /// Тип импорта: "import" или "from"
    pub kind: String,
}

/// Извлечённый вызов функции
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedCall {
    pub caller: String,
    pub callee: String,
    pub line: usize,
}

/// Извлечённая переменная
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedVariable {
    pub name: String,
    pub value: Option<String>,
    pub line: usize,
}

/// Результат парсинга одного файла
#[derive(Debug, Clone)]
pub struct ParseResult {
    pub functions: Vec<ParsedFunction>,
    pub classes: Vec<ParsedClass>,
    pub imports: Vec<ParsedImport>,
    pub calls: Vec<ParsedCall>,
    pub variables: Vec<ParsedVariable>,
    pub lines_total: usize,
}

/// Результат парсинга текстового файла
#[derive(Debug, Clone)]
pub struct TextParseResult {
    pub content: String,
    pub lines_total: usize,
}

/// Предел глубины рекурсивного обхода дерева разбора — защита от переполнения
/// стека. Переполнение в Rust не паника, а аварийное завершение процесса: его
/// нельзя перехватить, упадёт весь демон индексации, а не один файл.
///
/// Значение выбрано по замеру (сборка release, стек рабочего потока 2 МБ):
/// обход парсера переполняет стек между глубиной 2 000 и 5 000, поэтому 500
/// даёт запас в 4–10 раз. При этом он много выше прежних разрозненных пределов
/// (50 у java/js/ts, 80 у bsl/rust, 100 у go), из-за которых факты в глубоко
/// вложенном коде терялись молча.
///
/// Глубина отсчитывается заново для тела каждой функции, так что предел
/// ограничивает одну цепочку вложенности, а не файл целиком.
pub const MAX_VISIT_DEPTH: usize = 500;

/// Порог времени на разбор ОДНОГО файла — страховка от нелинейной деградации
/// tree-sitter на патологическом вводе. 10 с даёт многократный запас над самым
/// медленным законным файлом и при этом обрывает деградацию за секунды, а не
/// за минуты. Общий для всех языков: минифицированные и сгенерированные файлы
/// встречаются не только в BSL, где защита появилась первой.
pub const PARSE_TIMEOUT_MS: u64 = 10_000;

/// Признак, что под расширением исходника лежат двоичные данные.
/// NUL-байт в первых килобайтах — надёжный маркер не-текста (так же поступают
/// git и file): в исходнике его не бывает, а tree-sitter на бесструктурном
/// вводе деградирует квадратично по размеру.
pub fn looks_binary(source: &str) -> bool {
    source.as_bytes().iter().take(8192).any(|&b| b == 0)
}

/// Пустой результат — для файлов, пропущенных защитой (двоичные либо
/// превысившие порог времени). Файл при этом остаётся в индексе как код,
/// просто без извлечённых фактов.
pub fn empty_parse_result(source: &str) -> ParseResult {
    ParseResult {
        functions: Vec::new(),
        classes: Vec::new(),
        imports: Vec::new(),
        calls: Vec::new(),
        variables: Vec::new(),
        lines_total: source.lines().count(),
    }
}
