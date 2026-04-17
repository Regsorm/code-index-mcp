# Changelog

Формат — [Keep a Changelog](https://keepachangelog.com/ru/1.0.0/).
Версионирование — [SemVer](https://semver.org/lang/ru/).

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
