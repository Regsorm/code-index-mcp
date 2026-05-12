# bsl-indexer / code-index v0.8.0

**Версия 0.8.0 — 2026-05-05**

---

## Что это

`code-index` — скомпилированный Rust-бинарник для структурного поиска по исходному коду. Он решает конкретную проблему: AI-модели (Claude Code, VS Code Copilot, собственные агенты) тратят десятки вызовов `grep`/`find` только чтобы найти одну функцию или проследить граф вызовов. На крупных репозиториях (60K+ файлов) это превращается в минуты ожидания.

`code-index` заменяет этот процесс на одиночный запрос к SQLite-индексу, результат которого возвращается за миллисекунды. Он парсит исходный код через tree-sitter в AST, записывает функции, классы, импорты, граф вызовов и полный текст в SQLite с FTS5. Фоновый демон поддерживает индекс в актуальном состоянии через file-watcher; MCP-сервер предоставляет 18–23 инструмента AI-клиентам.

**Для кого:** команды, работающие с крупными кодовыми базами; разработчики 1С, интегрирующие AI-агентов в рабочий процесс; любой, кто подключает Claude Code к проекту с тысячами файлов.

**Два бинарника:**
- `code-index` — публичный, поддерживает Python, Rust, Go, Java, JavaScript, TypeScript, HTML + текстовые файлы.
- `bsl-indexer` — приватный, включает всё из `code-index` плюс 5 BSL-специфичных MCP-инструментов для конфигураций 1С:Предприятие.

---

## Архитектура

### Два процесса

| Процесс | Роль | Запуск |
|---|---|---|
| **Daemon** (единственный писатель) | Обходит папки, парсит файлы, пишет в SQLite | `code-index daemon run` / Scheduled Task / systemd |
| **MCP serve** (только чтение) | Принимает запросы от AI-клиентов, читает SQLite | `code-index serve --transport http --port 8011` |

Демон держит глобальный PID-lock (`daemon.pid`) — на машине работает ровно один экземпляр. MCP-сервер открывает SQLite в `SQLITE_OPEN_READ_ONLY`; сколько угодно параллельных Claude Code / VS Code / субагентов могут работать с одним репо без блокировок.

Перед каждым tool-call MCP опрашивает у демона статус папки. Если индексация ещё идёт — инструмент возвращает структурированный JSON с прогрессом, а не сваливается с ошибкой.

### Схема SQLite (`.code-index/index.db`)

База данных создаётся автоматически в каждом индексируемом репозитории.

| Таблица | Назначение |
|---|---|
| `files` | Метаданные файлов: path, content_hash, language, mtime, file_size, lines_total |
| `functions` | AST-функции и процедуры: name, qualified_name, args, body, line_start/end, override_type/target |
| `classes` | AST-классы и HTML-элементы: name, bases, docstring, body |
| `imports` | Импорты и зависимости: module, name, alias, kind |
| `calls` | Граф вызовов: caller → callee, line |
| `variables` | Переменные и HTML-атрибуты (CSS-классы, input-поля) |
| `text_files` | Содержимое текстовых файлов (.md, .yaml, .json, .xml и др.) для FTS |
| `file_contents` | **v0.8.0**: содержимое code-файлов (.py, .bsl, .rs, .ts и др.) в формате zstd |
| `fts_functions` | FTS5 — полнотекстовый поиск по функциям |
| `fts_classes` | FTS5 — полнотекстовый поиск по классам |
| `fts_text_files` | FTS5 — полнотекстовый поиск по текстовым файлам |

Миграции схемы идемпотентны: v2 (override_type/target для BSL), v3 (mtime/file_size), v4 (file_contents). Накатываются автоматически при первом запуске новой версии.

### Поддерживаемые языки

| Язык | Парсер | Расширения |
|---|---|---|
| Python | tree-sitter-python | `.py` |
| JavaScript | tree-sitter-javascript | `.js`, `.jsx` |
| TypeScript | tree-sitter-typescript | `.ts`, `.tsx` |
| Java | tree-sitter-java | `.java` |
| Rust | tree-sitter-rust | `.rs` |
| Go | tree-sitter-go | `.go` |
| 1C (BSL) | tree-sitter-onescript | `.bsl`, `.os` |
| HTML | tree-sitter-html | `.html`, `.htm` |
| XML (1C) | quick-xml | `.xml` (метаданные конфигурации) |

