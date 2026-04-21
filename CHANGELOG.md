# Changelog

Формат — [Keep a Changelog](https://keepachangelog.com/ru/1.0.0/).
Версионирование — [SemVer](https://semver.org/lang/ru/).

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
