// `BslLanguageProcessor` — точка входа bsl-extension в систему расширений
// code-index. На этапе 2 содержит только идентичность (имя «bsl») и
// auto-detect по Configuration.xml. Парсер исходников .bsl/.os уже есть
// в core (`code_index_core::parser::bsl`); пробрасываем его без изменений
// — на этапе 3 парсер переедет сюда вместе с XML-парсером метаданных.

use std::path::Path;
use std::sync::Arc;

use code_index_core::extension::{IndexTool, LanguageProcessor};
use code_index_core::parser::{bsl::BslParser, LanguageParser};
use code_index_core::storage::Storage;

/// Процессор языка 1С BSL. Реализует `LanguageProcessor` для регистрации
/// в `bsl-indexer`. На этапе 2 минимален — без специфичных SQL-схем и
/// без `additional_tools`.
pub struct BslLanguageProcessor {
    parser: BslParser,
}

impl BslLanguageProcessor {
    pub fn new() -> Self {
        Self { parser: BslParser::new() }
    }
}

impl Default for BslLanguageProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageProcessor for BslLanguageProcessor {
    fn name(&self) -> &str {
        "bsl"
    }

    fn parser(&self) -> Option<&dyn LanguageParser> {
        Some(&self.parser)
    }

    /// Auto-detect: наличие `Configuration.xml` в корне репо ЛИБО на
    /// глубине ≤ 2 уровней. Этот более либеральный маркер нужен для
    /// типичных multi-config git-репо вида:
    ///
    /// ```text
    /// MyRepo/
    /// ├── base/Configuration.xml          ← основная конфигурация
    /// ├── extensions/
    /// │   ├── EF_X/Configuration.xml      ← расширение
    /// │   └── EF_Y/Configuration.xml
    /// └── external/                        ← внешние обработки
    /// ```
    ///
    /// `index_extras` разбирает каждый найденный Configuration.xml как
    /// отдельный sub-config и собирает их объекты в одну таблицу
    /// `metadata_objects` (UNIQUE по `(repo, full_name)`, conflicts
    /// разрешаются `INSERT OR IGNORE` — заимствованные в расширениях
    /// объекты с тем же full_name пропускаются, base-версия сохраняется).
    ///
    /// Глубина ограничена 2 уровнями по двум причинам: (1) защита от
    /// случайных Configuration.xml глубоко внутри test-fixtures репо,
    /// (2) соответствие реальной layout-структуре git-репо 1С.
    fn detects(&self, repo_root: &Path) -> bool {
        // Быстрый путь: классическая single-config выгрузка.
        if repo_root.join("Configuration.xml").is_file() {
            return true;
        }
        // Рекурсивный путь — multi-config: base/, extensions/<name>/, ...
        walkdir::WalkDir::new(repo_root)
            .max_depth(3) // root=0, depth=1=base/, depth=2=extensions/<name>/, depth=3=Configuration.xml
            .min_depth(2)
            .into_iter()
            .filter_map(|e| e.ok())
            .any(|e| {
                e.file_type().is_file()
                    && e.file_name().to_str() == Some("Configuration.xml")
            })
    }

    /// SQLite-расширения схемы для конфигураций 1С: `metadata_objects`,
    /// `metadata_forms`, `event_subscriptions`. Применяются один раз при
    /// первом открытии БД репо с `language = "bsl"` (точка применения —
    /// в core, через `Storage::apply_schema_extensions`, на этапе 4).
    fn schema_extensions(&self) -> &[&str] {
        crate::schema::SCHEMA_EXTENSIONS
    }

    /// Идемпотентно догнать схему существующей БД до текущей версии расширений
    /// (новые `*_key`-колонки) ДО `apply_schema_extensions` — иначе `CREATE INDEX`
    /// по отсутствующей колонке рвёт DDL-батч на БД от старого бинарника.
    fn migrate_schema(&self, conn: &rusqlite::Connection) -> anyhow::Result<()> {
        crate::schema::migrate_extensions(conn)
    }