Текстовые файлы `.md`, `.json`, `.yaml`, `.toml`, `.sql`, `.env` и другие индексируются для FTS и доступны через `read_file`/`grep_text`.

### Federation (serve.toml)

Начиная с rc6, один MCP-сервер может обслуживать репозитории с нескольких машин. Если в `CODE_INDEX_HOME` есть `serve.toml` — сервер при получении tool-call с `repo=X` проверяет IP репо: если совпадает с `[me].ip` — читает локальный SQLite, иначе форвардит POST-запрос к удалённому `serve`. Всё это прозрачно для клиента, который видит одинаковый URL и одинаковый параметр `repo`.

---

## Установка

### Готовые бинарники (GitHub Releases)

На каждый тег публикуются 6 артефактов:

```
code-index-windows-x64.exe   / code-index-linux-x64 / code-index-macos-arm64
bsl-indexer-windows-x64.exe  / bsl-indexer-linux-x64 / bsl-indexer-macos-arm64
```

Скачать: https://github.com/Regsorm/code-index-mcp/releases

### Сборка из исходников

```bash
git clone https://github.com/Regsorm/code-index-mcp.git
cd code-index-mcp

# Публичный бинарник
cargo build --release -p code-index

# bsl-indexer с 1С-инструментами и опциональным LLM-обогащением
cargo build --release -p bsl-indexer --features enrichment
```

Требования: Rust 1.78+. Подробности сборки под Linux: [docs/deploy-vm-rag.md](deploy-vm-rag.md).

### Минимальная настройка

1. Создайте папку, положите туда бинарник:
   ```
   C:\tools\code-index\code-index.exe
   ```

2. Укажите переменную окружения:
   ```powershell
   # Windows
   setx CODE_INDEX_HOME "C:\tools\code-index"
   ```
   ```bash
   # Linux / macOS
   export CODE_INDEX_HOME="$HOME/.local/code-index"
   ```

3. Создайте `$CODE_INDEX_HOME/daemon.toml` с путями к репозиториям:
   ```toml
   [daemon]
   http_port = 0   # 0 = выбрать свободный порт автоматически

   [[paths]]
   path = "C:/your-repo"
   ```

4. Запустите демон и MCP-сервер:
   ```bash
   code-index daemon run          # в отдельном окне или как фоновый процесс
   code-index serve --transport http --port 8011
   ```

5. Добавьте в `.mcp.json` проекта:
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

**Автозапуск на Windows** через Scheduled Task:
```powershell
powershell -ExecutionPolicy Bypass -File scripts\install-daemon-autostart.ps1 `
  -BinaryPath "C:\tools\code-index\code-index.exe" `
  -CodeIndexHome "C:\tools\code-index" `
  -StartNow
```

---

## Конфигурация

### daemon.toml — полный референс

Файл живёт в `$CODE_INDEX_HOME/daemon.toml`. Содержит три секции.

#### Секция `[daemon]`

| Параметр | Тип | По умолчанию | Описание |
|---|---|---|---|
| `http_host` | string | `"127.0.0.1"` | Хост HTTP health-эндпоинта демона |
| `http_port` | u16 | `0` | Порт; `0` = выбрать свободный автоматически |
| `log_level` | string | `"info"` | Уровень логирования (перекрывается `RUST_LOG`) |
| `max_concurrent_initial` | usize | `1` | Сколько папок одновременно проходят initial reindex |

#### Секция `[indexer]` (v0.8.0)

| Параметр | Тип | По умолчанию | Описание |
|---|---|---|---|
| `max_code_file_size_bytes` | usize | `5242880` (5 МБ) | Глобальный лимит размера code-файла для сохранения content в `file_contents` |

#### Секция `[[paths]]`

| Параметр | Тип | По умолчанию | Описание |
|---|---|---|---|
| `path` | string | — | Абсолютный путь к репозиторию (обязательно) |
| `alias` | string | последний сегмент пути | Имя репо для параметра `repo` в tool-call |
| `language` | string | auto-detect | Язык: `python`, `rust`, `go`, `java`, `javascript`, `typescript`, `bsl` |
| `debounce_ms` | u64 | 1500 | Задержка watcher (мс) перед переиндексацией после изменения |
| `batch_ms` | u64 | 2000 | Верхняя граница накопления событий в одном батче |
| `max_code_file_size_bytes` | usize | из `[indexer]` / 5 МБ | Per-path лимит для `file_contents` (приоритет над глобальным) |

**Пример:**
```toml
[daemon]
http_port = 0
max_concurrent_initial = 1

