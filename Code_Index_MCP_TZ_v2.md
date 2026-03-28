# ТЕХНИЧЕСКОЕ ЗАДАНИЕ: Code Index MCP v2.1 — Rust Edition

> Высокопроизводительный индексатор кода в базу данных для мгновенного поиска AI-моделями через MCP-протокол

**Версия:** 2.1 | **Дата:** март 2026 | **Платформа:** Rust

---

## 1. Проблема

AI-модели (Claude, GPT и др.) при работе с кодовыми базами вынуждены выполнять последовательный поиск по файлам: grep, find, чтение файлов целиком. Это приводит к:

- **Скорость:** на проекте 1000+ файлов поиск занимает секунды, модель делает 10–15 вызовов grep прежде чем найдёт нужное
- **Контекстное окно:** каждый grep/find/read расходует токены на вывод, оставляя меньше места для полезной работы
- **Неточность:** модель может пропустить вхождения, не угадать имя файла, искать не в том каталоге

> **Реальный пример:** поиск `RuntimeErrorProcessing` в Java-проекте bsl-debug-server потребовал 14 вызовов grep/find, половина вернула пустоту. С индексом это один запрос `find_symbol` — мгновенный ответ.

---

## 2. Решение

Скомпилированный бинарник на Rust, который:

1. Парсит исходный код в AST (Abstract Syntax Tree) с помощью **tree-sitter**
2. Раскладывает структурные элементы (функции, классы, импорты, вызовы) в индексированную базу данных
3. Предоставляет набор MCP-инструментов для AI-модели (search, callers, callees, symbols)
4. Работает как фоновый процесс (daemon), исключая startup overhead при каждом вызове

**Целевая платформа:** Rust (скомпилированный бинарник, кроссплатформенный: Windows, Linux, macOS)

**Целевые языки кода:** Python, JavaScript/TypeScript, Java, 1С (BSL) — через tree-sitter грамматики

---

## 3. Требования к производительности

> **Критично.** Прототип на Python зависал на файлах 5000+ строк. Rust-версия должна обрабатывать такие файлы за миллисекунды.

### 3.1. Целевые метрики

| Операция | Python (было) | Rust (цель) |
|---|---|---|
| Парсинг 1 файла (200 строк) | 25 мс | < 1 мс |
| Парсинг 1 файла (5000 строк) | **зависание** | < 5 мс |
| Индексация 1000 файлов (полная) | 25+ сек | < 1 сек |
| Индексация 1 файла (onChange) | 55 мс (30 мс startup) | < 2 мс (daemon) |
| Поиск символа (find_symbol) | < 5 мс | < 1 мс |
| Полнотекстовый поиск (FTS) | < 10 мс | < 5 мс |

### 3.2. Режим daemon (устранение startup overhead)

Критическая проблема Python-прототипа: каждый вызов onChange запускал отдельный Python-процесс (~30 мс только на старт интерпретатора). В Rust-версии парсер работает как постоянный фоновый процесс:

- Запускается один раз при открытии проекта в VS Code
- VS Code расширение общается с ним через stdin/stdout или unix socket
- Нет overhead на запуск процесса — только время парсинга (< 5 мс)
- Автоматически завершается при закрытии VS Code

---

## 4. Оптимизация записи и обновления

> **Каждые 1.5 секунды после окончания ввода парсить и писать в БД весь файл — расточительно. Нужна многоуровневая проверка.**

### 4.1. Трёхуровневая проверка изменений

| Уровень | Где | Что проверяет | Стоимость |
|---|---|---|---|
| 1 | VS Code (JavaScript) | SHA-256 хеш всего текста буфера | ~0.1 мс. Если хеш не изменился — ничего не делать |
| 2 | Daemon (Rust) | Сравнение нового AST со старым (tree-sitter edit) | < 1 мс. Определяет какие именно узлы изменились |
| 3 | База данных | Обновление только изменённых записей | UPDATE вместо DELETE+INSERT всего файла |

