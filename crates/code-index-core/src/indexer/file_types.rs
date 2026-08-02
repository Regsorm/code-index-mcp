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
    ("rs", "rust"),
    ("go", "go"),
    ("bsl", "bsl"),
    ("os", "bsl"),
    ("php", "php"),
    ("php5", "php"),
    ("phtml", "php"),
    ("c", "c"),
    ("h", "c"),
    ("cpp", "cpp"),
    ("cxx", "cpp"),
    ("cc", "cpp"),
    ("hpp", "cpp"),
    ("hxx", "cpp"),
    ("hh", "cpp"),
    ("cs", "csharp"),
    ("rb", "ruby"),
    ("swift", "swift"),
    ("html", "html"),
    ("htm", "html"),
];

/// Расширения текстовых файлов для полнотекстового поиска.
/// Внимание: `html`/`htm` ушли в CODE_EXTENSIONS (v0.7.1) — для них применяется
/// AST-парсинг + дополнительная text-индексация (см. `is_dual_indexed_language`).
const TEXT_EXTENSIONS: &[&str] = &[
    "md", "txt", "rst",
    "json", "yaml", "yml", "toml",
    "xml", "css",
    "kt",
    "csv", "env", "ini", "cfg",
    "sql", "sh", "bat", "ps1",
];

/// Языки, для которых при индексации делается «двойная вставка»: и
/// AST-парсинг (functions/classes/imports/variables), и сохранение
/// raw-content в `text_files` для FTS+regex+read_file.
///
/// Введено для HTML в v0.7.1: пользователи привыкли искать
/// `search_text("...")` и `grep_text(...)` по html-файлам, новые
/// structured queries (`get_class("cart")`, `find_symbol("submitOrder")`,
/// `get_imports(module=...)`) добавляются сверху без регрессии.
pub fn is_dual_indexed_language(language: &str) -> bool {
    matches!(
        language,
        "html" | "php" | "c" | "cpp" | "csharp" | "ruby" | "swift"
    )
}

/// Директории, которые следует исключать при обходе
pub const EXCLUDE_DIRS: &[&str] = &[
    "node_modules", ".venv", "__pycache__", ".git",
    ".code-index", "target", ".mypy_cache", ".pytest_cache",
    ".tox", "dist", "build", "venv", "env", ".env",
];

/// Категория файла с учётом языка репозитория.
///
/// Нужно ровно для одного расширения — `.h`. Оно принадлежит сразу двум
/// языкам: в C-проекте это заголовок C, в C++-проекте — заголовок C++
/// (`.hpp` завели не все, крупные проекты вроде rocksdb пишут просто `.h`).
/// По имени файла отличить нельзя, поэтому спрашиваем язык репозитория.
///
/// Все остальные расширения определяются как раньше — язык репозитория
/// на них не влияет.
pub fn categorize_file_in_repo(path: &Path, repo_language: Option<&str>) -> FileCategory {
    let category = categorize_file(path);
    if repo_language != Some("cpp") {
        return category;
    }
    let is_h = path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.eq_ignore_ascii_case("h"))
        .unwrap_or(false);
    match category {
        FileCategory::Code(ref lang) if is_h && lang == "c" => {
            FileCategory::Code("cpp".to_string())
        }
        other => other,
    }
}

