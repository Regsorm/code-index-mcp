// Определение преобладающего языка репозитория по содержимому корня и
// дозапись результата обратно в `daemon.toml`.
//
// Назначение:
// 1. Миграция существующих установок code-index. У уже работающих пользователей
//    `daemon.toml` ещё не содержит поля `language` в `[[paths]]`; при первом
//    старте новой версии демон должен сам определить язык и заполнить поле,
//    чтобы пользователь не возился с ручной правкой конфига.
// 2. Подсказка оператору при добавлении нового репо без указания языка.
//
// Запись обратно использует `toml_edit`, а не `toml::Value` round-trip:
// `toml_edit` сохраняет комментарии, пустые строки и порядок ключей в
// исходном файле — для конфигурационного файла, который пользователь
// часто читает глазами, это критично.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

/// Простая эвристика по корню репо: смотрит на сигнальные файлы
/// (`Configuration.xml` для 1С, `Cargo.toml` для Rust и т.д.).
/// Возвращает `Some(<язык>)` если уверены, иначе `None` — пусть
/// caller решает, fallback'ить ли по преобладанию расширений.
pub fn detect_by_root_markers(root: &Path) -> Option<&'static str> {
    // Порядок проверок — от самых точных к более общим.
    // Configuration.xml — однозначный признак выгрузки 1С.
    if root.join("Configuration.xml").is_file() {
        return Some("bsl");
    }
    if root.join("Cargo.toml").is_file() {
        return Some("rust");
    }
    if root.join("pyproject.toml").is_file() || root.join("setup.py").is_file() {
        return Some("python");
    }
    if root.join("go.mod").is_file() {
        return Some("go");
    }
    if root.join("pom.xml").is_file()
        || root.join("build.gradle").is_file()
        || root.join("build.gradle.kts").is_file()
    {
        return Some("java");
    }
    // package.json — общий маркер JS-экосистемы. Если рядом tsconfig.json —
    // это TypeScript-проект, иначе считаем JS.
    if root.join("package.json").is_file() {
        if root.join("tsconfig.json").is_file() {
            return Some("typescript");
        }
        return Some("javascript");
    }
    None
}

/// Fallback: посчитать расширения файлов в корне (1 уровень) и выбрать
/// язык с наибольшим количеством. Не рекурсивно — это первая прикидка,
/// глубокий обход уже сделает индексер сам.
///
/// Если в корне ни одного файла известного расширения — `None`.
pub fn detect_by_extension_majority(root: &Path) -> Option<&'static str> {
    use std::collections::HashMap;

    let entries = fs::read_dir(root).ok()?;
    let mut counts: HashMap<&'static str, usize> = HashMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_ascii_lowercase());
        let lang = match ext.as_deref() {
            Some("py") => "python",
            Some("rs") => "rust",
            Some("go") => "go",
            Some("java") => "java",
            Some("js") | Some("jsx") => "javascript",
            Some("ts") | Some("tsx") => "typescript",
            Some("bsl") | Some("os") => "bsl",
            _ => continue,
        };
        *counts.entry(lang).or_insert(0) += 1;
    }
    counts.into_iter().max_by_key(|(_, c)| *c).map(|(l, _)| l)
}

/// Главный entry point: маркеры → fallback по расширениям → `None`.
pub fn detect_language(root: &Path) -> Option<&'static str> {
    detect_by_root_markers(root).or_else(|| detect_by_extension_majority(root))
}

/// Дописать `language = "..."` в нужную запись `[[paths]]` файла
/// `daemon.toml`, сохранив форматирование исходного файла.
///
/// `target_path` сравнивается с полем `path` каждой записи как строка,
/// без canonicalize — чтобы избежать неожиданной нормализации
/// `C:\` ↔ `\\?\C:\` на Windows. Совпадение должно быть точным.
///
/// Возвращает `Ok(true)` если запись найдена и обновлена, `Ok(false)`
/// если ни одна `[[paths]]` не совпала с `target_path` (caller решит,
/// нужен ли warning), `Err` — при I/O или ошибках парсинга.
pub fn write_language_back(
    daemon_toml_path: &Path,
    target_path: &Path,
    language: &str,
) -> Result<bool> {
    let text = fs::read_to_string(daemon_toml_path)
        .with_context(|| format!("Не удалось прочитать {}", daemon_toml_path.display()))?;

    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| format!("Невалидный TOML в {}", daemon_toml_path.display()))?;

    let paths = doc
        .get_mut("paths")
        .ok_or_else(|| anyhow!("В {} отсутствует [[paths]]", daemon_toml_path.display()))?
        .as_array_of_tables_mut()
        .ok_or_else(|| {
            anyhow!(
                "В {} ключ `paths` не является массивом таблиц",
                daemon_toml_path.display()
            )
        })?;

    let target_str = target_path.to_string_lossy();
    let mut updated = false;
    for entry in paths.iter_mut() {
        let entry_path = match entry.get("path").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => continue,
        };
        if entry_path == target_str {
            entry.insert("language", toml_edit::value(language));
            updated = true;
            break;
        }
    }

    if updated {
        fs::write(daemon_toml_path, doc.to_string()).with_context(|| {
            format!("Не удалось записать {}", daemon_toml_path.display())
        })?;
    }
    Ok(updated)
}