### 4.2. Инкрементальный парсинг (tree-sitter)

Tree-sitter поддерживает инкрементальный парсинг нативно: при получении нового текста он не перестраивает AST с нуля, а применяет edit к существующему дереву. Это означает:

- Изменил одну строчку в функции — перестроилась одна ветка AST, обновилась одна запись в таблице `functions`
- Добавил новый метод в класс — INSERT одной записи, а не перезапись всех 500 функций файла
- Файл 5000 строк с одним изменением обрабатывается так же быстро, как файл 50 строк

---

## 5. База данных

### 5.1. Выбор хранилища

| Параметр | SQLite (файл) | SQLite (in-memory) | Рекомендация |
|---|---|---|---|
| Скорость записи | Быстро (WAL) | Максимально | In-memory + flush |
| Скорость чтения | Быстро | Максимально | In-memory |
| Персистентность | Автоматическая | Нет (RAM) | Периодический flush на диск |
| Потеря данных | Нет | При краше | Допустимо: переиндексация быстрая |
| Зависимости | Нет (встроен) | Нет (встроен) | rusqlite crate |

### 5.2. Рекомендуемая стратегия: hybrid

Работать в режиме in-memory для максимальной скорости, с периодическим сбросом на диск:

1. При старте daemon: загрузить БД с диска в память (если существует)
2. Все операции чтения/записи — в памяти (микросекунды)
3. Flush на диск: каждые 30 секунд или при явном save
4. При закрытии VS Code: финальный flush
5. При краше: потеря максимум 30 секунд изменений, полная переиндексация < 1 сек

### 5.3. Схема таблиц

Сохраняется из прототипа с добавлением поля `ast_hash` для инкрементального обновления:

- **files** — path, content_hash, ast_hash, indexed_at, lines_total
- **functions** — name, qualified_name, line_start, line_end, args, docstring, body, is_async, node_hash
- **classes** — name, bases, docstring, body, node_hash
- **imports** — module, name, alias, line
- **calls** — caller, callee, line
- **variables** — name, value, line
- **fts_functions, fts_classes** — полнотекстовый поиск (FTS5)

**node_hash:** хеш конкретного узла AST. При onChange сравниваются хеши узлов — обновляются только изменённые записи.

---

## 6. Архитектура Rust-бинарника

### 6.1. Компоненты

| Модуль | Crate / библиотека | Ответственность |
|---|---|---|
| parser | tree-sitter + грамматики | Парсинг AST для Python, JS/TS, Java, BSL. Инкрементальный режим |
| indexer | walkdir, notify | Обход директорий, отслеживание изменений файлов |
| storage | rusqlite | In-memory SQLite, FTS5, периодический flush на диск |
| mcp_server | tokio, axum / tower | MCP-протокол (SSE/stdio), 10+ инструментов для AI |
| daemon | tokio | Фоновый процесс, IPC с VS Code, lifecycle management |
| cli | clap | Командная строка: index, serve, stats, install-hook |

### 6.2. Режимы запуска

- `code-index serve` — запустить daemon (MCP-сервер + watcher). Основной режим для VS Code
- `code-index index /path` — однократная индексация (для CI/CD, git hooks)
- `code-index stats` — показать статистику БД
- `code-index query 'symbol_name'` — быстрый поиск из терминала
- `code-index install-hook /path` — установить git hooks

### 6.3. IPC с VS Code

VS Code расширение общается с Rust daemon через stdio (stdin/stdout) в формате JSON-RPC. Расширение отправляет:

- `file_changed {path, content}` — при onChange (после debounce + hash check)
- `file_saved {path}` — при onSave
- `query {tool, arguments}` — MCP-запрос от AI-модели

Daemon отвечает результатами в том же потоке. Никаких HTTP-запросов, никакого сетевого overhead.

---

## 7. Поддержка языков программирования

### 7.1. Tree-sitter грамматики

Tree-sitter — универсальный инкрементальный парсер. Для каждого языка подключается отдельная грамматика (crate):

