use std::path::Path;

/// Категория файла для принятия решения об индексации
#[derive(Debug, Clone, PartialEq)]
pub enum FileCategory {
    /// Файл с исходным кодом, поддерживает AST-парсинг (название языка)
    Code(String),
    /// Текстовый файл — индексируется через FTS без AST
    Text,
    /// Бинарный файл — пропускается
    Binary,
}

/// Расширения файлов с поддержкой AST-парсинга и соответствующие названия языков
const CODE_EXTENSIONS: &[(&str, &str)] = &[
    ("py", "python"),
    ("js", "javascript"),
    ("jsx", "javascript"),
    ("ts", "typescript"),
    ("tsx", "typescript"),
    ("java", "java"),
];

/// Расширения текстовых файлов для полнотекстового поиска
const TEXT_EXTENSIONS: &[&str] = &[
    "md", "txt", "rst",
    "json", "yaml", "yml", "toml",
    "xml", "html", "css",
    "rs", "go", "c", "h", "cpp", "hpp", "cs", "rb", "php", "swift", "kt",
    "csv", "env", "ini", "cfg",
    "sql", "sh", "bat", "ps1",
];

/// Директории, которые следует исключать при обходе
pub const EXCLUDE_DIRS: &[&str] = &[
    "node_modules", ".venv", "__pycache__", ".git",
    ".code-index", "target", ".mypy_cache", ".pytest_cache",
    ".tox", "dist", "build", "venv", "env", ".env",
];

/// Определить категорию файла по расширению пути
pub fn categorize_file(path: &Path) -> FileCategory {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    // Проверяем расширения кода (AST-парсинг)
    for (code_ext, language) in CODE_EXTENSIONS {
        if ext == *code_ext {
            return FileCategory::Code(language.to_string());
        }
    }

    // Проверяем расширения текстовых файлов (FTS)
    if TEXT_EXTENSIONS.contains(&ext.as_str()) {
        return FileCategory::Text;
    }

    // Всё остальное — бинарные файлы, пропускаем
    FileCategory::Binary
}

/// Проверить, нужно ли исключить директорию с данным именем
pub fn is_excluded_dir(dir_name: &str) -> bool {
    EXCLUDE_DIRS.contains(&dir_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_code_extension() {
        assert_eq!(
            categorize_file(Path::new("script.py")),
            FileCategory::Code("python".to_string())
        );
    }

    #[test]
    fn test_text_extensions() {
        assert_eq!(categorize_file(Path::new("readme.md")), FileCategory::Text);
        assert_eq!(categorize_file(Path::new("config.toml")), FileCategory::Text);
        assert_eq!(categorize_file(Path::new("data.json")), FileCategory::Text);
        assert_eq!(categorize_file(Path::new("setup.cfg")), FileCategory::Text);
    }

    #[test]
    fn test_binary_extension() {
        assert_eq!(categorize_file(Path::new("image.png")), FileCategory::Binary);
        assert_eq!(categorize_file(Path::new("archive.zip")), FileCategory::Binary);
        assert_eq!(categorize_file(Path::new("lib.so")), FileCategory::Binary);
    }

    #[test]
    fn test_no_extension() {
        assert_eq!(categorize_file(Path::new("Makefile")), FileCategory::Binary);
    }

    #[test]
    fn test_case_insensitive() {
        assert_eq!(
            categorize_file(Path::new("script.PY")),
            FileCategory::Code("python".to_string())
        );
        assert_eq!(categorize_file(Path::new("README.MD")), FileCategory::Text);
    }

    #[test]
    fn test_excluded_dirs() {
        assert!(is_excluded_dir("node_modules"));
        assert!(is_excluded_dir(".git"));
        assert!(is_excluded_dir("target"));
        assert!(is_excluded_dir("__pycache__"));
        assert!(!is_excluded_dir("src"));
        assert!(!is_excluded_dir("my_project"));
    }
}