[indexer]
max_code_file_size_bytes = 5242880

[[paths]]
path = "C:/your-main-repo"
alias = "main"
language = "python"

[[paths]]
path = "C:/your-bsl-repo"
alias = "ut"
language = "bsl"
debounce_ms = 500
max_code_file_size_bytes = 10485760
```

**Приоритет `max_code_file_size_bytes`:** per-path → `[indexer]` → дефолт 5 МБ.

#### Секция `[enrichment]` (только bsl-indexer)

Опциональная; при отсутствии — LLM-обогащение выключено.

| Параметр | По умолчанию | Описание |
|---|---|---|
| `enabled` | `false` | Главный тумблер |
| `provider` | `"openai_compatible"` | Протокол (POST /v1/chat/completions) |
| `url` | — | URL endpoint'а (OpenRouter, Ollama, любой совместимый) |
| `model` | — | Имя модели в нотации провайдера |
| `api_key_env` | `null` | Имя env-переменной с API-ключом |
| `batch_size` | `20` | Количество параллельных HTTP-соединений |
| `prompt_template` | встроенный | Шаблон system-промпта для генерации терминов |

### serve.toml — federation

Живёт в `$CODE_INDEX_HOME/serve.toml`. При его наличии сервер активирует федеративный режим.

```toml
[me]
ip = "192.0.2.10"      # IP этой машины
# token = "..."        # заготовка под авторизацию (rc7)

[[paths]]
alias = "ut"
ip = "192.0.2.50"      # репо находится на другой машине
# port = 8011          # optional, default 8011

[[paths]]
alias = "dev"
ip = "192.0.2.10"      # репо на этой же машине
```

Правило форвардинга: `repo.ip == me.ip` → локальный SQLite, иначе `POST /federate/<tool>` к удалённому serve. IP-whitelist: допускаются только адреса из `[[paths]]` плюс loopback.

**Важно:** при добавлении нового инструмента в протокол (например `grep_code` в v0.8.0) обе ноды федерации должны быть обновлены синхронно, иначе старая нода вернёт 404 на новый route.

### .code-index/config.json — per-project

Создаётся автоматически при первом запуске. Переопределяет дефолты для конкретного репозитория.

| Параметр | По умолчанию | Описание |
|---|---|---|
| `exclude_dirs` | `[]` | Дополнительные каталоги для исключения |
| `exclude_file_patterns` | `[]` | Glob-паттерны имён файлов (по basename) |
| `extra_text_extensions` | `[]` | Дополнительные расширения для FTS |
| `max_file_size` | `1048576` (1 МБ) | Лимит текстового файла |
| `max_code_file_size_bytes` | `5242880` (5 МБ) | Лимит content code-файла (Phase 2) |
| `max_files` | `0` (без лимита) | Максимальное число файлов |
| `bulk_threshold` | `10` | Порог для bulk-режима (drop indexes → insert → rebuild) |
| `languages` | все поддерживаемые | Активные языки AST-парсинга |
| `batch_size` | `2000` | Записей в одной транзакции |
| `storage_mode` | `"auto"` | Режим SQLite: `auto` / `memory` / `disk` |
| `memory_max_percent` | `25` | Максимум RAM для in-memory в auto-режиме |
| `debounce_ms` | `1500` | Задержка watcher (мс) |
| `batch_ms` | `2000` | Максимальное время накопления батча (мс) |
| `flush_interval_sec` | `30` | Интервал периодической записи на диск |

---

## MCP-инструменты — все 23

Каждый tool-call принимает обязательный параметр `repo: String` — алиас репозитория из `daemon.toml`. Исключение: `get_stats` (без `repo` возвращает сводку по всем репо).

### Универсальные инструменты (18 штук)

#### Поиск по AST

| Инструмент | Описание |
|---|---|
| `search_function` | Полнотекстовый поиск по функциям (имя, docstring, тело) |
| `search_class` | Полнотекстовый поиск по классам |
| `get_function` | Получить функцию по точному имени |
| `get_class` | Получить класс по точному имени |
| `find_symbol` | Поиск по всем таблицам (функции, классы, переменные, импорты) |
| `get_callers` | Кто вызывает данную функцию (граф вызовов) |
| `get_callees` | Что вызывает данная функция (граф вызовов) |
| `get_imports` | Импорты файла или модуля |
| `get_file_summary` | Полная карта файла: функции, классы, импорты, переменные |

**Пример — `get_function`:**
```json
// Запрос:
{ "repo": "main", "function_name": "process_order" }

