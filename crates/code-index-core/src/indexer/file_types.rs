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
    // Выгрузка 1C:EDT — тот же XML, что у формата Конфигуратора, но с другими
    // расширениями: `.mdo` (описание объекта), `.form` (форма), `.rights`
    // (права роли). Без них файлы считались двоичными: поиска по метаданным
    // EDT не было вовсе, а наблюдатель за файлами не порождал событий —
    // правка конфигурации не доезжала до индекса до полной переиндексации (E-4).
    "mdo", "form", "rights",
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

/// Файлы выгрузки 1С, которые индексируются независимо от `max_file_size`.
///
/// В крупных конфигурациях они перерастают лимит текстового файла и молча
/// выпадали из индекса целиком: в УТ `Configuration.xml` — 1,2 МБ, крупнейший
/// `Rights.xml` — 5 МБ, две формы — свыше мегабайта. Искать по ним нужно
/// (оглавление конфигурации, права ролей, состав формы), а размер у них
/// ограничен самой конфигурацией.
///
/// НЕ включён намеренно `Template.xml` — макеты печатных форм, до 78 МБ на
/// файл, содержимое — разметка табличного документа, искать по ней нечего.
/// Служебная опись `ConfigDumpInfo.xml` в списке не нужна: она отсеивается
/// раньше и жёстче — как двоичная (см. `categorize_file`).
///
/// Те же три файла в выгрузке 1C:EDT называются иначе — `Configuration.mdo`,
/// `Rights.rights`, `Form.form`, — и перерастают лимит ровно так же: описание
/// конфигурации 1,6 МБ, крупнейшие права 2,7 МБ, крупные формы за мегабайт.
const SIZE_EXEMPT_FILES: &[&str] = &[
    "Configuration.xml",
    "Rights.xml",
    "Form.xml",
    "Configuration.mdo",
    "Rights.rights",
    "Form.form",
];

/// Освобождён ли файл от лимита размера для текстовых файлов.
pub fn is_size_exempt(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| SIZE_EXEMPT_FILES.contains(&n))
        .unwrap_or(false)
}

/// Директории, которые следует исключать при обходе
pub const EXCLUDE_DIRS: &[&str] = &[
    "node_modules", ".venv", "__pycache__", ".git",
    ".code-index", "target", ".mypy_cache", ".pytest_cache",
    ".tox", "dist", "build", "venv", "env", ".env",
];

/// Категория файла с учётом языка репозитория и дополнительных текстовых
/// расширений из настроек проекта (`extra_text_extensions`).
///
/// Язык репозитория нужен ровно для одного расширения — `.h`. Оно принадлежит
/// сразу двум языкам: в C-проекте это заголовок C, в C++-проекте — заголовок
/// C++ (`.hpp` завели не все, крупные проекты вроде rocksdb пишут просто `.h`).
/// По имени файла отличить нельзя, поэтому спрашиваем язык репозитория.
///
/// `extra_text` расширяет только границу «двоичный → текстовый»: расширение
/// из настроек делает файл текстовым, если иначе он был бы пропущен как
/// двоичный. Перебить язык оно не может — иначе указанный по ошибке `py`
/// выбросил бы разбор кода из индекса.
pub fn categorize_file_in_repo(
    path: &Path,
    repo_language: Option<&str>,
    extra_text: &[String],
) -> FileCategory {
    let category = categorize_file(path);
    let category = match category {
        FileCategory::Binary if matches_extra_text(path, extra_text) => FileCategory::Text,
        other => other,
    };
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

/// Совпадает ли расширение файла с одним из дополнительных текстовых,
/// заданных в настройках проекта. Регистр не важен, ведущая точка
/// допускается: люди пишут и `log`, и `.log`.
fn matches_extra_text(path: &Path, extra_text: &[String]) -> bool {
    if extra_text.is_empty() {
        return false;
    }
    let ext = match path.extension().and_then(|s| s.to_str()) {
        Some(e) => e.to_lowercase(),
        None => return false,
    };
    extra_text
        .iter()
        .any(|e| e.trim().trim_start_matches('.').eq_ignore_ascii_case(&ext))
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
            categorize_file_in_repo(h, Some("cpp"), &[]),
            FileCategory::Code("cpp".to_string()),
            "в C++-проекте .h — заголовок C++"
        );
        assert_eq!(
            categorize_file_in_repo(h, Some("c"), &[]),
            FileCategory::Code("c".to_string()),
            "в C-проекте .h — заголовок C"
        );
        assert_eq!(
            categorize_file_in_repo(h, None, &[]),
            FileCategory::Code("c".to_string()),
            "язык репо неизвестен — как раньше, по таблице расширений"
        );
        assert_eq!(
            categorize_file_in_repo(Path::new("SRC/SERVER.H"), Some("cpp"), &[]),
            FileCategory::Code("cpp".to_string()),
            "регистр расширения значения не имеет"
        );
    }

    /// `extra_text_extensions` из настроек проекта — настройка была объявлена
    /// и предлагалась в подсказке `init`, но не читалась ни одним этапом
    /// индексации: люди задавали её и считали, что расширили индекс.
    #[test]
    fn дополнительные_текстовые_расширения_из_настроек_применяются() {
        let extra = vec!["log".to_string(), ".conf".to_string(), "TPL".to_string()];

        // Иначе файл был бы пропущен как двоичный.
        assert_eq!(
            categorize_file_in_repo(Path::new("app.log"), None, &extra),
            FileCategory::Text
        );
        // Ведущая точка в настройке допускается.
        assert_eq!(
            categorize_file_in_repo(Path::new("nginx.conf"), None, &extra),
            FileCategory::Text
        );
        // Регистр не важен ни в настройке, ни в имени файла.
        assert_eq!(
            categorize_file_in_repo(Path::new("Page.tpl"), None, &extra),
            FileCategory::Text
        );
        // Не перечисленное остаётся двоичным.
        assert_eq!(
            categorize_file_in_repo(Path::new("image.png"), None, &extra),
            FileCategory::Binary
        );
        // Пустой список ничего не меняет.
        assert_eq!(
            categorize_file_in_repo(Path::new("app.log"), None, &[]),
            FileCategory::Binary
        );
    }

    /// Ошибочно указанное расширение кода не должно выбрасывать разбор:
    /// `py` в списке текстовых не делает питон текстом.
    #[test]
    fn дополнительные_расширения_не_перебивают_код_и_текст() {
        let extra = vec!["py".to_string(), "md".to_string()];
        assert_eq!(
            categorize_file_in_repo(Path::new("script.py"), None, &extra),
            FileCategory::Code("python".to_string())
        );
        assert_eq!(
            categorize_file_in_repo(Path::new("readme.md"), None, &extra),
            FileCategory::Text
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
                categorize_file_in_repo(Path::new(path), Some("cpp"), &[]),
                FileCategory::Code(lang.to_string()),
                "{} не должен зависеть от языка репозитория",
                path
            );
        }
        // Текстовые и двоичные тоже не затрагиваются
        assert_eq!(
            categorize_file_in_repo(Path::new("readme.md"), Some("cpp"), &[]),
            FileCategory::Text
        );
        assert_eq!(
            categorize_file_in_repo(Path::new("image.png"), Some("cpp"), &[]),
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