/// Удобный тип результата прохода auto-detect: какие репо
/// удалось определить, какие — нет (нуждаются в ручном указании).
#[derive(Debug, Default)]
pub struct AutoDetectResult {
    /// (path, detected_language) — успешно определены.
    pub detected: Vec<(PathBuf, &'static str)>,
    /// Пути, для которых ни маркеры, ни преобладание расширений
    /// не дали ответа. Caller выводит warning и пропускает.
    pub unresolved: Vec<PathBuf>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn touch(dir: &Path, name: &str) {
        let p = dir.join(name);
        std::fs::File::create(&p).unwrap();
    }

    #[test]
    fn detects_bsl_by_configuration_xml() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "Configuration.xml");
        assert_eq!(detect_language(tmp.path()), Some("bsl"));
    }

    #[test]
    fn detects_rust_by_cargo_toml() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "Cargo.toml");
        assert_eq!(detect_language(tmp.path()), Some("rust"));
    }

    #[test]
    fn detects_python_by_pyproject() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "pyproject.toml");
        assert_eq!(detect_language(tmp.path()), Some("python"));
    }

    #[test]
    fn detects_typescript_when_tsconfig_present() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "package.json");
        touch(tmp.path(), "tsconfig.json");
        assert_eq!(detect_language(tmp.path()), Some("typescript"));
    }

    #[test]
    fn detects_javascript_without_tsconfig() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "package.json");
        assert_eq!(detect_language(tmp.path()), Some("javascript"));
    }

    #[test]
    fn falls_back_to_extension_majority() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "a.py");
        touch(tmp.path(), "b.py");
        touch(tmp.path(), "c.go");
        // Маркеров нет, преобладают .py — должен вернуть python.
        assert_eq!(detect_language(tmp.path()), Some("python"));
    }

    #[test]
    fn returns_none_for_empty_dir() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(detect_language(tmp.path()), None);
    }

    #[test]
    fn write_language_back_inserts_into_matching_entry() {
        let tmp = TempDir::new().unwrap();
        let toml_path = tmp.path().join("daemon.toml");
        let original = r#"# Комментарий должен сохраниться
[daemon]
http_port = 8009

[[paths]]
path = "/srv/repos/ut"
alias = "ut"

[[paths]]
path = "/srv/repos/foo"
"#;
        std::fs::File::create(&toml_path)
            .unwrap()
            .write_all(original.as_bytes())
            .unwrap();

        let updated =
            write_language_back(&toml_path, Path::new("/srv/repos/ut"), "bsl").unwrap();
        assert!(updated);

        let new_text = std::fs::read_to_string(&toml_path).unwrap();
        // Комментарий и существующие поля сохранены.
        assert!(new_text.contains("# Комментарий должен сохраниться"));
        assert!(new_text.contains("http_port = 8009"));
        // language добавлен только в первой записи.
        assert!(new_text.contains(r#"language = "bsl""#));

        // Проверяем что parse_str (через основной модуль config) принимает
        // обновлённый файл и видит новое поле.
        let cfg = super::super::config::parse_str(&new_text).unwrap();
        assert_eq!(cfg.paths[0].language.as_deref(), Some("bsl"));
        assert!(cfg.paths[1].language.is_none());
    }

    #[test]
    fn write_language_back_returns_false_when_no_match() {
        let tmp = TempDir::new().unwrap();
        let toml_path = tmp.path().join("daemon.toml");
        let original = r#"
[[paths]]
path = "/srv/repos/other"
"#;
        std::fs::File::create(&toml_path)
            .unwrap()
            .write_all(original.as_bytes())
            .unwrap();

        let updated =
            write_language_back(&toml_path, Path::new("/srv/repos/missing"), "bsl").unwrap();
        assert!(!updated);
    }
}
