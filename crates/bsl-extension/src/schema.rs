// SQLite-схема, специфичная для конфигураций 1С.
//
// Таблицы здесь дополняют базовую схему `code_index_core::storage::schema`.
// Они не реплицируют generic-классы (для них есть `classes` в core),
// а добавляют именно метаданные 1С: типы объектов, реквизиты, формы и
// их обработчики, подписки на события.
//
// На этапе 3 здесь только DDL — заполнение приходит на этапе 4 одновременно
// с графом вызовов. Сами таблицы создаются через
// `LanguageProcessor::schema_extensions()` при первом открытии БД
// репозитория с `language = "bsl"`.

/// CREATE TABLE / INDEX для специфичных 1С-таблиц.
/// Идемпотентно — все CREATE через IF NOT EXISTS.
pub const SCHEMA_EXTENSIONS: &[&str] = &[
    // ── metadata_objects ──────────────────────────────────────────────────
    // Один объект конфигурации 1С: справочник, документ, регистр и т.д.
    // `meta_type` — категория (Catalog / Document / InformationRegister / ...);
    // `attributes_json` — реквизиты, табличные части, ресурсы, измерения
    // в виде структурированного JSON (форма извлекается xml::configuration).
    //
    // `(repo, full_name)` уникален в пределах одного репо: full_name —
    // канонический идентификатор вида `Catalog.Контрагенты`.
    "
    CREATE TABLE IF NOT EXISTS metadata_objects (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        repo TEXT NOT NULL,
        full_name TEXT NOT NULL,
        meta_type TEXT NOT NULL,
        name TEXT NOT NULL,
        synonym TEXT,
        attributes_json TEXT,
        UNIQUE(repo, full_name)
    );
    ",
    "CREATE INDEX IF NOT EXISTS idx_metadata_objects_repo ON metadata_objects(repo);",
    "CREATE INDEX IF NOT EXISTS idx_metadata_objects_meta_type ON metadata_objects(repo, meta_type);",
    "CREATE INDEX IF NOT EXISTS idx_metadata_objects_name ON metadata_objects(name);",

    // ── metadata_forms ────────────────────────────────────────────────────
    // Управляемая форма объекта конфигурации. `owner_full_name` —
    // владелец формы (например, `Document.РеализацияТоваровУслуг`),
    // `form_name` — её имя (`ФормаДокумента`).
    //
    // `handlers_json` — список обработчиков событий формы:
    // [{event: "ПриОткрытии", handler: "ПриОткрытии"}, ...]
    // (имя метода не всегда совпадает с именем события — БСП-расширения
    // часто нацеливаются на свои handlers).
    "
    CREATE TABLE IF NOT EXISTS metadata_forms (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        repo TEXT NOT NULL,
        owner_full_name TEXT NOT NULL,
        form_name TEXT NOT NULL,
        handlers_json TEXT,
        UNIQUE(repo, owner_full_name, form_name)
    );
    ",
    "CREATE INDEX IF NOT EXISTS idx_metadata_forms_repo ON metadata_forms(repo);",
    "CREATE INDEX IF NOT EXISTS idx_metadata_forms_owner ON metadata_forms(repo, owner_full_name);",

    // ── event_subscriptions ───────────────────────────────────────────────
    // Подписка на события — связь «событие → процедура общего модуля».
    // Используется при построении графа вызовов: edge типа `subscription`
    // соединяет триггер платформы с реальным обработчиком.
    "
    CREATE TABLE IF NOT EXISTS event_subscriptions (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        repo TEXT NOT NULL,
        name TEXT NOT NULL,
        event TEXT NOT NULL,
        handler_module TEXT NOT NULL,
        handler_proc TEXT NOT NULL,
        sources_json TEXT,
        UNIQUE(repo, name)
    );
    ",
    "CREATE INDEX IF NOT EXISTS idx_event_subscriptions_repo ON event_subscriptions(repo);",
    "CREATE INDEX IF NOT EXISTS idx_event_subscriptions_handler ON event_subscriptions(repo, handler_module, handler_proc);",

    // ── proc_call_graph ───────────────────────────────────────────────────
    // Граф вызовов процедур/функций. Ребро — `(caller_proc_key, callee_*) +
    // тип ребра`.
    //
    // Типы рёбер (`call_type`):
    //   * `direct` — прямой вызов из BSL-кода (через AST core-парсера).
    //   * `subscription` — триггер платформы (запись документа) → handler
    //     общего модуля. Источник — таблица `event_subscriptions`.
    //   * `form_event` — событие формы (ПриОткрытии и т.п.) → процедура
    //     модуля формы. Источник — таблица `metadata_forms`.
    //   * `extension_override` — перехват в расширении (CFE). На этапе 4d
    //     не заполняется — нужен парсер расширения.
    //   * `external_assignment` — динамическое назначение через `Имя.Метод()`
    //     где `Имя` — переменная неопределённого типа. На этапе 4d не
    //     заполняется — требует runtime-анализа.
    //
    // `caller_proc_key` — стабильный идентификатор процедуры-вызывателя
    // (формат `<module>.<procedure>`). `callee_proc_key` — то же для
    // callee, NULL когда resolution неуспешен (имя не нашлось в индексе).
    // `callee_proc_name` — сырое имя как видно в источнике (для
    // последующего resolve в графе и для отображения).
    //
    // UNIQUE-ключ предотвращает дубликаты — повторное `index_extras`
    // не плодит лишних записей.
    "
    CREATE TABLE IF NOT EXISTS proc_call_graph (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        repo TEXT NOT NULL,
        caller_proc_key TEXT NOT NULL,
        callee_proc_name TEXT NOT NULL,
        callee_proc_key TEXT,
        call_type TEXT NOT NULL,
        UNIQUE(repo, caller_proc_key, callee_proc_name, call_type)
    );
    ",
    "CREATE INDEX IF NOT EXISTS idx_pcg_repo ON proc_call_graph(repo);",
    "CREATE INDEX IF NOT EXISTS idx_pcg_caller ON proc_call_graph(repo, caller_proc_key);",
    "CREATE INDEX IF NOT EXISTS idx_pcg_callee_name ON proc_call_graph(repo, callee_proc_name);",
    "CREATE INDEX IF NOT EXISTS idx_pcg_call_type ON proc_call_graph(repo, call_type);",

    // ── metadata_modules ──────────────────────────────────────────────────
    // Модули BSL (`Module.bsl`, `ManagerModule.bsl`, `ObjectModule.bsl`,
    // `Forms/.../Module.bsl` и т.д.), привязанные к стабильным
    // отладочным идентификаторам платформы 1С:
    //
    //   * `object_id`     — UUID объекта-владельца / формы
    //                       (атрибут `uuid` корневого элемента в XML
    //                       объекта или формы).
    //   * `property_id`   — UUID типа модуля (известная константа платформы,
    //                       одна из 11 — см. `module_constants::MODULE_TYPE_PROPERTY_ID`).
    //                       Для отладки не достаточно одного `object_id` —
    //                       платформа разделяет «модуль объекта» и
    //                       «модуль менеджера» одного и того же документа.
    //   * `config_version`— хеш версии из `ConfigDumpInfo.xml`. Меняется
    //                       при каждом изменении конфигурации; пара
    //                       `(object_id, config_version)` однозначно
    //                       идентифицирует модуль для протокола dbgs.
    //
    // Тройка `(object_id, property_id, config_version)` — точное
    // соответствие тому что отправляет в `setBreakpoint` наш сервис
    // `dbgs-debug` (и платформенный отладчик 1С в целом). Эта таблица
    // позволяет агентам ставить breakpoint'ы по человекочитаемому
    // имени модуля без обращения к live-ИБ.
    //
    // `code_path` — путь к `.bsl`-файлу относительно корня репо;
    // совпадает с `files.path` core-индекса, что упрощает джойны.
    // `extension_name` — имя расширения для CFE (например
    // `extensions/EF_00_00805744_2`); пустая строка для base.
    //
    // `(repo, full_name)` уникален; `full_name` имеет вид
    // `<MetaType>.<Name>.<ModuleType>`, например
    // `Document.РеализацияТоваровУслуг.ManagerModule`.
    "
    CREATE TABLE IF NOT EXISTS metadata_modules (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        repo TEXT NOT NULL,
        full_name TEXT NOT NULL,
        object_name TEXT NOT NULL,
        module_type TEXT NOT NULL,
        object_id TEXT NOT NULL,
        property_id TEXT NOT NULL,
        config_version TEXT,
        code_path TEXT,
        extension_name TEXT,
        UNIQUE(repo, full_name)
    );
    ",
    "CREATE INDEX IF NOT EXISTS idx_mm_repo ON metadata_modules(repo);",
    "CREATE INDEX IF NOT EXISTS idx_mm_object_name ON metadata_modules(repo, object_name);",
    "CREATE INDEX IF NOT EXISTS idx_mm_module_type ON metadata_modules(repo, module_type);",
    "CREATE INDEX IF NOT EXISTS idx_mm_object_id ON metadata_modules(object_id);",
    "CREATE INDEX IF NOT EXISTS idx_mm_extension ON metadata_modules(repo, extension_name);",

    // ── procedure_enrichment ──────────────────────────────────────────────
    // LLM-обогащение процедур бизнес-терминами (этап 5a).
    //
    // Хранится отдельной таблицей, а НЕ колонкой в core::functions.
    // Причины:
    //   * core не должен знать про enrichment — это фича `bsl-extension`;
    //   * LLM-вывод стабилен между перепарсингами (не привязан к node_hash
    //     функции), но привязан к стабильному `proc_key` (= module.proc),
    //     который используется и в `proc_call_graph`;
    //   * включение/выключение enrichment не требует ALTER core-таблицы.
    //
    // `proc_key` — стабильный ключ процедуры в пределах репо
    // (`<module_name>.<procedure_name>`, тот же формат, что
    // `caller_proc_key` в `proc_call_graph`).
    //
    // `terms` — список бизнес-терминов через запятую, как вернула LLM.
    // Это и есть основной канал для FTS-поиска через `search_terms`.
    //
    // `signature` — отпечаток конфигурации, которой обогащали именно эту
    // запись. При смене модели в `[enrichment]` старые записи остаются,
    // но новые строки получают новую подпись; команда `enrich --reenrich`
    // обновляет всё под текущую подпись.
    //
    // `updated_at` — Unix epoch в секундах. Заполняется явно из Rust
    // (а не DEFAULT через strftime), потому что у нас разный приоритет
    // — bulk-import ставит время батча, а не каждой строки.
    "
    CREATE TABLE IF NOT EXISTS procedure_enrichment (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        repo TEXT NOT NULL,
        proc_key TEXT NOT NULL,
        terms TEXT,
        signature TEXT,
        updated_at INTEGER NOT NULL DEFAULT 0,
        UNIQUE(repo, proc_key)
    );
    ",
    "CREATE INDEX IF NOT EXISTS idx_pe_repo ON procedure_enrichment(repo);",
    "CREATE INDEX IF NOT EXISTS idx_pe_proc_key ON procedure_enrichment(repo, proc_key);",

    // FTS5 виртуальная таблица для полнотекстового поиска по terms.
    // content='procedure_enrichment' + content_rowid='id' — стандартный
    // паттерн «external content» (как в core::fts_functions). Сама
    // виртуальная таблица не дублирует данные: хранит только
    // FTS-индекс, исходник в `procedure_enrichment`. Триггеры ниже
    // синхронизируют изменения автоматически.
    //
    // tokenize='unicode61 remove_diacritics 1' — разумный default
    // для русского + английского текста (Ё→е, кириллица учитывается
    // как буквы), без специфики порфера БСП.
    "
    CREATE VIRTUAL TABLE IF NOT EXISTS fts_procedure_enrichment USING fts5(
        terms,
        content='procedure_enrichment',
        content_rowid='id',
        tokenize='unicode61 remove_diacritics 1'
    );
    ",

    // Триггеры синхронизации FTS при INSERT/DELETE/UPDATE.
    // Аналог core::TRIGGERS_SQL для functions/classes — те же 3 события,
    // явное удаление-перед-вставкой при UPDATE.
    "
    CREATE TRIGGER IF NOT EXISTS pe_fts_insert
    AFTER INSERT ON procedure_enrichment BEGIN
        INSERT INTO fts_procedure_enrichment(rowid, terms)
        VALUES (new.id, new.terms);
    END;
    ",
    "
    CREATE TRIGGER IF NOT EXISTS pe_fts_delete
    AFTER DELETE ON procedure_enrichment BEGIN
        INSERT INTO fts_procedure_enrichment(fts_procedure_enrichment, rowid, terms)
        VALUES ('delete', old.id, old.terms);
    END;
    ",
    "
    CREATE TRIGGER IF NOT EXISTS pe_fts_update
    AFTER UPDATE ON procedure_enrichment BEGIN
        INSERT INTO fts_procedure_enrichment(fts_procedure_enrichment, rowid, terms)
        VALUES ('delete', old.id, old.terms);
        INSERT INTO fts_procedure_enrichment(rowid, terms)
        VALUES (new.id, new.terms);
    END;
    ",

    // ── embedding_meta ────────────────────────────────────────────────────
    // Глобальная (не per-repo) служебная таблица «ключ-значение» для
    // отпечатков моделей enrichment / embeddings. Хранит:
    //   * `enrichment_signature` = `<provider>:<model>` (этап 5a);
    //   * `embedding_signature`  = `<provider>:<model>:<dim>` (этап 5b).
    //
    // При первом запуске enrichment подпись пишется. На последующих —
    // сравнивается с конфигом; рассинхрон → warning + рекомендация
    // `bsl-indexer enrich --reenrich`. Подробнее — в `enrichment::signature`.
    "
    CREATE TABLE IF NOT EXISTS embedding_meta (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL,
        updated_at INTEGER NOT NULL DEFAULT (CAST(strftime('%s','now') AS INTEGER))
    );
    ",
];

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn schema_extensions_apply_cleanly() {
        let conn = Connection::open_in_memory().unwrap();
        for ddl in SCHEMA_EXTENSIONS {
            conn.execute_batch(ddl).expect("DDL должен выполниться");
        }
        // Идемпотентность — повторный execute не должен валиться.
        for ddl in SCHEMA_EXTENSIONS {
            conn.execute_batch(ddl).expect("DDL должен быть идемпотентным");
        }
    }

    #[test]
    fn proc_call_graph_unique_constraint_works() {
        let conn = Connection::open_in_memory().unwrap();
        for ddl in SCHEMA_EXTENSIONS {
            conn.execute_batch(ddl).unwrap();
        }
        // Первая вставка — ОК.
        conn.execute(
            "INSERT INTO proc_call_graph (repo, caller_proc_key, callee_proc_name, call_type) \
             VALUES (?, ?, ?, ?)",
            rusqlite::params!["ut", "ОбщегоНазначенияСервер.Старт", "Логирование.Записать", "direct"],
        )
        .unwrap();
        // Повтор — должен сломаться по UNIQUE(repo, caller, callee_name, call_type).
        let dup = conn.execute(
            "INSERT INTO proc_call_graph (repo, caller_proc_key, callee_proc_name, call_type) \
             VALUES (?, ?, ?, ?)",
            rusqlite::params!["ut", "ОбщегоНазначенияСервер.Старт", "Логирование.Записать", "direct"],
        );
        assert!(dup.is_err());
        // А вот другой call_type на ту же пару — допустим (нет конфликта).
        conn.execute(
            "INSERT INTO proc_call_graph (repo, caller_proc_key, callee_proc_name, call_type) \
             VALUES (?, ?, ?, ?)",
            rusqlite::params!["ut", "ОбщегоНазначенияСервер.Старт", "Логирование.Записать", "subscription"],
        )
        .unwrap();
    }

    #[test]
    fn procedure_enrichment_inserts_propagate_to_fts() {
        // Проверяем что insert в основную таблицу действительно синхронизирует
        // FTS через триггер pe_fts_insert. Если триггер не сработал — поиск
        // через MATCH не находит вставленные termы.
        let conn = Connection::open_in_memory().unwrap();
        for ddl in SCHEMA_EXTENSIONS {
            conn.execute_batch(ddl).unwrap();
        }
        conn.execute(
            "INSERT INTO procedure_enrichment (repo, proc_key, terms, signature, updated_at) \
             VALUES (?, ?, ?, ?, ?)",
            rusqlite::params![
                "ut",
                "ОбщегоНазначения.Старт",
                "запуск, инициализация, проведение",
                "openai_compatible:claude-haiku-4.5",
                0i64
            ],
        )
        .unwrap();

        // FTS-поиск по слову «проведение» — должен найти одну строку.
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM fts_procedure_enrichment WHERE terms MATCH 'проведение'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "FTS должна найти запись после insert через триггер");

        // Совместный JOIN — типичный запрос tool'а search_terms.
        let row: (String, String, String) = conn
            .query_row(
                "SELECT pe.repo, pe.proc_key, pe.terms \
                 FROM fts_procedure_enrichment fts \
                 JOIN procedure_enrichment pe ON pe.id = fts.rowid \
                 WHERE fts.terms MATCH 'инициализация'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(row.0, "ut");
        assert_eq!(row.1, "ОбщегоНазначения.Старт");
        assert!(row.2.contains("инициализация"));
    }

    #[test]
    fn procedure_enrichment_update_resyncs_fts() {
        // При UPDATE termов FTS должна перестраиваться: старое значение
        // больше не находится, новое — находится. Пара delete+insert
        // в триггере pe_fts_update обеспечивает это.
        let conn = Connection::open_in_memory().unwrap();
        for ddl in SCHEMA_EXTENSIONS {
            conn.execute_batch(ddl).unwrap();
        }
        conn.execute(
            "INSERT INTO procedure_enrichment (repo, proc_key, terms, signature, updated_at) \
             VALUES (?, ?, ?, ?, ?)",
            rusqlite::params!["ut", "М.П", "старое, удалить", "sig", 0i64],
        )
        .unwrap();
        conn.execute(
            "UPDATE procedure_enrichment SET terms = ? WHERE repo = ? AND proc_key = ?",
            rusqlite::params!["новое, обновлено", "ut", "М.П"],
        )
        .unwrap();

        let old_hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM fts_procedure_enrichment WHERE terms MATCH 'старое'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(old_hits, 0, "старое значение FTS должна удалить через триггер update");
        let new_hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM fts_procedure_enrichment WHERE terms MATCH 'обновлено'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(new_hits, 1, "новое значение должно появиться в FTS");
    }

    #[test]
    fn embedding_meta_keeps_signatures() {
        // Минимальная проверка: таблица создана и принимает upsert по PK.
        let conn = Connection::open_in_memory().unwrap();
        for ddl in SCHEMA_EXTENSIONS {
            conn.execute_batch(ddl).unwrap();
        }
        conn.execute(
            "INSERT INTO embedding_meta (key, value) VALUES (?, ?)",
            rusqlite::params!["enrichment_signature", "openai_compatible:claude-haiku-4.5"],
        )
        .unwrap();
        // Повторный insert по тому же ключу должен ломаться (UNIQUE PK).
        let dup = conn.execute(
            "INSERT INTO embedding_meta (key, value) VALUES (?, ?)",
            rusqlite::params!["enrichment_signature", "иное-значение"],
        );
        assert!(dup.is_err(), "PK на key должен предотвращать дубль");
        // Корректное обновление — через REPLACE / ON CONFLICT.
        conn.execute(
            "INSERT INTO embedding_meta (key, value) VALUES (?, ?) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params!["enrichment_signature", "иное-значение"],
        )
        .unwrap();
        let v: String = conn
            .query_row(
                "SELECT value FROM embedding_meta WHERE key = ?",
                rusqlite::params!["enrichment_signature"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, "иное-значение");
    }

    #[test]
    fn metadata_objects_table_accepts_inserts() {
        let conn = Connection::open_in_memory().unwrap();
        for ddl in SCHEMA_EXTENSIONS {
            conn.execute_batch(ddl).unwrap();
        }
        conn.execute(
            "INSERT INTO metadata_objects (repo, full_name, meta_type, name, synonym, attributes_json) \
             VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params!["ut", "Catalog.Контрагенты", "Catalog", "Контрагенты", "Контрагенты", "[]"],
        )
        .unwrap();

        // UNIQUE(repo, full_name) — повтор должен сломаться.
        let dup = conn.execute(
            "INSERT INTO metadata_objects (repo, full_name, meta_type, name) VALUES (?, ?, ?, ?)",
            rusqlite::params!["ut", "Catalog.Контрагенты", "Catalog", "Контрагенты"],
        );
        assert!(dup.is_err(), "UNIQUE-ограничение должно сработать");
    }
}
