// Формат и чтение конфигурации демона (daemon.toml).
//
// Пример содержимого:
//
// ```toml
// [daemon]
// http_host = "127.0.0.1"    # опционально, по умолчанию loopback
// http_port = 0              # 0 = автовыбор свободного порта
// log_level = "info"
//
// [[paths]]
// path = "C:\\RepoUT"
//
// [[paths]]
// path = "C:\\RepoBP_SS"
// debounce_ms = 2000         # опциональное переопределение per-папка
// ```

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Полная конфигурация демона, прочитанная из `daemon.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DaemonFileConfig {
    /// Общие настройки демона. Отсутствие секции → значения по умолчанию.
    #[serde(default)]
    pub daemon: DaemonSection,

    /// Список отслеживаемых папок.
    #[serde(default, rename = "paths")]
    pub paths: Vec<PathEntry>,
}

/// Секция `[daemon]` из конфига.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonSection {
    /// Хост HTTP-сервера демона (loopback по умолчанию).
    #[serde(default = "default_http_host")]
    pub http_host: String,

    /// Порт HTTP-сервера. `0` означает «выбрать свободный автоматически»
    /// и записать фактический порт в runtime_info_file().
    #[serde(default)]
    pub http_port: u16,

    /// Уровень логирования. Перекрывается переменной RUST_LOG.
    #[serde(default = "default_log_level")]
    pub log_level: String,

    /// Сколько папок одновременно в фазе `initial_indexing`.
    /// `1` (по умолчанию) — последовательно, безопасно даже для HDD и при
    /// большом количестве папок. `0` — без ограничений, фаза стартует у всех
    /// параллельно (старое поведение). Ограничение действует ТОЛЬКО на
    /// initial reindex; watcher-события у уже `ready`-папок обрабатываются
    /// параллельно всегда.
    #[serde(default = "default_max_concurrent_initial")]
    pub max_concurrent_initial: usize,
}

impl Default for DaemonSection {
    fn default() -> Self {
        Self {
            http_host: default_http_host(),
            http_port: 0,
            log_level: default_log_level(),
            max_concurrent_initial: default_max_concurrent_initial(),
        }
    }
}

fn default_max_concurrent_initial() -> usize {
    1
}

fn default_http_host() -> String {
    "127.0.0.1".to_string()
}

fn default_log_level() -> String {
    "info".to_string()
}

/// Отдельная папка в `[[paths]]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathEntry {
    /// Абсолютный путь к папке. Относительные пути не поддерживаются —
    /// демон работает как системный процесс без предсказуемого cwd.
    pub path: PathBuf,

    /// Переопределение debounce для этой папки. `None` — использовать
    /// значение из `.code-index/config.json` проекта.
    #[serde(default)]
    pub debounce_ms: Option<u64>,

    /// Переопределение batch_ms для этой папки.
    #[serde(default)]
    pub batch_ms: Option<u64>,

    /// Псевдоним репозитория для MCP-сервера (параметр `repo` в tool-call).
    /// Поле используется `code-index serve --config ...`; демон его игнорирует.
    /// Если не задан — вычисляется из последнего сегмента `path`
    /// (см. [`PathEntry::effective_alias`]).
    #[serde(default)]
    pub alias: Option<String>,
}

impl PathEntry {
    /// Эффективный алиас репо: явный из TOML либо нормализованное имя
    /// последнего сегмента пути (нижний регистр, пробелы → `_`).
    /// Для пустого пути — `"default"`.
    pub fn effective_alias(&self) -> String {
        if let Some(a) = &self.alias {
            return a.clone();
        }
        self.path
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.to_ascii_lowercase().replace(' ', "_"))
            .unwrap_or_else(|| "default".to_string())
    }
}

/// Прочитать конфиг с указанного пути. Ошибка чтения/парсинга прокидывается наверх.
pub fn load_from(path: &Path) -> anyhow::Result<DaemonFileConfig> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("Не удалось прочитать {}: {}", path.display(), e))?;
    parse_str(&text)
}

/// Разобрать конфиг из строки. Используется в тестах.
pub fn parse_str(text: &str) -> anyhow::Result<DaemonFileConfig> {
    toml::from_str(text)
        .map_err(|e| anyhow::anyhow!("Ошибка парсинга daemon.toml: {}", e))
}

/// Загрузить конфиг по пути `$CODE_INDEX_HOME/daemon.toml`. Если файла нет —
/// возвращается пустая конфигурация (демон поднимется, но ничего не индексирует —
/// пользователь должен создать `daemon.toml` или вызвать `daemon reload`).
/// Если `CODE_INDEX_HOME` не задана — возвращает ошибку с инструкцией установки.
pub fn load_or_default() -> anyhow::Result<DaemonFileConfig> {
    let path = super::paths::config_path()?;
    if !path.exists() {
        return Ok(DaemonFileConfig::default());
    }
    load_from(&path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_values_when_sections_missing() {
        let cfg: DaemonFileConfig = parse_str("").unwrap();
        assert_eq!(cfg.daemon.http_host, "127.0.0.1");
        assert_eq!(cfg.daemon.http_port, 0);
        assert_eq!(cfg.daemon.log_level, "info");
        assert!(cfg.paths.is_empty());
    }

    #[test]
    fn parses_path_list() {
        let text = r#"
            [daemon]
            http_port = 61782

            [[paths]]
            path = "/tmp/a"

            [[paths]]
            path = "/tmp/b"
            debounce_ms = 2500
        "#;
        let cfg = parse_str(text).unwrap();
        assert_eq!(cfg.daemon.http_port, 61782);
        assert_eq!(cfg.paths.len(), 2);
        assert_eq!(cfg.paths[0].path, PathBuf::from("/tmp/a"));
        assert_eq!(cfg.paths[1].debounce_ms, Some(2500));
        // alias по-умолчанию отсутствует — старые конфиги продолжают работать.
        assert!(cfg.paths[0].alias.is_none());
    }

    #[test]
    fn parses_explicit_alias() {
        let text = r#"
            [[paths]]
            path = "C:/Выгрузка обработок"
            alias = "widgets"

            [[paths]]
            path = "C:/RepoUT"
        "#;
        let cfg = parse_str(text).unwrap();
        // Явный алиас из TOML.
        assert_eq!(cfg.paths[0].alias.as_deref(), Some("widgets"));
        assert_eq!(cfg.paths[0].effective_alias(), "widgets");
        // Без явного алиаса — последний сегмент в нижнем регистре.
        assert_eq!(cfg.paths[1].alias, None);
        assert_eq!(cfg.paths[1].effective_alias(), "repout");
    }

    #[test]
    fn effective_alias_normalizes_spaces() {
        let entry = PathEntry {
            path: PathBuf::from("C:/Some Folder Name"),
            debounce_ms: None,
            batch_ms: None,
            alias: None,
        };
        assert_eq!(entry.effective_alias(), "some_folder_name");
    }
}
