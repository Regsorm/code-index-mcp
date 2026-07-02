// Trait `LanguageProcessor` — описание одного языка для core.
//
// На него опирается:
// 1. Auto-detect: для каждого пути из `daemon.toml` вызывается `detects`
//    у всех зарегистрированных процессоров.
// 2. Conditional registration MCP-tools: множество активных языков
//    вычисляется как `{ proc.name() | proc.detects(repo_root) }` для
//    всех `[[paths]]`. В `tools/list` идут все `additional_tools()` от
//    активных процессоров плюс универсальные core tools.
// 3. Парсинг исходников: index walker ищет процессор через
//    `parser_for_extension` и парсит файл.
// 4. SQLite-схема: расширения добавляют свои таблицы через
//    `schema_extensions()` — выполняется один раз при открытии БД.
//
// `StandardLanguageProcessor` ниже — generic-обёртка вокруг существующих
// `LanguageParser`-ов из core (Python/Rust/JS/TS/Java/Go/BSL). Никаких
// схем не добавляет, специфичных tools не имеет.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::parser::types::ParseResult;
use crate::parser::LanguageParser;
use crate::storage::Storage;

use super::tool::IndexTool;

/// Контекст одного распарсенного файла для сборщика extras в фазе парсинга.
/// `parse_result` присутствует только для code-файлов (у text-файлов — None).
pub struct ParsedFileCtx<'a> {
    pub rel_path: &'a str,
    pub language: &'a str,
    pub content: &'a str,
    pub parse_result: Option<&'a ParseResult>,
}

/// Сборщик специфичного «сырья» в фазе ПАРАЛЛЕЛЬНОГО парсинга ядра.
///
/// Реализуется расширением языка (bsl-extension), чтобы вытащить свои данные
/// (обращения к объектам, комментарии процедур, метаданные XML), пока
/// содержимое файла ещё горячее в RAM — вместо повторного чтения диска в
/// `index_extras` после парсинга. Для универсальных сборок сборщика нет.
///
/// `on_parsed` зовётся из rayon `par_iter` (обязан быть `Send + Sync` и копить
/// результаты потоко-безопасно). `write` вызывается СЕРИЙНО после фазы записи
/// ядра — сбрасывает накопленное в БД (SQLite однопоточен на запись).
pub trait ParseExtrasCollector: Send + Sync {
    /// Обработать один распарсенный файл (параллельно). Только накопление
    /// в потоко-безопасные буферы — без записи в БД.
    fn on_parsed(&self, ctx: ParsedFileCtx);

    /// Сбросить накопленное сырьё в БД. Серийно, после фазы записи ядра.
    fn write(&self, storage: &mut Storage) -> anyhow::Result<()>;
}

/// Описание одного языка/расширения для code-index.
///
/// Реализации должны быть `Send + Sync` — экземпляры шарятся между
/// потоками индексации (rayon) и MCP-сессиями.
pub trait LanguageProcessor: Send + Sync {
    /// Стабильное имя языка. Совпадает с `LanguageParser::language_name()`,
    /// чтобы упростить взаимоотображение. Используется как ключ в
    /// `daemon.toml` (`language = "..."`) и в `IndexTool::applicable_languages`.
    fn name(&self) -> &str;

    /// Парсер исходников. Может быть `None` если процессор обслуживает
    /// что-то нестандартное (только XML-метаданные, например).
    fn parser(&self) -> Option<&dyn LanguageParser> {
        None
    }

    /// Эвристика auto-detect: глядя на корень репо, сказать «да, это мой».
    /// Реализация по умолчанию — `false` (всегда требуется явное указание
    /// `language` в TOML); встроенные процессоры переопределяют.
    fn detects(&self, _repo_root: &Path) -> bool {
        false
    }

    /// Дополнительные SQL-DDL для SQLite-схемы (CREATE TABLE/INDEX/...).
    /// Применяются после базовой схемы core при открытии каждой БД репо
    /// этого языка. Пустой срез = нет специфичных таблиц.
    fn schema_extensions(&self) -> &[&str] {
        &[]
    }

    /// Идемпотентная миграция уже существующей БД ПЕРЕД `schema_extensions`.
    /// Догоняет колонки/таблицы, добавленные в новых версиях, которых нет в БД
    /// от старого бинарника: `CREATE TABLE IF NOT EXISTS` не добавляет колонку
    /// в существующую таблицу, а следующий `CREATE INDEX` по отсутствующей
    /// колонке рвёт весь DDL-батч `apply_schema_extensions`. Default — no-op.
    fn migrate_schema(&self, _conn: &rusqlite::Connection) -> anyhow::Result<()> {
        Ok(())
    }

