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
// [indexer]
// max_code_file_size_bytes = 5242880   # глобальный лимит content для code (5 МБ default)
//
// [[paths]]
// path = "C:\\RepoUT"
//
// [[paths]]
// path = "C:\\RepoBP_1"
// debounce_ms = 2000                   # опциональное переопределение per-папка
// max_code_file_size_bytes = 10485760  # этой папке — мягче (10 МБ)
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

    /// Опциональная конфигурация LLM-обогащения процедур (этап 5a, BSL).
    /// Отсутствие секции `[enrichment]` в TOML → `None`, фича выключена.
    /// Сама структура живёт в core (это просто описание полей TOML),
    /// логика выполнения — в `bsl_extension::enrichment` под cargo
    /// feature `enrichment`.
    #[serde(default)]
    pub enrichment: Option<EnrichmentConfig>,

    /// Опциональная глобальная секция `[indexer]` (Phase 2, v0.8.0).
    /// Сейчас содержит один параметр — `max_code_file_size_bytes`. Может расти.
    #[serde(default)]
    pub indexer: IndexerSection,
}

/// Дефолтный hardcoded-лимит размера code-файла, content которого сохраняется
/// в `file_contents` с zstd-сжатием. Файлы крупнее не получают content, но
/// продолжают индексироваться по AST/FTS. Подробности — в `IndexerSection`.
pub const DEFAULT_MAX_CODE_FILE_SIZE_BYTES: usize = 5 * 1024 * 1024; // 5 МБ

/// Секция `[indexer]` из конфига демона (Phase 2, v0.8.0).
///
/// Сейчас содержит только лимит на размер code-файла, content которого
/// будет сохранён в БД. Файлы крупнее лимита остаются полностью
/// проиндексированными по AST/FTS, но `read_file` для них вернёт
/// `oversize=true` без content; читать такие файлы — через
/// `get_function`/`get_class`/`grep_body` (тела функций/классов
/// хранятся отдельно и не подпадают под лимит).
///
/// Приоритет значения для конкретной папки:
///   1. `paths[i].max_code_file_size_bytes` — per-path override (если задано);
///   2. `[indexer].max_code_file_size_bytes` — глобальный override (если задано);
///   3. `DEFAULT_MAX_CODE_FILE_SIZE_BYTES` — hardcoded дефолт 5 МБ.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IndexerSection {
    /// Глобальный лимит размера code-файла для сохранения content.
    /// `None` → используется hardcoded дефолт 5 МБ.
    #[serde(default)]
    pub max_code_file_size_bytes: Option<usize>,
}

/// Секция `[enrichment]` из конфига демона.
///
/// Не используется напрямую в core — потребляется `bsl-extension`,
/// если та собрана с feature `enrichment`. Помещена здесь, чтобы
/// daemon.toml парсился одинаково независимо от того, какой бинарник
/// его читает (универсальный `code-index` или `bsl-indexer`).
///
/// Пример:
/// ```toml
/// [enrichment]
/// enabled = true
/// provider = "openai_compatible"
/// url = "https://openrouter.ai/api/v1/chat/completions"
/// model = "anthropic/claude-haiku-4.5"
/// api_key_env = "OPENROUTER_API_KEY"
/// batch_size = 20
/// prompt_template = "Опиши в 2-3 предложениях..."
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrichmentConfig {
    /// Главный тумблер фичи. `false` — конфиг загружен, но enrichment
    /// не запускается даже при `bsl-indexer enrich`. `true` — фича
    /// активна, ждёт явного вызова подкоманды.
    #[serde(default)]
    pub enabled: bool,

    /// Семейство протокола HTTP-клиента. Сейчас единственное значение —
    /// `"openai_compatible"` (POST /v1/chat/completions с messages-форматом).
    /// Добавление других провайдеров — отдельная задача.
    #[serde(default = "default_provider")]
    pub provider: String,

    /// Полный URL endpoint'а chat-completions. Примеры:
    ///   * `https://openrouter.ai/api/v1/chat/completions`
    ///   * `http://127.0.0.1:11434/v1/chat/completions` (Ollama локально)
    pub url: String,

    /// Имя модели в нотации провайдера. Примеры:
    ///   * `anthropic/claude-haiku-4.5` (OpenRouter)
    ///   * `qwen2.5:7b` (Ollama)
    pub model: String,

    /// Имя переменной окружения, из которой читается API-ключ. Если
    /// `None` — заголовок `Authorization: Bearer ...` не отправляется
    /// (для локальных провайдеров типа Ollama без авторизации).
    #[serde(default)]
    pub api_key_env: Option<String>,

    /// Сколько процедур обрабатывать параллельно одним проходом.
    /// При значении 20 одновременно открыто до 20 HTTP-соединений.
    #[serde(default = "default_batch_size")]
    pub batch_size: u32,

    /// Шаблон system-промпта, описывающий что должна вернуть модель.
    /// Текст процедуры подставляется в user-message; модель должна
    /// вернуть список ключевых терминов через запятую.
    #[serde(default = "default_prompt_template")]
    pub prompt_template: String,
}

