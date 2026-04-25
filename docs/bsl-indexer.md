# bsl-indexer — code-index с поддержкой 1С:Предприятие

`bsl-indexer` — приватная сборка [code-index](../README_RU.md), которая дополнительно умеет:

* парсить XML-выгрузку конфигурации 1С (`Configuration.xml`, `Forms/*.xml`, `EventSubscriptions/*.xml`);
* строить граф вызовов BSL-процедур с тремя типами рёбер (`direct`, `subscription`, `form_event`);
* отдавать через MCP четыре 1С-специфичных tool'а: `get_object_structure`, `get_form_handlers`, `get_event_subscriptions`, `find_path`;
* (опционально, под cargo feature `enrichment`) — обогащать процедуры бизнес-терминами через LLM и искать по ним FTS5-методом через MCP-tool `search_terms`.

Если 1С вам не нужна — берите `code-index`, не тащите этот вариант. Если 1С — нужна, выбор между ними сводится к одной строке в `main.rs`: bsl-indexer регистрирует `BslLanguageProcessor` помимо обычных Python/Rust/Go/.../bsl процессоров.

---

## Сборка

```bash
# Базовая сборка — без LLM, без сетевых зависимостей.
cargo build --release -p bsl-indexer

# С LLM-обогащением: тащит reqwest, открывает доступ к
# подкоманде `bsl-indexer enrich` и активирует search_terms.
cargo build --release -p bsl-indexer --features enrichment
```

Без feature `enrichment` подкоманда `bsl-indexer enrich` доступна, но возвращает понятную ошибку «соберите с `--features enrichment`». MCP-tool `search_terms` доступен в обеих сборках — он не зависит от reqwest, просто отдаёт пустой массив, если в репо ещё ни одна процедура не обогащена.

---

## Базовая индексация

Команды совпадают с публичным `code-index`:

```bash
bsl-indexer index /srv/repos/ut             # одноразовая индексация
bsl-indexer daemon run                       # фоновая индексация по daemon.toml
bsl-indexer serve --transport http --port 8011 \
                  --config /etc/code-index/daemon.toml
bsl-indexer search_function ОбработкаПроведения --path /srv/repos/ut
```

При `bsl-indexer index <path>` для репо с `Configuration.xml` в корне автоматически срабатывают:

1. `BslLanguageProcessor::detects()` — авто-определение языка `bsl`.
2. `apply_schema_extensions` — создаёт таблицы `metadata_objects`, `metadata_forms`, `event_subscriptions`, `proc_call_graph`, `procedure_enrichment`, FTS-индекс на termах, `embedding_meta`.
3. `index_extras` — парсит XML и заполняет таблицы метаданных + строит граф вызовов.

Дальше любой MCP-tool (`get_object_structure`, `find_path` и т.д.) уже доступен — даже без enrichment.

---

## LLM-обогащение (опционально, feature `enrichment`)

Идея: для большинства запросов вида «найди процедуру про скидки/проведение/склад» FTS на именах функций недостаточно — имена в 1С часто формальные (`ОбработкаПроведения`, `ПриЗаписи`). Решение — пройтись LLM по каждой процедуре, попросить вернуть 5-15 ключевых бизнес-терминов, сохранить их в отдельной FTS5-таблице. После этого MCP-tool `search_terms` ищет процедуры по бизнес-смыслу, оффлайн, за миллисекунды.

### 1. Минимальная конфигурация в `daemon.toml`

```toml
[enrichment]
enabled = true
url = "https://openrouter.ai/api/v1/chat/completions"
model = "anthropic/claude-haiku-4.5"
api_key_env = "OPENROUTER_API_KEY"
batch_size = 20
# prompt_template — опционально, при отсутствии используется default:
# "Опиши в 2-3 предложениях, что делает эта 1С-процедура и какие
#  бизнес-термины она задействует. Верни только список ключевых слов
#  и фраз через запятую, без пояснений и нумерации."
```