    /// Дополнительные MCP-инструменты, поставляемые этим процессором.
    /// Регистрируются в `tools/list` если хотя бы один репо имеет
    /// `language = self.name()`.
    fn additional_tools(&self) -> Vec<Arc<dyn IndexTool>> {
        Vec::new()
    }

    /// Дополнительная индексация специфичных таблиц после основного
    /// прохода. Вызывается из команды `Index` уже после `Storage::flush_to_disk`,
    /// то есть на полностью открытой БД.
    ///
    /// Для bsl-extension здесь будет обход репо, парсинг XML-метаданных
    /// и заполнение `metadata_objects` / `metadata_forms` /
    /// `event_subscriptions` / `proc_call_graph`. Реализация по умолчанию
    /// — no-op (для всех универсальных процессоров).
    ///
    /// Sync, не async — Storage обёрнут rusqlite::Connection, который
    /// не Send. Если в будущем понадобится async — переходим на
    /// async_runtime::spawn_blocking в caller'е.
    fn index_extras(&self, _repo_root: &Path, _storage: &mut Storage) -> anyhow::Result<()> {
        Ok(())
    }

    /// Инкрементальное обновление специфичных таблиц для конкретных файлов
    /// батча watcher'а — вместо полного `index_extras`. `changed` —
    /// созданные/изменённые пути, `deleted` — удалённые. Вызывается из
    /// watcher-цикла демона после `commit_batch` на полностью открытой БД
    /// (реализация ведёт свои BEGIN/COMMIT внутри).
    ///
    /// Реализация по умолчанию — no-op (универсальные процессоры специфичных
    /// таблиц не имеют). BSL переопределяет: slice-rebuild затронутых слоёв
    /// графа вызовов + per-object апдейт XML-слоёв (data_links / структура /
    /// формы / подписки) только для реально изменённых объектов.
    fn index_extras_for_files(
        &self,
        _repo_root: &Path,
        _storage: &mut Storage,
        _changed: &[PathBuf],
        _deleted: &[PathBuf],
    ) -> anyhow::Result<()> {
        Ok(())
    }

    /// Наполнены ли специфичные extras-таблицы процессора в текущей БД.
    /// Используется демоном на старте: если `full_reindex` по mtime не нашёл
    /// изменений (0 проиндексировано / 0 удалено) И extras уже на месте —
    /// полный `index_extras` пропускается (он дорогой: перестроение
    /// metadata_*/terms/графа занимает минуты, а делать его при неизменных
    /// данных незачем). Реализация по умолчанию — `false` (универсальные
    /// процессоры extras не имеют; их `index_extras` и так no-op, поэтому
    /// гейт для них роли не играет). BSL переопределяет проверкой непустоты
    /// ключевых таблиц.
    ///
    /// ВНИМАНИЕ: метод не отслеживает СХЕМУ extras. Если релиз добавил новую
    /// extras-таблицу, при неизменных данных она останется пустой — для таких
    /// релизов нужен разовый полный пересбор (`index --force` или ребилд БД).
    fn extras_present(&self, _storage: &Storage) -> bool {
        false
    }

    /// Сборщик extras для фазы ПАРАЛЛЕЛЬНОГО парсинга (только полный путь
    /// индексации). `None` (по умолчанию) — расширение в парсинге не
    /// участвует, extras целиком делаются в `index_extras` после. bsl-extension
    /// возвращает сборщик, вытаскивающий code_usages/термы/метаданные во время
    /// парсинга (содержимое горячее в RAM, диск не перечитывается).
    /// Инкрементальный путь (`index_extras_for_files`) сборщик НЕ использует.
    fn parse_collector(&self) -> Option<Box<dyn ParseExtrasCollector>> {
        None
    }
}

/// Реестр зарегистрированных `LanguageProcessor`-ов. Заполняется в
/// `main()` каждого крейта-bin: `code-index` регистрирует только
/// универсальные процессоры, `bsl-indexer` (приватный) добавляет ещё
/// `BslLanguageProcessor` из крейта `bsl-extension`.
#[derive(Default)]
pub struct ProcessorRegistry {
    processors: Vec<Arc<dyn LanguageProcessor>>,
}

