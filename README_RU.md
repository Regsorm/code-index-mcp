<a href="https://infostart.ru/1c/tools/2677918/" title="Публикация на Инфостарте">
  <img src="https://infostart.ru/bitrix/templates/sandbox_empty/assets/tpl/abo/img/logo.svg" alt="Infostart" height="32">
</a>

Опубликовано на Инфостарте: [Code Index — структурный поиск по выгрузке кода 1С через MCP](https://infostart.ru/1c/tools/2677918/)

---

# Code Index MCP

[English version](README.md)

Мгновенный поиск по коду для AI-моделей. Заменяет grep на запросы за миллисекунды.

> **Ключевые метрики:** 93K файлов перепроверяются за 4 сек (mtime fast-path) · 282K функций за <1 мс · 8 языков · 12 MCP-инструментов

## Проблема

AI-модели тратят десятки вызовов `grep`/`find` для навигации по большим проектам. На крупных кодовых базах это превращается в минуты ожидания.

Например, найти все места использования `RuntimeErrorProcessing` в Java-проекте с помощью стандартных инструментов — это 14 вызовов `grep`/`find`, которые выполняются последовательно. С Code Index — один запрос, мгновенный ответ.

## Решение

Скомпилированный Rust-бинарник с архитектурой **один писатель, много читателей**:

1. Парсит исходный код в AST через tree-sitter
2. Индексирует результат в SQLite с FTS5 для полнотекстового поиска
3. Отдельный **фоновый демон** — единственный писатель: один процесс на машину, который следит за списком папок из своего конфига и поддерживает `.code-index/index.db` в актуальном состоянии.
4. **MCP-сервер** — тонкий **read-only**-клиент: сколько угодно параллельных Claude Code / VS Code / субагентов могут работать с одним проектом одновременно без конфликтов pidlock и без повторной индексации на каждой сессии.

## Поддерживаемые языки

| Язык | Парсер | Расширения |
|------|--------|------------|
| Python | tree-sitter-python | `.py` |
| JavaScript | tree-sitter-javascript | `.js`, `.jsx` |
| TypeScript | tree-sitter-typescript | `.ts`, `.tsx` |
| Java | tree-sitter-java | `.java` |
| Rust | tree-sitter-rust | `.rs` |
| Go | tree-sitter-go | `.go` |
| 1С (BSL) | tree-sitter-onescript | `.bsl`, `.os` |
| XML (1С) | quick-xml | `.xml` (метаданные конфигураций) |

Текстовые файлы (`.md`, `.json`, `.yaml`, `.toml`, `.xml`, `.sql`, `.env` и др.) индексируются для полнотекстового поиска.

## Быстрый старт

### Сборка из исходников

```bash
git clone https://github.com/Regsorm/code-index-mcp.git
cd code-index-mcp
cargo build --release -p code-index               # публичный бинарник для Python/Rust/Go/Java/JS/TS
cargo build --release -p bsl-indexer --features enrichment   # дополнительная сборка с поддержкой 1С + LLM-обогащением
```

Бинарники:
* `target/release/code-index[.exe]` — основной (без 1С).
* `target/release/bsl-indexer[.exe]` — с полной поддержкой 1С (XML-парсеры, граф вызовов BSL, MCP-tools `get_object_structure`/`get_form_handlers`/`find_path`/`search_terms` и опциональный LLM-enrichment под cargo feature `enrichment`).

В Releases на GitHub публикуются 6 готовых артефактов на каждый тег: `code-index` × {Win, Linux, macOS} + `bsl-indexer` × {Win, Linux, macOS}.

### Настройка фонового демона (v0.5+)

Портативная раскладка: одна папка на всё (бинарник + конфиг + runtime-файлы). На неё указывает переменная окружения `CODE_INDEX_HOME`.

1. Создайте папку для демона, положите туда `code-index.exe` (например, `C:\tools\code-index\`).

2. Укажите переменную `CODE_INDEX_HOME`:

   **Windows (постоянно, для пользователя):**
   ```powershell
   setx CODE_INDEX_HOME "C:\tools\code-index"
   # Откройте новую консоль — переменная видна там.
   ```

   **Linux** — добавьте в `~/.bashrc` или `~/.zshrc`:
   ```bash
   export CODE_INDEX_HOME="$HOME/.local/code-index"
   ```

   **macOS** — то же самое для shell; для launchd-агентов используйте `launchctl setenv`.

   **Любая ОС — локально на уровне одного проекта через `.mcp.json`** (системную переменную трогать не нужно):
   ```json
   {
     "mcpServers": {
       "code-index": {
         "command": "C:\\tools\\code-index\\code-index.exe",
         "args": ["serve", "--path", "."],
         "env": { "CODE_INDEX_HOME": "C:\\tools\\code-index" }
       }
     }
   }
   ```

3. Создайте `daemon.toml` в этой папке и перечислите отслеживаемые папки:

   ```toml
   [daemon]
   http_port = 0                  # 0 = выбрать свободный порт автоматически
   max_concurrent_initial = 1     # папки обрабатываются последовательно при initial reindex

   [[paths]]
   path = "C:\\RepoUT"

   [[paths]]
   path = "C:\\RepoBP_1"
   debounce_ms = 500              # per-папка переопределение: быстрее чем дефолт 1500 мс
   batch_ms    = 1000
   ```

   Per-папка `debounce_ms` / `batch_ms` — **необязательны**. Если не заданы, демон использует значения из `.code-index/config.json` проекта, а если и там нет — встроенные дефолты (1500 мс / 2000 мс).

4. Запустите демон (foreground):

   ```bash
   code-index daemon run
   ```

   Либо установите автозапуск через Windows Scheduled Task (триггер — вход пользователя; скрипт сам сделает `setx CODE_INDEX_HOME`):

   ```powershell
   powershell -ExecutionPolicy Bypass -File scripts\install-daemon-autostart.ps1 `
     -BinaryPath "C:\tools\code-index\code-index.exe" `
     -CodeIndexHome "C:\tools\code-index" `
     -StartNow
   ```

5. Проверка статуса:

   ```bash
   code-index daemon status        # человекочитаемо
   code-index daemon status --json # JSON
   code-index daemon reload        # перечитать daemon.toml после редактирования
   code-index daemon stop
   ```

Если `CODE_INDEX_HOME` не задан, демон использует fallback: `%APPDATA%\code-index\daemon.toml` для конфига и `%LOCALAPPDATA%\code-index\` для runtime-файлов (на Linux/macOS — соответствующие XDG-каталоги).

### Одноразовая индексация (без демона)

```bash
code-index index /path/to/project
code-index stats --path /path/to/project --json
```

### Запуск MCP-сервера (read-only)

```bash
code-index serve --path /path/to/project
```

Это тонкий read-only-клиент демона. Сам он не индексирует — это делает демон. Если папка ещё индексируется или её нет в `daemon.toml`, инструменты возвращают структурированный ответ `{status, message, progress}` вместо падения.

### Транспорты (stdio и HTTP)

`serve` поддерживает два транспорта:

| Транспорт | Модель процесса | Когда использовать |
|-----------|-----------------|-------------------|
| `stdio` (по умолчанию) | Один процесс `serve` на каждую MCP-сессию | Простые сетапы, один клиент, разовые запуски |
| `http` (streamable) | Один общий процесс, много клиентов по `http://host:port/mcp` | Мульти-проектные сетапы, управление через супервизор, чтобы не дублировать CLI-аргументы в каждой сессии |

```bash
# stdio — per-session, алиасы задаются в CLI
code-index serve --path ut=/repos/ut --path bp=/repos/bp

# HTTP — общий процесс, алиасы берутся из daemon.toml
code-index serve --transport http --port 8011 --config /etc/code-index/daemon.toml
```

`--path` принимает форму `alias=dir` и может повторяться (мульти-репо режим). Каждый tool-call получает параметр `repo` для выбора репозитория. Без `=` — старый одиночный контракт под `alias=default`.

В HTTP-режиме при указании `--config` алиасы берутся из `[[paths]]` файла `daemon.toml`: явный `alias = "..."` либо вычисляется из последнего сегмента пути (нижний регистр, пробелы → `_`). CLI-аргумент `--path` имеет приоритет над конфигом.

## Подключение к Claude Code

Добавьте в `.mcp.json` вашего проекта. Для `stdio`:

```json
{
  "mcpServers": {
    "code-index": {
      "type": "stdio",
      "command": "/path/to/code-index",
      "args": ["serve", "--path", "."]
    }
  }
}
```

Для общего HTTP-процесса:

```json
{
  "mcpServers": {
    "code-index": {
      "type": "http",
      "url": "http://127.0.0.1:8011/mcp"
    }
  }
}
```

## MCP-инструменты

| Инструмент | Описание |
|------------|----------|
| `search_function` | Полнотекстовый поиск по функциям (имя, docstring, тело) |
| `search_class` | Полнотекстовый поиск по классам |
| `get_function` | Получить функцию по точному имени |
| `get_class` | Получить класс по точному имени |
| `get_callers` | Кто вызывает данную функцию? |
| `get_callees` | Что вызывает данная функция? |
| `find_symbol` | Поиск символа везде (функции, классы, переменные, импорты) |
| `get_imports` | Импорты по модулю или файлу |
| `get_file_summary` | Полная карта файла без чтения исходника |
| `get_stats` | Статистика индекса |
| `search_text` | Полнотекстовый поиск по текстовым файлам |
| `grep_body` | Поиск подстроки или regex в телах функций и классов. Возвращает `match_lines` (первые 3 номера строк) и `match_count` (всего, если > 3). v0.7.0: опциональные `path_glob`, `context_lines` |
| `stat_file` | **(v0.7.0)** Метаданные одного файла: exists, size, mtime, language, lines_total, content_hash, indexed_at, category (`text`/`code`) |
| `list_files` | **(v0.7.0)** Плоский список файлов с опциональными `pattern` (glob `**/*.py`), `path_prefix`, `language`, `limit` |
| `read_file` | **(v0.7.0)** Чтение содержимого **text-файла**. Опциональные `line_start`/`line_end` (1-based, inclusive). Soft-cap 5000 строк или 500 КБ, hard-cap 2 МБ. Для code-файлов вернётся `category="code"` с пустым content (Phase 2 в работе) |
| `grep_text` | **(v0.7.0)** Regex-поиск по содержимому text-файлов через REGEXP. Закрывает дыру FTS5 со спецсимволами (точки, скобки, экраны). Опциональные `path_glob`, `language`, `context_lines`. Hard-cap 1 МБ на размер ответа |
| `health` | Статус MCP-сервера и подключённых репо |

Все поисковые инструменты (`search_function`, `search_class`, `get_function`, `get_class`, `find_symbol`, `search_text`, `grep_body`) принимают опциональный параметр **`path_glob`** (v0.7.0) для сужения выдачи по подкаталогу (например, `src/auth/**`, `Documents/**/*.bsl`). Реализация — post-filter через crate `globset` после SQL-выборки.

### Дополнительно для 1С-репо (только в `bsl-indexer`, v0.6+)

При наличии BSL-репо в `daemon.toml` (`language = "bsl"`) автоматически добавляются 5 BSL-инструментов:

| Инструмент | Описание |
|------------|----------|
| `get_object_structure` | Структура объекта конфигурации 1С (Catalog, Document, InformationRegister...) по `full_name` вида `Document.РеализацияТоваровУслуг` |
| `get_form_handlers` | Обработчики событий управляемой формы по `(owner_full_name, form_name)`. Например, для `Documents.РеализацияТоваровУслуг` / `ФормаДокумента` отдаёт ~120 пар `(event, handler)` |
| `get_event_subscriptions` | Все подписки на события из `EventSubscriptions/*.xml` с фильтром по handler-модулю |
| `find_path` | Цепочка вызовов между двумя процедурами через `proc_call_graph` (recursive CTE, max_depth=3) |
| `search_terms` | FTS-поиск по бизнес-терминам процедур, обогащённым LLM (после `bsl-indexer enrich`) |

Эти инструменты появляются в `tools/list` **только при наличии BSL-репо** (conditional registration). При смене состава репо в `daemon.toml` сервер шлёт `notifications/tools/list_changed`; на текущей версии Claude Code 2.1.120 уведомление [игнорируется](https://github.com/anthropics/claude-code/issues/13646), workaround — `/mcp Reconnect`.

Подробности и инструкция по настройке — [docs/bsl-indexer.md](docs/bsl-indexer.md).

Все инструменты поддерживают фильтр по языку: `search_function(query="X", language="python")`

### grep_body

В отличие от FTS-поиска, `grep_body` поддерживает буквальные подстроки (включая точки и спецсимволы) и регулярные выражения. Это критично для поиска обращений к объектам метаданных 1С вида `Справочники.Контрагенты`.

```
grep_body(pattern="Справочники.Контрагенты", language="bsl")
grep_body(regex="Справочники\\.(Контрагенты|Организации)", language="bsl")
```

Возвращает `[{file_path, name, kind, line_start, line_end, match_lines, match_count}]` — конкретные функции/классы с совпадениями.

Каждый результат содержит `match_lines` — до 3 абсолютных номеров строк в файле, где найдено совпадение. Если совпадений больше 3, `match_count` показывает общее количество.

```json
[
  {
    "file_path": "src/Catalogs/Products/ObjectModule.bsl",
    "name": "OnWrite",
    "kind": "function",
    "line_start": 45,
    "line_end": 82,
    "match_lines": [51, 63, 78]
  }
]
```

## Справочник CLI

Все 14 подкоманд с описанием параметров:

```bash
# Фоновый демон (писатель — один на машину)
code-index daemon run                          # foreground, запускается Scheduled Task / systemd
code-index daemon status [--json]              # GET /health через loopback
code-index daemon reload                       # перечитать daemon.toml
code-index daemon stop                         # POST /stop

# MCP-сервер (read-only клиент; используется Claude Code, VS Code, субагентами)
code-index serve --path /project

# Однократная индексация (без демона)
code-index index /project [--force]

# Управление проектом
code-index init --path /project          # Создать конфиг
code-index clean --path /project         # Удалить устаревшие записи
code-index stats --path /project [--json]

# Поиск символов
code-index query "имя" --path /project [--language rust] [--json]

# Полнотекстовый поиск (JSON вывод)
code-index search-function "запрос" --path /project [--language python] [--limit 20]
code-index search-class "запрос" --path /project [--language python] [--limit 20]
code-index search-text "запрос" --path /project [--limit 20]

# Точный поиск (JSON вывод)
code-index get-function "точное_имя" --path /project
code-index get-class "точное_имя" --path /project

# Граф вызовов (JSON вывод)
code-index get-callers "имя_функции" --path /project [--language python]
code-index get-callees "имя_функции" --path /project [--language python]

# Навигация (JSON вывод)
code-index get-imports --path /project [--module "имя"] [--file-id 42]
code-index get-file-summary "src/main.rs" --path /project

# Поиск подстроки или regex в телах функций/классов (поддерживает точки и спецсимволы)
code-index grep-body --pattern "Справочники.Контрагенты" --path /project [--language bsl] [--limit 100]
code-index grep-body --regex "Справочники\.(Контрагенты|Организации)" --path /project
```

## Использование CLI из субагентов

Субагенты (Agent tool в Claude Code) не имеют доступа к MCP-серверам. Все 12 MCP-инструментов продублированы как CLI-подкоманды с JSON-выводом — это позволяет использовать code-index из любого подпроцесса или скрипта.

```bash
# Вместо MCP-вызова search_function:
code-index search-function "authenticate" --path /my/project --language python

# Граф вызовов через CLI:
code-index get-callers "process_order" --path /my/project

# Карта файла:
code-index get-file-summary "src/auth/login.py" --path /my/project
```

## Настройка CLAUDE.md

Добавьте в `CLAUDE.md` вашего проекта, чтобы субагенты использовали code-index:

````markdown
```markdown
## Code Index — быстрый поиск по коду

Для поиска по коду используй CLI-индексатор вместо grep/find/Read:
- Поиск: code-index query "имя" --path /путь/к/проекту --json
- FTS поиск: code-index search-function "запрос" --path /путь/к/проекту
- Граф вызовов: code-index get-callers "функция" --path /путь/к/проекту
- Карта файла: code-index get-file-summary "файл" --path /путь/к/проекту
- Статистика: code-index stats --path /путь/к/проекту --json
Все команды выводят JSON. Это мгновенный поиск по индексированной базе.

> **Примечание:** Read-команды CLI открывают БД в режиме `SQLITE_OPEN_READ_ONLY`, поэтому работают параллельно с MCP-демоном без блокировок.
```
````

Путь к проекту должен быть абсолютным. На Windows — указывайте полный путь до `.exe`, например `C:\MCP-Servers\code-index\target\release\code-index.exe`.

## Daemon-режим (v0.5+)

Начиная с v0.5, `code-index` использует архитектуру **один писатель, много читателей**:

### Фоновый демон (единственный писатель)

`code-index daemon run` запускает длительный процесс, который:

1. Читает список отслеживаемых папок из `daemon.toml`.
2. Для каждой папки открывает `.code-index/index.db`, делает полный reindex с mtime fast-path (v0.4.0), затем переключается на `notify`-watcher и переиндексирует файлы при изменениях (debounce 1.5 с, batch 2 с).
3. Слушает локальный HTTP-эндпоинт health/управления на loopback (порт записывается в `daemon.json` в каталоге состояния).
4. Держит глобальный PID-lock (`daemon.pid`), чтобы на одной машине не было двух демонов одновременно.

Жизненный цикл папки: `not_started → initial_indexing → ready ⇄ reindexing_batch / error`. Каждый переход виден через `daemon status`.

### MCP-серверы (сколько угодно read-only читателей)

`code-index serve --path <project>` открывает `.code-index/index.db` в режиме `SQLITE_OPEN_READ_ONLY` и предоставляет MCP-инструменты через stdio. Несколько экземпляров MCP на одном проекте работают параллельно без взаимных блокировок.

Перед каждым tool-call MCP опрашивает у демона статус папки. Если он не `ready` — инструмент возвращает структурированный JSON:

```json
{ "status": "indexing", "progress": {"files_done": 4200, "files_total": 10000, "percent": 42.0}, "message": "Первичная индексация в процессе" }
```

Если демон недоступен:

```json
{ "status": "daemon_offline", "message": "Демон code-index не доступен. Запустите 'code-index daemon run' или Scheduled Task." }
```

## Конфигурация

Файл `.code-index/config.json` создаётся автоматически при первом запуске:

```json
{
  "exclude_dirs": ["node_modules", ".venv", "__pycache__", ".git", "target", "output"],
  "extra_text_extensions": [],
  "max_file_size": 1048576,
  "max_files": 0,
  "bulk_threshold": 10,
  "languages": ["python", "javascript", "typescript", "java", "rust", "go", "bsl"],
  "batch_size": 500,
  "storage_mode": "auto",
  "memory_max_percent": 25,
  "debounce_ms": 1500,
  "batch_ms": 2000
}
```

Ключевые поля:

- `storage_mode` — режим хранения: `auto` (выбирается автоматически по доступной памяти), `memory` (только in-memory), `disk` (только на диск)
- `memory_max_percent` — максимальный процент RAM для in-memory базы при `auto`-режиме
- `debounce_ms` — задержка перед переиндексацией после изменения файла (мс); собирает burst правок в один батч
- `batch_ms` — верхняя граница накопления событий в одном батче после прихода первого
- `batch_size` — количество записей в одной транзакции при индексации
- `bulk_threshold` — минимальное количество файлов для активации bulk-режима (drop indexes → insert → rebuild)

### Настройка реакции watcher'а (`debounce_ms`, `batch_ms`)

Дефолты 1500 мс / 2000 мс — оптимальны для типового сценария IDE (save + форматтер + линтер) и для git-операций, трогающих много файлов сразу. Для интерактивной работы одним пользователем можно уменьшить, пожертвовав батчингом ради быстрой реакции.

Демон разрешает эти значения в порядке (первое найденное выигрывает):

1. **Переопределение per-папка в `daemon.toml`:**
   ```toml
   [[paths]]
   path = "C:/RepoBP_1"
   debounce_ms = 500      # реакция ~0.6 с вместо ~1.6 с
   batch_ms    = 1000
   ```
2. **Per-project `.code-index/config.json`** — действует только на эту папку.
3. **Встроенные дефолты** (1500 / 2000).

Применить после правки `daemon.toml`:

```bash
code-index daemon reload
```

Рекомендуемые значения:

| Сценарий | `debounce_ms` |
|----------|---------------|
| Интерактивная IDE, точечные правки | 300–500 |
| 1С-репо / git-операции / массовые правки | 1500 (дефолт) |
| CI или скриптованные batch-правки | 3000+ |

## Бенчмарки

Протестировано на конфигурациях 1С:Предприятие (HDD, Windows):

| Проект | Файлов | Первичная | Повторный запуск | Ускорение |
|--------|--------|-----------|-----------------|-----------|
| Управление Торговлей | 63K | 65 сек | **5 сек** | 13x |
| Бухгалтерия | 93K | 164 сек | **4 сек** | 40x |

Повторный запуск использует `mtime + file_size` fast-path: только `stat()` на каждый файл, ни одного чтения, ни одного SHA-256.

| Метрика | Значение |
|---------|----------|
| Функций (УТ) | 282,575 |
| Вызовов (граф) | 1,533,337 |
| Время поиска | < 1 мс |
| Размер бинарника | 13.5 МБ |

Сравнение с grep:

| Операция | grep | Code Index |
|----------|------|------------|
| Найти функцию по имени | O(n) файлов, секунды | < 1 мс |
| Кто вызывает функцию X? | grep по всем файлам | < 1 мс |
| Карта файла | cat + анализ | < 1 мс |
| Полнотекстовый поиск | grep -r, секунды | < 1 мс |

## Архитектура

```
Source Files → Tree-sitter Parser → SQLite (in-memory) → MCP Server → AI Model
                                         ↑
                    File Watcher ────────┘ (auto re-index)
```

Ключевые оптимизации:

- **In-memory SQLite с событийным flush** — все операции в RAM, запись на диск только при реальных изменениях (см. ниже)
- **Rayon** — параллельный парсинг файлов на всех доступных ядрах
- **Bulk mode** — при большом количестве файлов: drop indexes → batch insert → rebuild indexes
- **mtime/size fast-path** — при рестарте каждый файл проверяется через `stat()` (mtime + file_size); если совпадают — файл не читается вообще, ни SHA-256, ни I/O. Только изменённые файлы читаются и перехешируются
- **PID-lock** — защита от запуска нескольких демонов на одном `index.db`

### Политика сброса на диск (flush)

Демон работает в in-memory режиме для максимальной производительности. База сбрасывается на диск **только** при реальных изменениях данных — никаких периодических таймеров, никакого лишнего I/O:

| Событие | Flush? | Условие |
|---------|--------|---------|
| Начальная индексация завершена | Да | Проиндексирован или удалён хотя бы 1 файл |
| Watcher обработал батч изменений | Да | В батче была хотя бы 1 реальная запись/удаление |
| Watcher сработал, но ничего не изменилось | **Нет** | Хеш файла не изменился → нет записи → нет flush |
| Простой (файлы не менялись) | **Нет** | Нулевая дисковая активность |
| Завершение демона (graceful shutdown) | Да | Всегда — финальный страховочный flush |

Это означает: если вы просто общаетесь с AI и не редактируете код, демон не производит **никакой дисковой активности**.
- **Batch transactions** — вставка 500 записей в одной транзакции вместо отдельных INSERT

## Для 1С-разработчиков

Code Index специально поддерживает экосистему 1С:Предприятие.

Из BSL-файлов извлекаются:

- Процедуры и функции с полным текстом тела
- Директивы компиляции (`&НаСервере`, `&НаКлиенте`, `&НаСервереБезКонтекста`)
- Аннотации расширений (`&Вместо`, `&После`, `&Перед`)
- Двуязычные ключевые слова (поддержка русского и английского синтаксиса BSL)

Данные сохраняются в двух отдельных полях:
- `override_type`: «Перед», «После» или «Вместо»
- `override_target`: имя оригинальной процедуры, которую переопределяет аннотация

Из XML-выгрузок конфигурации извлекаются:

- Объекты метаданных (справочники, документы, регистры и др.)
- Реквизиты и табличные части
- Формы объектов

### bsl-indexer — расширенная сборка для 1С (workspace-refactor v0.6+)

Помимо публичного `code-index` есть приватная сборка `bsl-indexer`:
дополнительные MCP-tool'ы (`get_object_structure`, `get_form_handlers`,
`get_event_subscriptions`, `find_path`, `search_terms`), парсер XML-выгрузки,
граф вызовов BSL-процедур, опциональное LLM-обогащение через OpenAI-совместимый
endpoint (Ollama / OpenRouter / любой другой). Подробности и инструкция по
настройке — [docs/bsl-indexer.md](docs/bsl-indexer.md).

> **Важно при правке `daemon.toml`:** на текущей версии Claude Code
> (2.1.120, 2026-04) уведомление MCP `tools/list_changed` игнорируется —
> после изменения списка репо/языков сделайте `/mcp` → `Reconnect` для
> сервера, иначе свежий состав инструментов не появится. Сервер
> уведомление шлёт корректно, проблема на стороне клиента
> ([anthropics/claude-code#13646](https://github.com/anthropics/claude-code/issues/13646)).

## Системные требования

- **ОС:** Windows, Linux, macOS
- **RAM:** от 512 МБ (малые проекты) до 4 ГБ (крупные конфигурации 1С)
- **Диск:** размер индекса ~1-2 ГБ для проектов 60K+ файлов
- **Для сборки:** Rust 1.77+ (установить через [rustup.rs](https://rustup.rs))

## Лицензия

MIT. См. [LICENSE](LICENSE).

## Благодарности

- [tree-sitter](https://tree-sitter.github.io/) — инкрементальный парсер для множества языков
- [tree-sitter-onescript](https://github.com/1c-syntax/tree-sitter-onescript) — грамматика BSL/OneScript от сообщества 1c-syntax
- [rusqlite](https://github.com/rusqlite/rusqlite) — SQLite для Rust
- [rayon](https://github.com/rayon-rs/rayon) — параллелизм данных без лишних усилий
- [rmcp](https://github.com/modelcontextprotocol/rust-sdk) — Rust MCP SDK