    /// MCP-tools, специфичные для конфигураций 1С. Регистрируются в
    /// MCP `tools/list` только если хотя бы у одного репо
    /// `language = "bsl"` (conditional registration этапа 1.5).
    fn additional_tools(&self) -> Vec<Arc<dyn IndexTool>> {
        vec![
            Arc::new(crate::tools::GetObjectStructureTool),
            Arc::new(crate::tools::GetFormHandlersTool),
            Arc::new(crate::tools::GetEventSubscriptionsTool),
            Arc::new(crate::tools::FindPathBslTool),
            // Граф связей данных (этап 1): окрестность объекта и путь между
            // объектами по ссылочным реквизитам/измерениям (data_links).
            Arc::new(crate::tools::GetDataLinksTool),
            Arc::new(crate::tools::FindDataPathTool),
            // Регистраторы регистра / движения документа (recorder-рёбра
            // data_links): «кто пишет в регистр» и «куда пишет документ».
            Arc::new(crate::tools::GetRegisterWritersTool),
            // search_terms — поиск по обогащённым termам (этап 5a).
            // Регистрируется всегда, даже без feature `enrichment`:
            // tool сам по себе read-only, не требует HTTP-клиента.
            // Если таблица пуста — просто вернёт {"results": []}.
            Arc::new(crate::tools::SearchTermsTool),
            // bsl_sql — произвольный read-only SELECT по index.db репо
            // («инструмент инструментов»: длинный хвост запросов по
            // метаданным/графам без отдельного named-tool). Guard:
            // только SELECT/WITH + Statement::readonly() + row-cap + таймаут.
            Arc::new(crate::tools::BslSqlTool),
            // get_object_profile — полный паспорт объекта за 1 вызов
            // (структура + формы + модули + связи данных). Горячий агрегат
            // вместо серии get_object_structure/get_form_handlers/get_data_links.
            Arc::new(crate::tools::GetObjectProfileTool),
            // find_references — «карта влияния»: всё, что ссылается на объект,
            // одним вызовом (реверс data_links + metadata_code_usages +
            // role_rights). Реверс get_data_links плюс код и права.
            Arc::new(crate::tools::FindReferencesTool),
        ]
    }

    /// Дополнительная индексация специфичных таблиц 1С после основного
    /// прохода (этап 4c). Парсит Configuration.xml/Forms/EventSubscriptions
    /// и заполняет `metadata_objects`/`metadata_forms`/`event_subscriptions`.
    /// Реализация в [`crate::index_extras::run_index_extras`].
    fn index_extras(
        &self,
        repo_root: &std::path::Path,
        storage: &mut Storage,
    ) -> anyhow::Result<()> {
        crate::index_extras::run_index_extras(repo_root, storage)
    }

    /// Инкрементальное обновление extras для файлов одного watcher-батча.
    /// Slice-rebuild затронутых слоёв графа вызовов + per-object/per-file
    /// апдейт XML-слоёв. Реализация в
    /// [`crate::index_extras::run_incremental_extras`].
    fn index_extras_for_files(
        &self,
        repo_root: &std::path::Path,
        storage: &mut Storage,
        changed: &[std::path::PathBuf],
        deleted: &[std::path::PathBuf],
    ) -> anyhow::Result<()> {
        crate::index_extras::run_incremental_extras(repo_root, storage, changed, deleted)
    }