| Язык | Crate | Приоритет | Статус |
|---|---|---|---|
| Python | tree-sitter-python | P0 (MVP) | Есть прототип |
| JavaScript | tree-sitter-javascript | P1 | Планируется |
| TypeScript | tree-sitter-typescript | P1 | Планируется |
| Java | tree-sitter-java | P1 | Планируется |
| 1С (BSL) | tree-sitter-bsl (custom?) | P2 | Нужна грамматика |
| C# / Rust / Go | tree-sitter-* | P3 | По запросу |

### 7.2. Универсальный извлекатель

Для каждого языка нужен маппинг tree-sitter узлов на нашу схему БД. Маппинг описывается декларативно:

```
Python:  function_definition → functions,  class_definition → classes,  import_statement → imports
Java:    method_declaration → functions,  class_declaration → classes,  import_declaration → imports
JS/TS:   function_declaration | arrow_function → functions,  class_declaration → classes
```

Добавление нового языка = новый tree-sitter crate + маппинг узлов (~50 строк конфига). Без перекомпиляции основного кода.

### 7.3. Типы файлов и стратегии обработки

Не все файлы в проекте — код. Daemon различает три категории:

#### Категория A: Код (AST-парсинг)

Файлы с tree-sitter грамматиками. Разбираются на функции, классы, импорты, вызовы, переменные.

| Расширения | Язык |
|---|---|
| `.py` | Python |
| `.js`, `.jsx` | JavaScript |
| `.ts`, `.tsx` | TypeScript |
| `.java` | Java |
| `.bsl`, `.os` | 1С (BSL) |

#### Категория B: Текстовые файлы (FTS-индексация)

Нет AST-структуры, но содержимое полезно для полнотекстового поиска. Сохраняются в отдельную таблицу `text_files` с FTS5-индексом. AI может искать "где в проекте упоминается API_KEY" и найти это в `.env` или `README.md`.

| Расширения | Тип |
|---|---|
| `.md`, `.txt`, `.rst` | Документация |
| `.json`, `.yaml`, `.yml`, `.toml` | Конфигурация |
| `.xml`, `.html`, `.css` | Разметка/стили |
| `.csv`, `.env`, `.ini`, `.cfg` | Данные/настройки |
| `.sql` | SQL-скрипты |
| `.sh`, `.bat`, `.ps1` | Shell-скрипты |
| `.dockerfile`, `Makefile`, `Cargo.toml`, `package.json` | Сборка |

