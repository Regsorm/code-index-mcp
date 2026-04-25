# Changelog

Формат — [Keep a Changelog](https://keepachangelog.com/ru/1.0.0/).
Версионирование — [SemVer](https://semver.org/lang/ru/).

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

  Эта тройка позволяет агентам ставить breakpoint'ы по человекочитаемому имени модуля, не дёргая live-ИБ. На УТ-масштаб ~8 тыс модулей, на BP_SS/BP_TDK ~10 тыс.

- **MCP-tool `search_terms`** — третий канал семантического поиска (после `search_function` и будущего `semantic_search`). Использует FTS5 на колонке `procedure_enrichment.terms`, заполняемой LLM-обогащением. Поддерживает FTS-синтаксис (AND, OR, NOT, "точная фраза", префикс*). NULL-записи (необогащённые процедуры) просто не находятся — это progressive enhancement, не баг.

- **Подкоманда `bsl-indexer enrich [--path P] [--limit N] [--reenrich]`** под cargo feature `enrichment`. HTTP-клиент к OpenAI-compatible chat-completions endpoint (OpenRouter / Ollama / любой совместимый). Параллельная обработка через `tokio::task::JoinSet` с настраиваемым `batch_size`. Защита от рассинхрона моделей через `embedding_meta.enrichment_signature` — при смене модели в конфиге выводится warning с предложением `--reenrich`.

- **Секция `[enrichment]` в `daemon.toml`** — провайдер, URL endpoint, имя модели, имя env-переменной API-key, batch-size, шаблон промпта. По умолчанию выключено (фича опциональная).

- **Auto-detect языка с записью обратно в `daemon.toml`** через `toml_edit` (сохраняет комментарии). Алгоритм: `Configuration.xml` → bsl, `pyproject.toml`/`setup.py` → python, `Cargo.toml` → rust, `package.json` → javascript/typescript, иначе по преобладанию расширений. Если эвристика не сработала — warning в лог и пропуск (без молчаливого фолбэка).

- **`Storage::apply_schema_extensions(extensions: &[&str])`** — точка применения дополнительных DDL от LanguageProcessor'ов. Вызывается один раз при первом открытии БД репо для языка, требующего специфичных таблиц.

- **`LanguageProcessor::index_extras(repo_root, &mut storage)`** — hook для специфичных постобработок после основной индексации (например, парсинг XML и заполнение `metadata_*`-таблиц). Дефолтная реализация — no-op.

### Изменено

- **Параллельный прогон 4 репо на VM RAG (8 ядер Intel Xeon)** — суммарное время полной индексации УТ + BP_SS + BP_TDK + ZUP уменьшилось с ~8м30с (последовательно) до **3м11с** (×2.7 выигрыш). Узкое место — single-thread SQLite FTS-rebuild у каждого процесса; диск (NVMe) не блокирует, разница холодный↔горячий кеш всего ~5 сек.

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
  | RepoBP_TDK | 4.7 ГБ | **19 ГБ** |
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