    /// Extras считаются наполненными, когда непусты ОБЕ ключевые таблицы,
    /// которые гарантированно есть у нормального BSL-репо: `metadata_objects`
    /// (из Configuration.xml) и механические термы в `procedure_enrichment`
    /// (из .bsl). Любая отсутствует/пуста (старая схема, прерванный прогон) →
    /// `false` → демон сделает полный `index_extras`. Прочие extras
    /// (event_subscriptions/forms) не проверяем — они легитимно бывают пусты.
    fn extras_present(&self, storage: &Storage) -> bool {
        let conn = storage.conn();
        let meta: i64 = conn
            .query_row("SELECT COUNT(*) FROM metadata_objects", [], |r| r.get(0))
            .unwrap_or(0);
        if meta == 0 {
            return false;
        }
        let terms: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM procedure_enrichment WHERE signature LIKE 'mech:%'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        terms > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn name_is_bsl() {
        assert_eq!(BslLanguageProcessor::new().name(), "bsl");
    }

    #[test]
    fn parser_returns_bsl_language_parser() {
        let p = BslLanguageProcessor::new();
        let parser = p.parser().expect("BSL процессор должен иметь парсер");
        assert_eq!(parser.language_name(), "bsl");
        assert!(parser.file_extensions().contains(&"bsl"));
    }

    #[test]
    fn detects_configuration_xml() {
        let tmp = TempDir::new().unwrap();
        let p = BslLanguageProcessor::new();

        // Без маркера — не наш репо.
        assert!(!p.detects(tmp.path()));

        // С Configuration.xml — наш.
        std::fs::File::create(tmp.path().join("Configuration.xml")).unwrap();
        assert!(p.detects(tmp.path()));
    }

    #[test]
    fn detects_multi_config_layout() {
        // Реалистичная структура git-репо 1С:
        //   MyRepo/base/Configuration.xml
        //   MyRepo/extensions/EF_X/Configuration.xml
        //   MyRepo/external/...
        // — Configuration.xml в корне НЕТ, но рекурсивный поиск находит
        // вложенные. detects() должен вернуть true.
        let tmp = TempDir::new().unwrap();
        let p = BslLanguageProcessor::new();
        assert!(!p.detects(tmp.path()), "пустой репо — не наш");

        let base = tmp.path().join("base");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::File::create(base.join("Configuration.xml")).unwrap();
        assert!(p.detects(tmp.path()), "base/Configuration.xml — наш");

        // Дополним репо расширением — поведение остаётся true.
        let ext = tmp.path().join("extensions").join("EF_X");
        std::fs::create_dir_all(&ext).unwrap();
        std::fs::File::create(ext.join("Configuration.xml")).unwrap();
        assert!(p.detects(tmp.path()), "base + extensions — всё ещё наш");
    }

    #[test]
    fn detects_does_not_recurse_too_deep() {
        // Ограничение глубины: Configuration.xml на 4-м уровне (root/a/b/c/Cfg.xml)
        // не должен срабатывать как маркер — это защита от случайных
        // тестовых fixtures глубоко в дереве.
        let tmp = TempDir::new().unwrap();
        let deep = tmp.path().join("a").join("b").join("c");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::File::create(deep.join("Configuration.xml")).unwrap();
        let p = BslLanguageProcessor::new();
        assert!(!p.detects(tmp.path()), "слишком глубоко — не должны срабатывать");
    }

    #[test]
    fn schema_extensions_include_bsl_specific_tables() {
        // На этапе 3 у BslLanguageProcessor появились DDL-расширения:
        // metadata_objects, metadata_forms, event_subscriptions.
        // Сами tools пока пусты — они придут на этапе 6.
        let p = BslLanguageProcessor::new();
        let exts = p.schema_extensions();
        assert!(!exts.is_empty(), "schema_extensions не должен быть пуст");
        let joined = exts.concat();
        assert!(joined.contains("metadata_objects"));
        assert!(joined.contains("metadata_forms"));
        assert!(joined.contains("event_subscriptions"));
    }

    #[test]
    fn additional_tools_registered() {
        // 11 1С-tool'ов: 4 от метаданных + search_terms + 2 графа связей данных
        // + get_register_writers (регистраторы/движения) + bsl_sql (произвольный
        // read-only SELECT) + get_object_profile (паспорт объекта за 1 вызов)
        // + find_references (карта влияния: реверс data_links + код + права).
        let p = BslLanguageProcessor::new();
        let tools = p.additional_tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"get_object_structure"));
        assert!(names.contains(&"get_form_handlers"));
        assert!(names.contains(&"get_event_subscriptions"));
        assert!(names.contains(&"find_path_bsl"));
        assert!(names.contains(&"search_terms"));
        assert!(names.contains(&"get_data_links"));
        assert!(names.contains(&"find_data_path"));
        assert!(names.contains(&"get_register_writers"));
        assert!(names.contains(&"bsl_sql"));
        assert!(names.contains(&"get_object_profile"));
        assert!(names.contains(&"find_references"));
        assert_eq!(tools.len(), 11);
    }

    #[test]
    fn all_tools_are_bsl_specific() {
        // Каждый tool должен заявлять applicable_languages = ["bsl"]
        let p = BslLanguageProcessor::new();
        for tool in p.additional_tools() {
            let langs = tool.applicable_languages();
            assert_eq!(
                langs,
                Some(&["bsl"][..]),
                "tool '{}' должен быть привязан к bsl, а не к {:?}",
                tool.name(),
                langs
            );
        }
    }
}
