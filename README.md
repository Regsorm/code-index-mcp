# Code Index MCP

**Instant code search for AI models. Replaces grep with millisecond queries.**

> 62K files indexed in 43 seconds • 282K functions searchable in < 1ms • 8 languages • 11 MCP tools

---

## Проблема

AI-модели (Claude, GPT, Cursor) при работе с кодом делают десятки вызовов `grep` и `find`, тратя время и контекстное окно. На крупных проектах (тысячи файлов) это занимает минуты.

**Реальный пример:** поиск `RuntimeErrorProcessing` в Java-проекте — 14 вызовов grep/find. С Code Index — один запрос, мгновенный ответ.

## Решение

Скомпилированный Rust-бинарник, который:

1. **Парсит** исходный код в AST через [tree-sitter](https://tree-sitter.github.io/)
2. **Индексирует** функции, классы, импорты, вызовы в SQLite с полнотекстовым поиском (FTS5)
3. **Предоставляет** 11 MCP-инструментов для AI-моделей
4. **Следит** за изменениями файлов (daemon-режим с file watcher)

## Бенчмарки

Протестировано на конфигурации 1С:Управление Торговлей:

| Метрика | Значение |
|---|---|
| Файлов в проекте | 61,706 |
| Функций/процедур | 282,575 |
| Вызовов (граф) | 1,533,337 |
| Время индексации | **43 секунды** |
| Время поиска | **< 1 мс** |
| Размер бинарника | 13.5 МБ |

Сравнение поиска:

| Операция | grep | Code Index |
|---|---|---|
| Найти функцию по имени | O(n) файлов, секунды | < 1 мс |
| Кто вызывает функцию X? | grep по всем файлам | < 1 мс |
| Карта файла | cat + анализ | < 1 мс |
| Полнотекстовый поиск | grep -r, секунды | < 1 мс |

## Поддерживаемые языки

| Язык | Парсер | Расширения |
|---|---|---|
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

Бинарник: `target/release/code-index` (Linux/Mac) или `target/release/code-index.exe` (Windows)

### CLI — ручная индексация

```bash
# Проиндексировать проект
code-index index /path/to/project

# Статистика
code-index stats --path /path/to/project

# Поиск
code-index query "function_name" --path /path/to/project

# Очистка устаревших записей
code-index clean --path /path/to/project
```

### MCP-сервер — для AI-моделей

```bash
# Запустить daemon (MCP + file watcher + auto-reindex)
code-index serve --path /path/to/project

# Без file watcher
code-index serve --path /path/to/project --no-watch
```

### Подключение к Claude Code / VS Code

Добавить в `.mcp.json` проекта:

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
|---|---|
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

Все инструменты поддерживают фильтр по языку: `search_function(query="X", language="bsl")`

## Daemon-режим

При запуске `code-index serve` daemon:

1. **Startup scan** — проверяет все файлы, индексирует новые и изменённые
2. **File watcher** — отслеживает изменения в реальном времени (notify crate)
3. **MCP-сервер** — принимает запросы от AI через stdio
4. **Periodic flush** — сбрасывает in-memory базу на диск каждые 30 секунд

Изменил файл → через 1.5 сек (debounce) → автоматическая переиндексация.

## Конфигурация

При первом запуске создаётся `.code-index/config.json`:

```json
{
  "exclude_dirs": ["node_modules", ".venv", "__pycache__", ".git", "output"],
  "languages": ["python", "bsl", "rust", "java", "go", "javascript", "typescript"],
  "max_file_size": 1048576,
  "extra_text_extensions": []
}
```

## Архитектура

```
Source Files → Tree-sitter Parser → SQLite (in-memory) → MCP Server → AI Model
                                         ↑
                    File Watcher ─────────┘ (auto re-index)
```

Оптимизации:
- **In-memory SQLite** с периодическим flush на диск
- **Rayon** — параллельный парсинг на всех ядрах CPU
- **Bulk mode** — drop indexes → insert → rebuild (первичная индексация)
- **SHA-256 hash check** — пропуск неизменённых файлов
- **Batch transactions** — группировка INSERT по 500 записей

## Для 1С-разработчиков

Code Index MCP извлекает из BSL-файлов:
- Процедуры и функции с полным текстом
- Директивы компиляции (`&НаСервере`, `&НаКлиенте`, `&НаСервереБезКонтекста`)
- Аннотации расширений (`&Вместо`, `&После`, `&Перед`)
- Двуязычные ключевые слова

Из XML-выгрузок:
- Объекты метаданных (справочники, документы, регистры)
- Реквизиты, табличные части
- Формы

## Системные требования

- **ОС:** Windows, Linux, macOS
- **RAM:** от 512 МБ (малые проекты) до 4 ГБ (крупные конфигурации 1С)
- **Диск:** размер индекса ≈ 1-2 ГБ для проектов 60K+ файлов
- **Для сборки:** Rust 1.77+ (`rustup.rs`)

## Лицензия

Apache License 2.0. См. [LICENSE](LICENSE).

## Благодарности

- [tree-sitter](https://tree-sitter.github.io/) — инкрементальный парсер
- [tree-sitter-onescript](https://github.com/1c-syntax/tree-sitter-onescript) — грамматика BSL/OneScript от сообщества 1c-syntax
- [rusqlite](https://github.com/rusqlite/rusqlite) — SQLite для Rust
- [rayon](https://github.com/rayon-rs/rayon) — параллелизм данных
- [rmcp](https://github.com/anthropics/rmcp) — Rust MCP SDK
