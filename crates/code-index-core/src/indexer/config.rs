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

    /// Phase 2 (v0.8.0): максимальный размер code-файла, content которого
    /// сохраняется в `file_contents` с zstd-сжатием. Файлы крупнее
    /// продолжают индексироваться по AST/FTS, но `read_file` для них
    /// вернёт `oversize=true` без content. Дефолт 5 МБ. Можно переопределить
    /// в `daemon.toml` (`[indexer].max_code_file_size_bytes` или
    /// `[[paths]].max_code_file_size_bytes`); worker присваивает эффективное
    /// значение этому полю перед запуском Indexer'а.
    #[serde(default = "default_max_code_file_size")]
    pub max_code_file_size_bytes: usize,

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

    /// Язык репозитория целиком (из `[[paths]] language` в daemon.toml либо
    /// автоопределения). Влияет только на неоднозначные расширения — сейчас
    /// это `.h`, который в C-проекте относится к C, а в C++-проекте к C++.
    /// `None` — считаем как раньше, по одной таблице расширений.
    #[serde(default)]
    pub repo_language: Option<String>,

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
    ///
    /// Для новой базы размер файла нулевой и ни о чём не говорит, поэтому в
    /// режиме "auto" демон оценивает будущий расход по весу исходников папки.
    #[serde(default = "default_storage_mode")]
    pub storage_mode: String,

    /// Максимальный процент свободной RAM, который разрешено занять под БД.
    ///
    /// Используется только при `storage_mode = "auto"`. По умолчанию 50%:
    /// половина СВОБОДНОЙ памяти, а не всей.
    #[serde(default = "default_memory_max_percent")]
    pub memory_max_percent: u8,

    /// Во сколько раз работа с базой в памяти обходится дороже, чем весят
    /// исходники папки. Используется только при `storage_mode = "auto"`.
    ///
    /// Решение принимается так:
    ///
    /// ```text
    /// ожидаемый расход = вес исходников × memory_estimate_factor
    /// разрешено занять = свободная память × memory_max_percent / 100
    ///
    /// ожидаемый расход ≤ разрешено занять → база собирается в памяти
    /// иначе                               → база сразу на диске
    /// ```
    ///
    /// Вес исходников — сумма размеров файлов, которые пойдут в индекс (с теми
    /// же исключениями каталогов, что и при индексации). Оба числа и принятое
    /// решение пишутся в журнал.
    ///
    /// По умолчанию 3 — это середина наблюдаемого разброса, а НЕ запас сверху.
    /// Замеры на разных папках дают примерно от 2 до 4 с лишним: 3,8 ГБ
    /// исходников → 8,6 ГБ израсходованной памяти (×2,3), 6,7 ГБ → 19,0 ГБ
    /// (×2,8), 5,3 ГБ → 18,6 ГБ (×3,5), 1,8 ГБ → 8,0 ГБ (×4,4). Разброс
    /// зависит от языка, плотности кода в файлах и состава папки, поэтому
    /// предсказать множитель заранее нельзя — он подбирается по журналу.
    ///
    /// Когда увеличивать (4 и выше): памяти мало, машина слабая, и уйти на
    /// диск безопаснее, чем рисковать нехваткой. Индексация будет медленнее,
    /// но предсказуемее. На слабых машинах это направление и есть основное.
    ///
    /// Когда уменьшать (2,0–2,5): памяти заведомо много и есть готовность
    /// сверяться с журналом. Больше папок пойдёт в память — индексация
    /// быстрее. Риск реальный: расход выше оценки встречается чаще, чем ниже,
    /// и при нехватке памяти работа с папкой прервётся.
    ///
    /// Ноль, отрицательное или нечисловое значение считается опиской — берётся
    /// умолчание.
    #[serde(default = "default_memory_estimate_factor")]
    pub memory_estimate_factor: f32,

    /// Задержка debounce для file watcher в миллисекундах.
    ///
    /// Ждёт `debounce_ms` тишины после последнего события, затем обрабатывает батч.
    /// По умолчанию 1500 мс.
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,

    /// Потолок накопления пачки изменений для наблюдателя, в миллисекундах.
    ///
    /// Обычно сбор заканчивается тишиной (`debounce_ms`) — тогда число
    /// изменившихся файлов точное. Потолок нужен на случай, когда поток
    /// событий не утихает вовсе: массовое обновление конфигурации идёт
    /// десятками секунд, и без запаса пачка резалась бы на куски, каждый из
    /// которых обрабатывался бы дорогим пофайловым путём. По умолчанию
    /// 30 000 мс.
    #[serde(default = "default_batch_ms")]
    pub batch_ms: u64,

    /// С какого числа изменившихся файлов пачку выгоднее обработать полным
    /// проходом, а не по одному файлу.
    ///
    /// Пофайловый путь платит за каждый файл (замер на типовой торговой
    /// конфигурации — около 49 мс), а полный проход стоит почти одинаково
    /// независимо от числа изменений (около 200 с, из них 176 — надстройка,
    /// которая перестраивает конфигурацию целиком). Поэтому на обычных
    /// правках частичный путь быстрее в десятки раз, и обгоняет его полный
    /// проход только на массовом обновлении.
    ///
    /// Замер на типовой торговой конфигурации (57 тыс. файлов, база на диске):
    /// 3 000 файлов — 110 с пофайлово против 210 с полным проходом; 5 000 —
    /// 200 против 230; 7 000 — 240 против 230. Равновесие около семи тысяч, и
    /// в самой точке разница ничтожна, поэтому по умолчанию берётся 8000 — с
    /// запасом. На слабой машине запас тем более уместен: полный проход
    /// обходит всё дерево и перестраивает индексы по всей базе, и от
    /// медленного диска страдает сильнее пофайлового пути.
    #[serde(default = "default_bulk_batch_threshold")]
    pub bulk_batch_threshold: usize,

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
    50
}

