use serde::{Deserialize, Serialize};
use std::path::Path;
use anyhow::Result;
use globset::{Glob, GlobSet, GlobSetBuilder};

/// Конфигурация индексатора для проекта
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexConfig {
    /// Дополнительные директории для исключения (кроме стандартных)
    #[serde(default)]
    pub exclude_dirs: Vec<String>,

    /// Glob-паттерны имён файлов для исключения (например: "*.tmp.*", "*.bak", "*.orig").
    /// Матчится имя файла (basename), не полный путь.
    #[serde(default)]
    pub exclude_file_patterns: Vec<String>,

    /// Дополнительные расширения для FTS-индексации
    #[serde(default)]
    pub extra_text_extensions: Vec<String>,

    /// Максимальный размер текстового файла для индексации (в байтах, по умолчанию 1 МБ).
    /// Не применяется к файлам исходного кода — они индексируются независимо от размера.
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

    /// Размер батча транзакций при индексации (по умолчанию 500).
    ///
    /// Каждые `batch_size` файлов накопленные INSERT-ы коммитятся одной транзакцией,
    /// что устраняет fsync на каждую запись и ускоряет массовую индексацию.
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,

    /// Режим хранения SQLite: "auto" | "memory" | "disk".
    ///
    /// "auto" — автоматически выбирает in-memory если БД помещается в RAM,
    /// иначе работает с файлом. "memory" — всегда in-memory. "disk" — всегда файл.
    #[serde(default = "default_storage_mode")]
    pub storage_mode: String,

    /// Максимальный процент свободной RAM, который разрешено занять под БД.
    ///
    /// Используется только при `storage_mode = "auto"`. По умолчанию 25%.
    #[serde(default = "default_memory_max_percent")]
    pub memory_max_percent: u8,

    /// Задержка debounce для file watcher в миллисекундах.
    ///
    /// Ждёт `debounce_ms` тишины после последнего события, затем обрабатывает батч.
    /// По умолчанию 1500 мс.
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,

    /// Максимальное время накопления батча для file watcher в миллисекундах.
    ///
    /// Даже при непрерывных событиях батч обрабатывается через `batch_ms`.
    /// По умолчанию 2000 мс.
    #[serde(default = "default_batch_ms")]
    pub batch_ms: u64,

    /// Интервал периодической записи БД на диск в секундах (для daemon).
    ///
    /// По умолчанию 30 секунд.
    #[serde(default = "default_flush_interval")]
    pub flush_interval_sec: u64,
}

fn default_storage_mode() -> String {
    "auto".to_string()
}

fn default_memory_max_percent() -> u8 {
    25
}

fn default_debounce_ms() -> u64 {
    1500
}

fn default_batch_ms() -> u64 {
    2000
}

fn default_flush_interval() -> u64 {
    30
}

fn default_max_file_size() -> usize {
    1_048_576 // 1 МБ
}

fn default_batch_size() -> usize {
    2000
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
        "go".to_string(),
        "bsl".to_string(),
    ]
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            exclude_dirs: vec![],
            exclude_file_patterns: vec![],
            extra_text_extensions: vec![],
            max_file_size: default_max_file_size(),
            max_files: 0,
            bulk_threshold: default_bulk_threshold(),
            languages: default_languages(),
            batch_size: default_batch_size(),
            storage_mode: default_storage_mode(),
            memory_max_percent: default_memory_max_percent(),
            debounce_ms: default_debounce_ms(),
            batch_ms: default_batch_ms(),
            flush_interval_sec: default_flush_interval(),
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

    /// Скомпилировать GlobSet из exclude_file_patterns для последующего быстрого матчинга.
    /// Некорректные паттерны логируются в stderr и пропускаются.
    /// Если список пуст — возвращается пустой GlobSet, который ничего не матчит.
    pub fn build_file_exclude_matcher(&self) -> GlobSet {
        let mut builder = GlobSetBuilder::new();
        for pat in &self.exclude_file_patterns {
            match Glob::new(pat) {
                Ok(g) => { builder.add(g); }
                Err(e) => {
                    eprintln!("[config] некорректный exclude_file_pattern '{}': {}", pat, e);
                }
            }
        }
        builder.build().unwrap_or_else(|e| {
            eprintln!("[config] GlobSetBuilder.build failed: {}", e);
            GlobSet::empty()
        })
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