/// Определить категорию файла по расширению пути
pub fn categorize_file(path: &Path) -> FileCategory {
    // `ConfigDumpInfo.xml` — служебная опись выгрузки 1С (uuid + configVersion
    // всех объектов и под-элементов). В общий текстовый индекс не кладём:
    // поиск по хэшам версий бессмысленен, а базовая опись весит десятки МБ.
    // Единственный потребитель — заполнение таблицы `config_manifest`
    // (bsl-extension), которое читает файл напрямую с диска, а не из индекса.
    // Binary = «пропустить, не индексировать».
    if path.file_name().and_then(|n| n.to_str()) == Some("ConfigDumpInfo.xml") {
        return FileCategory::Binary;
    }

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
    fn html_is_code_with_dual_indexing() {
        // v0.7.1: .html и .htm — code-категория с language=html, плюс
        // отдельная пометка про дополнительную FTS-индексацию.
        assert_eq!(
            categorize_file(Path::new("index.html")),
            FileCategory::Code("html".to_string())
        );
        assert_eq!(
            categorize_file(Path::new("legacy.htm")),
            FileCategory::Code("html".to_string())
        );
        assert!(is_dual_indexed_language("html"));
        assert!(!is_dual_indexed_language("python"));
    }

    #[test]
    fn multilang_extensions_are_code_with_dual_indexing() {
        // C/C++/C#/Ruby/Swift переведены из TEXT в CODE — AST плюс сохранение
        // raw-content, чтобы grep_text/read_file по ним не потеряли покрытие.
        for (path, lang) in [
            ("main.c", "c"),
            ("api.h", "c"),
            ("engine.cpp", "cpp"),
            ("engine.hpp", "cpp"),
            ("Service.cs", "csharp"),
            ("worker.rb", "ruby"),
            ("View.swift", "swift"),
        ] {
            assert_eq!(
                categorize_file(Path::new(path)),
                FileCategory::Code(lang.to_string()),
                "{} должен быть code/{}",
                path,
                lang
            );
            assert!(is_dual_indexed_language(lang), "{} — dual-indexed", lang);
        }
        // Kotlin пока без грамматики под tree-sitter 0.25 — остаётся текстовым
        assert_eq!(categorize_file(Path::new("Main.kt")), FileCategory::Text);
    }

    /// `.h` — единственное расширение, принадлежащее сразу двум языкам.
    /// В C-проекте это C, в C++-проекте — C++; решает язык репозитория.
    #[test]
    fn h_header_follows_repo_language() {
        let h = Path::new("db/db_impl.h");
        assert_eq!(
            categorize_file_in_repo(h, Some("cpp")),
            FileCategory::Code("cpp".to_string()),
            "в C++-проекте .h — заголовок C++"
        );
        assert_eq!(
            categorize_file_in_repo(h, Some("c")),
            FileCategory::Code("c".to_string()),
            "в C-проекте .h — заголовок C"
        );
        assert_eq!(
            categorize_file_in_repo(h, None),
            FileCategory::Code("c".to_string()),
            "язык репо неизвестен — как раньше, по таблице расширений"
        );
        assert_eq!(
            categorize_file_in_repo(Path::new("SRC/SERVER.H"), Some("cpp")),
            FileCategory::Code("cpp".to_string()),
            "регистр расширения значения не имеет"
        );
    }

    /// Язык репозитория трогает ТОЛЬКО `.h`. Остальные расширения
    /// определяются как раньше, даже в C++-проекте.
    #[test]
    fn repo_language_touches_only_h_extension() {
        for (path, lang) in [
            ("main.c", "c"),
            ("engine.cpp", "cpp"),
            ("engine.hpp", "cpp"),
            ("script.py", "python"),
            ("app.rb", "ruby"),
            ("Service.cs", "csharp"),
            ("mod.rs", "rust"),
        ] {
            assert_eq!(
                categorize_file_in_repo(Path::new(path), Some("cpp")),
                FileCategory::Code(lang.to_string()),
                "{} не должен зависеть от языка репозитория",
                path
            );
        }
        // Текстовые и двоичные тоже не затрагиваются
        assert_eq!(
            categorize_file_in_repo(Path::new("readme.md"), Some("cpp")),
            FileCategory::Text
        );
        assert_eq!(
            categorize_file_in_repo(Path::new("image.png"), Some("cpp")),
            FileCategory::Binary
        );
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
    fn config_dump_info_skipped_by_name() {
        // Вариант 2: опись выгрузки 1С не индексируется как текст —
        // единственный потребитель файла — заполнение config_manifest.
        assert_eq!(
            categorize_file(Path::new("extensions/ent_Наборы/ConfigDumpInfo.xml")),
            FileCategory::Binary
        );
        assert_eq!(
            categorize_file(Path::new("base/ConfigDumpInfo.xml")),
            FileCategory::Binary
        );
        // Обычный объектный XML остаётся текстовым (xml_1c-апгрейд — позже в indexer).
        assert_eq!(
            categorize_file(Path::new("base/Catalogs/Контрагенты.xml")),
            FileCategory::Text
        );
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
