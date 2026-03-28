use serde::{Deserialize, Serialize};
use std::path::Path;
use anyhow::Result;

/// Конфигурация индексатора для проекта
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexConfig {
    /// Дополнительные директории для исключения (кроме стандартных)
    #[serde(default)]
    pub exclude_dirs: Vec<String>,

    /// Дополнительные расширения для FTS-индексации
    #[serde(default)]
    pub extra_text_extensions: Vec<String>,

    /// Максимальный размер файла для индексации (в байтах, по умолчанию 1 МБ)
    #[serde(default = "default_max_file_size")]
    pub max_file_size: usize,

    /// Максимальное количество файлов для индексации (0 = без лимита)
    #[serde(default)]
    pub max_files: usize,

    /// Порог количества файлов для включения bulk-load режима (по умолчанию 10).
    ///
    /// Если число файлов, требующих индексации, превышает этот порог —
    /// перед загрузкой удаляются индексы и триггеры, а после — пересоздаются.
    #[serde(default = "default_bulk_threshold")]
    pub bulk_threshold: usize,

    /// Активные языки для AST-парсинга (по умолчанию все).
    /// Допустимые значения: "python", "javascript", "typescript", "java"
    #[serde(default = "default_languages")]
    pub languages: Vec<String>,
}

fn default_max_file_size() -> usize {
    1_048_576 // 1 МБ
}

fn default_bulk_threshold() -> usize {
    10
}

/// Языки по умолчанию — все поддерживаемые
fn default_languages() -> Vec<String> {
    vec![
        "python".to_string(),
        "javascript".to_string(),
        "typescript".to_string(),
        "java".to_string(),
        "rust".to_string(),
        "bsl".to_string(),
    ]
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            exclude_dirs: vec![],
            extra_text_extensions: vec![],
            max_file_size: default_max_file_size(),
            max_files: 0,
            bulk_threshold: default_bulk_threshold(),
            languages: default_languages(),
        }
    }
}

impl IndexConfig {
    /// Загрузить конфигурацию из .code-index/config.json.
    /// Если файл не существует — вернуть конфиг по умолчанию.
    pub fn load(project_root: &Path) -> Result<Self> {
        let config_path = project_root.join(".code-index").join("config.json");
        if config_path.exists() {
            let content = std::fs::read_to_string(&config_path)?;
            let config: IndexConfig = serde_json::from_str(&content)?;
            Ok(config)
        } else {
            Ok(Self::default())
        }
    }

    /// Сохранить конфигурацию (для создания дефолтного файла)
    pub fn save(&self, project_root: &Path) -> Result<()> {
        let config_dir = project_root.join(".code-index");
        std::fs::create_dir_all(&config_dir)?;
        let config_path = config_dir.join("config.json");
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(config_path, content)?;
        Ok(())
    }

    /// Проверить, нужно ли исключить директорию
    pub fn is_excluded_dir(&self, dir_name: &str) -> bool {
        use crate::indexer::file_types::EXCLUDE_DIRS;
        EXCLUDE_DIRS.contains(&dir_name)
            || self.exclude_dirs.iter().any(|d| d == dir_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_default_config() {
        let cfg = IndexConfig::default();
        assert_eq!(cfg.max_file_size, 1_048_576);
        assert_eq!(cfg.max_files, 0);
        assert!(cfg.exclude_dirs.is_empty());
        assert!(cfg.extra_text_extensions.is_empty());
    }

    #[test]
    fn test_is_excluded_dir_standard() {
        let cfg = IndexConfig::default();
        // Стандартные директории всегда исключаются
        assert!(cfg.is_excluded_dir("node_modules"));
        assert!(cfg.is_excluded_dir(".git"));
        assert!(cfg.is_excluded_dir("target"));
        // Обычные директории не исключаются
        assert!(!cfg.is_excluded_dir("src"));
    }

    #[test]
    fn test_is_excluded_dir_custom() {
        let cfg = IndexConfig {
            exclude_dirs: vec!["vendor".to_string(), "tmp".to_string()],
            ..Default::default()
        };
        // Пользовательские директории исключаются
        assert!(cfg.is_excluded_dir("vendor"));
        assert!(cfg.is_excluded_dir("tmp"));
        // Стандартные по-прежнему исключаются
        assert!(cfg.is_excluded_dir("node_modules"));
        // Незаявленные — нет
        assert!(!cfg.is_excluded_dir("src"));
    }

    #[test]
    fn test_save_and_load() {
        let tmp = TempDir::new().unwrap();
        let cfg = IndexConfig {
            exclude_dirs: vec!["vendor".to_string()],
            max_file_size: 512_000,
            max_files: 100,
            ..Default::default()
        };
        cfg.save(tmp.path()).unwrap();

        let loaded = IndexConfig::load(tmp.path()).unwrap();
        assert_eq!(loaded.exclude_dirs, vec!["vendor"]);
        assert_eq!(loaded.max_file_size, 512_000);
        assert_eq!(loaded.max_files, 100);
    }

    #[test]
    fn test_load_missing_returns_default() {
        let tmp = TempDir::new().unwrap();
        let cfg = IndexConfig::load(tmp.path()).unwrap();
        assert_eq!(cfg.max_file_size, default_max_file_size());
    }
}
