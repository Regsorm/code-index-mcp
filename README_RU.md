<a href="https://infostart.ru/1c/tools/2677918/" title="Публикация на Инфостарте">
  <img src="https://infostart.ru/bitrix/templates/sandbox_empty/assets/tpl/abo/img/logo.svg" alt="Infostart" height="32">
</a>

Опубликовано на Инфостарте: [Code Index — структурный поиск по выгрузке кода 1С через MCP](https://infostart.ru/1c/tools/2677918/)

---

# code-index-mcp

[English version](README.md)

**Rust-native индекс кода для AI-агентов. Статический бинарник. Production-grade поддержка BSL/1С.**

Один статический бинарник под Windows/Linux/macOS — без рантайма, без зависимостей. Индексирует крупные репозитории за секунды, отдаёт результат AI-агенту через MCP за миллисекунды. 31 tools: 20 универсальных + 11 BSL-специфичных для конфигураций 1С:Предприятие.

## Что внутри

- **Производительность.** 151 000 файлов проиндексировано за 41 секунду (v0.47.0, боевой PHP-сайт), поиск <1 мс на запрос. Подходит для монорепо 100K+ файлов.
- **31 MCP tools.** 20 универсальных (функции, классы, callers/callees, content файлов, grep) + 11 BSL-tools (структура и паспорт объекта, обработчики форм, подписки на события, граф вызовов, граф связей данных, регистраторы движений, карта влияния, read-only SQL).
- **Native BSL/1С.** Парсит выгрузки конфигураций 1С:Предприятие 8.3 как из Конфигуратора (XML), так и из 1С:EDT (`.mdo`). Граф связей данных (рёбра объект→объект по ссылочным типам реквизитов) для типичной бухгалтерии — ~60 000 рёбер за пару секунд.
- **Federation.** Один MCP-сервер обслуживает несколько репозиториев из разных машин — `repo: "alias"` в каждом tool-call.
- **Сжатое хранилище content.** Содержимое файлов хранится в SQLite через zstd, дешёвый random-access read для AI-агента.
- **Tree-sitter AST.** 14 языков с полным разбором (Python, JavaScript, TypeScript, Java, Rust, Go, PHP, C, C++, C#, Ruby, Swift, 1С (BSL), HTML) + полнотекстовая индексация 50+ текстовых форматов.

Подключается к Claude Code, Cursor, любому MCP-клиенту по HTTP.

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
| PHP | tree-sitter-php | `.php`, `.php5`, `.phtml` (v0.46.0) |
| C | tree-sitter-c | `.c`, `.h` (v0.46.0) |
| C++ | tree-sitter-cpp | `.cpp`, `.cxx`, `.cc`, `.hpp`, `.hxx`, `.hh` (v0.46.0) |
| C# | tree-sitter-c-sharp | `.cs` (v0.46.0) |
| Ruby | tree-sitter-ruby | `.rb` (v0.46.0) |
| Swift | tree-sitter-swift | `.swift` (v0.46.0) |
| 1С (BSL) | tree-sitter-bsl | `.bsl`, `.os` |
| XML (1С) | quick-xml | `.xml` (метаданные конфигураций) |
| Метаданные 1С:EDT | quick-xml | `.mdo`, `.form`, `.rights` (v0.53.0) |
| HTML | tree-sitter-html | `.html`, `.htm` (v0.7.1, по запросу пользователя — см. маппинг ниже) |

Расширение `.h` неоднозначно между C и C++: оно разбирается как C++, если язык репозитория — `cpp`, иначе как C. Язык репозитория берётся из `daemon.toml` либо определяется автоматически по преобладающему расширению.

PHP, C, C++, C#, Ruby и Swift индексируются двойным способом (как HTML): AST-разбор плюс сырое содержимое в `text_files`, поэтому `search_text` / `grep_text` продолжают работать по ним наряду со структурными запросами.

Текстовые файлы (`.md`, `.json`, `.yaml`, `.toml`, `.xml`, `.sql`, `.env` и др.) индексируются для полнотекстового поиска.

### HTML — маппинг сущностей (v0.7.2)

У HTML нет родного понятия «функция»/«класс», поэтому маппинг — конвенциональный. **Двойная индексация**: html-файлы проходят И через AST-парсер, И через `text_files`, чтобы `search_text` / `grep_text` / `read_file` продолжали работать наравне с новыми structured queries.

| HTML | → | Таблица code-index | Имя |
|------|---|--------------------|-----|
| `<element id="X">…</element>` | → | `classes` | `X` (body=outerHTML, bases=tag_name) |
| `<form id|name="X">` | → | `classes` | `form_X` (bases=`form`) |
| `<form>` без id/name | → | `classes` | `form_<line>` |
| `<input/select/textarea name="Y">` | → | `variables` | `Y` |
| `<a href="URL">` | → | `imports` | `module=URL`, `kind="link"` |
| `<link href="URL" rel="X">` | → | `imports` | `module=URL`, `kind=X` (или `"stylesheet"`) |
| `<script src="URL">` | → | `imports` | `module=URL`, `kind="script"` |
| `<img/iframe/video/audio/source/embed src="URL">` | → | `imports` | `module=URL`, `kind=tag` |
| `<script>…inline JS…</script>` | → | `functions` | `inline_script_<line>` (body=содержимое) |
| `<style>…inline CSS…</style>` | → | `functions` | `inline_style_<line>` (body=содержимое) |
| Атрибут `class="foo bar baz"` | → | `variables` | `class:foo`, `class:bar`, `class:baz` (по одной записи на каждый) |

Все MCP-инструменты, которые работают с HTML после переиндексации:

```
# === Discovery / метаданные ===
list_files(repo="X", pattern="**/*.html")                # все html (вернёт language="html")
list_files(repo="X", path_prefix="src/templates/")
stat_file(repo="X", path="src/templates/base.html")      # language="html", category="text"
get_stats(repo="X")                                       # сводные счётчики

# === Структурные (AST) — новинка 0.7.x ===
# id-элементы, формы, css-классы, ссылки, inline-блоки → AST-таблицы
get_class(repo="X", name="cart")                          # outerHTML <... id="cart">
get_class(repo="X", name="form_login")                    # форма <form id="login"> целиком
search_class(repo="X", query="container", language="html")
get_function(repo="X", name="inline_script_42")           # body <script> на строке 42
search_function(repo="X", query="inline_script", language="html")
find_symbol(repo="X", name="form_login")                  # точный поиск по всем 4 таблицам
find_symbol(repo="X", name="class:htmx-indicator")        # использования CSS-класса
get_imports(repo="X", module="https://unpkg.com/htmx.org@1.9.12")  # кто зависит от CDN
get_file_summary(repo="X", path="src/templates/base.html")         # полная карта файла

# === Body-level grep (работает по inline_script body) ===
grep_body(repo="X", regex="fetch\\(", language="html")    # в <script>-блоках
grep_body(repo="X", pattern="color:", language="html")    # в <style>-блоках
grep_body(repo="X", regex="hx-target", language="html", path_glob="src/templates/**", context_lines=2)

# === Text-level (продолжают работать через двойную индексацию) ===
read_file(repo="X", path="src/templates/base.html", line_start=1, line_end=20)
search_text(repo="X", query="DOCTYPE", language="html")
grep_text(repo="X", regex="\\{%\\s*include", path_glob="**/*.html", context_lines=1)  # Jinja-includes
```

`get_callers` / `get_callees` для HTML не наполняются (парсер не извлекает рёбра вызовов между скриптами).

Шаблонизаторы (Jinja/Django/EJS): `{{ … }}` и `{% … %}` парсер пропускает как text-content без падения; элементы вокруг них извлекаются нормально.

## Быстрый старт

### Установка через npm (самый простой способ)

```bash
npm install -g @regsorm/code-index-mcp
```

Шаг `postinstall` скачивает готовый нативный бинарник под вашу платформу (Windows x64, Linux x64, macOS arm64) из GitHub Releases — ничего не компилируется. Запуск как MCP-сервера:

```bash
npx @regsorm/code-index-mcp serve --path /путь/к/репозиторию
```

Опубликован также в [официальном MCP-реестре](https://registry.modelcontextprotocol.io/) как `io.github.Regsorm/code-index`. Обёртка содержит только публичный бинарник `code-index` (без поддержки 1С); для `bsl-indexer` собирайте из исходников.

### Сборка из исходников

```bash
git clone https://github.com/Regsorm/code-index-mcp.git
cd code-index-mcp
cargo build --release -p code-index               # публичный бинарник для Python/Rust/Go/Java/JS/TS
cargo build --release -p bsl-indexer --features enrichment   # дополнительная сборка с поддержкой 1С + LLM-обогащением
```

Бинарники:
* `target/release/code-index[.exe]` — основной (без 1С).
* `target/release/bsl-indexer[.exe]` — с полной поддержкой 1С (XML-парсеры, граф вызовов BSL, граф связей данных, MCP-tools `get_object_structure`/`get_form_handlers`/`find_path_bsl`/`search_terms`/`get_data_links`/`find_data_path`/`get_register_writers` и опциональный LLM-enrichment под cargo feature `enrichment`).

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
   path = "C:\\Repo1C"

   [[paths]]
   path = "C:\\Repo1C-2"
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

`CODE_INDEX_HOME` **обязателен** — fallback'а нет. Если переменная не задана, и `daemon`, и `serve` завершатся с ошибкой, объясняющей, как её задать.

> **Решение проблемы — «демон не запущен / runtime-info отсутствует», хотя демон РАБОТАЕТ.**
>
> Процесс `serve` и демон находят друг друга только через `$CODE_INDEX_HOME/daemon.json`. Если у `serve` другой (или пустой) `CODE_INDEX_HOME`, чем у демона, он ищет `daemon.json` не там и считает демон офлайн — хотя тот жив.
>
> Самая частая причина на Linux/macOS: **GUI-клиенты MCP (VS Code, Continue, Cline) не читают `~/.bashrc` / `~/.zshrc`**, поэтому запущенный ими `serve` с пустым `env` не видит `CODE_INDEX_HOME`, который вы экспортировали в шелле. А демон, запущенный из терминала, видит — и они оказываются в разных папках.
>
> **Решение:** задайте `CODE_INDEX_HOME` явно в секции `env` MCP-конфигурации клиента, тем же **абсолютным путём**, что у демона (`$HOME` там не раскрывается — используйте реальный путь). Перезапустите клиент и проверьте через `code-index daemon status`.

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
      "command": "npx",
      "args": ["-y", "@regsorm/code-index-mcp", "serve", "--path", "."]
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
| `get_function` | Получить функцию по точному имени (регистронезависимый fallback; **(v0.44.0)** при 0 совпадений возвращает `did_you_mean` с похожими именами) |
| `get_class` | Получить класс по точному имени (регистронезависимый fallback; **(v0.44.0)** при 0 совпадений возвращает `did_you_mean` с похожими именами) |
| `get_object_structure` | Структура объекта 1С. **v0.63.0:** в перечне появились макеты объектов — строка `Catalog.X.Template.Y` с владельцем, видом макета и признаком `content_indexed` (крупные макеты в индекс не берутся, но сам макет виден и находится по имени) |
| `get_callers` | Кто вызывает данную функцию? **(v0.35.0)** каждая запись несёт `path` файла-вызывателя (различает одноимённых вызывателей из разных файлов). **v0.62.0:** для конфигураций 1С к вызывателям из кода добавляются привязки обработчиков форм (`kind: form_binding`) — процедуру выполняет платформа по записи в описании формы, вызова в коде у неё нет; когда имя носят несколько процедур, вместо списка приходит запись с образцом вызова `get_form_handlers` |
| `get_callees` | Что вызывает данная функция? **(v0.35.0)** каждая запись несёт `path` файла-источника |
| `find_path` | **(v0.23.0)** Кратчайший путь в графе вызовов от функции `from` до `to` (итеративный cycle-safe BFS по уникальным узлам `calls`, `max_depth=5`, любой язык). Возвращает рёбра пути `[{caller, callee, line}]`. **v0.57.0:** пустой ответ несёт признаки обрыва обхода `walk_depth_exhausted` / `walk_nodes_capped`, а подсказка различает «упёрлись в потолок узлов», «не хватило глубины» и «достижимый подграф исчерпан целиком» — раньше все три случая выглядели как «пути нет» |
| `get_call_tree` | **(v0.23.0)** Дерево вызовов от функции `root` на глубину `max_depth` (по умолчанию 3). `direction`: `callees`/`down` (вглубь) или `callers`/`up`. Плоский список рёбер `[{caller, callee, line, depth, path}]` (**(v0.35.0)** `path` = файл-источник ребра) + вложенное дерево `{name, children}`; cap `max_nodes` |
| `find_symbol` | Поиск символа везде (функции, классы, переменные, импорты) |
| `get_imports` | Импорты по модулю или файлу |
| `get_file_summary` | Полная карта файла без чтения исходника |
| `get_stats` | Статистика индекса |
| `search_text` | Полнотекстовый поиск по текстовым файлам |
| `grep_body` | Поиск подстроки или regex в телах функций и классов. Возвращает `match_lines` (первые 3 номера строк) и `match_count` (всего, если > 3). v0.7.0: опциональные `path_glob`, `context_lines` |
| `stat_file` | **(v0.7.0)** Метаданные одного файла: exists, size, mtime, language, lines_total, content_hash, indexed_at, category (`text`/`code`). **(v0.8.0)** добавляет `oversize: bool` для code-файлов |
| `list_files` | **(v0.7.0)** Плоский список файлов с опциональными `pattern` (glob `**/*.py`), `path_prefix`, `language`, `limit`. **v0.49.8:** умолчание `limit` — 500; если подходящих файлов больше, ответ несёт `{truncated, total, shown, limit}` — раньше список обрезался молча и выглядел полным |
| `read_file` | **(v0.7.0)** Чтение содержимого файла. Опциональные `line_start`/`line_end` (1-based, inclusive). Soft-cap 5000 строк или 500 КБ, hard-cap 2 МБ. **(v0.8.0)** работает для **code-файлов** (`.py`, `.bsl`, `.rs`, `.ts` и др.) — content хранится в таблице `file_contents` (zstd). Файлы-oversize (дефолт > 5 МБ) возвращают `oversize: true` с пустым `content` и подсказкой |
| `grep_text` | **(v0.7.0)** Regex-поиск по содержимому text-файлов через REGEXP. Закрывает дыру FTS5 со спецсимволами (точки, скобки, экраны). Опциональные `path_glob`, `language`, `context_lines`. Hard-cap 1 МБ на размер ответа |
| `grep_code` | **(v0.8.0)** Regex-поиск по содержимому **code-файлов** (`.py`, `.bsl`, `.rs`, `.ts` и др.) через таблицу `file_contents` (zstd-decode в Rust). Параметры аналогичны `grep_text`: `regex`, `path_glob?`, `language?`, `limit?`, `context_lines?`. С v0.66.0 оба инструмента принимают также `pattern` — буквальную подстроку без учёта регистра (экранируется перед поиском), с тем же смыслом, что у `grep_body`. Дополняет `grep_body` (тот ищет только в телах функций/классов). Oversize-файлы пропускаются |
| `health` | Статус MCP-сервера и подключённых репо |

Все поисковые инструменты (`search_function`, `search_class`, `get_function`, `get_class`, `find_symbol`, `search_text`, `grep_body`) принимают опциональный параметр **`path_glob`** (v0.7.0) для сужения выдачи по подкаталогу (например, `src/auth/**`, `Documents/**/*.bsl`). Реализация — post-filter через crate `globset` после SQL-выборки. С v0.32.0 в `path_glob`/`pattern` поддерживаются brace-альтернативы `{a,b}` (`**/*.{bsl,xml}`) — в том числе в `grep_code`/`grep_text`/`grep_body`/`list_files`, где фильтр работает на уровне SQLite GLOB (паттерн раскрывается в OR-группу условий). В `search_function`/`search_class`/`search_text` фильтр применяется после выборки записей по релевантности, поэтому сужённая выдача может быть неполной; с v0.49.8 такой ответ несёт `prefilter_exhausted` вместе с `prefetched`/`shown` — и на непустом результате, а не только на пустом.

### Хранение содержимого code-файлов (v0.8.0)

С v0.8.0 содержимое code-файлов хранится в таблице `file_contents` (zstd-сжатие) и возвращается `read_file`, а также доступно для поиска через `grep_code`. Крупные файлы можно исключить из хранения через `max_code_file_size_bytes` (дефолт **5 МБ**):

```toml
[indexer]
max_code_file_size_bytes = 5242880   # 5 МБ, глобальный override

[[paths]]
path = "C:/Repo1C"
max_code_file_size_bytes = 10485760  # только для этого репо — 10 МБ
```

Приоритет: per-path → секция `[indexer]` → дефолт 5 МБ. Файлы сверх лимита сохраняются с `oversize=1` и `content_blob=NULL`; AST-парсинг, FTS и граф вызовов для них работают в полном объёме. `read_file` и `grep_code` возвращают подсказку как искать такие файлы через `get_function`/`get_class`/`grep_body`.

### Пул соединений (v0.24.0)

В `serve` каждый репо читается через пул read-only SQLite-соединений вместо одного соединения под мьютексом. Запросы к **одному** репо теперь идут параллельно — тяжёлый запрос (`bsl_sql`, полный `grep_code`, рекурсивные `find_path`/`get_call_tree`) больше не блокирует мгновенный `get_function` того же репо. Настраивается опциональной секцией `[pool]` в `serve.toml`:

```toml
[pool]
pool_size = 4               # соединений на репо (дефолт 4)
per_conn_cache_kib = 16384  # page-cache на соединение, КиБ (дефолт 16384 = 16 МБ)
busy_timeout_ms = 5000      # SQLite busy_timeout на соединение, мс (дефолт 5000)
```

Все поля опциональны; без секции берутся дефолты. Дефолт нейтрален по памяти: `4 × 16 МБ = 64 МБ` на активный репо — столько же, сколько у прежнего единственного соединения. Соединения открываются лениво до `pool_size` и возвращаются в пул по завершении запроса; `0` приводится к безопасному значению. Режим WAL (уже используется индексом) делает несколько читателей безопасными рядом с записями демона-индексатора.

### Дополнительно для 1С-репо (только в `bsl-indexer`, v0.6+)

При наличии BSL-репо в `daemon.toml` (`language = "bsl"`) автоматически добавляются 11 BSL-инструментов:

| Инструмент | Описание |
|------------|----------|
| `get_object_structure` | Полная структура объекта конфигурации 1С по `full_name` (`Document.РеализацияТоваровУслуг`): реквизиты с типами в 1С-нотации (+`synonym` — UI-подпись, +`required` — обязательность заполнения, v0.32.0; +`indexing` — признак индексирования `Index`/`IndexWithAdditionalOrder`, v0.45.1), табличные части с колонками, измерения/ресурсы регистров; `enum_values` для перечислений (+`enum_synonyms` — UI-подписи значений); `predefined` для объектов с предопределёнными элементами (справочники, планы счетов); `posting` для документов (свойства проведения из корневого `<Properties>`); `owners` — владельцы подчинённого справочника; `value_types` — тип значения ПВХ/константы (для ПВХ — доступные аналитики); `properties` — свойства шапки (периодичность ИР, режим записи, нумерация, иерархия); `commands` — команды объекта с синонимами (всё — v0.32.0). Базовые секции (`attributes`/`dimensions`/`resources`/`tabular_sections`) присутствуют всегда (пустые — как `[]`). Параметр `sections` (v0.29.0) — узкая выборка: вернуть только указанные секции (например `["posting"]` — ~0.2 КБ вместо полного объекта). **Критерий-селектор (v0.41.0):** `name_like` (подстрока имени объекта) + опц. `meta_type` возвращают структуры ВСЕХ подходящих объектов за один вызов (`{matched, truncated, results}`, потолок 50) — для тематического набора (например `name_like="ЭДО"`) вместо вызовов по одному. **v0.49.0:** `names_only=true` — только паспорт каждого объекта (`full_name`/`meta_type`/`name`/`synonym`) без структуры, самый дешёвый режим поиска; `max_response_bytes` — бюджет размера этого ответа поверх серверного, когда нужны полные секции. **v0.55.0:** `offset` — страницы по ОДНОЙ запрошенной секции (`sections=['enum_values']`): порция набирается по размеру ответа, рядом `<секция>_shown`/`_total`/`_offset`/`_has_more` и готовый вызов следующей страницы. До этого секция на сотни значений выбрасывалась стражем целиком и была недоступна |
| `get_form_handlers` | Обработчики событий управляемой формы по `(owner_full_name, form_name)`. Владелец принимается в обоих форматах — `Document.X` и формат папки выгрузки `Documents.X` (v0.31.0). **v0.54.0:** отдаёт тройки `(event, handler, element)` — `element` это элемент формы, которому принадлежит обработчик (у обработчиков самой формы поля нет); необязательный фильтр `element` сужает выдачу до одного элемента, пустая строка — только события самой формы (крупная форма: 308 записей → 8), в ответ добавляются `handlers_shown`/`handlers_total`. Имена событий приводятся к русским в обоих форматах выгрузки — словарь на 91 событие. Несуществующая форма → ошибка с `available_forms` владельца. **v0.55.0:** `form_name` необязателен — без него обработчики ВСЕХ форм объекта за один вызов (было 255 вызовов на объекте с 255 формами); появился бюджет `max_response_bytes`. Не влезает — перечень форм с числом обработчиков либо счётчики по элементам формы, и в обоих случаях готовый вызов за нужным куском |
| `get_event_subscriptions` | Все подписки на события из `EventSubscriptions/*.xml`. Фильтры: handler-модуль, событие (русское имя или английский enum платформы — `OnWrite`→`ПриЗаписи`), `source` — по объекту-источнику (`Document.X`/`DocumentObject.X`/короткое имя, v0.31.0). Неизвестные параметры отклоняются с перечнем допустимых; default limit 50 (`truncated`+`total` при превышении) |
| `find_path_bsl` | Цепочка вызовов между двумя процедурами через `proc_call_graph` (recursive CTE, max_depth=3). BSL-вариант универсального `find_path` — `proc_call_graph` хранит `call_type` и ключи процедур. **(v0.35.0)** `from`/`to` и ключи процедур — `<rel_path>::<name>` (для нерезолвленных листьев допустимо голое имя); обход по резолвленному `callee_proc_key`. **(v0.36.0)** BSL хранит callee склеенно (`Модуль.Метод`, как `obj.method` у Python); цель вызова резолвится точно в адрес общего модуля (`…/CommonModules/X/Ext/Module.bsl::M`) и менеджер-модуля (`…/Catalogs/X/Ext/ManagerModule.bsl::M`), объектный шум отсеян — резолв direct-рёбер ~80-82% |
| `search_terms` | **Смысловой поиск процедур (v0.30.0):** термы наполняются механически при индексации — слова имени процедуры (CamelCase-сплит), имя и синоним объекта-владельца, комментарий над процедурой; LLM не нужен. Триграммный FTS: словоформы и подстроки от 3 символов, регистр и ё/е не важны; многословный запрос ищется как OR по словам (лучшие совпадения сверху). Первый выбор для «где реализован функционал X». LLM-обогащение (`bsl-indexer enrich`) остаётся опциональной надстройкой |
| `get_data_links` | **Граф связей данных (v0.10.0):** на что ссылается объект / кто ссылается на него — по ссылочным реквизитам, измерениям регистров и реквизитам табличных частей (таблица `data_links`). `direction=out\|in\|both`, `depth=1..4`. Заменяет серию `get_object_structure` при трассировке связей. Цели вида `*CatalogRef`/`*AnyRef`/`*DefinedType.X` — обобщённые ссылки (терминал, не разворачиваются). **v0.56.0:** обход — поиск в ширину с учётом посещённых узлов вместо рекурсивного запроса по путям (на центральном справочнике направление «кто ссылается» на глубине 4: 42,9 с → доли секунды), прерывание по времени 8 с с признаком `walk_stopped_by_time`, потолок числа рёбер. Рёбра отдаются страницей по размеру ответа (`offset`, `<dir>_shown`/`_total`/`_offset`/`_has_more`), рядом `<dir>_by_kind` — счётчики по видам связи, чтобы сузить запрос вместо перебора страниц. На глубине 1 полное число рёбер точное; глубже, если обход упёрся в потолок или время, оно помечается `<dir>_total_partial` |
| `find_data_path` | **Граф связей данных (v0.10.0):** цепочка ссылочных связей от одного объекта к другому (BFS по `data_links`, аналог `find_path`, но про данные, а не вызовы). **v0.57.0:** при неудаче ответ различает «пути нет» и «не хватило шагов» — признаки `depth_exhausted`, `visited_nodes` и готовый вызов с увеличенной глубиной |
| `get_register_writers` | **Регистраторы регистра / движения документа (v0.16.0):** для регистра (`AccumulationRegister.ТоварыНаСкладах`) возвращает `writers` — документы, пишущие движения; для документа — `writes_to` (регистры-приёмники). По декларативному составу `<RegisterRecords>` (recorder-рёбра `data_links`). Один вызов закрывает оба направления |
| `get_object_profile` | **Паспорт объекта за один вызов (v0.21.0):** полный портрет объекта — структура + формы + модули + связи данных — вместо серии `get_object_structure`/`get_form_handlers`/`get_data_links`. Параметр `sections` (`['structure'\|'forms'\|'modules'\|'data_links']`) сужает ответ. **v0.55.0: обзор вместо выгрузки** — формы отдаются перечнем с числом обработчиков, модули счётчиками по типам, связи данных только счётчиками по видам (без рёбер: их объём задаётся не объектом, а всей конфигурацией). Содержимое — параметром `expand` (`forms.handlers`, `modules.list`) либо само, когда ответ укладывается в бюджет (признаки `forms_handlers_included`, `modules_listed`); свёрнутые секции сопровождаются готовым вызовом за содержимым. На объекте с 255 формами ответ 336 КБ → 19 КБ |
| `find_references` | **Карта влияния (v0.21.0):** всё, что ссылается на объект, одним вызовом — реверс `data_links` (структурные ссылки из метаданных) + `metadata_code_usages` (обращения в коде `.bsl`) + `role_rights` (роли с правами на объект), с разбивкой по видам и примерами (`limit`). **v0.55.0:** `section` (`data_refs`/`code_usages`/`role_rights`) + `offset` — одна секция целиком, страницами по размеру ответа, с готовым вызовом следующей страницы: на центральном справочнике 2 443 обращения в коде забираются за 10 ходов, раньше дальше 200 примеров пройти было нечем |
| `bsl_sql` | **Произвольный read-only SQL (v0.21.0):** запрос `SELECT`/`WITH` по `index.db` репо для длинного хвоста вопросов по метаданным/графам без отдельного named-tool (роли/RLS, join'ы, агрегации). Guard: только `SELECT`/`WITH` + `Statement::readonly()` + потолок строк + таймаут. Таблицы: `metadata_objects`, `metadata_modules`, `metadata_forms`, `event_subscriptions`, `data_links`, `role_rights`, `proc_call_graph`, базовые `functions`/`files`. Пустая выборка по таблицам процедур автоматически откатывается на `search_terms` |

**Регистр имени объекта не важен (v0.57.0).** Имена объектов 1С кириллические, и SQLite их регистр не сворачивает, поэтому `Catalog.организации` прежде давал молчаливый ноль: связи данных отвечали «на объект никто не ссылается», регистраторы — «регистраторов нет». Теперь имя приводится к записи из конфигурации на входе объектных инструментов (`get_data_links`, `find_data_path`, `get_register_writers`, `get_object_structure`, `get_object_profile`, `find_references`), а в ответе показывается канонически. Подбор похожих имён при опечатке тоже перестал зависеть от регистра. Схема получила колонку-ключ и индекс; **переиндексация не нужна** — миграция заполняет ключи при первом старте демона.

Эти инструменты появляются в `tools/list` **только при наличии BSL-репо** (conditional registration). При смене состава репо в `daemon.toml` сервер шлёт `notifications/tools/list_changed`; на текущей версии Claude Code 2.1.120 уведомление [игнорируется](https://github.com/anthropics/claude-code/issues/13646), workaround — `/mcp Reconnect`.

**Начиная с v0.8.1** эти BSL-tools работают во **всех** сценариях (в v0.8.0 были не работоспособны — см. CHANGELOG):

* через `bsl-indexer.exe daemon run` — даemon применяет `schema_extensions` и `index_extras` при инициализации каждого BSL-репо (создаёт `metadata_objects` / `metadata_forms` / `event_subscriptions` / `proc_call_graph` и заполняет их из `Configuration.xml`).
* через federation — extension-tools форвардятся по универсальному маршруту `POST /federate/extension`. Обе стороны должны быть на **≥ 0.8.1**; старая нода вернёт 404 на новый route.
* на репо без `Configuration.xml` (например, частичные выгрузки только форм/обработок) — таблицы создаются пустыми и инструменты возвращают `[]` вместо ошибки `no such table: metadata_objects`.

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
2. Для каждой папки открывает `.code-index/index.db`, делает полный reindex с mtime fast-path (v0.4.0), затем переключается на `notify`-watcher и переиндексирует файлы при изменениях (debounce 1.5 с, batch 2 с). Файл, который в момент чтения занят другим процессом (идёт выгрузка конфигурации), не теряется: событие откладывается и повторяется с нарастающей паузой 2 → 5 → 15 → 30 → 60 с, всего 6 попыток считая первую. Исчерпав их, демон называет файл в журнале по имени.
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

### Журнал: что писать в обращение, если индексация встала

Демон и сервер выдачи ведут журнал в каталоге `CODE_INDEX_HOME`:

| Файл | Кто пишет | Что внутри |
|------|-----------|------------|
| `daemon.log` | демон индексации | этапы обхода и разбора, итоги по каждой папке, пакеты изменений, пульс |
| `serve.log` | сервер выдачи | старт, порт, перечитка конфига, ошибки обработки запросов |

Прежние файлы при переполнении сдвигаются: `daemon.log.1`, `.2`, `.3` (предел
одного файла — 10 МБ).

**Уровень подробности.** `[daemon] log_level` в `daemon.toml` (`error`, `warn`,
`info` по умолчанию, `debug`, `trace`). Переменная `RUST_LOG` сильнее конфига —
удобно поднять подробность разово, не трогая файл. На `debug` добавляются
сообщения по отдельным файлам (не прочитан, не разобран, не записан); на `info`
их нет намеренно — на сломанной выгрузке они топят всё остальное.

**Как выглядит полный проход:**

```
[…\repo] новая база — режим хранилища: в оперативной памяти (настройка «auto», память демона 11 МБ)
[…\repo] начата первичная индексация
[этап 1] отбор кандидатов: 49 мс (543 файлов)
[этап 2] начат разбор 543 файлов в несколько потоков
[этап 2] разбор: 40 мс (543 файлов)
[этап 3] запись в базу: 86 мс (543 файлов)
[этап 4] индексы и полнотекстовый поиск: 34 мс
[…\repo] итог первичной индексации: 543 файлов, 232 мс,
         режим хранилища в оперативной памяти со сбросом на диск, память демона 22 МБ
```

**Как выглядит обработка изменений:**

```
[…\repo] пакет изменений: событий 3
[…\repo] надстройка обновлена точечно за 26 мс (изменено 3, удалено 0)
[…\repo] пакет обработан: событий 3, за 44 мс, режим хранилища на диске, память демона 23 МБ
```

**Пульс** пишется раз в минуту и отвечает на вопрос «процесс жив или встал»:

```
[пульс] работает 1 мин 0 с, память демона 778 МБ, папок 50: готовы 50, индексируются 0, ошибок 0, не начаты 0
[пульс] …\repo — первичная индексация, 1200/90000 файлов (1.3%), без изменений 1 ч 1 мин
```

Готовые папки поимённо не перечисляются; неготовых показывается не больше
десяти плюс строка об остатке. Строка «без изменений» — главный признак
зависания: если она растёт, а этап в журнале не меняется, встали именно на
названном этапе.

**Что приложить к обращению:** `daemon.log` (при необходимости и прежние
`daemon.log.1…3`), вывод `code-index daemon status --json`, содержимое
`daemon.toml`. Учтите: пока демон работает, проводник показывает у журнала
нулевой размер — файл всё это время пишется, читать нужно содержимое.

## Конфигурация

Файл `.code-index/config.json` создаётся автоматически при первом запуске:

```json
{
  "exclude_dirs": ["node_modules", ".venv", "__pycache__", ".git", "target", "output"],
  "extra_text_extensions": [],
  "max_file_size": 1048576,
  "max_files": 0,
  "bulk_threshold": 10,
  "languages": ["python", "javascript", "typescript", "java", "rust", "go", "bsl", "php", "c", "cpp", "csharp", "ruby", "swift", "html"],
  "batch_size": 500,
  "storage_mode": "auto",
  "memory_max_percent": 50,
  "memory_estimate_factor": 2.0,
  "chunk_budget_bytes": 536870912,
  "debounce_ms": 1500,
  "batch_ms": 2000
}
```

Ключевые поля:

- `storage_mode` — режим хранения: `auto` (по расчёту, см. ниже), `memory` (всегда в оперативной памяти), `disk` (всегда на диске)
- `memory_max_percent` — какую долю СВОБОДНОЙ памяти разрешено занять под базу в режиме `auto` (по умолчанию 50 %)
- `memory_estimate_factor` — во сколько раз работа в памяти дороже веса исходников (по умолчанию 2.0)
- `chunk_budget_bytes` — сколько байт исходников идёт в один заход полного разбора (по умолчанию 512 МБ); ноль отключает деление на порции

**Как работает `auto`.** Перед индексацией папка взвешивается: складывается вес файлов, которые пойдут в индекс (обход только сведений о файлах, 1–3 секунды на 60 тысяч файлов). Дальше — расчёт:

```text
ожидаемый расход = вес исходников × memory_estimate_factor
разрешено занять = свободная память × memory_max_percent / 100

ожидаемый расход ≤ разрешено занять → база собирается в памяти, потом сбрасывается на диск
иначе                               → база сразу на диске
```

Свободная память читается заново на каждую папку, а не один раз при старте. После работы в памяти освобождённое возвращается системе, поэтому следующая папка получает её обратно. Оба числа расчёта, множитель и принятое решение пишутся в журнал.

**Разбор идёт порциями (v0.68.0).** Пик расхода памяти задаётся не размером папки, а настройкой `chunk_budget_bytes` (по умолчанию 512 МБ). Файлы читаются, разбираются, сжимаются и пишутся в базу порциями такого объёма, и память каждой порции освобождается до начала следующей. Прежде всё содержимое папки и его разбор держались в памяти одновременно, поэтому на большой папке расход упирался в потолок машины или контейнера.

Замер на конфигурации 1С из 57 072 файлов (база на диске, первичная индексация с нуля):

| Бюджет порции | Пик памяти | Время ядра |
|---|---:|---:|
| деление отключено (как раньше) | 4,40 ГБ | 49,4 с |
| 512 МБ (умолчание) | 1,42 ГБ | 52,2 с |

На принудительном полном разборе того же репозитория бюджет в 128 МБ снижает пик до 0,67 ГБ против 4,46 ГБ без деления.

Границы порций стоят несколько процентов времени: однопоточная запись в базу не перекрывается разбором следующей порции.

Настройка задаётся и в файле настроек папки, и глобально в `daemon.toml`:

```toml
[indexer]
chunk_budget_bytes = 134217728   # 128 МБ — для контейнера с жёстким лимитом памяти
```

Ноль отключает деление: всё дерево читается разом, как до появления настройки. Деление работает там, где в разбор идёт всё дерево — первичная индексация, принудительный полный разбор и продолжение прерванной загрузки; на обычных правках файлов мало, и оно ничего не меняет.

**Продолжение прерванной загрузки.** Если процесс убили посреди первичной индексации (нехватка памяти, перезагрузка машины), записанные порции остаются в базе, а вместе с ними — отметка о незавершённой загрузке. Следующий запуск видит её, дочитывает только недостающие файлы и достраивает индексы, вместо того чтобы начинать с нуля. Пока отметка стоит, база не считается готовой: индексы и полнотекстовый поиск на ней ещё не построены.

**Когда менять `memory_estimate_factor`.** Умолчание 2.0 — середина наблюдаемого разброса, а не запас сверху. Замеры первичной индексации с базой в памяти:

| Вес исходников | Файлов | Израсходовано памяти | Множитель по факту |
|---:|---:|---:|---:|
| 1,4 ГБ | 191 417 | 2,4 ГБ | 1,7 |
| 3,7 ГБ | 60 294 | 4,4 ГБ | 1,2 |
| 5,3 ГБ | 92 300 | 8,8 ГБ | 1,6 |

До появления порционного разбора те же папки давали от 1,7 до 3,2, и умолчание было 3.0. Разброс зависит от языка, плотности кода в файлах и состава папки, поэтому предсказать множитель заранее нельзя — его подбирают по журналу, где записаны и оценка, и принятое решение.

| Ситуация | Значение | Что получится |
|---|---:|---|
| Памяти мало, машина слабая | 3.0 и выше | Чаще диск: медленнее, но предсказуемо, без риска нехватки. На слабых машинах это направление и есть основное |
| Обычная машина | 2.0 | Умолчание |
| Памяти заведомо много, есть готовность сверяться с журналом | 1.3–1.7 | Больше папок пойдёт в память, индексация быстрее. Риск реальный: расход выше оценки встречается не реже, чем ниже, и при нехватке памяти работа с папкой прервётся |

Ноль, отрицательное или нечисловое значение считается опиской — берётся умолчание, и в журнале видно, с каким множителем считали на самом деле.
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
   path = "C:/Repo1C-2"
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

### Защита выдачи от disk-offload (`[cap]`, v0.38.0)

Клиент (`claude` CLI / Claude Code) держит лимит на один `tool_result`, вливаемый inline в контекст (`MAX_MCP_OUTPUT_TOKENS` ≈ 25 000 токенов). Ответ сверх лимита harness сбрасывает в файл на диск, отдавая модели только путь + preview — структурный inline-доступ теряется. Чтобы крупные выдачи (карта большого модуля, длинные массивы значений/источников/реквизитов) не уходили в offload, `serve` режет их в источнике. Настраивается опциональной секцией `[cap]` в `daemon.toml`:

```toml
[cap]
max_response_bytes      = 48000   # бюджет ответа в байтах JSON; 0 — выключить cap_response
max_response_bytes_hard = 192000  # потолок клиентского max_response_bytes; 0/нет — 4× бюджета (v0.49.0)
cap_enabled             = true    # глобальный выключатель cap_response (приоритетнее cap_tools)
cap_tools               = ["get_event_subscriptions", "bsl_sql", "find_references", "get_register_writers"]
max_function_body_chars = 15000   # порог тела get_function/get_class; 0 — тело всегда целиком
```

Механизмы (все опциональны, действуют на serve-слое выдачи, переиндексации не требуют):

- **`cap_response`** — пока JSON ответа превышает `max_response_bytes`, усекает самый тяжёлый массив вдвое, оставляя `<ключ>_total` (исходное число) и `<ключ>_truncated: true`. Применяется к инструментам из `cap_tools` (при `cap_enabled = true`). Усекаются только массивы — большие строки (`read_file`/`grep`) не трогаются. Сюда же подключён `get_file_summary` (core): карта гигантского модуля (сотни функций) больше не уходит в offload.
- **`omit_oversize_sections`** (для `get_object_structure`) — где массив/мапа = полный авторитетный ответ (структура объекта 1С), тяжёлая секция выкидывается ЦЕЛИКОМ с `<секция>_omitted: true` + `<секция>_count: N` (частичный сэмпл соврал бы «вот все значения перечисления»).
- **`shrink_search_results`** (v0.49.0, для ответа-ПОИСКА `get_object_structure` по `name_like`/`full_names`) — опознание найденного неприкосновенно. Сначала снимаются тяжёлые секции ВНУТРИ элементов (`<секция>_omitted` + `<секция>_count`), затем, если этого мало, укорачивается сам массив с `results_total`/`results_shown`; массив не обнуляется ни при каком бюджете больше нуля, `full_name`/`meta_type`/`name`/`synonym` остаются у каждого элемента. Раньше самой тяжёлой секцией был сам массив `results`, и он вылетал первым же шагом — клиент получал `{matched: 38, results_omitted: true}` без единого имени.
- **`max_response_bytes` как параметр вызова** (v0.49.0, `get_object_structure`) — бюджет для одного ответа поверх серверного. Запрос сверх `max_response_bytes_hard` зажимается до потолка, применённое значение возвращается в `response_budget_applied`. Ноль (снять страж) на вызов не разрешён — только конфигом сервера.
- **Навигационный кап тела** (`get_function`/`get_class`) — тело длиннее `max_function_body_chars` отдаётся стабом голова+хвост+маркер+hint на `read_file(line_start,line_end)` / `grep_body`.

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
`get_event_subscriptions`, `find_path_bsl`, `search_terms`, `get_data_links`,
`find_data_path`), парсер XML-выгрузки, граф вызовов BSL-процедур,
граф связей данных, опциональное LLM-обогащение через OpenAI-совместимый
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

## Формат ответов MCP-tools (v0.9.0+)

Все data-инструменты возвращают унифицированный JSON:

```json
{
  "result": <prev plain payload>,
  "_meta": {
    "dependent_files": ["src/X.bsl", "src/Y.bsl"],
    "file_mtimes": { "src/X.bsl": 1717689600, "src/Y.bsl": 1717689600 }
  }
}
```

`_meta.dependent_files` — список файлов, на которых построен ответ; `_meta.file_mtimes` (с 0.20.0) сопоставляет каждому из них индексный mtime (unix-секунды). **С 0.42.0 serve срезает `_meta` из ответа клиенту сам** — модель всегда получает только `result`. Теперь `_meta` — внутренний сигнал: у serve есть **встроенный кэш результатов** (кросс-сессионный, только локальные репо) с **по-файловой** свежестью. При изменении файла демон шлёт `POST /mark-dirty {repo, files:[{path, mtime}]}` (файлы изменены на диске), после commit — `POST /invalidate {repo, file_paths}`. Ответ не кэшируется/не отдаётся из кэша **только** если хоть один его файл-источник «грязный» (mtime на диске новее индексного из `_meta.file_mtimes`); инвалидация по файлу сносит лишь ключи кэша, зависящие от изменённого файла (обратный индекс «файл→ключи»). Запросы про не тронутые файлы не страдают — без огрубления на весь репо. Канал подключается строкой `[[cache_targets]]` в `daemon.toml`. Раньше эти же поля использовал парный кэширующий прокси **`mcp-cache-ci`**; со встроенным кэшем serve отдельный прокси для ci-цепочки больше не обязателен. Наблюдаемость: `GET /cache-stats` на serve.

**Сессионный отсев повторной выдачи (0.48.0).** Строки, уже отданные в этой сессии, повторно не отдаются: опущенное помечается полем `rows_elided_already_delivered` и подсказкой, поэтому укороченный или пустой список никогда не читается как «данных нет» молча — изначально пустой результат пометки не получает, и два случая различимы. Область отсева — «инструмент + репозиторий», так что одинаковые строки из разных баз не гасят друг друга. Выключается `dedup_enabled = false` в секции `[mcp]` файла `daemon.toml`; память сбрасывается `POST /dedup-reset` — нужно, когда клиент сжал свой контекст: изнутри MCP это событие не наблюдаемо.

Без обёртки идут диагностические `health`, `get_stats`, `stat_file` — формат не менялся.

## Лицензия

MIT. См. [LICENSE](LICENSE).

## Благодарности

- [tree-sitter](https://tree-sitter.github.io/) — инкрементальный парсер для множества языков
- [tree-sitter-bsl](https://github.com/1c-syntax/tree-sitter-bsl) — грамматика BSL от сообщества 1c-syntax
- [rusqlite](https://github.com/rusqlite/rusqlite) — SQLite для Rust
- [rayon](https://github.com/rayon-rs/rayon) — параллелизм данных без лишних усилий
- [rmcp](https://github.com/modelcontextprotocol/rust-sdk) — Rust MCP SDK
