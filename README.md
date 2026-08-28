<a href="https://infostart.ru/1c/tools/2677918/" title="Публикация на Инфостарте">
  <img src="https://infostart.ru/bitrix/templates/sandbox_empty/assets/tpl/abo/img/logo.svg" alt="Infostart" height="32">
</a>

---

# code-index-mcp

[![Релиз](https://img.shields.io/github/v/release/Regsorm/code-index-mcp)](https://github.com/Regsorm/code-index-mcp/releases/latest)
[![npm](https://img.shields.io/npm/v/%40regsorm%2Fcode-index-mcp)](https://www.npmjs.com/package/@regsorm/code-index-mcp)
[![Лицензия](https://img.shields.io/github/license/Regsorm/code-index-mcp)](LICENSE)

**Поиск по коду для ИИ-агентов. Один бинарник, индекс в SQLite, ответ за миллисекунды.
Разбирает выгрузки 1С:Предприятие 8.3 — и из Конфигуратора, и из 1С:EDT.**

[Полное руководство](README_RU.md) · [English](README_EN.md) · [Документация](docs/) · [Журнал изменений](CHANGELOG.md)

---

## Установка

### Windows — одной командой

```powershell
irm https://raw.githubusercontent.com/Regsorm/code-index-mcp/main/install.ps1 | iex
```

Скачивает последний выпуск в `C:\tools\code-index`, запоминает папку в
переменной окружения, создаёт заготовку файла настроек и печатает готовый блок
для `.mcp.json`.

С параметрами — папка установки, папка с исходниками, автозапуск при входе в
систему:

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/Regsorm/code-index-mcp/main/install.ps1))) `
    -InstallDir 'D:\code-index' -Repo 'main=D:\Repo1C' -RegisterAutostart
```

Автозапуск идёт через папку автозагрузки пользователя: прав администратора не
требует, окон не показывает. Остальные параметры — `-Flavor core` для сборки
без 1С, `-Version 1.0.0` для конкретного выпуска, `-Port` и `-DaemonPort`, если
порты по умолчанию заняты. Полный список — `Get-Help .\install.ps1 -Detailed`.

### Windows — вручную, готовый архив из выпуска

Скачивает последний выпуск, распаковывает в `C:\tools\code-index` и запоминает
эту папку в переменной окружения:

```powershell
$dst = 'C:\tools\code-index'
New-Item -ItemType Directory -Force $dst | Out-Null
$url = (Invoke-RestMethod https://api.github.com/repos/Regsorm/code-index-mcp/releases/latest).assets |
       Where-Object name -eq 'bsl-indexer-windows-x64.zip' |
       Select-Object -ExpandProperty browser_download_url
Invoke-WebRequest $url -OutFile "$env:TEMP\code-index.zip"
Expand-Archive "$env:TEMP\code-index.zip" -DestinationPath $dst -Force
setx CODE_INDEX_HOME $dst
```

`bsl-indexer` — сборка с поддержкой 1С (32 инструмента). Нужна работа без 1С —
возьмите `code-index-windows-x64.zip` (20 инструментов). Для Linux и macOS в том
же выпуске лежат `*-linux-x64.tar.gz` и `*-macos-arm64.tar.gz`.

### npm

```bash
npm install -g @regsorm/code-index-mcp
npx @regsorm/code-index-mcp serve --path /путь/к/репозиторию
```

Шаг `postinstall` скачивает готовый бинарник под вашу платформу — ничего не
компилируется. Пакет есть и в [реестре MCP](https://registry.modelcontextprotocol.io/)
под именем `io.github.Regsorm/code-index`. В обёртке только сборка без 1С.

### Сборка из исходников

```bash
git clone https://github.com/Regsorm/code-index-mcp.git
cd code-index-mcp
cargo build --release -p code-index                          # без 1С
cargo build --release -p bsl-indexer --features enrichment   # с поддержкой 1С
```

Нужен Rust 1.77+.

## Подключение к клиенту

Общий процесс по HTTP — один индекс на все сессии и все проекты:

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

Отдельный процесс на сессию (`stdio`), без фонового демона:

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

Работает с Claude Code, Cursor, VS Code, LibreChat — с любым клиентом MCP.
Настройка фонового демона, список репозиториев, тонкие параметры —
[полное руководство](README_RU.md#настройка-фонового-демона-v05).

## Зачем это нужно

Языковая модель без индекса ищет по коду тем же способом, что и человек без среды
разработки: перебором. Один вопрос «кто вызывает эту процедуру» превращается в
десяток последовательных обходов файлов, каждый из которых читает тысячи файлов и
возвращает в контекст модели куски текста — вместе с оплатой этих кусков.

`code-index` делает работу заранее: разбирает исходники в синтаксическое дерево,
складывает символы, тела, вызовы и метаданные в SQLite и отдаёт агенту готовый
ответ по протоколу MCP. Модель получает три строки вместо трёх файлов.

## Цифры

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/bench-summary-dark.svg">
  <img alt="В среднем в 2,04 раза дешевле по токенам, в 1,78 раза быстрее, 51 % экономии за сеанс"
       src="docs/bench-summary-light.svg" width="880">
</picture>

| Кодовая база | Файлов | Полная индексация | Запуск на готовом индексе |
|---|---:|---:|---:|
| 1С:Управление Торговлей | 57 072 | 2 мин 41 с | **2,3 с** |
| 1С:Бухгалтерия предприятия | 88 284 | 5 мин 51 с | **5,3 с** |
| сайт на PHP | 157 772 | 13 мин 1 с | **8,0 с** |

**Полная индексация** — разбор всего с нуля, делается один раз. У Бухгалтерии эти
5 мин 51 с складываются так: 2 мин 38 с ядро (синтаксические деревья и запись),
1 мин 58 с надстройка 1С (метаданные, формы, права, граф вызовов), 56 с сброс
базы на диск. Дальше правки подхватываются по одной, за миллисекунды.

**Запуск на готовом индексе** сверяет время правки и размер каждого файла — ни
одного чтения содержимого, ни одного хеша.

| | |
|---|---|
| Ответ на запрос | около 10 мс по HTTP, повторный из кэша — доли миллисекунды |
| Размер бинарника | 39 МБ без 1С, 41 МБ с 1С |
| Функций в индексе Управления Торговлей | 261 548 |
| Вызовов в графе там же | 1 962 941 |

Замеры сделаны на одной машине под Windows с обычным жёстким диском, август 2026.

## Инструменты

Каждый вызов принимает алиас репозитория, так что один сервер обслуживает
несколько кодовых баз сразу — в том числе с других машин.

**Поиск и навигация**

| | |
|---|---|
| `search_function` `search_class` | полнотекстовый поиск по функциям и классам |
| `get_function` `get_class` | точное имя → готовое тело; при промахе подсказывает похожие имена |
| `find_symbol` | символ любого рода: функция, класс, переменная, импорт |
| `get_imports` | импорты модуля или файла |
| `get_file_summary` | карта файла без чтения исходника |

**Граф вызовов**

| | |
|---|---|
| `get_callers` `get_callees` | кто вызывает процедуру и кого вызывает она |
| `find_path` | кратчайшая цепочка вызовов между двумя функциями |
| `get_call_tree` | дерево вызовов вниз или вверх на заданную глубину |

**Содержимое файлов**

| | |
|---|---|
| `read_file` | чтение с диапазоном строк; содержимое кода лежит в индексе, сжатое zstd |
| `list_files` `stat_file` | список файлов по маске и метаданные одного файла |
| `grep_body` | подстрока или регулярное выражение в телах функций и классов |
| `grep_code` `grep_text` | то же по всему тексту файлов кода и текстовых файлов |
| `search_text` | полнотекстовый поиск по текстовым форматам |
| `get_stats` `health` | состояние индекса и сервера |

**Для конфигураций 1С** (сборка `bsl-indexer`, появляются сами при наличии
репозитория с выгрузкой)

| | |
|---|---|
| `get_object_structure` | реквизиты с типами и синонимами, табличные части, измерения и ресурсы, предопределённые элементы, свойства проведения |
| `get_object_profile` | паспорт объекта одним вызовом: структура, формы, модули, связи |
| `get_form_handlers` | обработчики событий управляемой формы, с привязкой к элементу |
| `get_event_subscriptions` | подписки на события с фильтрами по источнику и событию |
| `get_data_links` `find_data_path` | граф связей данных: кто на кого ссылается и цепочка между двумя объектами |
| `find_references` | карта влияния: ссылки из метаданных, обращения в коде, права ролей |
| `get_register_writers` | регистраторы регистра и движения документа |
| `find_path_bsl` | цепочка вызовов процедур по графу выгрузки |
| `search_terms` | смысловой поиск процедур по именам, синонимам и комментариям |
| `bsl_sql` | произвольный запрос на чтение к таблицам метаданных и графов |

## Что даёт поддержка 1С

- Разбираются обе формы выгрузки: XML из Конфигуратора и `.mdo` из 1С:EDT.
- Граф связей данных строится по ссылочным типам реквизитов, измерений и
  табличных частей: для Бухгалтерии предприятия это 65 421 ребро.
- Из модулей извлекаются директивы компиляции (`&НаСервере`, `&НаКлиенте`) и
  аннотации расширений (`&Вместо`, `&После`, `&Перед`) вместе с именем
  переопределяемой процедуры.
- Понимаются оба синтаксиса BSL — русский и английский.
- Регистр имени объекта не важен: кириллические имена приводятся к записи из
  конфигурации, в ответе имя показывается канонически.

Подробности — [docs/bsl-indexer.md](docs/bsl-indexer.md).

## Языки

Полный разбор синтаксиса: Python, JavaScript, TypeScript, Java, Rust, Go, PHP, C,
C++, C#, Ruby, Swift, 1С (BSL), HTML. Метаданные 1С — XML Конфигуратора и `.mdo`
из EDT. Плюс полнотекстовая индексация 50+ текстовых форматов (`.md`, `.json`,
`.yaml`, `.toml`, `.sql` и других).

## Дальше

- [Полное руководство на русском](README_RU.md) — демон, конфигурация, CLI, архитектура
- [docs/operations.md](docs/operations.md) — эксплуатация: перезапуск, добавление репозиториев, диагностика
- [docs/bsl-indexer.md](docs/bsl-indexer.md) — сборка для 1С
- [CHANGELOG.md](CHANGELOG.md) — журнал изменений

## Участие

Ошибки и предложения — в [issues](https://github.com/Regsorm/code-index-mcp/issues),
вопросы и обсуждения — в [Discussions](https://github.com/Regsorm/code-index-mcp/discussions).
Как прислать правку — [CONTRIBUTING.md](CONTRIBUTING.md), об уязвимостях —
[SECURITY.md](SECURITY.md).

## Лицензия

MIT, см. [LICENSE](LICENSE).

Проект стоит на [tree-sitter](https://tree-sitter.github.io/),
[грамматике BSL от сообщества 1c-syntax](https://github.com/1c-syntax/tree-sitter-bsl),
[rusqlite](https://github.com/rusqlite/rusqlite), [rayon](https://github.com/rayon-rs/rayon)
и [Rust SDK для MCP](https://github.com/modelcontextprotocol/rust-sdk).
