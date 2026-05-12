# Changelog

Формат — [Keep a Changelog](https://keepachangelog.com/ru/1.0.0/).
Версионирование — [SemVer](https://semver.org/lang/ru/).

## [0.9.1] — 2026-05-12

**Этап 3 миграции на event-based cache invalidation: уведомление `mcp-cache-ci` после переиндексации.**

Замыкает цепочку: сохранили файл → daemon (watcher) обнаружил → переиндексировал в SQLite → **отправил `POST /invalidate {file_paths: [...]}` в cache-ci**. Cache-ci по reverse_index (заполненному в этапе 2 через `_meta.dependent_files`) точечно сносит только зависимые entries, остальные cache hits сохраняются.

### Добавлено

- **`crates/code-index-core/src/daemon_core/cache_client.rs`** — `CacheClient` с пулом `reqwest::Client` (timeout 2s, keep-alive 60s) и списком target URL-ов. Метод `invalidate_files(&[String])` шлёт POST параллельно всем targets, на failure (сеть, 5xx, timeout) — `eprintln!` warning и продолжаем; падать не должны, TTL на стороне cache-ci подстрахует.
- **Секция `[[cache_targets]]` в `daemon.toml`** + структура `CacheTargetEntry { url: String }` в `daemon_core/config.rs`. Пример:

  ```toml
  [[cache_targets]]
  url = "http://127.0.0.1:8011"
  ```

  Несколько entries разрешено (multi-cache-ci топологии: локальный Windows + удалённый rag-VM). Отсутствие секции (или пустой список) → событийный канал выключен, поведение как до v0.9.1.
- **Хелпер `worker::collect_invalidate_paths(root, batch)`** — собирает дедуплицированный список relative file_path'ей из batch'а FS-событий. Учитывает все типы (Modified/Created/Deleted) — удаление файла тоже должно сносить связанные cache_entries.
- **Параметр `cache_client: Option<Arc<CacheClient>>`** в `worker::run_worker` и `runner::spawn_worker`. Пробрасывается из `runner::run` и `runner::handle_reload` (reload пересоздаёт `CacheClient` по новому конфигу для added-папок; existing workers сохраняют свой client до рестарта demon).
- **Юнит-тесты** для `cache_client.rs`: пустые targets → `is_empty()`; trailing slashes стрипуются; невалидный target не паникует (connection refused → 0 успехов). Тесты для config.rs `cache_targets_default_empty` и `parses_cache_targets_list`.

### Изменено

- **Сигнатура `worker::run_worker`** — новый последний параметр `cache_client`.
- **Сигнатура `runner::spawn_worker`** — то же.
- **`commit_batch()` теперь возвращает результат проверки** — если commit упал, invalidate не отправляется (новых данных в индексе всё равно нет; cache-ci пусть отдаёт старое — будет corrected либо при следующем успешном batch'е, либо через TTL).
- **Workspace version** 0.9.0 → 0.9.1.

### Совместимость

- `daemon.toml` без `[[cache_targets]]` — полностью работающий (поведение как до v0.9.1, без сетевого трафика к cache-ci).
- `daemon.toml` с `[[cache_targets]]` — событийный канал активируется автоматически на старте.
- API `run_worker` / `spawn_worker` — изменилась сигнатура (additive last param). Внешние клиенты крейта `code-index-core` (если есть) должны передать `None` для совместимости.

### Архитектура (final state цепочки)

После v0.9.1 + cache-ci 0.2.0:

1. **Read-tools daemon'а** возвращают `{result, _meta: {dependent_files: [...]}}` (v0.9.0).
2. **`mcp-cache-ci`** при cache-fill пишет `cache_key → file_paths` в reverse_index (cache-ci 0.2.0).
3. **Daemon watcher** на FS event → reindex → `commit_batch` → `cache_client.invalidate_files(...)` → cache-ci по reverse_index сносит точечно (v0.9.1).
4. **TTL fallback** — третий эшелон safety net: если событие потерялось (сеть, daemon упал, ReadDirectoryChangesW buffer overflow), entry протухнет за 600s/3600s сам.

## [0.9.0] — 2026-05-12

**Phase 2 (этап миграции на event-based cache invalidation): `_meta.dependent_files` в read-ответах.**

Все data-инструменты MCP теперь возвращают единый JSON-формат:

```json
{
  "result": <prev plain payload>,
  "_meta": { "dependent_files": ["src/X.bsl", "src/Y.bsl"] }
}
```

`dependent_files` — список path'ей файлов, из которых построен этот ответ. Целевой потребитель — `mcp-cache-ci`: при cache-fill он регистрирует связи `cache_key → file_path` в `reverse_index` и затем сносит точечно затронутые entries по сигналу от daemon после переиндексации файла (этап 3, готовится).

### Совместимость (BREAKING CHANGE формата ответа)

Все клиенты read-tools должны быть готовы к новой структуре `{result, _meta}`:

- Раньше: `search_function` возвращал плоский массив `[FunctionRecord, ...]`.
- Сейчас: `{"result": [FunctionRecord, ...], "_meta": {"dependent_files": [...]}}`.

Для существующего потребителя (`mcp-cache-ci` 0.2.0+) поведение обратно-совместимое: cache-ci парсит `_meta.dependent_files` если есть, иначе работает как раньше (insert без зависимостей, TTL fallback).

Tools **без** обёртки (формат ответа не изменён):

- `health` — non-cacheable.
- `get_stats` — диагностический, формат расширяется по федерации, обёртка ломала бы агрегацию.
- `stat_file` — single-file тривиальный.

### Добавлено

- **Wrapper-хелперы в `crates/code-index-core/src/mcp/tools.rs`:**
  - `wrap_with_meta<T: Serialize>(result, dependent_files)` — финальная сериализация в `{result, _meta}` с дедупликацией file_paths.
  - `collect_paths_via<R>(storage, records, extract: fn(&R) -> file_id)` — собрать пути из vec'а records через extractor.
- **Wrapper-хелперы в `crates/bsl-extension/src/tools/mod.rs`:**
  - `wrap_with_meta(result: Value, dependent_files: Vec<String>) -> Value` для BSL extension-tools.
  - `wrap_error(error_value: Value) -> Value` — даже на ошибке формат единый.
- **Поддержка `_meta.dependent_files` в data-tools core:**
  - `search_function`, `search_class` — DISTINCT file_paths из Vec'а records.
  - `get_function`, `get_class` — то же.
  - `find_symbol` — объединение path'ей из functions+classes+variables+imports.
  - `get_imports` (by file и by module).
  - `get_file_summary` — path из args.
  - `get_callers`, `get_callees` — file_ids из CallRecord.
  - `grep_body` — file_path напрямую из GrepBodyMatch.
  - `grep_code`, `grep_text`, `search_text` — path напрямую из match-структур.
  - `read_file` — path из args.
  - `list_files` — paths из ListedFile.
- **Поддержка `_meta.dependent_files` в BSL extension-tools** (пока пустой массив — XML-парсер метаданных не привязан к file_path, реальные зависимости — задача следующей итерации):
  - `get_object_structure`, `get_form_handlers`, `get_event_subscriptions`, `find_path`, `search_terms`.

### Изменено

- **Workspace version** bumped 0.8.1 → 0.9.0 (minor — обратно-совместимое расширение формата для cache-ci-клиента, breaking для клиентов, парсивших плоский payload).

### Следующие шаги

- Этап 3: `POST /invalidate {file_paths}` от daemon к cache-ci после `transaction.commit()` SQLite по batch'у FS-событий. На стороне cache-ci 0.2.0 уже готово принимать.

## [0.8.1] — 2026-05-06

**Patch-релиз: BSL extension-tools в daemon-режиме и через federation.** Закрывает две публичные регрессии v0.8.0, из-за которых пять BSL-инструментов (`get_object_structure`, `get_form_handlers`, `get_event_subscriptions`, `find_path`, `search_terms`) были нерабочими в штатном production-сценарии (репо обслуживаются демоном, federation-репо на удалённой ноде).

### Как нашли и почему починили сами

Регрессия обнаружена нами **в ходе эксплуатации v0.8.0** (2026-05-06): попытка вызвать `get_object_structure` на любом BSL-репо приводила к `database error: no such table: metadata_objects`, а на federation-репо — к `extension tool '...' currently supports only local repos`. До нас ошибки никто не сообщал — внешние пользователи v0.8.0 могли не дойти до 1С-ветки. Локализовано до двух точек в `code-index-core`: вызовы `apply_schema_extensions` / `index_extras` существовали только в CLI-команде `index` (`cli.rs`), а в `daemon_core/worker.rs` отсутствовали; в `mcp::call_tool` стоял жёсткий отказ для `is_local == false`. После полного цикла проверки (235 unit-тестов + smoke на 4 BSL-репо локально и через federation на VM) — фикс вкатан патчем v0.8.1 без участия внешнего сообщества.

### Исправлено

- **Daemon теперь применяет `schema_extensions` и `index_extras` процессоров.** В v0.8.0 эти вызовы были только в CLI-команде `index <path>`, а worker даемона их не делал. Результат: на любом BSL-репо, проиндексированном через `bsl-indexer.exe daemon run`, BSL-tools падали с `database error: no such table: metadata_objects`. Теперь worker `daemon_core/worker.rs` сам resolve'ит процессор по правилу «явный `language` из `daemon.toml` → fallback `detect()`», применяет `apply_schema_extensions` ДО `full_reindex` (создаёт пустые таблицы — DDL идемпотентен) и вызывает `index_extras` ДО `flush_to_disk` (наполняет таблицы из `Configuration.xml`). Для репо без `Configuration.xml` (например, старые выгрузки обработок) таблицы создаются пустыми — tools отвечают `[]` без exception.
- **Federation теперь форвардит extension-tools на удалённые ноды.** Раньше любой вызов BSL-tool на remote-репо (UT/BP_SS/BP_TDK/ZUP на VM rag) возвращал `extension tool '...' currently supports only local repos`. Введён универсальный route `POST /federate/extension` с payload `{tool_name, args}` — один маршрут на все extension-tools, расширяемо при добавлении новых LanguageProcessor'ов. На source-стороне `mcp::call_tool` форвардит вызов через `dispatcher::dispatch_remote_value`. Обе ноды federation должны быть обновлены до 0.8.1 синхронно — старая нода вернёт 404 на новый route.

### Добавлено

- **`ProcessorRegistry::resolve(explicit_language, repo_root)`** — двухступенчатый resolve процессора: сначала по явному `language` из конфига, потом fallback на `detect()` по маркерам корня. Используется в daemon-worker и в CLI-команде `index`. Унифицирует поведение «индексация» независимо от способа запуска.
- **Структура `mcp::ExtensionToolParams { tool_name, args }`** — payload для federation-форварда extension-tools.
- **Universal handler `handle_extension_tool` в `federation::server`** — находит tool в `extension_tools` snapshot, строит `ToolContext` для local repo и вызывает `IndexTool::execute`. Если на target-ноде такого tool нет (например, она запущена не с bsl-extension) — возвращает `federation_error` с понятным сообщением.

### Изменено

- **`run_worker` принимает `processor_registry: Option<Arc<ProcessorRegistry>>`** (последний параметр). `None` = universal-only сборка (`code-index.exe`); `Some(reg)` = `bsl-indexer.exe`. Используется для resolve процессора текущего репо.
- **`runner::run` принимает `processor_registry`** и пробрасывает в `spawn_worker` (initial loop + `handle_reload`).
- **`cli::handle_daemon` принимает `processor_registry`** — передаётся в `runner::run` при запуске даемона.
- **`Commands::Index` использует `resolve(None, root)`** вместо прямого `detect(root)` — поведение идентично, но единый кодпуть.

### Совместимость

Изменения сигнатур публичных API в `daemon_core::worker`/`runner`/`cli` — additive (новые параметры в конце). Сборка `bsl-indexer` 0.8.1 совместима с конфигом `daemon.toml` 0.8.0 — миграции БД не требуется (DDL `apply_schema_extensions` идемпотентен).

**Federation:** обе ноды нужно обновлять одновременно. До-0.8.1 нода вернёт `404 Not Found` на `POST /federate/extension`, и new-нода покажет это как `federation_error`.

## [0.8.0] — 2026-05-05

**Phase 2 «content для code-файлов»** — закрытие главного ограничения Phase 1. До v0.8.0 `read_file` для `.py`/`.bsl`/`.rs`/`.ts` и других code-файлов возвращал `category="code"` с пустым `content`. Теперь содержимое хранится в новой таблице `file_contents` (zstd-сжатие, миграция v4) и отдаётся при каждом вызове. Дополнительно: новый инструмент `grep_code` для regex-поиска непосредственно по содержимому code-файлов, oversize-механика для файлов крупнее настраиваемого лимита.

### Добавлено

- **Таблица `file_contents` (миграция v4).** DDL: `file_contents(file_id INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE, content_blob BLOB, oversize INTEGER NOT NULL DEFAULT 0)`. Backfill автоматический — выполняется в составе `full_reindex` при первом запуске v0.8.0 на существующей БД. Идемпотентна: повторный вызов безопасен (`INSERT OR REPLACE`). Оценка для УТ (~15 665 `.bsl`, ~620 МБ исходников): ~120 МБ blob после zstd (~5×), однократное время backfill ~1-2 минуты (чистый I/O + zstd encode).

- **`read_file` для code-файлов работает в полном объёме.** Для `.py`, `.bsl`, `.rs`, `.ts` и других AST-языков возвращается разжатый content из `file_contents`. `category="code"`. Старая логика чтения text-файлов через `text_files` не меняется.

- **Oversize-механика.** Файлы крупнее `max_code_file_size_bytes` (дефолт **5 МБ**) сохраняются с `oversize=1, content_blob=NULL`. AST-парсинг, FTS и граф вызовов для них работают в полном объёме. `read_file` для oversize-файла возвращает специальный ответ:
  ```json
  {
    "category": "code",
    "content": "",
    "oversize": true,
    "file_size": 8650240,
    "size_limit": null,
    "hint": "Файл oversize: content не сохранён в индексе. Используйте get_function/get_class/grep_body."
  }
  ```

- **`stat_file` показывает `oversize`** для code-файлов: поле `Option<bool>` добавлено в ответ. Для text-файлов — всегда `null`.

- **Конфигурация лимита `max_code_file_size_bytes`.** Hardcoded дефолт — 5 МБ (`DEFAULT_MAX_CODE_FILE_SIZE_BYTES` в `crate::daemon_core::config`). Переопределяется в `daemon.toml`:
  ```toml
  [indexer]
  max_code_file_size_bytes = 5242880   # глобальный override (5 МБ)

  [[paths]]
  path = "C:/RepoUT"
  max_code_file_size_bytes = 10485760  # для этого репо — 10 МБ
  ```
  Приоритет: per-path → секция `[indexer]` → дефолт 5 МБ. Логика выбора — хелпер `PathEntry::effective_max_code_file_size(&IndexerSection)`.

- **Новый MCP-инструмент `grep_code` (Phase 2 bonus).** Regex-поиск по содержимому code-файлов — закрывает слепую зону `grep_body` (тот ищет только в телах функций/классов). Источник данных — таблица `file_contents` (zstd-decode на лету в Rust; SQL делает pre-filter по path/language). Параметры идентичны `grep_text`: `regex`, `path_glob?`, `language?`, `limit?`, `context_lines?`. Файлы с `oversize=1` пропускаются. Storage-метод: `Storage::grep_code_filtered(regex, path_glob, language, limit, context_lines, max_total_bytes) -> Vec<GrepTextMatch>`. Сигнатура pub-функции: `pub async fn grep_code(entry, regex, path_glob, language, limit, context_lines)`.

- **Federation route `/federate/grep_code`** — аддитивный, не ломает существующие клиенты. При обращении к старой ноде (< 0.8.0) вернётся `404` — ожидаемое поведение; обе ноды нужно обновлять синхронно для использования `grep_code` в федерации.

### Изменено

- **`Indexer::write_code_to_db`** — добавлен последний параметр `raw_content: Option<&str>`. Если задан — content сохраняется в `file_contents` (zstd encode). Внутренний API.
- **`Storage::read_file_text`** — добавлен последний параметр `size_limit_bytes: Option<i64>`. Используется для заполнения поля `size_limit` в oversize-ответе. MCP-слой передаёт `None`.
- **`ParsedFile::Code` enum-вариант** — добавлено поле `raw_content: String`.
- **`worker::run_worker`** — добавлен параметр `IndexerSection` (последний). Внутри вычисляется effective лимит и записывается в `IndexConfig.max_code_file_size_bytes`.
- **`runner::spawn_worker`** — добавлен параметр `IndexerSection`, пробрасывается в `run_worker`.

### Безопасность

- **Защита от zstd-bomb.** Все вызовы декомпрессии в `read_file_content` и `grep_code_filtered` идут через приватный helper `Storage::decode_zstd_safe(blob) -> Result<Vec<u8>>`. Использует stream-decoder с `io::Read::take(limit + 1)` — если разжатый размер превысит `FILE_CONTENTS_MAX_DECOMPRESSED_BYTES` (256 МБ), возвращает ошибку, не аллоцирует RAM дальше. 256 МБ заведомо больше любого валидного code-файла (5 МБ default × ~5× zstd = ~25 МБ; запас на случай поднятого `max_code_file_size_bytes` оператором).

### Исправлено

- **Backfill теперь работает для всех code-файлов на стабильной БД (фикс бага первой превью-сборки).** Раньше backfill был встроен в обработку `metadata_updates` в `full_reindex` — это контейнер файлов с изменившимся mtime/file_size, но прежним content_hash. На «стабильной» БД (никто не трогал файлы с прошлой индексации) `metadata_updates` пустой, поэтому backfill **не запускался для UT/BP_SS/ZUP** — наполнялись только репо с реально изменившимися файлами (BP_TDK получал ~15 файлов из 90K). Фикс: вынесено в **отдельную фазу** `Этап 6` после удаления устаревших, через новый Storage-метод `list_code_files_without_content() -> Vec<(file_id, path)>`. Теперь backfill бьёт по всем code-файлам, у которых нет записи в `file_contents` И нет записи в `text_files`, независимо от того менялся ли mtime. Реальные показатели на VM rag после фикса: UT 32599/32599 за 31.7 сек, BP_SS 37535/37535 за 37.9 сек, ZUP 19066/19066 за 17.5 сек, BP_TDK аналогично.
- **Backfill в батчах вместо одной мега-транзакции.** Для 90K-репо вся фаза в `BEGIN TRANSACTION` без commit раздула бы WAL до многих ГБ. Промежуточный `commit_batch + begin_batch` каждые `batch_size.max(500)` файлов держит WAL в разумных пределах.

### Совместимость

- **MCP API без breaking-changes.** Все новые поля в response — `Option<...>` или `default false`; старые клиенты не сломаются. Изменение `read_file` для code-файлов (возвращает реальный content вместо пустого) — улучшение, не breaking.
- **Schema БД** — миграция v4 идемпотентна, безопасна на существующей БД v0.7.x. Откат на v0.7.x просто игнорирует новую таблицу — обе версии совместимы по чтению старых данных.
- **Storage API изменён несовместимо** для прямых пользователей крейта `code-index-core`: `Indexer::write_code_to_db`, `Storage::read_file_text`, `worker::run_worker`, `runner::spawn_worker` — новые параметры. Также добавлены публичные методы: `Storage::upsert_file_content`, `read_file_content`, `has_file_content`, `delete_file_content`, `get_file_id_by_path`, `has_text_file`, `list_code_files_without_content`, `grep_code_filtered`. Внешних callers в публичном API нет, но если есть приватный код с прямыми вызовами — обновить.
- **Federation** — новый route `/federate/grep_code` аддитивный. **Обе ноды federation должны быть обновлены синхронно** для использования `grep_code` в федерации (иначе старая нода вернёт 404 на этот route). Общий принцип `v0.7.0+` остаётся.
- **`grep_code` пропускает oversize-файлы** — это задокументированное ограничение, не баг. Для таких файлов по-прежнему работают `get_function`/`get_class`/`grep_body` по AST-данным.

## [0.7.3] — 2026-05-04

**Bug-fix**: extension-tools (`get_object_structure`, `get_form_handlers` и другие, поставляемые через `LanguageProcessor::additional_tools()`) **не регистрировались в `tools/list`** при работе сервера в федеративном режиме (`serve.toml` присутствует). У пользователей в моно-режиме всё было корректно.

### Исправлено

- **`CodeIndexServer::from_federated`** теперь принимает два дополнительных параметра: `registry: Option<ProcessorRegistry>` и `local_languages: BTreeMap<String, String>`. Реестр процессоров сохраняется в `Self.registry`, и сразу после построения карты репо вычисляется `extension_tools = collect_extension_tools(&active_languages, &reg)`. Раньше федеративный конструктор всегда инициализировал `extension_tools = Vec::new()` и `registry = None`, что обнуляло conditional registration на старте serve и при последующих `reload_extensions` (`registry_opt = None` → `new_tools = Vec::new()`).
- **`local_languages` для federation**: мапа `alias → language` собирается из локального `daemon.toml` (`PathEntry::effective_alias()` + `PathEntry.language`) и проставляется в `RepoEntry.language` для **local-репо**. Без этого `collect_active_languages` не находил bsl/python/rust в federation-сценарии (federation::repos::merge возвращает FederatedRepo без поля language). Remote-репо через federation продолжают приходить без языка — для них extension-tools регистрируются только если такой же язык активен у local-репо на этой ноде.
- **Поведенческое следствие**: на сборке `bsl-indexer` в federation-режиме `tools/list` теперь возвращает 22 инструмента вместо 17 — добавляются `find_path`, `get_event_subscriptions`, `get_form_handlers`, `get_object_structure`, `search_terms` (5 BSL-tools из `bsl-extension`).

### Совместимость

- **MCP API без изменений** — список tool-ов меняется только в federation-режиме сборки `bsl-indexer` при наличии хотя бы одного local-репо с `language = "bsl"` в `daemon.toml`. Клиенту это видно как штатный `notifications/tools/list_changed`.
- **Schema БД без миграций.**
- **Federation требует синхронного апгрейда обеих нод** — общий принцип v0.7.0+ остаётся (cross-node API не менялся, но полезный эффект достигается, только когда обе ноды собраны на 0.7.3).
- Сигнатура `from_federated` изменена несовместимо. Внешних вызовов в публичном API code-index нет (использовалось только из `cli::run`), но если у вас есть приватный код с прямым вызовом — обновите его.

## [0.7.2] — 2026-04-29

**Bug-fix к v0.7.1**: HTML-парсер не подхватывался в репо, у которых в `daemon.toml` явно указан `language="..."` (python/rust/bsl и т.п.). При попытке индексации `.html` файлов выдавалась ошибка `Нет парсера для расширения: html`.

### Исправлено

- **`ParserRegistry::from_languages`** теперь регистрирует HTML-парсер **всегда** дополнительно к указанному `language`. HTML — универсальный ассет (templates, generated docs, sphinx-output, vue/svelte SFC и т.п.), который встречается в репо любого «основного языка» и не указывается отдельно в `daemon.toml`. Ветка `"html" => …` в `match` сохраняется как явный no-op для документирования; реальная регистрация — после `match` безусловно.
- Это устраняет баг при `code-index index <repo> --force` для python-/rust-/bsl-репо с html-файлами.

### Совместимость

- MCP API без изменений.
- Schema БД без изменений.
- Бинарник 0.7.1 без этого фикса в production может оставаться — html-файлы просто не получат AST-записей до 0.7.2 + переиндексации.

## [0.7.1] — 2026-04-28

**HTML-парсер** через tree-sitter — добавлен **по запросу пользователя**. До 0.7.1 `.html` индексировался только как text-файл (FTS+regex+read_file). Теперь — полноценный AST с извлечением структурных сущностей: элементы с id, формы, поля ввода, ссылки, inline-скрипты/стили, CSS-классы. Сохранена обратная совместимость: search_text/grep_text/read_file для html продолжают работать через **двойную индексацию** (text_files + AST).

### Добавлено

- **Новый парсер** `crates/code-index-core/src/parser/html.rs` (~430 строк) на основе `tree-sitter-html` 0.23. Поддерживает `.html` и `.htm`. Зарегистрирован в `ParserRegistry::new_all()` и `from_languages()`.
- **Маппинг семантики HTML → таблицы code-index:**

  | HTML-конструкция | → | Таблица | Имя |
  |---|---|---|---|
  | `<element id="X">…</element>` | `classes` | `X` (body=outerHTML, bases=tag_name) |
  | `<form id|name="X">` | `classes` | `form_X` (bases="form") |
  | `<form>` без id/name | `classes` | `form_<line>` |
  | `<input/select/textarea name="Y">` | `variables` | `Y` (value=type/value-атрибут) |
  | `<a href="URL">` | `imports` | `module=URL`, `kind="link"` |
  | `<link href="URL" rel="X">` | `imports` | `module=URL`, `kind=X` (или "stylesheet") |
  | `<script src="URL">` | `imports` | `module=URL`, `kind="script"` |
  | `<img/iframe/video/audio/source/embed src="URL">` | `imports` | `module=URL`, `kind=tag_name` |
  | `<script>…inline JS…</script>` | `functions` | `inline_script_<line>` (body=содержимое) |
  | `<style>…inline CSS…</style>` | `functions` | `inline_style_<line>` (body=содержимое) |
  | Атрибут `class="foo bar baz"` | `variables` | `class:foo`, `class:bar`, `class:baz` (по одной записи на каждый) |

- **Двойная индексация**: для языков из `is_dual_indexed_language()` (на 0.7.1 — только `html`) при индексации параллельно создаётся запись в `text_files`. Это сохраняет работоспособность `search_text`/`grep_text`/`read_file` для HTML-файлов наряду с новыми structured queries (`get_class("cart")`, `find_symbol("submitOrder")`, `get_imports(module="bootstrap.css")` и т.п.). Реализовано через новое поле `text_for_fts: Option<String>` в `ParsedFile::Code` + дополнительный параметр `text_for_fts: Option<&str>` в `Indexer::write_code_to_db`.
- **Расширения файлов**: `("html", "html")` и `("htm", "html")` перенесены из TEXT_EXTENSIONS в CODE_EXTENSIONS (`indexer/file_types.rs`). Добавлена публичная функция `is_dual_indexed_language(language: &str) -> bool`.
- **13 unit-тестов** для парсера html (`parser/html.rs::tests`): id-элемент, форма с id/name/без обоих, input/select/textarea, link/script/img imports, inline-скрипт, inline-стиль, classes-атрибут, толерантность к Jinja-шаблонам, пустой HTML, вложенные элементы. Plus `file_types::html_is_code_with_dual_indexing` для проверки категоризации.
- **Tolerance к шаблонизаторам**: `{{ … }}` и `{% … %}` парсятся как text-content без падения. Структурные элементы вокруг них извлекаются нормально.

### Изменено

- **Сигнатура `Indexer::write_code_to_db`**: добавлен последний параметр `text_for_fts: Option<&str>`. Внутренний API, не MCP-видимый. Все известные callers (worker.rs:380 для html, worker.rs:416 для xml_1c) обновлены.

### Совместимость

- **MCP API без изменений** — никаких новых tool-ов, никаких новых параметров. После переиндексации html-файлы автоматически становятся доступны для существующих tool-ов: `get_class`, `find_symbol`, `search_function`, `get_imports`, `grep_body` + продолжают работать `search_text`, `grep_text`, `read_file`, `list_files`, `stat_file`.
- **Schema БД без миграций.** Используются существующие таблицы files / functions / classes / imports / variables / text_files. Двойная вставка для html — через прежний `insert_text_file`.
- **Federation без новых routes.** Внутренний механизм; обе ноды должны быть одной версии (требование 0.7.0 продолжает действовать).
- **Переиндексация:** при первом запуске v0.7.1 daemon найдёт mtime html-файлов неизменным относительно прошлой индексации и **не будет** их переиндексировать (mtime pre-filter из v0.4.0). Чтобы получить новые structured-записи для уже индексированных html, нужен либо явный re-index (`code-index index <repo>`), либо изменение mtime файла. Рекомендуется при первом обновлении на 0.7.1 — однократный полный re-index репо с html-файлами.

## [0.7.0] — 2026-04-28

**Phase 1 «read-only tools»** — закрытие пробелов code-index, чтобы удалённый репо через federation работал «как локальный» для большинства задач разведки и чтения. Релиз read-only: схема БД не трогается, переиндексация не нужна, обратная совместимость сохранена.

### Добавлено

- **`stat_file(repo, path)`** — метаданные одного файла из таблицы `files`. Возвращает `{exists, path, language, size, mtime, lines_total, content_hash, indexed_at, category}`. `category` — `"text"` (содержимое доступно через `read_file`) или `"code"` (Phase 1 не хранит content для AST-языков).
- **`list_files(repo, pattern?, path_prefix?, language?, limit?)`** — плоский список файлов с фильтрацией. `pattern` — glob (`**/*.py`), `path_prefix` — префикс (`src/auth/`). Возвращает `[{path, language, lines_total, size, mtime}]`. Без отдельного `tree`-эндпоинта — структура восстанавливается по path-строкам.
- **`read_file(repo, path, line_start?, line_end?)`** — содержимое **text-файла** (yaml/md/json/toml/xml/sh/INI/CSV/SQL и др.) через таблицу `text_files`. `line_start`/`line_end` — 1-based, inclusive. Soft-cap **5000 строк ИЛИ 500 КБ** (что наступит раньше) с флагом `truncated=true`. Hard-cap **2 МБ** даже с диапазоном (отказ). Для code-файлов — `category="code"` и пустой `content` (закроется в Phase 2). Возвращает `{content, lines_returned, lines_total, truncated, indexed_at, category}`.
- **`grep_text(repo, regex, path_glob?, language?, limit?, context_lines?)`** — regex-поиск по содержимому text-файлов через REGEXP. Закрывает дыру FTS5 со спецсимволами (точки, скобки, экраны). `path_glob` или `language` желателен — иначе full-scan, default-limit занижен до 100. `context_lines` — N строк до/после совпадения. Hard-cap на суммарный размер выдачи (1 МБ).
- **`path_glob`-параметр** в `search_function`, `search_class`, `get_function`, `get_class`, `find_symbol`, `search_text`, `grep_body`. Сужает выдачу по пути файла. Реализация — post-filter через crate `globset` после SQL-выборки. SQL-LIMIT увеличивается до 5× (но не больше 500), чтобы фильтр не оставил пустую выдачу.
- **`context_lines`-параметр** в `grep_body` — N строк контекста вокруг первых до 3 совпадений. Через новый `Storage::grep_body_with_options`. Существующий `grep_body` без context-параметра работает как раньше (обратная совместимость для cli.rs/тестов).
- **Hard-cap на суммарный размер ответа** в `grep_body` (с context_lines) и `grep_text` — 1 МБ. Защита от переполнения контекста модели на широком regex с большим context_lines.
- **`Storage::get_path_by_file_id`** — публичный метод для post-filter в MCP-слое.
- **`storage::normalize_glob`** (pub(crate)) — `**` → `*` для совместимости с привычным glob-синтаксисом (SQLite GLOB и `globset` уже понимают `*` как multi-char + `/`).
- **Federation routes:** `/federate/stat_file`, `/federate/list_files`, `/federate/read_file`, `/federate/grep_text`. Существующие routes расширены новыми параметрами в Params-структурах.
- **20 новых unit-тестов** для Phase 1: `normalize_glob`, `slice_with_caps` (4 кейса), `stat_file_meta` (3 кейса), `list_files_filtered` (3 кейса), `read_file_text` (4 кейса), `grep_text_filtered` (3 кейса), `grep_body_with_options`, `get_path_by_file_id`.

### Совместимость

- **MCP API без breaking-changes.** Все новые параметры — `Option<...>`, опциональные. Старые клиенты, не знающие о `path_glob`/`context_lines`, работают как раньше.
- **Storage API без breaking-changes.** Все существующие методы (`search_functions`, `search_classes`, `search_text`, `grep_body`, `find_symbol`) сохранили сигнатуру. Новая функциональность — в новых методах (`grep_body_with_options`) и в post-filter в MCP-слое.
- **Schema БД без изменений.** Никаких миграций, переиндексации не требуется.
- **Federation без breaking-changes.** Новые routes аддитивны. **Важно:** обе ноды federation (Windows и VM) должны быть обновлены до 0.7.0 одновременно — иначе при вызове новых tool-ов на старой ноде будет 404.

### Известные ограничения Phase 1

- **`read_file` для code-файлов** (.py/.rs/.bsl/.ts/...) возвращает `category="code"` и пустой `content`. Закроется в Phase 2 миграцией v4 + zstd-compressed blob в новой таблице `file_contents`.
- **Файлы без расширения** (Dockerfile, Makefile, Jenkinsfile, .gitignore, LICENSE) не индексируются walker-ом — слепая зона DevOps-репо. Сознательное ограничение.
- **Бинарные форматы 1С** (.epf, .erf, .cfe, .cf) не индексируются. Распаковка во внешнем pipeline.

## [0.6.1] — 2026-04-26

Технический долг rc7 закрыт: per-host порт удалённого `code-index serve` для federate-форвардинга. До 0.6.0 включительно порт удалённой ноды был захардкожен в `client.rs::DEFAULT_REMOTE_PORT = 8011`, и две serve-ноды на одной машине неизбежно перекрывали друг друга в connection pool — пара ключевалась только по IP. Изменение полностью обратно совместимое: `serve.toml` без поля `port` работает ровно как раньше (используется дефолт 8011).

### Добавлено

- **Поле `port: Option<u16>`** в `[[paths]]` секции `serve.toml` (`federation::config::ServePathEntry`). Опциональное, default — `DEFAULT_REMOTE_PORT` (8011). Метод `effective_port()` возвращает явное либо дефолт. Валидация запрещает `port = 0` (зарезервирован).
- **Поле `port: u16`** в `federation::repos::FederatedRepo` и `mcp::RepoEntry` — обязательное, заполняется из `ServePathEntry::effective_port()` при `merge`. Для local-записей значение информационное (форвардинг для них не используется).
- **Тесты:** `port_field_is_optional_and_defaults_to_remote_port`, `port_field_parses_when_explicit`, `zero_port_fails_validation` (config.rs), `port_defaults_when_not_set_and_propagates_when_set` (repos.rs), `pool_creates_separate_clients_for_different_ports_on_same_ip` (client.rs).

### Изменено

- **`RemoteClientPool` ключует клиентов по `(String, u16)`** вместо `String`. Сигнатура `get_or_create(&self, ip: &str, port: u16)`. Поле `default_port` убрано: пул сам по себе порт не фиксирует, он подаётся per-call из `RepoEntry::port`. `RemoteClientPool::new(timeout)` теперь принимает только таймаут.
- **`dispatcher::dispatch_remote` и `dispatch_remote_value` принимают `port: u16`** между `ip` и `tool`. Все 13 tool-handler-ов (`mcp/mod.rs`) и `tools::remote_stats` обновлены — пробрасывают `entry.port`.

### Совместимость

- **`serve.toml` без поля `port`** парсится как раньше, для всех записей используется `DEFAULT_REMOTE_PORT`. Никаких миграций не требуется.
- **Внешний MCP API без изменений** — поле `port` не появляется ни в одном tool-call, ни в одном tool-result. Это деталь конфигурации serve, наружу не уходит.
- **Кэширующий прокси (планируется)** будет читать `serve.toml` для определения, на какой `port` ходить к каждому репо — теперь это единая точка истины.

## [0.6.0] — 2026-04-26

Большой релиз: workspace-рефакторинг, новый бинарник `bsl-indexer` с полной 1С-спецификой, multi-config обработка одного репо с base/ + extensions/, парсинг `ConfigDumpInfo.xml` для UUID-идентификаторов отладки, опциональное LLM-обогащение процедур через cargo feature `enrichment`, защита от рассинхрона моделей через `embedding_signature`. Все наработки сделаны на ветке `workspace-refactor` (24+ коммита, 249 тестов).

### Добавлено

- **Cargo Workspace**. Один моно-крейт превращён в 4 крейта с чёткими зонами ответственности:
  - `code-index-core` (lib, publish=true) — универсальное ядро: file scanner, tree-sitter-парсеры (Python/Rust/Go/Java/JS/TS/BSL), SQLite-схема, MCP-сервер, federation.
  - `code-index` (bin, publish=true) — публичный бинарник без 1С-специфики.
  - `bsl-extension` (lib, publish=false) — 1С-специфика: XML-парсеры выгрузки, граф вызовов BSL, MCP-tools `get_object_structure`/`get_form_handlers`/`get_event_subscriptions`/`find_path`/`search_terms`, опциональный LLM-enrichment.
  - `bsl-indexer` (bin, publish=false) — приватный бинарник = core + bsl-extension. Используется на VM RAG для индексации конфигураций 1С.

- **Conditional MCP-tool registration**. MCP-сервер на старте читает `daemon.toml`, для каждого `[[paths]]` определяет `language` (явно или auto-detect по корню репо), собирает множество активных языков и регистрирует ТОЛЬКО tools от подходящих `LanguageProcessor`-ов. Если в репо нет ни одного BSL-репозитория — 1С-инструменты вообще не появляются в `tools/list`. Уведомление `notifications/tools/list_changed` отправляется при правке `daemon.toml` (file-watch с debounce 500мс через `notify-debouncer-full`).

- **`bsl-indexer` — новый отдельный бинарник** для конфигураций 1С. Релиз CI собирает его под Windows/Linux/macOS (с feature `enrichment` для прода). Подробная инструкция — в [docs/bsl-indexer.md](docs/bsl-indexer.md), деплой на VM RAG — [docs/deploy-vm-rag.md](docs/deploy-vm-rag.md).

- **Multi-config layout** (`<repo>/base/Configuration.xml` + `<repo>/extensions/<EF_*>/Configuration.xml`). `BslLanguageProcessor::detects()` теперь рекурсивно (глубина ≤ 2) находит любой `Configuration.xml`. `index_metadata_objects` обходит ВСЕ найденные конфигурации в дереве и сводит их объекты в одну таблицу (заимствованные в расширениях объекты пропускаются через `INSERT OR IGNORE`). `extension_name` хранится для каждого модуля — фильтр между base и CFE доступен запросом.

- **Таблица `metadata_modules`** с тройкой UUID для платформенного отладчика 1С (`dbgs-debug` setBreakpoint):
  - `object_id` — UUID объекта/формы из атрибута `uuid` корневого элемента в его XML.
  - `property_id` — UUID типа модуля (Module/ManagerModule/FormModule/...) — константа платформы, словарь в `module_constants.rs`.
  - `config_version` — хеш версии из `ConfigDumpInfo.xml` (отдельный парсер). Меняется при каждом изменении конфигурации.

  Эта тройка позволяет агентам ставить breakpoint'ы по человекочитаемому имени модуля, не дёргая live-ИБ. На УТ-масштаб ~8 тыс модулей, на BP-конфигурациях ~10 тыс.

- **MCP-tool `search_terms`** — третий канал семантического поиска (после `search_function` и будущего `semantic_search`). Использует FTS5 на колонке `procedure_enrichment.terms`, заполняемой LLM-обогащением. Поддерживает FTS-синтаксис (AND, OR, NOT, "точная фраза", префикс*). NULL-записи (необогащённые процедуры) просто не находятся — это progressive enhancement, не баг.

- **Подкоманда `bsl-indexer enrich [--path P] [--limit N] [--reenrich]`** под cargo feature `enrichment`. HTTP-клиент к OpenAI-compatible chat-completions endpoint (OpenRouter / Ollama / любой совместимый). Параллельная обработка через `tokio::task::JoinSet` с настраиваемым `batch_size`. Защита от рассинхрона моделей через `embedding_meta.enrichment_signature` — при смене модели в конфиге выводится warning с предложением `--reenrich`.

- **Секция `[enrichment]` в `daemon.toml`** — провайдер, URL endpoint, имя модели, имя env-переменной API-key, batch-size, шаблон промпта. По умолчанию выключено (фича опциональная).

- **Auto-detect языка с записью обратно в `daemon.toml`** через `toml_edit` (сохраняет комментарии). Алгоритм: `Configuration.xml` → bsl, `pyproject.toml`/`setup.py` → python, `Cargo.toml` → rust, `package.json` → javascript/typescript, иначе по преобладанию расширений. Если эвристика не сработала — warning в лог и пропуск (без молчаливого фолбэка).

- **`Storage::apply_schema_extensions(extensions: &[&str])`** — точка применения дополнительных DDL от LanguageProcessor'ов. Вызывается один раз при первом открытии БД репо для языка, требующего специфичных таблиц.

- **`LanguageProcessor::index_extras(repo_root, &mut storage)`** — hook для специфичных постобработок после основной индексации (например, парсинг XML и заполнение `metadata_*`-таблиц). Дефолтная реализация — no-op.

### Изменено

- **Параллельный прогон 4 репо на VM RAG (8 ядер Intel Xeon)** — суммарное время полной индексации УТ + BP_1 + BP_2 + ZUP уменьшилось с ~8м30с (последовательно) до **3м11с** (×2.7 выигрыш). Узкое место — single-thread SQLite FTS-rebuild у каждого процесса; диск (NVMe) не блокирует, разница холодный↔горячий кеш всего ~5 сек.

- **Защита от cascade-ошибок транзакций**. В каждой `index_*`-функции и `build_call_graph` добавлен идемпотентный `ROLLBACK` перед `BEGIN` — если предыдущая функция оставила открытую транзакцию, следующая корректно её закроет вместо падения с «cannot start a transaction within a transaction».

- **`config_watch::run_watch` — первичная затравка active_languages при старте**. До правки клиент, подключившийся ДО первого изменения файла, видел только core-tools (потому что в моно-режиме `RepoEntry.language=None` при загрузке через `cli::run`). После правки — первый `tools/list` сразу содержит правильный набор для текущего `daemon.toml`.

- **Настройка CI**. `.github/workflows/release.yml` теперь собирает 6 артефактов на каждый tag: `code-index` × {Windows, Linux, macOS} + `bsl-indexer` × {Windows, Linux, macOS} (с `--features enrichment`). Кеш cargo registry/git/target по `${{ runner.os }}-${{ matrix.target }}-${{ matrix.crate }}`.

### Безопасность

- **`.mcp.json` исключён из tracking** через `.gitignore` + `git rm --cached`. Файл — локальная конфигурация, содержит SSH-пути и URL'ы конкретного хоста; в репо ему не место.

- **Внутренние IP заменены на RFC 5737 doc-IP** (`192.0.2.0/24`) во всех тестах federation, комментариях и примерах конфигов. Конкретные адреса VM RAG в инструкции деплоя — на placeholder `<vm-rag-ip>`.

### Эмпирическая верификация на проде (этап 7-8)

- **Conditional registration на Claude Code 2.1.120** — `tools/list` корректно содержит 18 tools (5 BSL + 13 core) при наличии BSL-репо в `daemon.toml`, 13 tools (только core) без них.
- **`notifications/tools/list_changed` Claude Code на 2.1.120 ИГНОРИРУЕТ** — баг [anthropics/claude-code#13646](https://github.com/anthropics/claude-code/issues/13646) подтверждён эмпирически. Workaround — ручной `/mcp Reconnect`. Reconnect (issue #33779) на 2.1.120 уже корректно перечитывает `tools/list`.
- **VM RAG (Linux, 8 ядер, NVMe)** — RepoUT 53.6 с холодным кешем, 57.7 с горячим, разница 5 сек = диск не bottleneck. Параллельная индексация всех 4 репо за 3м11с при 8 ядрах × ~2 ядра rayon на процесс.

### Документация

- **[docs/bsl-indexer.md](docs/bsl-indexer.md)** — пользовательская инструкция по `bsl-indexer`: что умеет, как собрать с/без feature `enrichment`, как настроить enrichment с OpenRouter / Ollama, ограничения MCP-клиентов с workaround'ом.
- **[docs/bsl-indexer-architecture.md](docs/bsl-indexer-architecture.md)** — полное архитектурное ТЗ workspace-refactor с обоснованиями решений.
- **[docs/deploy-vm-rag.md](docs/deploy-vm-rag.md)** — пошаговая инструкция деплоя на VM (установка Rust toolchain, копирование исходников, настройка daemon.toml, systemd-unit, A/B-протокол сравнения с pg_indexer).
- **[deploy/systemd/bsl-indexer-daemon.service](deploy/systemd/bsl-indexer-daemon.service)** — готовый systemd-unit с лимитами ресурсов и защитой от записи вне разрешённых каталогов.

## [0.5.0-rc6] — 2026-04-25

### Добавлено

- **Федеративная архитектура `code-index serve`** (по образцу `1c-router`/`mcp__1c__`). Один процесс serve обслуживает реестр репозиториев из нескольких машин: для каждого tool-call с `repo=X` локальный serve смотрит ip — если совпадает с `[me].ip`, читает локальный SQLite, иначе делает HTTP-вызов к удалённому serve. Источник истины для каждого репо — на одной машине (это прокси, не репликация).

  **Новый конфиг** [`serve.toml`](src/federation/config.rs) — глобальный, одинаковый на всех нодах (раскатывается через общий git-репо `code-index-config`):

  ```toml
  [me]
  ip = "192.0.2.10"
  # token = "..."   # опционально, в rc6 не валидируется (заготовка под rc7)

  [[paths]]
  alias = "ut"
  ip = "192.0.2.50"

  [[paths]]
  alias = "dev"
  ip = "192.0.2.10"
  ```

  `daemon.toml` остаётся локальным (только пути этой машины, без изменений в схеме).

- **Внутренний endpoint `POST /federate/<tool_name>`** ([`src/federation/server.rs`](src/federation/server.rs)) — приёмная сторона форвардинга. Тело запроса — JSON, точно соответствующий нашим `*Params`-структурам. Ответ — то же, что вернул бы локальный tool-handler. `/federate` живёт на том же axum-роутере, что `/mcp`, защищён общим whitelist middleware.

- **IP-whitelist middleware** ([`src/federation/whitelist.rs`](src/federation/whitelist.rs)). serve биндится на `[me].ip` (не на `127.0.0.1`, не на `0.0.0.0`) — порт активен только на одном интерфейсе. Допустимые peer-IP — из `{все [[paths]].ip} ∪ {127.0.0.1, ::1}`. Чужой peer → `403 {"error":"forbidden","peer":"..."}`.

- **Параллельный fan-out у `get_stats(repo=None)`** ([`src/mcp/tools.rs`](src/mcp/tools.rs)) через `tokio::task::JoinSet`. Каждый remote-репо опрашивается с таймаутом 5 сек; недоступные возвращаются как `{"repo":"...","status":"unreachable","error":"..."}`, не блокируя остальные.

- **Флаг `--serve-config <FILE>` у `code-index serve`**. Если флаг не задан — ищется `$CODE_INDEX_HOME/serve.toml`. Если файла нет — serve работает как rc5 (моно-режим, bind 127.0.0.1, без whitelist). При `transport=stdio` или явном `--path` федерация не активируется.

  ```bash
  # Федеративный режим (rc6+):
  code-index serve --transport http --port 8011

  # Совместимый rc5-режим (моно):
  code-index serve --transport http --port 8011 --path ut=C:/RepoUT
  ```

- **Пул переиспользуемых HTTP-клиентов** ([`src/federation/client.rs`](src/federation/client.rs)) — один `reqwest::Client` на удалённый IP, lazy init через `RemoteClientPool::get_or_create`. Таймаут 5 сек; idle pool 60 сек.

### Изменено

- **`RepoEntry` теперь хранит `ip` и `is_local`**, поля `root_path` и `storage` обёрнуты в `Option` (`None` для remote). Старые конструкторы `open_readonly_multi` / `open_readonly` / `with_storage` ставят `is_local=true`, `ip="127.0.0.1"` — обратная совместимость для тестов и моно-режима.

- **`serve_http` принимает опциональные `federate_router` и `whitelist`**. Если переданы — `Router::merge` для `/federate/*` и `axum::middleware::from_fn_with_state` для whitelist. Listener теперь использует `into_make_service_with_connect_info::<SocketAddr>()` — без этого peer-IP в middleware не извлечётся.

- **`--host` стал `Option<String>`**. Если задан — приоритет CLI; иначе при наличии serve.toml — `[me].ip`, иначе `127.0.0.1` (rc5-default).

### Защита от циклов

- **Никаких заголовков** типа `X-Forwarded-Already`. Защита — статически по конфигу: каждая нода знает свой `[me].ip`, форвардит только если `repo.ip != own_ip`. При расхождении конфигов (`A: X→B`, `B: X→A`) запрос упадёт по таймауту 5s с понятной ошибкой.
- Приёмник `/federate/get_stats` без `repo` ограничивает fan-out только своими local-репо (не делает рекурсивный обход к чужим), чтобы исключить круг между нодами.

### Roadmap (вне rc6)

- Создание git-репо `code-index-config` с шаблоном `serve.toml` — операционная задача.
- Linux-бинарник + systemd unit для деплоя на VM 200.
- `[me].token` авторизация — Bearer header в `/federate/*`, проверка в whitelist-middleware. Поле уже парсится в схеме serve.toml.
- HEAD-ping к удалённым в `health` — низкоприоритетная фича.
- Hot-reload `serve.toml` без рестарта (`POST /reload` для serve).

## [0.5.0-rc5] — 2026-04-22

### Добавлено

- **HTTP-транспорт у `code-index serve`** через rmcp `StreamableHttpService`. Один процесс обслуживает все репозитории под `mcp-supervisor`, клиенты подключаются к общему URL без копирования `--path` в каждый `.mcp.json`.

  ```bash
  # stdio (per-session, как раньше)
  code-index serve --path ut=C:/RepoUT --path bp=C:/RepoBP

  # http (shared process)
  code-index serve --transport http --port 8011 --config C:/tools/code-index/daemon.toml
  ```

  Клиентский `.mcp.json`:
  ```json
  "code-index": { "type": "http", "url": "http://127.0.0.1:8011/mcp" }
  ```

  Реализация — [`src/main.rs`](src/main.rs) `serve_http`: `StreamableHttpService::new(factory, LocalSessionManager, StreamableHttpServerConfig::default())`, монтируется в `axum::Router::nest_service("/mcp", svc)`. Фабрика клонирует уже собранный `CodeIndexServer` (он `Clone`), так что все сессии разделяют общий набор открытых SQLite-баз.

- **Multi-repo в одном serve-процессе**. `--path` теперь принимает `alias=dir` и может указываться многократно — каждый tool-call передаёт параметр `repo=<alias>` для выбора репозитория. Без `=` — старый контракт `alias=default`. Параметры инструментов получили поле `repo: String`, внутренняя структура `RepoEntry` держит открытую read-only `Storage` и `root_path` per-репо.

- **Поле `alias` в `[[paths]]` daemon.toml** — [`src/daemon_core/config.rs`](src/daemon_core/config.rs) `PathEntry::alias: Option<String>`. Если не задан — алиас вычисляется через `PathEntry::effective_alias()` из последнего сегмента пути (нижний регистр, пробелы → `_`). Демон поле игнорирует; использует только `code-index serve --config ...` при сборке списка репо. Старые конфиги без `alias` продолжают работать (`#[serde(default)]`).

- **Флаги `--host`, `--port`, `--config` у `serve`**. `--config` указывает на `daemon.toml` — список репо и алиасов берётся оттуда. CLI `--path` имеет приоритет над конфигом. Порт по умолчанию — 8011 (следующий свободный в диапазоне mcp-supervisor: 8001/8002/8007/8010).

### Зависимости

- Включена фича `rmcp/transport-streamable-http-server` (подтянула `transport-streamable-http-server-session`, `server-side-http`, транзитивно — `uuid`, `sse-stream`). `axum` и `tower` уже были в deps для health-endpoint демона.

## [0.5.0-rc4] — 2026-04-17

### Исправлено

- **Демон падал при закрытии консоли на Windows**. `code-index` собирается как console-subsystem приложение: при запуске в пользовательской сессии (Scheduled Task c `LogonType=Interactive`, ручной вызов из `cmd`/PowerShell) процесс получает консольное окно и становится его дочерним. Закрытие окна отправляет `CTRL_CLOSE_EVENT`, и демон уходит вместе с ним. Для штатной установки через `scripts/install-daemon-autostart.ps1` это означало, что окно консоли всплывало при logon и любое его закрытие останавливало индексацию.

  **Фикс**: в [`src/main.rs`](src/main.rs) `handle_daemon` при `daemon run` на Windows выполняет self-detach — перезапускает себя с флагами `DETACHED_PROCESS | CREATE_NO_WINDOW`, выставляет переменную окружения `CODE_INDEX_DAEMON_DETACHED=1` и завершает родительский процесс. Detached-клон работает без консоли и переживает закрытие любой родительской сессии. На Unix self-detach не выполняется — демонизацией управляет `systemd`/`launchd`.

  Реализация использует только `std::os::windows::process::CommandExt::creation_flags`, новых зависимостей не добавляет.

## [0.5.0-rc3] — 2026-04-17

### Исправлено

- **Race condition при atomic-save редакторов**. Редакторы (VS Code, IDE, `git`) сохраняют файлы атомарно: сначала пишут во временный `<имя>.tmp.<pid>.<ts>`, затем переименовывают в целевой файл. Watcher через `ReadDirectoryChangesW` успевал увидеть событие `Create` на `.tmp`-файле, но к моменту вызова `hasher::file_hash()` файл уже был переименован. В логи лилась стена ошибок вида `file_hash \\?\...\.mcp.json.tmp.10296.1776427368309: Не удается найти указанный файл. (os error 2)`.

  **Фикс**: в [`daemon_core/worker.rs`](src/daemon_core/worker.rs) `apply_event` при `io::ErrorKind::NotFound` от `file_hash` теперь тихо выходит из обработчика. Логируются только настоящие ошибки (permission denied, read error и т.п.).

### Добавлено

- **Поле `exclude_file_patterns` в `.code-index/config.json`** — glob-паттерны имён файлов для исключения из индексации. Дополняет существующее `exclude_dirs`:

  ```json
  {
    "exclude_dirs": [".vscode", "experimental"],
    "exclude_file_patterns": ["*.tmp.*", "*.bak", "*.orig", "*.swp"]
  }
  ```

  Паттерны матчатся по **basename** (имя файла без пути). Применяются:
  - в [`watcher.rs`](src/watcher.rs) — события от файлов, попавших под паттерн, отбрасываются до вызова `file_hash`;
  - в [`indexer/mod.rs`](src/indexer/mod.rs) `collect_candidates` / `collect_candidates_standalone` — файл исключается из обхода WalkDir до категоризации.

  Синтаксис glob через crate `globset`. Некорректные паттерны логируются и пропускаются (не ломают старт).

### Зависимости

- Добавлен `globset = "0.4"`.

## [0.5.0-rc2] — 2026-04-17

### Исправлено

- **WAL-файлы разрастались до десятков ГБ в production**. После сутки работы на нашем стенде с 13 индексируемыми папками (5 крупных 1С-репо + 8 модулей MCP) WAL-файлы заняли ~43 ГБ при суммарном размере основных БД ~16 ГБ:

  | Репо | `index.db` | `index.db-wal` (до фикса) |
  |------|-----------|---------------------------|
  | RepoBP_2 | 4.7 ГБ | **19 ГБ** |
  | RepoUT | 2.1 ГБ | **17 ГБ** |
  | RepoZUP | 5.1 ГБ | 5.1 ГБ |
  | dbgs-debug | 1.4 ГБ | 1.4 ГБ |

  Свободное место на системном диске сократилось на ~45 ГБ за сутки.

  **Причина**, найденная анализом кода:
  1. `PRAGMA wal_autocheckpoint=500` (добавлен в v0.5.0-rc1) переносит страницы из WAL в основную БД, но **не уменьшает физический файл WAL** — это делает только явный `PRAGMA wal_checkpoint(TRUNCATE)`.
  2. На bulk-нагрузке (initial reindex 90К файлов, частые watcher-batch'и) checkpoint не успевает за темпом записи.
  3. В worker'е после каждого batch вызывался `Storage::flush_to_disk()` через `Connection::backup()` — в disk-режиме (а worker в нём всегда после reopen) это бесполезное копирование БД самой в себя, WAL не уменьшается.

  **Фикс**:
  - Добавлен метод `Storage::checkpoint_truncate()` — обёртка над `PRAGMA wal_checkpoint(TRUNCATE)`, реально схлопывает WAL.
  - В `worker.rs` после initial reindex (когда worker гарантированно в disk-режиме) — обязательный `checkpoint_truncate`. Это самый «жирный» источник WAL.
  - В watcher-цикле после `commit_batch` — `flush_to_disk` заменён на `checkpoint_truncate`. На graceful shutdown — то же самое.

  **Результат проверки на тех же 13 папках**: WAL остаётся **0 байт** после initial reindex и после правок файлов через watcher. Освобождено ~48 ГБ.

## [0.5.0-rc1] — 2026-04-17

Крупная переработка архитектуры: разделение на **фоновый демон-писатель** и **MCP-читателей**.

### Ломающие изменения

- **`code-index serve` теперь read-only**. Он больше не индексирует и не держит watcher — только подключается к БД, которую поддерживает отдельный демон. Если демон не запущен или папка не в его конфиге, tool-call возвращает структурированный ответ `{"status": "daemon_offline" | "not_started" | "indexing", ...}`, а не падает.
- **Per-project PID-lock удалён** (файл `.code-index/serve.pid` больше не создаётся). Сколько угодно MCP-процессов могут подключаться к одной `.code-index/index.db` параллельно.
- **Флаги `--no-watch`, `--flush-interval`** у `serve` удалены — они были специфичны для прежней роли writer'а и к read-only неприменимы.

### Добавлено

- **Подкоманда `daemon`**: `code-index daemon run/start/stop/status/reload`. `run` — foreground (для Scheduled Task / systemd), `start`/`stop`/`status`/`reload` — HTTP-клиент к запущенному демону.
- **Переменная окружения `CODE_INDEX_HOME`** — единая точка конфигурации. Содержит `daemon.toml`, туда же кладутся runtime-файлы `daemon.pid`, `daemon.json`, `daemon.log`. Работает и через системную переменную (`setx`), и через блок `"env"` в `.mcp.json`.
- **Конфиг `daemon.toml`** со списком отслеживаемых папок и параметрами:
  - `max_concurrent_initial` — сколько папок одновременно в фазе initial reindex (дефолт `1`, защита от RAM-всплеска).
  - Per-folder `debounce_ms` / `batch_ms` — переопределение watcher-задержки per-проект.
- **HTTP health-IPC на loopback**: `GET /health`, `GET /path-status?path=...`, `POST /reload`, `POST /stop`. Порт выбирается автоматически и пишется в `daemon.json`.
- **Per-folder жизненный цикл**: `not_started → initial_indexing → ready ⇄ reindexing_batch | error`. Видно в `daemon status`.
- **PowerShell-скрипт** `scripts/install-daemon-autostart.ps1` для установки Scheduled Task (триггер — вход пользователя, автоматически делает `setx CODE_INDEX_HOME`).

### Изменено

- **Память**: только один in-memory SQLite-storage одновременно. После initial reindex worker делает flush → переоткрывает тот же файл в disk-режиме (WAL) → освобождает permit семафора. Пиковая RAM не суммируется по папкам.
- **Повторный запуск**: если `.code-index/index.db` уже существует, worker открывает её сразу в disk-режиме (пропуская backup disk→memory→disk). На 2 1С-репо по ~90К файлов повторный старт — **~12 с** (раньше ~600 с при том же коде до фикса).
- **SQLite**: добавлены `PRAGMA wal_autocheckpoint=500` и `PRAGMA journal_size_limit=67108864` — WAL-файл не раздувается за длинные транзакции, truncate'ится до 64 МБ после checkpoint.
- **MCP-сервер** проверяет статус папки у демона перед каждым tool-call. Если папка не `ready` — возвращает структурированный JSON с прогрессом/подсказкой, а не эмпирический результат из неактуального индекса.

### Удалено

- Legacy-модули `src/daemon.rs` и `src/pidlock.rs`.

### Замеры (1С-репо, 2 папки по 88–92К файлов, 80% XML)

| Сценарий | Время |
|----------|-------|
| Initial reindex с нуля, обе папки последовательно (`max_concurrent_initial=1`) | ~10 мин, RAM-пик ~6 ГБ |
| Повторный старт на существующей БД | ~12 с |
| Watcher: от правки файла до появления в индексе | ~1.6 с (из них debounce 1.5 с — конфигурируется) |
| Корректное завершение (`daemon stop`) | БД на диске без `-wal`/`-shm` файлов |

### Технический долг

- Прогресс `0/0` в `daemon status` во время initial reindex (косметика — не обновляется из блокирующей фазы).
- Linux / macOS не проверены вживую — есть только теоретическая кроссплатформенность через crate `dirs` и `notify`. Просьба feedback при первых запусках на не-Windows.
- Интеграционных тестов для daemon_core нет — только unit-тесты на `config`, `ipc`, `state`.

## [0.4.0] — 2026-03-30

- `mtime`+`file_size` pre-filter для initial reindex: 93К файлов пере-проверяются за ~4 с вместо ~163 с (SHA-256 только для изменившихся файлов).
- Миграция `migrate_v3` — добавляет колонки `mtime`, `file_size` в таблицу `files`.
- Per-project PID-lock (`.code-index/serve.pid`) — защита от одновременного запуска двух `serve` на один проект.

## [0.3.0] — 2026-03-...

- Параллельный read+hash через rayon.
- `hash_ast` без `to_sexp` (быстрее).
- Снятие `max_file_size` для кода — большие BSL/XML-файлы теперь индексируются целиком.
- Тюнинг `mmap_size`, `batch_size` для initial indexation.
