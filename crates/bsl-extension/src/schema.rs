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

/// DDL таблицы `metadata_modules` — отдельной константой, потому что
/// используется и в SCHEMA_EXTENSIONS, и в миграции `index_metadata_modules`
/// (пересоздание таблицы при старом UNIQUE-ключе без extension_name).
pub const METADATA_MODULES_DDL: &str = "
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
        UNIQUE(repo, full_name, extension_name)
    );
    ";

/// Индексы `metadata_modules` — тоже переиспользуются миграцией.
pub const METADATA_MODULES_INDEXES: &[&str] = &[
    "CREATE INDEX IF NOT EXISTS idx_mm_repo ON metadata_modules(repo);",
    "CREATE INDEX IF NOT EXISTS idx_mm_object_name ON metadata_modules(repo, object_name);",
    "CREATE INDEX IF NOT EXISTS idx_mm_module_type ON metadata_modules(repo, module_type);",
    "CREATE INDEX IF NOT EXISTS idx_mm_object_id ON metadata_modules(object_id);",
    "CREATE INDEX IF NOT EXISTS idx_mm_extension ON metadata_modules(repo, extension_name);",
];

/// CREATE TABLE / INDEX для специфичных 1С-таблиц.
/// Идемпотентно — все CREATE через IF NOT EXISTS.
pub const SCHEMA_EXTENSIONS: &[&str] = &[
    // ── metadata_objects ──────────────────────────────────────────────────
    // Один объект конфигурации 1С: справочник, документ, регистр и т.д.
    // `meta_type` — категория (Catalog / Document / InformationRegister / ...);
    // `attributes_json` — реквизиты, табличные части, ресурсы, измерения,
    // команды объекта (commands, W4) и пр. секции в виде структурированного
    // JSON (форма извлекается xml::configuration).
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
    // `caller_proc_key` — стабильный идентификатор процедуры-вызывателя в
    // формате `<rel_path>::<procedure>` (тот же, что у
    // `procedure_enrichment.proc_key`; для direct строится JOIN'ом
    // calls ⋈ files). Это разводит одноимённые процедуры из разных модулей.
    // `callee_proc_key` — адрес цели в том же формате `<rel_path>::<name>`,
    // заполняет резолвер этапа 4e (локальный вызов / уникальный экспорт);
    // NULL когда статически не выводится однозначно (неоднозначный экспорт,
    // динамика, платформа). `callee_proc_name` — сырое имя как видно в
    // источнике (recall по имени, основа UNIQUE-дедупа, отображение).
    // subscription/form_event используют синтетические ключи (`event::…`,
    // `form::…`); extension_override — голые имена (резолв — этап 4f).
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
    METADATA_MODULES_DDL,
    METADATA_MODULES_INDEXES[0],
    METADATA_MODULES_INDEXES[1],
    METADATA_MODULES_INDEXES[2],
    METADATA_MODULES_INDEXES[3],
    METADATA_MODULES_INDEXES[4],

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
    // tokenize='trigram' (с 0.30.0; раньше unicode61) — substring-поиск и
    // словоформы без стемминга: запрос 'уточн' находит «УточнитьДанные»,
    // «Штрихкоду» матчится по подстроке «штрихкод». Кириллица фолдится
    // регистронезависимо (проверено на SQLite 3.45). Ограничение триграмм:
    // запрос короче 3 символов не матчится. Цена — FTS-индекс ~3× толще
    // unicode61, на термах (не полнотекст) это десятки МБ на конфигурацию.
    // Миграция существующих БД — `ensure_trigram_tokenizer` (drop+rebuild).
    "
    CREATE VIRTUAL TABLE IF NOT EXISTS fts_procedure_enrichment USING fts5(
        terms,
        content='procedure_enrichment',
        content_rowid='id',
        tokenize='trigram'
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

    // ── data_links ────────────────────────────────────────────────────────
    // Граф связей ДАННЫХ конфигурации 1С: ребро «объект → объект» по
    // ссылочному типу реквизита/измерения. Дополняет `proc_call_graph`
    // (граф вызовов кода) — это про структуру данных, а не про поток
    // управления. Источник — XML отдельных объектов (Catalogs/<X>.xml,
    // Documents/<Y>.xml и т.д.), которые парсит `xml::object_attributes`.
    //
    // Ребро: `from_object` --[from_path]--> `to_object`.
    //   * `from_object` — владелец, канонический `<MetaType>.<Name>`
    //     (например `Document.РеализацияТоваровУслуг`).
    //   * `from_path`   — путь к реквизиту: имя реквизита шапки
    //     (`Контрагент`) либо `<ТабЧасть>.<Реквизит>` (`Товары.Номенклатура`),
    //     для измерения регистра — имя измерения.
    //   * `to_object`   — цель. Для конкретного типа — `<MetaType>.<Name>`
    //     (`Catalog.Контрагенты`). Для обобщённого («вся категория») —
    //     служебный *-узел: `*CatalogRef` / `*DocumentRef` / `*AnyRef`.
    //
    // `link_kind`:
    //   * `attr`          — ссылочный реквизит шапки.
    //   * `tabular_attr`  — ссылочный реквизит табличной части.
    //   * `register_dim`  — измерение регистра.
    //   * `recorder`      — движение: документ → регистр (этап 2).
    //   * `owner`         — подчинённый справочник → владелец (W6, 0.32):
    //     `from_object` = подчинённый, `to_object` = владелец, from_path пуст.
    //   ── связи конфигурационного уровня (xml::metadata_refs, этап 3.1) ──
    //   * `subsystem_content`          — подсистема → объект её состава.
    //     `from_object` = `Subsystem.<Имя>` (листовое имя подсистемы).
    //   * `exchange_plan_content`      — план обмена → объект состава.
    //     `from_object` = `ExchangePlan.<Имя>`.
    //   * `defined_type_content`       — определяемый тип → конкретный тип.
    //     `from_object` = `DefinedType.<Имя>`.
    //   * `functional_option_content`  — ФО → объект/реквизит её состава
    //     (`<Content>`, W1). `from_object` = `FunctionalOption.<Имя>`.
    //   * `functional_option_location` — ФО → объект хранения значения
    //     (константа/ресурс регистра). `from_object` = `FunctionalOption.<Имя>`,
    //     `from_path` — полный путь хранения из `<Location>`.
    //
    // `is_composite` — ребро из составного типа (перечислено несколько
    // конкретных типов). `is_universal` — обобщённый тип, схлопнут в
    // *-узел (терминал обхода; защита от fan-out 2000+ и зашумления).
    //
    // Граница конкретный/обобщённый — КАЧЕСТВЕННАЯ, по форме типа в XML:
    // есть имя после `Ref.` → конкретное ребро (хоть 20 в составном);
    // имени нет (`cfg:CatalogRef`, `cfg:AnyRef`) → один *-узел.
    //
    // UNIQUE предотвращает дубликаты при повторном `index_extras`.
    "
    CREATE TABLE IF NOT EXISTS data_links (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        repo TEXT NOT NULL,
        from_object TEXT NOT NULL,
        from_path TEXT NOT NULL,
        to_object TEXT NOT NULL,
        link_kind TEXT NOT NULL,
        is_composite INTEGER NOT NULL DEFAULT 0,
        is_universal INTEGER NOT NULL DEFAULT 0,
        -- to_object_key = lower(to_object): регистронезависимый реверс-поиск по
        -- кириллице (SQLite lower() её не берёт — считаем в Rust), используется
        -- find_references.data_refs. Заполняется при наполнении data_links.
        to_object_key TEXT NOT NULL DEFAULT '',
        UNIQUE(repo, from_object, from_path, to_object)
    );
    ",
    // idx_dl_from — прямой обход «на что ссылается X» (get_data_links out).
    "CREATE INDEX IF NOT EXISTS idx_dl_from ON data_links(repo, from_object);",
    // idx_dl_to — обратный обход «кто ссылается на X» (get_data_links in).
    "CREATE INDEX IF NOT EXISTS idx_dl_to ON data_links(repo, to_object);",
    // idx_dl_to_key — регистронезависимый реверс по lower(to_object) (find_references).
    "CREATE INDEX IF NOT EXISTS idx_dl_to_key ON data_links(repo, to_object_key);",

    // ── direct_edge_files ─────────────────────────────────────────────────
    // Привязка direct-рёбер графа вызовов к файлам-источникам. Нужна ТОЛЬКО
    // для инкрементального per-file обновления: `proc_call_graph` хранит
    // direct-рёбра дедуплицированно, по голым именам, без файла — поэтому
    // при правке одного .bsl нельзя узнать, какие рёбра «его». Эта таблица
    // помнит, какие рёбра (caller→callee) даёт каждый файл, и позволяет
    // обновлять граф точечно: удалить прежние рёбра файла, добавить новые,
    // не трогая остальной граф (см. `update_call_graph_direct_for_file`).
    //
    // Строка на ребро файла: (repo, caller, callee, source_file). При правке
    // одного .bsl читаем прежние рёбра файла (по source_file), сносим их,
    // пишем новые из `calls` — точечно, не трогая остальной граф.
    // «Прежние» рёбра нельзя взять из `calls` (её базовый индексатор обновляет
    // ДО нашего шага), поэтому нужна отдельная таблица. proc_call_graph
    // остаётся дедуплицированной — find_path_bsl не затронут. Заполняется при
    // полном построении (`build_call_graph`).
    "
    CREATE TABLE IF NOT EXISTS direct_edge_files (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        repo TEXT NOT NULL,
        caller TEXT NOT NULL,
        callee TEXT NOT NULL,
        source_file TEXT NOT NULL,
        UNIQUE(repo, caller, callee, source_file)
    );
    ",
    // Прежние рёбра файла + per-file DELETE при обновлении/удалении файла.
    "CREATE INDEX IF NOT EXISTS idx_def_source ON direct_edge_files(repo, source_file);",

    // ── role_rights ───────────────────────────────────────────────────────
    // Права ролей конфигурации: одна строка = одно granted-право пары
    // роль↔объект. Источник — `Roles/<Имя>/Ext/Rights.xml` (парсит
    // `xml::metadata_refs::parse_role_rights`). Хранятся только включённые
    // права (`<value>true</value>`).
    //
    // Право — это атрибут пары (роль, объект), а не ссылка объект→объект,
    // поэтому отдельная таблица, а не `link_kind` в `data_links` (решение
    // дизайна #1475: data_links — однородный граф объект↔объект).
    //
    //   * `role_name`   — имя роли (`ДобавлениеИзменениеДокументов`).
    //   * `object_name` — полное имя объекта (`Document.X`, `Configuration.Y`).
    //   * `right_name`  — имя права (`Read`, `Insert`, `Posting`, `ThinClient`).
    //
    // UNIQUE защищает от дублей при повторном `index_extras`.
    "
    CREATE TABLE IF NOT EXISTS role_rights (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        repo TEXT NOT NULL,
        role_name TEXT NOT NULL,
        object_name TEXT NOT NULL,
        right_name TEXT NOT NULL,
        -- object_name_key = lower(object_name): регистронезависимый поиск прав
        -- по объекту (кириллица — в Rust), используется find_references.role_rights.
        object_name_key TEXT NOT NULL DEFAULT '',
        UNIQUE(repo, role_name, object_name, right_name)
    );
    ",
    // idx_rr_object — «какие роли дают права на объект X» (основной hot path).
    "CREATE INDEX IF NOT EXISTS idx_rr_object ON role_rights(repo, object_name);",
    // idx_rr_object_key — регистронезависимый поиск прав по lower(object_name).
    "CREATE INDEX IF NOT EXISTS idx_rr_object_key ON role_rights(repo, object_name_key);",
    // idx_rr_role — «что разрешает роль X».
    "CREATE INDEX IF NOT EXISTS idx_rr_role ON role_rights(repo, role_name);",

    // ── metadata_code_usages ──────────────────────────────────────────────
    // Обратный индекс использований объектов метаданных В КОДЕ (.bsl). Одна
    // строка = одно обращение. КОД-производная (в отличие от data_links —
    // декларативных ссылок из XML): наполняется лёгким source-aware regex-слоем
    // по телам модулей (`code_usages::extract_code_usages`).
    //
    //   * `object_ref`     — канонический объект (`Document.РеализацияТоваровУслуг`).
    //   * `object_ref_key` — `lower(object_ref)` для индексного поиска без UDF
    //     (точное сравнение по кириллице).
    //   * `member_path`    — имя ТЧ для `query` (3-й сегмент пути), иначе NULL.
    //   * `usage_kind`     — `manager` (Документы.X) | `ref_type` ("ДокументСсылка.X")
    //     | `query` (путь метаданных в тексте запроса).
    //   * `file_path`      — путь модуля относительно корня репо (forward slash).
    //   * `line`           — номер строки (1-based).
    //
    // Без UNIQUE: один объект может упоминаться многократно (в т.ч. в одной
    // строке). Идемпотентность — через DELETE по repo (полный пересбор) или по
    // file_path (инкремент) перед INSERT.
    "
    CREATE TABLE IF NOT EXISTS metadata_code_usages (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        repo TEXT NOT NULL,
        object_ref TEXT NOT NULL,
        object_ref_key TEXT NOT NULL,
        member_path TEXT,
        usage_kind TEXT NOT NULL,
        file_path TEXT NOT NULL,
        line INTEGER NOT NULL
    );
    ",
    // idx_mcu_ref — «где в коде используется объект X» (основной hot path).
    "CREATE INDEX IF NOT EXISTS idx_mcu_ref ON metadata_code_usages(repo, object_ref_key);",
    // idx_mcu_file — per-file DELETE при инкрементальном обновлении модуля.
    "CREATE INDEX IF NOT EXISTS idx_mcu_file ON metadata_code_usages(repo, file_path);",
];

/// Идемпотентная миграция существующей БД до текущей схемы расширений.
/// Догоняет `*_key`-колонки, добавленные позже создания таблиц: `CREATE TABLE
/// IF NOT EXISTS` не добавляет колонку в уже существующую таблицу, а следующий
/// `CREATE INDEX` по отсутствующей колонке рвёт весь DDL-батч
/// `apply_schema_extensions`. Вызывать ДО применения `SCHEMA_EXTENSIONS`.
/// Безопасно на свежей БД (таблиц ещё нет — ALTER пропускается) и при повторе.
pub fn migrate_extensions(conn: &rusqlite::Connection) -> anyhow::Result<()> {
    ensure_column(conn, "data_links", "to_object_key", "TEXT NOT NULL DEFAULT ''")?;
    ensure_column(conn, "role_rights", "object_name_key", "TEXT NOT NULL DEFAULT ''")?;
    ensure_trigram_tokenizer(conn)?;
    Ok(())
}

/// Миграция 0.30.0: FTS-индекс термов на trigram-токенайзер. У существующей
/// БД `CREATE VIRTUAL TABLE IF NOT EXISTS` токенайзер не поменяет — проверяем
/// DDL в sqlite_master и при несовпадении пересоздаём таблицу + rebuild из
/// content-таблицы (`procedure_enrichment`). Триггеры живут на
/// content-таблице и пересоздания не требуют. На свежей БД (FTS ещё нет) —
/// no-op: создаст `SCHEMA_EXTENSIONS` сразу с trigram.
fn ensure_trigram_tokenizer(conn: &rusqlite::Connection) -> anyhow::Result<()> {
    use rusqlite::OptionalExtension;
    let ddl: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='fts_procedure_enrichment'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(ddl) = ddl {
        if !ddl.contains("trigram") {
            conn.execute_batch(
                "DROP TABLE fts_procedure_enrichment;
                 CREATE VIRTUAL TABLE fts_procedure_enrichment USING fts5(
                     terms,
                     content='procedure_enrichment',
                     content_rowid='id',
                     tokenize='trigram'
                 );
                 INSERT INTO fts_procedure_enrichment(fts_procedure_enrichment) VALUES('rebuild');",
            )?;
            tracing::info!("fts_procedure_enrichment: мигрирован на trigram-токенайзер");
        }
    }
    Ok(())
}

/// `ALTER TABLE <table> ADD COLUMN <column> <decl>` — только если таблица уже
/// существует и колонки в ней нет. На свежей БД (таблицы ещё нет) — no-op:
/// колонку создаст `CREATE TABLE` из `SCHEMA_EXTENSIONS`.
fn ensure_column(
    conn: &rusqlite::Connection,
    table: &str,
    column: &str,
    decl: &str,
) -> anyhow::Result<()> {
    let table_exists: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [table],
        |r| r.get(0),
    )?;
    if table_exists == 0 {
        return Ok(());
    }
    let mut stmt = conn.prepare(&format!("PRAGMA table_info(\"{}\")", table))?;
    let has_column = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .filter_map(Result::ok)
        .any(|name| name == column);
    drop(stmt);
    if !has_column {
        conn.execute_batch(&format!(
            "ALTER TABLE \"{}\" ADD COLUMN \"{}\" {};",
            table, column, decl
        ))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn column_exists(conn: &Connection, table: &str, column: &str) -> bool {
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info(\"{}\")", table))
            .unwrap();
        let found = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .filter_map(Result::ok)
            .any(|n| n == column);
        found
    }

    #[test]
    fn migrate_extensions_backfills_missing_key_column() {
        // Симуляция старой БД (как 0.20.0): data_links без to_object_key,
        // role_rights ещё нет. migrate_extensions + SCHEMA_EXTENSIONS не должны
        // падать, а колонка/индекс по *_key — появиться.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE data_links (
                repo TEXT NOT NULL, from_object TEXT NOT NULL, from_path TEXT,
                to_object TEXT NOT NULL, link_kind TEXT NOT NULL,
                is_composite INTEGER NOT NULL DEFAULT 0,
                is_universal INTEGER NOT NULL DEFAULT 0,
                UNIQUE(repo, from_object, from_path, to_object));",
        )
        .unwrap();
        assert!(!column_exists(&conn, "data_links", "to_object_key"));

        super::migrate_extensions(&conn).unwrap();
        for ddl in SCHEMA_EXTENSIONS {
            conn.execute_batch(ddl)
                .expect("DDL после migrate_extensions не должен падать");
        }
        assert!(column_exists(&conn, "data_links", "to_object_key"));

        let idx: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='index' AND name='idx_dl_to_key'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(idx, 1, "idx_dl_to_key должен создаться после миграции");

        // role_rights создалась штатно и колонка object_name_key на месте.
        assert!(column_exists(&conn, "role_rights", "object_name_key"));

        // Идемпотентность: повтор миграции — no-op, не падает.
        super::migrate_extensions(&conn).unwrap();
    }

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
    fn migrate_to_trigram_rebuilds_fts() {
        // Эмуляция БД, созданной до 0.30.0: FTS на unicode61. После
        // migrate_extensions токенайзер должен стать trigram, индекс —
        // пересобраться из content-таблицы (substring-поиск работает).
        let conn = Connection::open_in_memory().unwrap();
        for ddl in SCHEMA_EXTENSIONS {
            conn.execute_batch(ddl).unwrap();
        }
        conn.execute_batch(
            "DROP TABLE fts_procedure_enrichment;
             CREATE VIRTUAL TABLE fts_procedure_enrichment USING fts5(
                 terms, content='procedure_enrichment', content_rowid='id',
                 tokenize='unicode61 remove_diacritics 1');",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO procedure_enrichment (repo, proc_key, terms, signature, updated_at) \
             VALUES ('ut', 'A.bsl::P', 'уточнить данные по штрихкоду', 'mech:v1', 0)",
            [],
        )
        .unwrap();
        // На unicode61 substring НЕ матчится.
        let pre: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM fts_procedure_enrichment WHERE terms MATCH '\"трихкод\"'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        assert_eq!(pre, 0, "unicode61 не должен находить подстроку");

        migrate_extensions(&conn).unwrap();

        let ddl: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='fts_procedure_enrichment'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(ddl.contains("trigram"), "после миграции токенайзер trigram: {ddl}");
        // Substring и словоформа находятся; индекс пересобран из content-таблицы.
        for q in ["трихкод", "штрихкоду", "УТОЧНИТЬ"] {
            let hits: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM fts_procedure_enrichment WHERE terms MATCH ?1",
                    [q],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(hits, 1, "trigram должен находить '{q}'");
        }
        // Повторный вызов — no-op (идемпотентность).
        migrate_extensions(&conn).unwrap();
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