impl ProcessorRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, p: Arc<dyn LanguageProcessor>) {
        self.processors.push(p);
    }

    pub fn iter(&self) -> impl Iterator<Item = &Arc<dyn LanguageProcessor>> {
        self.processors.iter()
    }

    /// Поиск процессора по имени. Используется при разрешении языка
    /// репо (после auto-detect или явного указания в TOML).
    pub fn get(&self, name: &str) -> Option<&Arc<dyn LanguageProcessor>> {
        self.processors.iter().find(|p| p.name() == name)
    }

    /// Auto-detect: первый процессор, для которого `detects(root)` истина.
    /// Если подходящих несколько — побеждает зарегистрированный раньше.
    pub fn detect(&self, repo_root: &Path) -> Option<&Arc<dyn LanguageProcessor>> {
        self.processors.iter().find(|p| p.detects(repo_root))
    }

    /// Двухступенчатый resolve: сначала пробуем явное имя (если задано
    /// в `daemon.toml` через `language = "..."`), потом fallback на
    /// auto-detect по маркерам корня.
    ///
    /// Зачем: для репо вида «выгрузка обработок без Configuration.xml в
    /// корне» (`wms`, кастомные сборки) auto-detect не сработает, но
    /// пользователь явно указал `language = "bsl"` — мы должны это
    /// уважать и применить bsl-специфичные schema_extensions, иначе
    /// BSL-tools падают с `no such table: metadata_objects`.
    pub fn resolve(
        &self,
        explicit_language: Option<&str>,
        repo_root: &Path,
    ) -> Option<&Arc<dyn LanguageProcessor>> {
        if let Some(name) = explicit_language {
            if let Some(p) = self.get(name) {
                return Some(p);
            }
        }
        self.detect(repo_root)
    }

    /// Все имена зарегистрированных языков — для логов и диагностики.
    pub fn names(&self) -> Vec<&str> {
        self.processors.iter().map(|p| p.name()).collect()
    }
}

// ── Стандартный процессор для встроенных в core языков ────────────────────
//
// Зачем generic: у `LanguageParser::language_name()` возвращается `&str`
// — ссылка с lifetime парсера. Чтобы хранить `name` в struct и отдавать
// `&str` через trait, надо либо сделать `Box<dyn LanguageParser>` и
// проксировать, либо сохранить `&'static str` отдельно. Второе проще.

/// Generic-обёртка вокруг существующего `LanguageParser` из core, добавляющая
/// поведение auto-detect через закрепление функции `detects`.
///
/// Использовать через статические конструкторы: `python()`, `rust()` и т.д.
pub struct StandardLanguageProcessor {
    name: &'static str,
    parser: Box<dyn LanguageParser>,
    detects_fn: fn(&Path) -> bool,
}

impl StandardLanguageProcessor {
    /// Сборка процессора из готовых компонентов. Используется в фабриках
    /// ниже (`python()`, `rust()`, ...) и в тестах.
    pub fn new(
        name: &'static str,
        parser: Box<dyn LanguageParser>,
        detects_fn: fn(&Path) -> bool,
    ) -> Self {
        Self { name, parser, detects_fn }
    }

    pub fn python() -> Self {
        Self {
            name: "python",
            parser: Box::new(crate::parser::python::PythonParser::new()),
            detects_fn: detect_python,
        }
    }

    pub fn rust() -> Self {
        Self {
            name: "rust",
            parser: Box::new(crate::parser::rust_lang::RustParser::new()),
            detects_fn: detect_rust,
        }
    }

    pub fn go() -> Self {
        Self {
            name: "go",
            parser: Box::new(crate::parser::go::GoParser::new()),
            detects_fn: detect_go,
        }
    }

    pub fn java() -> Self {
        Self {
            name: "java",
            parser: Box::new(crate::parser::java::JavaParser::new()),
            detects_fn: detect_java,
        }
    }

    pub fn javascript() -> Self {
        Self {
            name: "javascript",
            parser: Box::new(crate::parser::javascript::JavaScriptParser::new()),
            detects_fn: detect_javascript,
        }
    }

    pub fn typescript() -> Self {
        Self {
            name: "typescript",
            parser: Box::new(crate::parser::typescript::TypeScriptParser::new()),
            detects_fn: detect_typescript,
        }
    }

    /// BSL-парсер пока остаётся в core (на этапе 2 переедет в bsl-extension
    /// вместе с XML-парсером метаданных и MCP-tools 1С).
    pub fn bsl() -> Self {
        Self {
            name: "bsl",
            parser: Box::new(crate::parser::bsl::BslParser::new()),
            detects_fn: detect_bsl,
        }
    }
}

impl LanguageProcessor for StandardLanguageProcessor {
    fn name(&self) -> &str {
        self.name
    }

    fn parser(&self) -> Option<&dyn LanguageParser> {
        Some(self.parser.as_ref())
    }

    fn detects(&self, repo_root: &Path) -> bool {
        (self.detects_fn)(repo_root)
    }
}

// ── Detect-функции для встроенных языков ──────────────────────────────────
//
// Эвристики совпадают с `daemon_core::language_detect::detect_by_root_markers`,
// но с разделением по языку — каждый процессор знает только о своих маркерах.
// Это даёт композируемость: bsl-extension добавит свой `detect_bsl_extended`
// (с проверкой Configuration.xml + Ext-папок), не трогая core.

fn detect_python(root: &Path) -> bool {
    root.join("pyproject.toml").is_file() || root.join("setup.py").is_file()
}

