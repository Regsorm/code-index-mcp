# Code Index MCP

[English version](README.md)

Мгновенный поиск по коду для AI-моделей. Заменяет grep на запросы за миллисекунды.

> **Ключевые метрики:** 62K файлов за 43 сек · 282K функций за <1 мс · 8 языков · 12 MCP-инструментов

## Проблема

AI-модели тратят десятки вызовов `grep`/`find` для навигации по большим проектам. На крупных кодовых базах это превращается в минуты ожидания.

Например, найти все места использования `RuntimeErrorProcessing` в Java-проекте с помощью стандартных инструментов — это 14 вызовов `grep`/`find`, которые выполняются последовательно. С Code Index — один запрос, мгновенный ответ.

## Решение

Скомпилированный Rust-бинарник, который:

1. Парсит исходный код в AST через tree-sitter
2. Индексирует результат в SQLite с FTS5 для полнотекстового поиска
3. Предоставляет 12 MCP-инструментов для AI-моделей
4. Следит за изменениями файлов в реальном времени (daemon-режим)

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
cargo build --release
```

Бинарник: `target/release/code-index` (Linux/macOS) или `target/release/code-index.exe` (Windows).

### Индексация проекта

```bash
code-index index /path/to/project
code-index stats --path /path/to/project --json
```

### Запуск MCP-сервера

```bash
code-index serve --path /path/to/project
```

## Подключение к Claude Code

Добавьте в `.mcp.json` вашего проекта:

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
| `grep_body` | Поиск подстроки или regex в телах функций и классов |

Все инструменты поддерживают фильтр по языку: `search_function(query="X", language="python")`

### grep_body

В отличие от FTS-поиска, `grep_body` поддерживает буквальные подстроки (включая точки и спецсимволы) и регулярные выражения. Это критично для поиска обращений к объектам метаданных 1С вида `Справочники.Контрагенты`.

```
grep_body(pattern="Справочники.Контрагенты", language="bsl")
grep_body(regex="Справочники\\.(Контрагенты|Организации)", language="bsl")
```

Возвращает `[{file_path, name, kind, line_start, line_end}]` — конкретные функции/классы с совпадениями.

## Справочник CLI

Все 14 подкоманд с описанием параметров:

```bash
# MCP-сервер (daemon-режим)
code-index serve --path /project [--no-watch] [--flush-interval 30]

# Однократная индексация
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
```
````

Путь к проекту должен быть абсолютным. На Windows — указывайте полный путь до `.exe`, например `C:\MCP-Servers\code-index\target\release\code-index.exe`.

## Daemon-режим

При запуске `code-index serve` выполняются четыре процесса параллельно:

1. **Background scan** — индексирует новые и изменённые файлы в фоне (MCP-сервер доступен сразу)
2. **File watcher** — отслеживает изменения в реальном времени (notify crate)
3. **MCP-сервер** — принимает запросы через stdio
4. **Periodic flush** — сбрасывает in-memory базу на диск каждые 30 секунд

Изменил файл → через 1.5 сек (debounce) → автоматическая переиндексация.

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
  "batch_ms": 2000,
  "flush_interval_sec": 30
}
```

Ключевые поля:

- `storage_mode` — режим хранения: `auto` (выбирается автоматически по доступной памяти), `memory` (только in-memory), `disk` (только на диск)
- `memory_max_percent` — максимальный процент RAM для in-memory базы при `auto`-режиме
- `debounce_ms` — задержка перед переиндексацией после изменения файла (мс)
- `batch_size` — количество записей в одной транзакции при индексации
- `bulk_threshold` — минимальное количество файлов для активации bulk-режима (drop indexes → insert → rebuild)

## Бенчмарки

Протестировано на конфигурации 1С:Управление Торговлей:

| Метрика | Значение |
|---------|----------|
| Файлов | 61,706 |
| Функций | 282,575 |
| Вызовов (граф) | 1,533,337 |
| Время индексации | 43 секунды |
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

- **In-memory SQLite с flush** — все операции в RAM, периодическая запись на диск
- **Rayon** — параллельный парсинг файлов на всех доступных ядрах
- **Bulk mode** — при большом количестве файлов: drop indexes → batch insert → rebuild indexes
- **SHA-256 хеш-проверка** — файлы без изменений пропускаются при переиндексации
- **Batch transactions** — вставка 500 записей в одной транзакции вместо отдельных INSERT

## Для 1С-разработчиков

Code Index специально поддерживает экосистему 1С:Предприятие.

Из BSL-файлов извлекаются:

- Процедуры и функции с полным текстом тела
- Директивы компиляции (`&НаСервере`, `&НаКлиенте`, `&НаСервереБезКонтекста`)
- Аннотации расширений (`&Вместо`, `&После`, `&Перед`)
- Двуязычные ключевые слова (поддержка русского и английского синтаксиса BSL)

Из XML-выгрузок конфигурации извлекаются:

- Объекты метаданных (справочники, документы, регистры и др.)
- Реквизиты и табличные части
- Формы объектов

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