// Ответ:
{
  "name": "process_order",
  "file_path": "src/orders/processor.py",
  "line_start": 142,
  "line_end": 201,
  "language": "python",
  "body": "..."
}
```

**Пример — `get_callers`:**
```json
// Запрос:
{ "repo": "main", "function_name": "send_notification" }

// Ответ:
[
  { "caller_name": "process_order", "caller_file": "src/orders/processor.py", "line": 195 },
  { "caller_name": "cancel_order",  "caller_file": "src/orders/cancel.py",    "line": 47 }
]
```

**Пример — `search_function`:**
```json
// Запрос:
{ "repo": "main", "query": "authentication", "language": "python", "path_glob": "src/auth/**" }

// Ответ:
[
  { "name": "authenticate_user", "file_path": "src/auth/backend.py", "line_start": 23, "line_end": 61 },
  { "name": "check_auth_token",  "file_path": "src/auth/middleware.py", "line_start": 88, "line_end": 110 }
  // ...
]
```

#### Regex-поиск

| Инструмент | Описание |
|---|---|
| `grep_body` | Regex/подстрока по телам функций и классов (поддерживает точки и спецсимволы) |
| `grep_text` | Regex по содержимому текстовых файлов (.yaml, .md, .xml и др.) |
| `grep_code` | **(v0.8.0)** Regex по содержимому code-файлов через таблицу `file_contents` |

Все три принимают: `regex?: string`, `path_glob?: string`, `language?: string`, `limit?: int`, `context_lines?: int`.

**Пример — `grep_body`:**
```json
// Запрос:
{ "repo": "ut", "pattern": "Справочники.Контрагенты", "language": "bsl" }

// Ответ:
[
  {
    "file_path": "src/Catalogs/Products/ObjectModule.bsl",
    "name": "OnWrite",
    "kind": "function",
    "line_start": 45,
    "line_end": 82,
    "match_lines": [51, 63, 78],
    "match_count": 3
  }
]
```

**Пример — `grep_code` (v0.8.0):**
```json
// Запрос:
{ "repo": "main", "regex": "def\\s+\\w+Payment", "language": "python", "context_lines": 2 }

// Ответ:
[
  {
    "path": "src/billing/stripe.py",
    "line": 89,
    "content": "def processPayment(amount, currency):",
    "context_before": ["# Stripe integration", "class StripeGateway:"],
    "context_after": ["    \"\"\"Process a payment via Stripe.\"\"\"", "    client = stripe.Client()"]
  }
]
```

#### Файловые операции (Phase 1, v0.7.0+)

| Инструмент | Описание |
|---|---|
| `list_files` | Список файлов репо с фильтрами `pattern` (glob), `path_prefix`, `language`, `limit` |
| `stat_file` | Метаданные файла: exists, size, mtime, language, lines_total, category, oversize (v0.8.0) |
| `read_file` | Содержимое файла с диапазоном строк `line_start`/`line_end` (1-based). Soft-cap: 5000 строк или 500 КБ; hard-cap: 2 МБ |

**Пример — `stat_file`:**
```json
// Запрос:
{ "repo": "main", "path": "src/orders/processor.py" }

// Ответ:
{
  "exists": true,
  "path": "src/orders/processor.py",
  "language": "python",
  "size": 8240,
  "mtime": 1746393600,
  "lines_total": 201,
  "content_hash": "a3f7c...",
  "indexed_at": "2026-05-05T10:00:00Z",
  "category": "code",
  "oversize": false
}
```

**Пример — `read_file` (v0.8.0, code-файл):**
```json
// Запрос:
{ "repo": "main", "path": "src/orders/processor.py", "line_start": 140, "line_end": 160 }