fn detect_rust(root: &Path) -> bool {
    root.join("Cargo.toml").is_file()
}

fn detect_go(root: &Path) -> bool {
    root.join("go.mod").is_file()
}

fn detect_java(root: &Path) -> bool {
    root.join("pom.xml").is_file()
        || root.join("build.gradle").is_file()
        || root.join("build.gradle.kts").is_file()
}

fn detect_javascript(root: &Path) -> bool {
    // package.json без tsconfig.json — JS-проект.
    root.join("package.json").is_file() && !root.join("tsconfig.json").is_file()
}

fn detect_typescript(root: &Path) -> bool {
    root.join("package.json").is_file() && root.join("tsconfig.json").is_file()
}

fn detect_bsl(root: &Path) -> bool {
    root.join("Configuration.xml").is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn touch(dir: &Path, name: &str) {
        std::fs::File::create(dir.join(name)).unwrap();
    }

    #[test]
    fn registry_finds_processor_by_name() {
        let mut reg = ProcessorRegistry::new();
        reg.register(Arc::new(StandardLanguageProcessor::python()));
        reg.register(Arc::new(StandardLanguageProcessor::rust()));
        assert!(reg.get("python").is_some());
        assert!(reg.get("rust").is_some());
        assert!(reg.get("brainfuck").is_none());
    }

    #[test]
    fn registry_auto_detects_by_marker_files() {
        let mut reg = ProcessorRegistry::new();
        reg.register(Arc::new(StandardLanguageProcessor::python()));
        reg.register(Arc::new(StandardLanguageProcessor::rust()));
        reg.register(Arc::new(StandardLanguageProcessor::bsl()));

        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "Cargo.toml");
        let detected = reg.detect(tmp.path()).map(|p| p.name());
        assert_eq!(detected, Some("rust"));
    }

    #[test]
    fn typescript_takes_priority_when_tsconfig_present() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "package.json");
        touch(tmp.path(), "tsconfig.json");

        // Регистрируем JS первым — он увидит package.json без tsconfig.
        // С tsconfig только TS-detector сработает, т.е. JS не должен.
        let js = StandardLanguageProcessor::javascript();
        let ts = StandardLanguageProcessor::typescript();
        assert!(!js.detects(tmp.path()));
        assert!(ts.detects(tmp.path()));
    }

    #[test]
    fn standard_processor_exposes_parser() {
        let p = StandardLanguageProcessor::python();
        let parser = p.parser().expect("Python должен иметь парсер");
        assert_eq!(parser.language_name(), "python");
        assert!(parser.file_extensions().contains(&"py"));
    }

    #[test]
    fn default_no_extra_tools_or_schema() {
        let p = StandardLanguageProcessor::rust();
        assert!(p.additional_tools().is_empty());
        assert!(p.schema_extensions().is_empty());
    }

    #[test]
    fn resolve_prefers_explicit_language_even_without_marker() {
        // Конфиг говорит «bsl», но Configuration.xml в корне нет —
        // resolve всё равно должен вернуть BSL-процессор.
        let mut reg = ProcessorRegistry::new();
        reg.register(Arc::new(StandardLanguageProcessor::rust()));
        reg.register(Arc::new(StandardLanguageProcessor::bsl()));
        let tmp = TempDir::new().unwrap();
        let resolved = reg.resolve(Some("bsl"), tmp.path()).map(|p| p.name());
        assert_eq!(resolved, Some("bsl"));
    }

    #[test]
    fn resolve_falls_back_to_detect_when_no_explicit_language() {
        let mut reg = ProcessorRegistry::new();
        reg.register(Arc::new(StandardLanguageProcessor::rust()));
        reg.register(Arc::new(StandardLanguageProcessor::python()));
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "Cargo.toml");
        let resolved = reg.resolve(None, tmp.path()).map(|p| p.name());
        assert_eq!(resolved, Some("rust"));
    }

    #[test]
    fn resolve_falls_back_to_detect_when_explicit_unknown() {
        // Имя процессора не зарегистрировано — берём fallback на маркеры.
        let mut reg = ProcessorRegistry::new();
        reg.register(Arc::new(StandardLanguageProcessor::python()));
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "pyproject.toml");
        let resolved = reg.resolve(Some("brainfuck"), tmp.path()).map(|p| p.name());
        assert_eq!(resolved, Some("python"));
    }

    #[test]
    fn resolve_returns_none_when_nothing_matches() {
        let mut reg = ProcessorRegistry::new();
        reg.register(Arc::new(StandardLanguageProcessor::python()));
        let tmp = TempDir::new().unwrap();
        // Нет ни маркера, ни явного language → None.
        assert!(reg.resolve(None, tmp.path()).is_none());
    }
}