impl Default for EnrichmentConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: default_provider(),
            url: String::new(),
            model: String::new(),
            api_key_env: None,
            batch_size: default_batch_size(),
            prompt_template: default_prompt_template(),
        }
    }
}

impl EnrichmentConfig {
    /// Канонический отпечаток конфигурации обогащения для
    /// `embedding_meta.enrichment_signature`. Меняется при смене
    /// провайдера или модели — `bsl-extension` использует это, чтобы
    /// детектировать рассинхрон с уже накопленными `procedure_enrichment`
    /// и предупредить оператора.
    pub fn signature(&self) -> String {
        format!("{}:{}", self.provider, self.model)
    }
}

fn default_provider() -> String {
    "openai_compatible".to_string()
}

fn default_batch_size() -> u32 {
    20
}

fn default_prompt_template() -> String {
    "Опиши в 2-3 предложениях, что делает эта 1С-процедура и какие \
     бизнес-термины она задействует. Верни только список ключевых \
     слов и фраз через запятую, без пояснений и нумерации."
        .to_string()
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

    /// Преобладающий язык репозитория. Опциональное на уровне TOML для
    /// обратной совместимости со старыми конфигами, но после первого
    /// старта демона оно будет заполнено для всех записей: либо явно
    /// оператором, либо результатом auto-detect с дозаписью обратно в
    /// TOML через `toml_edit` (см. модуль `language_detect`).
    ///
    /// Допустимые значения совпадают с `LanguageParser::language_name()`:
    /// `python`, `rust`, `go`, `java`, `javascript`, `typescript`, `bsl`.
    #[serde(default)]
    pub language: Option<String>,

    /// Per-path override лимита размера code-файла для сохранения content
    /// в `file_contents` (Phase 2). `None` → использовать глобальный
    /// `[indexer].max_code_file_size_bytes` либо hardcoded дефолт 5 МБ.
    #[serde(default)]
    pub max_code_file_size_bytes: Option<usize>,
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

    /// Эффективный лимит на размер code-файла для сохранения content.
    /// Приоритет: per-path → глобальный `[indexer]` → hardcoded дефолт 5 МБ.
    pub fn effective_max_code_file_size(&self, indexer: &IndexerSection) -> usize {
        self.max_code_file_size_bytes
            .or(indexer.max_code_file_size_bytes)
            .unwrap_or(DEFAULT_MAX_CODE_FILE_SIZE_BYTES)
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
            language: None,
            max_code_file_size_bytes: None,
        };
        assert_eq!(entry.effective_alias(), "some_folder_name");
    }

    #[test]
    fn parses_explicit_language() {
        let text = r#"
            [[paths]]
            path = "/srv/repos/ut"
            language = "bsl"

            [[paths]]
            path = "/srv/repos/myproject"
        "#;
        let cfg = parse_str(text).unwrap();
        assert_eq!(cfg.paths[0].language.as_deref(), Some("bsl"));
        // Без явного language — None (auto-detect отработает на старте демона).
        assert!(cfg.paths[1].language.is_none());
    }

    #[test]
    fn enrichment_section_is_optional() {
        // Старые конфиги без секции [enrichment] должны парситься как и раньше.
        let cfg: DaemonFileConfig = parse_str("").unwrap();
        assert!(cfg.enrichment.is_none(), "по умолчанию enrichment выключен");
    }

    #[test]
    fn parses_enrichment_section_with_required_fields() {
        let text = r#"
            [enrichment]
            enabled = true
            url = "https://openrouter.ai/api/v1/chat/completions"
            model = "anthropic/claude-haiku-4.5"
            api_key_env = "OPENROUTER_API_KEY"
        "#;
        let cfg = parse_str(text).unwrap();
        let e = cfg.enrichment.expect("секция [enrichment] разобралась");
        assert!(e.enabled);
        assert_eq!(e.provider, "openai_compatible");        // default
        assert_eq!(e.batch_size, 20);                       // default
        assert!(!e.prompt_template.is_empty(), "default-промпт не пуст");
        assert_eq!(e.url, "https://openrouter.ai/api/v1/chat/completions");
        assert_eq!(e.model, "anthropic/claude-haiku-4.5");
        assert_eq!(e.api_key_env.as_deref(), Some("OPENROUTER_API_KEY"));
        assert_eq!(
            e.signature(),
            "openai_compatible:anthropic/claude-haiku-4.5"
        );
    }

    #[test]
    fn indexer_section_default_when_missing() {
        let cfg: DaemonFileConfig = parse_str("").unwrap();
        // Секция [indexer] отсутствует → дефолтная (поле = None).
        assert!(cfg.indexer.max_code_file_size_bytes.is_none());
    }

    #[test]
    fn indexer_section_parses_global_limit() {
        let text = r#"
            [indexer]
            max_code_file_size_bytes = 10485760
        "#;
        let cfg = parse_str(text).unwrap();
        assert_eq!(cfg.indexer.max_code_file_size_bytes, Some(10_485_760));
    }

    #[test]
    fn path_entry_max_code_file_size_optional() {
        let text = r#"
            [[paths]]
            path = "/tmp/a"

            [[paths]]
            path = "/tmp/b"
            max_code_file_size_bytes = 2097152
        "#;
        let cfg = parse_str(text).unwrap();
        assert_eq!(cfg.paths[0].max_code_file_size_bytes, None);
        assert_eq!(cfg.paths[1].max_code_file_size_bytes, Some(2_097_152));
    }

    #[test]
    fn effective_max_code_file_size_priority() {
        // 1. Если задан per-path — он побеждает.
        let entry_with_override = PathEntry {
            path: PathBuf::from("/x"),
            debounce_ms: None,
            batch_ms: None,
            alias: None,
            language: None,
            max_code_file_size_bytes: Some(1024),
        };
        let indexer_with_global = IndexerSection {
            max_code_file_size_bytes: Some(2048),
        };
        assert_eq!(
            entry_with_override.effective_max_code_file_size(&indexer_with_global),
            1024,
            "per-path должен перекрывать глобальный"
        );

        // 2. Per-path не задан — берётся глобальный.
        let entry_no_override = PathEntry {
            path: PathBuf::from("/x"),
            debounce_ms: None,
            batch_ms: None,
            alias: None,
            language: None,
            max_code_file_size_bytes: None,
        };
        assert_eq!(
            entry_no_override.effective_max_code_file_size(&indexer_with_global),
            2048,
            "без per-path должен браться глобальный"
        );

        // 3. Ни один не задан — hardcoded дефолт 5 МБ.
        let indexer_empty = IndexerSection::default();
        assert_eq!(
            entry_no_override.effective_max_code_file_size(&indexer_empty),
            DEFAULT_MAX_CODE_FILE_SIZE_BYTES,
            "без override должен браться hardcoded дефолт"
        );
        assert_eq!(DEFAULT_MAX_CODE_FILE_SIZE_BYTES, 5 * 1024 * 1024);
    }

    #[test]
    fn enrichment_disabled_explicitly() {
        let text = r#"
            [enrichment]
            enabled = false
            url = "http://127.0.0.1:11434/v1/chat/completions"
            model = "qwen2.5:7b"
        "#;
        let cfg = parse_str(text).unwrap();
        let e = cfg.enrichment.expect("секция [enrichment] разобралась");
        assert!(!e.enabled);
        assert_eq!(e.signature(), "openai_compatible:qwen2.5:7b");
    }
}