fn default_memory_estimate_factor() -> f32 {
    crate::indexer::DEFAULT_MEMORY_ESTIMATE_FACTOR
}

fn default_debounce_ms() -> u64 {
    1500
}

fn default_batch_ms() -> u64 {
    30_000
}

fn default_bulk_batch_threshold() -> usize {
    8000
}

fn default_flush_interval() -> u64 {
    30
}

fn default_max_file_size() -> usize {
    1_048_576 // 1 МБ
}

fn default_max_code_file_size() -> usize {
    5 * 1_048_576 // 5 МБ — Phase 2 (см. также DEFAULT_MAX_CODE_FILE_SIZE_BYTES в daemon_core::config)
}

fn default_batch_size() -> usize {
    2000
}

fn default_bulk_threshold() -> usize {
    10
}

/// Языки по умолчанию — все поддерживаемые
pub(crate) fn default_languages() -> Vec<String> {
    vec![
        "python".to_string(),
        "javascript".to_string(),
        "typescript".to_string(),
        "java".to_string(),
        "rust".to_string(),
        "go".to_string(),
        "bsl".to_string(),
        "php".to_string(),
        "c".to_string(),
        "cpp".to_string(),
        "csharp".to_string(),
        "ruby".to_string(),
        "swift".to_string(),
        // html регистрируется в ParserRegistry безусловно, здесь — для полноты списка
        "html".to_string(),
    ]
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            exclude_dirs: vec![],
            exclude_file_patterns: vec![],
            extra_text_extensions: vec![],
            max_file_size: default_max_file_size(),
            max_code_file_size_bytes: default_max_code_file_size(),
            max_files: 0,
            bulk_threshold: default_bulk_threshold(),
            languages: default_languages(),
            repo_language: None,
            batch_size: default_batch_size(),
            storage_mode: default_storage_mode(),
            memory_max_percent: default_memory_max_percent(),
            memory_estimate_factor: default_memory_estimate_factor(),
            debounce_ms: default_debounce_ms(),
            batch_ms: default_batch_ms(),
            bulk_batch_threshold: default_bulk_batch_threshold(),
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

    /// Во что примерно обойдётся работа в памяти с папкой такого веса, в байтах.
    ///
    /// Это левая часть расчёта, по которому в режиме `auto` выбирается место
    /// для базы: вес исходников, умноженный на [`IndexConfig::memory_estimate_factor`].
    /// Правая часть — разрешённая доля свободной памяти — считается в
    /// `storage::memory`.
    pub fn memory_estimate_bytes(&self, source_bytes: u64) -> u64 {
        // Приведение f64 → u64 в Rust насыщающее: переполнения не будет.
        (source_bytes as f64 * self.memory_estimate_factor_effective() as f64) as u64
    }

    /// Множитель, который будет применён на самом деле.
    ///
    /// Ноль, отрицательное или нечисловое значение в файле настроек — описка.
    /// Молча взять его нельзя: нулевая оценка увела бы папку на диск, и человек
    /// списал бы это на сам расчёт, а не на свою опечатку. Берётся умолчание, и
    /// в журнале видно, с каким числом на самом деле считали.
    pub fn memory_estimate_factor_effective(&self) -> f32 {
        if self.memory_estimate_factor.is_finite() && self.memory_estimate_factor > 0.0 {
            self.memory_estimate_factor
        } else {
            crate::indexer::DEFAULT_MEMORY_ESTIMATE_FACTOR
        }
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
    fn test_memory_estimate_default_factor() {
        let cfg = IndexConfig::default();
        assert_eq!(cfg.memory_estimate_factor, 3.0);
        // 1 ГБ исходников при множителе 3 — 3 ГБ ожидаемого расхода.
        assert_eq!(cfg.memory_estimate_bytes(1_000_000_000), 3_000_000_000);
    }

    #[test]
    fn test_memory_estimate_fractional_factor() {
        let cfg = IndexConfig {
            memory_estimate_factor: 1.8,
            ..Default::default()
        };
        // Дробный множитель хранится с одинарной точностью, поэтому на
        // гигабайте оценка расходится с точным произведением на десятки байт.
        // Для решения «память или диск» это безразлично, сверяем с допуском.
        let expected = 1_800_000_000_i64;
        let got = cfg.memory_estimate_bytes(1_000_000_000) as i64;
        assert!(
            (got - expected).abs() < 1_000,
            "оценка {} слишком далека от {}",
            got,
            expected
        );
    }

    #[test]
    fn test_memory_estimate_bad_factor_falls_back_to_default() {
        // Ноль, отрицательное и нечисловое — описка в файле настроек: берётся
        // умолчание, а не молчаливый уход на диск по нулевой оценке.
        for bad in [0.0, -2.0, f32::NAN, f32::INFINITY] {
            let cfg = IndexConfig {
                memory_estimate_factor: bad,
                ..Default::default()
            };
            assert_eq!(cfg.memory_estimate_factor_effective(), 3.0);
            assert_eq!(cfg.memory_estimate_bytes(1_000_000_000), 3_000_000_000);
        }
    }

    #[test]
    fn test_memory_estimate_factor_absent_in_file() {
        // Файл настроек, написанный до появления настройки, читается как прежде.
        let cfg: IndexConfig = serde_json::from_str(r#"{"storage_mode":"auto"}"#).unwrap();
        assert_eq!(cfg.memory_estimate_factor, 3.0);
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