// Ответ:
{
  "content": "def process_order(order_id):\n    ...",
  "lines_returned": 21,
  "lines_total": 201,
  "truncated": false,
  "category": "code"
}
```

**Ответ для oversize-файла:**
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

#### Статистика и здоровье

| Инструмент | Описание |
|---|---|
| `get_stats` | Статистика индекса: файлы, функции, классы, языки, статус. `repo` опционален — без него возвращает fan-out по всем репо |
| `health` | Статус MCP-сервера и список подключённых репо |

**Пример — `get_stats`:**
```json
// Запрос: {}  (без repo)

// Ответ:
[
  { "repo": "main",   "files": 4821,  "functions": 28350, "classes": 1240, "status": "ready" },
  { "repo": "ut",     "files": 32599, "functions": 282575, "classes": 14200, "status": "ready" },
  { "repo": "remote", "status": "unreachable", "error": "timeout 5s" }
]
```

### BSL-специфичные инструменты (5 штук)

Активируются автоматически при наличии хотя бы одного `[[paths]]` с `language = "bsl"` в `daemon.toml`. Доступны только в сборке `bsl-indexer`.

| Инструмент | Описание |
|---|---|
| `get_object_structure` | Структура объекта конфигурации 1С (реквизиты, табличные части, формы) без запуска платформы |
| `get_form_handlers` | Обработчики событий управляемой формы по `(owner_full_name, form_name)` |
| `get_event_subscriptions` | Все подписки на события из XML-выгрузки, фильтр по handler-модулю |
| `find_path` | Цепочка вызовов между двумя процедурами (recursive CTE по `proc_call_graph`, max_depth=3) |
| `search_terms` | FTS-поиск по бизнес-терминам процедур, обогащённым LLM после `bsl-indexer enrich` |

**Пример — `get_object_structure`:**
```json
// Запрос:
{ "repo": "ut", "full_name": "Document.SalesOrder" }

// Ответ:
{
  "name": "SalesOrder",
  "meta_type": "Document",
  "attributes": [
    { "name": "Counterparty", "type": "CatalogRef.Counterparties" },
    { "name": "Amount",       "type": "Number" }
  ],
  "tabular_sections": [
    { "name": "Goods", "columns": ["Nomenclature", "Quantity", "Price"] }
  ]
}
```

**Пример — `find_path`:**
```json
// Запрос:
{ "repo": "ut", "from_proc": "ProcessPosting", "to_proc": "WriteRegisterAccumulation" }