Для JSON/YAML дополнительно: извлечение ключей верхнего уровня в таблицу `variables`. Для Markdown: извлечение заголовков (# ...) как ориентиров.

#### Категория C: Бинарные файлы (игнорировать)

Не парсятся, не индексируются, полностью пропускаются.

Изображения (`.png`, `.jpg`, `.gif`, `.svg`, `.ico`), видео/аудио, архивы (`.zip`, `.tar`, `.gz`), скомпилированное (`.pyc`, `.class`, `.exe`, `.dll`, `.so`, `.o`), шрифты, PDF, специфическое (`.lock`, `.map`, `.min.js`, `.min.css`).

**Определение категории:** белый список расширений. Файл не в списке A и не в списке B — игнорируется. Конфигурируемо через `codeIndexMcp.languages` и `codeIndexMcp.textExtensions`.

### 7.4. Таблица для текстовых файлов

```sql
CREATE TABLE text_files (
    id          INTEGER PRIMARY KEY,
    file_id     INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    content     TEXT NOT NULL
);

CREATE VIRTUAL TABLE fts_text_files USING fts5(
    content, content='text_files', content_rowid='id'
);
```

---

## 7.5. Хранение индекса: одна папка = одна база

Для каждого проекта (папки, открытой в VS Code) создаётся отдельная база данных:

```
my-project/
├── .code-index/
│   ├── index.db          # SQLite база (in-memory + flush сюда)
│   └── config.json       # языки, исключения, настройки
├── src/
├── tests/
└── ...
```

- Открыл в VS Code папку `/projects/my-scraper` — расширение создаёт `.code-index/index.db` в этой папке
- Другой проект — другая база. Они не пересекаются
- Multi-root workspace: для каждой корневой папки свой daemon и своя база
- `.code-index/` добавляется в `.gitignore` — это локальный кэш, у каждого разработчика свой
- При удалении `.code-index/` — daemon при старте выполнит полную переиндексацию (< 1 сек)

---

## 8. Триггеры обновления индекса

### 8.1. Два режима работы: человек vs AI

Человек и AI-модель работают с кодом по-разному. Расширение поддерживает оба паттерна:

**Человек** печатает код буква за буквой, думает, правит. Ему нужен **onChange** — индекс обновляется в реальном времени по мере набора.

**AI-модель** (Claude Code, Cursor, Copilot) редактирует файлы целиком, часто несколько за раз, и сохраняет. Ей нужен **onSave** — индекс обновляется после сохранения, когда AI будет делать следующий запрос к индексу.

#### Пайплайн AI-модели

1. AI получает задачу → запрашивает данные из индекса (`find_symbol`, `get_callers`) → **индекс актуален, ответы мгновенные**
2. AI понял код → редактирует файлы A, B, C → **индекс не трогается, AI пишет**
3. AI сохраняет файлы → **onSave → батч-индексация всех изменённых файлов одной транзакцией**
4. AI хочет проверить результат → запрашивает из индекса → **индекс уже обновлён**

Цикл: **читаю → пишу → save → индексация → читаю**. Индекс нужен на этапах чтения, между ними — естественная пауза (сохранение).

### 8.2. onChange (для человека)

1. Пользователь печатает код в VS Code
2. VS Code вызывает `onDidChangeTextDocument` (каждая буква)
3. Расширение сбрасывает debounce-таймер (1.5 сек, настраиваемо)
4. Пользователь перестаёт печатать
5. Через 1.5 сек: расширение вычисляет SHA-256 текста буфера в JS (~0.1 мс)
6. Сравнивает с предыдущим хешем в Map. Если совпадает — **СТОП, ничего не делать**
7. Хеш изменился — отправляет `{path, content}` в Rust daemon через stdin
8. Daemon: tree-sitter инкрементальный парсинг (< 5 мс)
9. Daemon: сравнивает хеши узлов, обновляет только изменённые записи в in-memory БД
10. Daemon: отвечает `{status: ok, updated: 1}` расширению
11. Статус-бар показывает ✓

**Общее время от окончания ввода до обновления индекса:** ~1505 мс (1500 мс debounce + 5 мс парсинг). Файл НЕ нужно сохранять.

### 8.3. onSave (для AI и Ctrl+S)

Срабатывает при: Ctrl+S, кнопка "Save", автосохранение VS Code, AI сохраняет файлы после редактирования.

При onSave используется **батчинг** — все сохранённые файлы обрабатываются одной пачкой, одной транзакцией в БД.

### 8.4. Батчинг (мультифайловые изменения)

Когда AI (или человек) меняет несколько файлов за короткий промежуток, вместо отдельной индексации каждого файла — копим в очередь и обрабатываем разом:

```
AI сохраняет файл A → добавить A в очередь, запустить таймер (500 мс)
AI сохраняет файл B → добавить B в очередь, сбросить таймер
AI сохраняет файл C → добавить C в очередь, сбросить таймер
...тишина 500 мс...
таймер сработал → отправить {files: [A, B, C, ...]} в daemon ОДНИМ запросом
daemon → одна транзакция: парсинг всех файлов, обновление БД, COMMIT
```

**Зачем одна транзакция:** если AI переименовал функцию в файле A и обновил вызовы в файле B — оба файла обновляются атомарно. В базе никогда не будет промежуточного состояния "функция переименована, а вызовы старые".

### 8.5. Проверка при открытии папки (startup scan)

При открытии проекта в VS Code daemon обязан проверить актуальность индекса. Между сессиями файлы могли измениться: `git pull`, копирование, редактирование в другом редакторе.

Последовательность:

1. Daemon стартует, загружает `index.db` из `.code-index/`
2. Рекурсивно обходит папку проекта (учитывая `excludeDirs`)
3. Для каждого файла с поддерживаемым расширением — считает SHA-256
4. Сравнивает с хешем в базе. **Без парсинга** — только хеши
5. Файлы с изменённым хешем — парсит и обновляет (батчем, одна транзакция)
6. Файлы в базе, но отсутствующие на диске — удаляет из индекса
7. Новые файлы (есть на диске, нет в базе) — парсит и добавляет

На проекте 1000 файлов: обход + хеши = ~100 мс. Перепарсить только изменённые. При полном совпадении — индекс готов мгновенно.

### 8.6. Дополнительные триггеры

- **git hooks (post-commit, post-merge)** — `code-index index /path --quiet`
- **Ручной запуск** — `code-index index /path`
- **VS Code палитра команд** — Code Index: Reindex / Force Reindex / Show Stats

---

## 9. MCP-инструменты для AI-модели

### 9.1. Список инструментов (10+)

| Tool | Описание | Вместо |
|---|---|---|
| `search_function` | Полнотекстовый поиск по функциям (имя, docstring, тело) | `grep -r 'def ...'` |
| `search_class` | Полнотекстовый поиск по классам | `grep -r 'class ...'` |
| `get_function` | Получить функцию по точному имени (с исходным кодом) | `find + cat` |
| `get_class` | Получить класс по точному имени | `find + cat` |
| `get_callers` | Кто вызывает данную функцию? Граф вызовов | `grep по всем файлам` |
| `get_callees` | Что вызывает данная функция? | `чтение файла + grep` |
| `find_symbol` | Поиск символа везде: функции, классы, переменные, импорты | `10+ grep-ов` |
| `get_imports` | Импорты по модулю или файлу | `grep 'import'` |
| `get_file_summary` | Полная карта файла: все элементы без чтения исходника | `cat + анализ` |
| `get_stats` | Статистика базы: сколько файлов, функций, классов | `find \| wc -l` |
| `search_text` | Полнотекстовый поиск по текстовым файлам (md, json, yaml, env и др.) | `grep -r 'API_KEY' .` |

### 9.2. Системный промпт для AI-модели

Чтобы AI-модель использовала индекс вместо grep, в конфигурации MCP-клиента добавляется инструкция:

```
You have access to a Code Index database via MCP tools.
ALWAYS use these tools instead of grep/find/reading files:

- Need to find a function? → search_function or get_function
- Need to find who calls X? → get_callers
- Need to find what X calls? → get_callees
- Need to find where symbol is defined? → find_symbol
- Need to understand a file? → get_file_summary
- Need to find imports? → get_imports

NEVER use grep, find, or read entire files when the index
tools can answer the question. They are instant (<5ms)
and complete.
```

---

## 10. VS Code расширение

Расширение — тонкая JS-обёртка. Вся тяжёлая работа в Rust daemon.

### 10.1. Ответственность расширения

- Подписка на события VS Code (onChange, onSave, onOpen)
- Debounce (1.5 сек после последнего ввода, настраиваемо)
- Проверка хеша в JS (уровень 1 — отсечение без вызова Rust)
- Запуск/остановка Rust daemon
- IPC: отправка содержимого буфера в daemon через stdin
- UI: иконка статус-бара, Output Channel, команды палитры

### 10.2. Настройки

- `codeIndexMcp.trigger`: `onChange` | `onSave` | `manual` (onChange для человека, onSave для AI-сценариев)
- `codeIndexMcp.changeDebounceMs`: `1500` (debounce для onChange)
- `codeIndexMcp.saveDebounceMs`: `500` (debounce для батчинга onSave)
- `codeIndexMcp.flushIntervalSec`: `30` (как часто сбрасывать in-memory БД на диск)
- `codeIndexMcp.languages`: `[python, javascript, java]` (какие языки парсить в AST)
- `codeIndexMcp.textExtensions`: `[md, txt, json, yaml, yml, toml, env, xml, sql]` (текстовые файлы для FTS)
- `codeIndexMcp.excludeDirs`: `[node_modules, .venv, __pycache__, .git, .code-index]`
- `codeIndexMcp.binaryPath`: `auto` (путь к Rust-бинарнику)

---

## 11. Сборка и дистрибуция

### 11.1. Кроссплатформенная сборка

- `cargo build --release` — бинарник для текущей платформы
- Cross compile для Windows (x86_64-pc-windows-msvc), Linux (x86_64-unknown-linux-gnu), macOS (aarch64-apple-darwin)
- Бинарник самодостаточный, без зависимостей (SQLite статически линкуется)

### 11.2. Дистрибуция

- **VS Code Marketplace:** расширение включает Rust-бинарник для всех платформ
- **CLI:** отдельный бинарник для использования без VS Code (CI/CD, скрипты)
- `cargo install code-index-mcp` — установка из crates.io

---

## 12. Этапы разработки

### Этап 1: MVP (Rust парсер + CLI)

- Rust: tree-sitter парсер для Python, запись в SQLite
- Rust: CLI (`index`, `stats`, `query`)
- Rust: MCP-сервер (HTTP, 10 инструментов)
- Тест на проекте 5000+ строк — подтвердить < 5 мс

### Этап 2: Daemon + VS Code

- Rust: daemon режим (stdin/stdout IPC)
- Rust: in-memory SQLite + periodic flush
- Rust: инкрементальный парсинг (tree-sitter edit)
- JS: VS Code расширение (onChange, debounce, hash check)

### Этап 3: Мультиязычность

- Tree-sitter грамматики: JavaScript, TypeScript, Java
- Универсальный маппинг узлов (декларативный конфиг)
- 1С (BSL): custom tree-sitter грамматика или адаптация существующей

### Этап 4: Полировка

- MCP-протокол SSE/stdio для Claude Desktop
- Системный промпт для AI-моделей (из коробки)
- Публикация: VS Code Marketplace + crates.io
- Документация, примеры, бенчмарки

---

## 13. Структура проекта (целевая)

```
code-index-mcp/
├── Cargo.toml
├── src/
│   ├── main.rs              # CLI entry point (clap)
│   ├── daemon.rs             # Background process, IPC, lifecycle
│   ├── parser/
│   │   ├── mod.rs            # Universal parser interface
│   │   ├── python.rs         # Python tree-sitter mappings
│   │   ├── javascript.rs     # JS/TS mappings
│   │   ├── java.rs           # Java mappings
│   │   ├── bsl.rs            # 1С (BSL) mappings
│   │   └── text.rs           # Text files (FTS-only, no AST)
│   ├── storage/
│   │   ├── mod.rs            # DB interface
│   │   ├── schema.rs         # Tables, indexes, FTS5, text_files
│   │   └── memory.rs         # In-memory + periodic flush
│   ├── indexer/
│   │   ├── mod.rs            # Directory walker
│   │   ├── file_types.rs     # Extension → category mapping (A/B/C)
│   │   ├── hasher.rs         # SHA-256 hash check, content_hash cache
│   │   ├── batch.rs          # Batch processing (multi-file transactions)
│   │   └── startup.rs        # Startup scan (diff index vs disk)
│   ├── mcp/
│   │   ├── mod.rs            # MCP server
│   │   └── tools.rs          # 11 tool implementations (incl. search_text)
│   └── ipc.rs                # JSON-RPC stdin/stdout
├── vscode-extension/
│   ├── package.json
│   └── extension.js          # Thin JS wrapper (hash check, debounce, batching)
├── grammars/                 # Tree-sitter grammar configs
└── tests/
    ├── bench_5000_lines.rs   # Performance regression test
    ├── batch_test.rs         # Multi-file transaction test
    ├── startup_scan_test.rs  # Index vs disk consistency test
    └── integration/
```