`provider` сейчас единственный — `openai_compatible` (POST `/v1/chat/completions` с messages-форматом). Любой совместимый endpoint подходит:

| Сценарий | url | model | api_key_env |
|---|---|---|---|
| **OpenRouter (cloud)** | `https://openrouter.ai/api/v1/chat/completions` | `anthropic/claude-haiku-4.5`, `meta-llama/llama-3.3-70b-instruct`, `qwen/qwen-2.5-72b-instruct` и др. | `OPENROUTER_API_KEY` |
| **OpenAI** | `https://api.openai.com/v1/chat/completions` | `gpt-4o-mini`, `gpt-5-mini` | `OPENAI_API_KEY` |
| **Anthropic-нативный** | поддерживается через OpenRouter (нативный API использует другой формат). | — | — |
| **Ollama локально** (CPU/GPU) | `http://127.0.0.1:11434/v1/chat/completions` | `qwen2.5:7b`, `llama3.2:3b`, любая `ollama pull`-модель | пусто (не нужен) |

Локально через CPU реалистичен только GPU-вариант: на 313k процедур УТ-масштаба CPU занимается несколько недель. Через OpenRouter Haiku — порядка $5–25 за полный прогон, ~2–4 часа, 50 параллельных запросов.

### 2. Прогон обогащения

```bash
# Установите ключ один раз в окружение, бинарник прочитает по api_key_env:
export OPENROUTER_API_KEY=sk-or-v1-xxxxxxxxx

# Smoke на 5 процедурах (рекомендуется ВСЕГДА перед большим прогоном):
bsl-indexer enrich --path /srv/repos/ut --limit 5

# Полный прогон по всем необогащённым процедурам:
bsl-indexer enrich --path /srv/repos/ut

# Принудительно переобогатить — например, после смены модели:
bsl-indexer enrich --path /srv/repos/ut --reenrich
```

Команда печатает в конце сводку:

```
enrichment: attempted=5, written=4, empty=1, failed=0
```

`written` — сколько процедур успешно записано в `procedure_enrichment`. `empty` — модель вернула пустой ответ (типично для совсем коротких процедур-болванок); `failed` — HTTP-ошибки или неразобранный JSON.

### 3. Подпись модели и защита от рассинхрона

При первом успешном прогоне `bsl-indexer` пишет в `embedding_meta.enrichment_signature` отпечаток `<provider>:<model>` (например, `openai_compatible:anthropic/claude-haiku-4.5`). На последующих прогонах:

* подпись совпадает → ничего не делаем, лишь добавляем недостающие записи;
* подпись отличается → warning в лог:

  ```
  WARN enrichment_signature в БД (openai_compatible:claude-haiku-4.5)
       != конфиг (openai_compatible:gpt-5-mini). Старые termы остаются и
       будут смешиваться с новыми; запустите `bsl-indexer enrich --reenrich`
       для пересборки.
  ```

  Старт **НЕ** валится — данные остаются полезными, просто `search_terms` будет смешивать стили формулировок двух моделей. Если важна однородность — `--reenrich` перезапишет всё под новую подпись.

### 4. Поиск через MCP-tool `search_terms`

Любой MCP-клиент (Claude Code, LibreChat и др.), подключённый к `bsl-indexer serve`, увидит инструмент `search_terms` в `tools/list` (он зарегистрирован, как и остальные BSL-tools, при наличии `language = "bsl"` хотя бы в одном `[[paths]]`).

Параметры:

* `repo` (string, required) — алиас репозитория из `--path` или `daemon.toml`;
* `query` (string, required) — FTS5-выражение: `"скидки"`, `"товары AND склад"`, `'"приём заказа"'`, `"провед*"`;
* `limit` (integer, optional, default=20) — максимум результатов.

Ответ:

```json
{
  "query": "товары AND склад",
  "results": [
    {
      "proc_key": "Documents/РеализацияТоваровУслуг/.../Module.bsl::ОбработкаПроведения",
      "terms": "товары, склад, проведение, реализация, остатки",
      "signature": "openai_compatible:anthropic/claude-haiku-4.5",
      "score": -8.2
    }
  ]
}
```

`score` — BM25 от FTS5 (меньше = лучше). Используйте для сортировки поверх клиентских фильтров.

### 5. Ограничения 5a (важно знать)

* **Один embedder-channel канал на репо** — все процедуры обогащаются одной моделью; смесь моделей внутри одного индекса не поддерживается. Это намеренно (карточка 261, раздел «Гибрид моделей и автоматический fallback — ОТКАЗ»): векторы/термы разных моделей лежат в разных пространствах, fallback дал бы обманчиво похожий на правду мусор.
* **`proc_key` — `<file_path>::<function_name>`** — стабильный в пределах одного индекса. При перемещении файла enrichment устаревает; запустите `--reenrich` для пострадавших путей либо `enrich` без флага и оно автоматически добавит новые.
* **Процедуры >16K char усекаются** при отправке в LLM (защита от context-length). Для большинства процедур 1С это не проблема — крупные модули с многократно дублированными секциями обогащаются по началу, что обычно содержит репрезентативную лексику. Полный summary длинных процедур — отдельный задел этапа 5b.
* **Семантический поиск через embeddings (`semantic_search`) — этап 5b**. Сейчас единственный «семантический» канал — FTS на termах (`search_terms`). Это намеренно «минимально-достаточная» версия: для 90% запросов 1С её хватает, embedder поднимать не обязательно.

---

## Совместимость артефактов индекса

Базовая сборка bsl-indexer и сборка `--features enrichment` пишут в одну и ту же `.code-index/index.db` — таблица `procedure_enrichment` есть в обеих, просто без feature её никто не заполняет. Если у вас на VM RAG живёт `bsl-indexer --features enrichment`, а на локальном Windows — базовая сборка, оба корректно открывают одну и ту же БД, и оба корректно обслуживают `search_terms` через MCP.

---

## Ограничения MCP-клиентов (важно знать)

Сервер `bsl-indexer serve` использует **conditional registration** — 1С-инструменты появляются в `tools/list` только когда хотя бы у одного `[[paths]]` в `daemon.toml` указано `language = "bsl"`. При правке конфига сервер автоматически:

1. Подхватывает изменение через file-watch (debounce 500мс).
2. Пересобирает множество активных языков.
3. Отправляет клиенту уведомление `notifications/tools/list_changed` (часть протокола MCP).

**Эмпирически (на 2026-04-26, Claude Code 2.1.120):** клиент это уведомление **игнорирует** — список инструментов не обновляется до явного `/mcp reconnect`. Это известная проблема Claude Code ([upstream issue #13646](https://github.com/anthropics/claude-code/issues/13646)), не нашего сервера.

**Что делать пользователю:**

* После правки `daemon.toml` (добавление/удаление репо, смена `language`) — **сделайте `/mcp` → `code-index` (или `bsl-indexer`) → `Reconnect`** в Claude Code. После reconnect свежий состав инструментов виден сразу.
* Если только что подключили MCP-сервер впервые — список собирается **корректно с первого `tools/list`**, никаких действий не требуется.
* `tools/list_changed` мы продолжаем отправлять — другие клиенты (LibreChat, OpenWebUI) могут его обрабатывать; у Claude Code дойдут руки в будущей версии — всё заработает без правок с нашей стороны.

Полное описание теста и таблица результатов — в [bsl-indexer-architecture.md, раздел «Поведение клиентов на tools/list_changed»](bsl-indexer-architecture.md).

---

## Связанные документы

* [README_RU.md](../README_RU.md) — общий обзор code-index (универсальная часть).
* [bsl-indexer-architecture.md](bsl-indexer-architecture.md) — полное ТЗ workspace-refactor + дизайн-решения по 5 этапам.