// Ответ:
[
  ["ProcessPosting", "CalculateTotals", "WriteRegisterAccumulation"]
]
```

**Все поисковые инструменты** (`search_function`, `search_class`, `get_function`, `get_class`, `find_symbol`, `search_text`, `grep_body`) поддерживают параметр `path_glob` для ограничения выдачи по подкаталогу. Пример: `path_glob="Documents/**/*.bsl"`.

---

## Phase 2 (v0.8.0) — что нового подробно

### Таблица `file_contents` и полное чтение code-файлов

До v0.8.0 `read_file` для `.py`/`.bsl`/`.rs`/`.ts` возвращал `category="code"` с пустым `content`. Начиная с v0.8.0 содержимое code-файлов хранится в новой таблице `file_contents` (миграция v4) в формате zstd-сжатия.

Backfill выполняется автоматически при первом запуске v0.8.0 на существующей базе. Реализован через отдельную фазу `list_code_files_without_content()` — обходит все code-файлы без записи в `file_contents` независимо от того, менялся ли их mtime. Это отличает backfill от инкрементального watcher, который срабатывает только при изменениях. Backfill идемпотентен (`INSERT OR REPLACE`).

Ориентиры по времени на крупном BSL-репо (~15 665 файлов, ~620 МБ исходников): однократный backfill ~30–40 секунд, итоговый размер blob — ~120 МБ (сжатие ~5×).

### Инструмент grep_code

`grep_code` — новый инструмент для regex-поиска по содержимому code-файлов. Закрывает слепую зону `grep_body`, который ищет только внутри тел функций и классов. `grep_code` покрывает весь файл: комментарии, директивы компилятора, строки вне функций, объявления переменных на уровне модуля.

Файлы с `oversize=1` пропускаются при `grep_code` — для них по-прежнему доступны `get_function`/`get_class`/`grep_body` по AST-данным.

### Oversize-механика

Файлы крупнее `max_code_file_size_bytes` (дефолт 5 МБ) индексируются полностью по AST, FTS и графу вызовов, но их content не сохраняется в `file_contents`. Вместо этого запись получает `oversize=1, content_blob=NULL`. При `read_file` и `grep_code` для таких файлов возвращается сигнальный ответ с подсказкой.

### Защита от zstd-bomb

Все операции декомпрессии идут через helper `decode_zstd_safe` с лимитом 256 МБ на разжатый размер. Превышение возвращает ошибку без дополнительной аллокации памяти. Лимит выбран с многократным запасом над дефолтным `max_code_file_size_bytes` × коэффициент zstd (~5×).

---

## Производительность и ограничения

### Бенчмарки (1С-репо, HDD, Windows)

| Проект | Файлов | Первичная индексация | Повторный запуск |
|---|---|---|---|
| Управление торговлей | 63K | 65 сек | 5 сек |
| Бухгалтерия | 93K | 164 сек | 4 сек |

Повторный запуск использует `mtime + file_size` fast-path: только `stat()` на каждый файл, без чтения и без SHA-256.

### Время поиска

| Операция | Время |
|---|---|
| Поиск функции по имени | < 1 мс |
| Граф вызовов (get_callers/get_callees) | < 1 мс |
| FTS-поиск по 282K функций | < 1 мс |
| read_file (code, zstd-decode) | < 10 мс |

### Ограничения `read_file`

- **Soft-cap:** 5000 строк или 500 КБ (возвращается `truncated=true`).
- **Hard-cap:** 2 МБ — запрос отклоняется.

### Ограничения `grep_text` / `grep_code`

- Hard-cap на суммарный размер ответа: 1 МБ.

### Что не индексируется

- **Бинарные форматы 1С** (`.epf`, `.erf`, `.cf`, `.cfe`) — требуют предварительной распаковки внешним инструментом.
- **Файлы без расширения** (`Dockerfile`, `Makefile`, `Jenkinsfile`, `.gitignore`) — слепая зона walker.
- **Git-история** — индексируется только текущий срез файловой системы.
- **Oversize code-файлы** — content не сохраняется, доступны только AST-операции.

### Совместимость federation

Обе ноды federation должны быть одной версии. При добавлении нового инструмента (как `grep_code` в v0.8.0) обновление нод должно быть синхронным: старая нода вернёт `404` на новый `/federate/grep_code`.

---

## Совместимость и поддержка

### MCP-протокол

Реализован через библиотеку `rmcp 1.3`. Транспорты:
- `streamable-http` (рекомендуемый): `http://host:port/mcp`.
- `stdio`: для совместимости с клиентами, поддерживающими только stdio.

### MCP API — обратная совместимость

Все новые поля в ответах — `Option<...>` или `default false`. Старые клиенты, не знающие о новых полях, продолжают работать без изменений. Добавление `read_file` для code-файлов в v0.8.0 — улучшение, не breaking change (раньше возвращался пустой content, теперь реальный).

### Storage API

`code-index-core` предоставляет библиотеку для прямой работы с SQLite. Storage API v0.8.0 изменён несовместимо: `Indexer::write_code_to_db`, `Storage::read_file_text`, `worker::run_worker` получили новые параметры. Внешних публичных callers нет, но при прямом использовании крейта требуется обновление.

### Схема БД

Миграция v4 идемпотентна. Откат на v0.7.x просто игнорирует новую таблицу `file_contents` — чтение старых данных совместимо в обе стороны.

### Платформы

| Платформа | Статус |
|---|---|
| Windows x64 (gnu) | Поддерживается, CI-артефакт |
| Linux x64 (gnu / musl) | Поддерживается, CI-артефакт |
| macOS arm64 | Поддерживается, CI-артефакт |

### Сборка из исходников

Требуется Rust 1.78+. Установить: https://rustup.rs

### Лицензия

MIT. Файл [LICENSE](../LICENSE).

---

## Куда обращаться

- **GitHub:** https://github.com/Regsorm/code-index-mcp
- **Релизы и готовые бинарники:** https://github.com/Regsorm/code-index-mcp/releases
- **Документация по bsl-indexer:** [docs/bsl-indexer.md](bsl-indexer.md)
- **Деплой на Linux:** [docs/deploy-vm-rag.md](deploy-vm-rag.md)
- **Архитектура workspace:** [docs/bsl-indexer-architecture.md](bsl-indexer-architecture.md)
