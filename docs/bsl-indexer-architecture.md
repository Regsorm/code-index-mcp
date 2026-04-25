# Техническое задание: bsl-indexer (Rust workspace + conditional MCP tools)

**Дата:** 2026-04-25
**Статус:** ТЗ для доработки. Не для слияния в основную ветку code-index до проверки работоспособности.

---

## Контекст и мотивация

В нашей инфраструктуре уже работает `bsl-agent` на VM RAG (192.0.2.20) — RAG-агент для поиска по 1С-коду на основе PostgreSQL + pgvector + ReAct-цикла с 14 инструментами. Под ним стоит индексатор `pg_indexer.py` (Python), который полностью переиндексирует ~313k процедур 1С за 2-5 часов.

Мы хотим:
1. Заменить `pg_indexer.py` на Rust-индексатор (скорость 10-50x).
2. Перейти с PostgreSQL на SQLite + sqlite-vec для BSL-индекса (PostgreSQL остаётся только для knowledge cards, см. отдельные карточки rag-query [259]).
3. Сохранить универсальность code-index — он должен оставаться универсальным индексатором для любых языков.
4. Реализовать 1С-специфичный функционал (XML-разбор метаданных, граф вызовов с 5 типами рёбер, MCP-tools `get_object_structure`, `get_form_handlers`, `get_event_subscriptions`) **отдельно**, не размывая core.

Замеры производительности и архитектурное обоснование см. карточки в rag-query: [259] (план миграции), [258] (GitLab креды), [252] (удаление bsl-platform-context).

## Цель

Превратить текущий моно-крейт `code-index` в **Cargo Workspace** из нескольких crate, где:
- Универсальное ядро остаётся публичным.
- 1С-специфика выносится в отдельный приватный crate.
- На VM RAG в проде запускается приватный binary `bsl-indexer` (= core + bsl-extension), на любой другой машине — публичный `code-index` (только core).
- MCP tools регистрируются **conditionally** в зависимости от того, какие репо подключены (есть ли BSL-репо в `daemon.toml`).

## Текущее состояние (что есть в репо сейчас)

Корень: [C:/MCP-Servers/code-index/](file:///C:/MCP-Servers/code-index/)

- [Cargo.toml](file:///C:/MCP-Servers/code-index/Cargo.toml) — single-crate проект
- [src/](file:///C:/MCP-Servers/code-index/src/) — весь код в одном крейте
- [README.md](file:///C:/MCP-Servers/code-index/README.md), [README_RU.md](file:///C:/MCP-Servers/code-index/README_RU.md) — публичная документация
- Бинарник: `code-index.exe` (один)
- Версия: 0.5.0-rc6 на 2026-04-25

Текущие MCP-инструменты (все универсальные, language-agnostic):
`search_function`, `search_class`, `get_function`, `get_class`, `find_symbol`, `get_callers`, `get_callees`, `get_imports`, `get_file_summary`, `search_text`, `grep_body`, `get_stats`, `health`.

Конфиг подключённых репо: `C:/tools/code-index/daemon.toml` или `[путь к daemon-toml на конкретном хосте]`. Сейчас у `[[paths]]` есть только `path` и опциональный `alias`. Нужно расширить.

## Архитектурное решение

### Cargo Workspace из 4 crate

```
code-index/                          ← workspace root
├── Cargo.toml                       ← [workspace] members = ["crates/*"]
├── crates/
│   ├── code-index-core/            ← lib (публичная)
│   ├── code-index/                  ← bin (публичный, opensource-ready)
│   ├── bsl-extension/               ← lib (приватная, 1С-специфика)
│   └── bsl-indexer/                 ← bin (приватный, использует core + bsl-extension)
└── docs/
    └── bsl-indexer-architecture.md  ← этот файл
```

### Что в каждом crate

#### `code-index-core` (публичный lib)

Общее ядро — переезжает существующий код из текущего `src/`:
- File scanner (walkdir, gitignore, language detection by extension)
- Tree-sitter integration: парсеры Python, Rust, Java, Go, JS/TS, BSL и т.д.
- Change tracking: file_hash, proc_hash, mtime; таблица `files_main` в SQLite
- SQLite schema base: `procedures`, `functions`, `classes`, `imports`, `calls`
- MCP base server (HTTP streamable, JSON-RPC, conditional tool registration)
- Client для embed-провайдеров (HTTP к OpenAI-compatible /v1/embeddings)
- Конфиг-парсер `daemon.toml`

**Trait-API для расширений:**

```rust
pub trait LanguageProcessor: Send + Sync {
    fn name(&self) -> &str;                       // "bsl", "python", ...
    fn detects(&self, repo_root: &Path) -> bool;   // auto-detect
    fn parse_file(&self, path: &Path, content: &str) -> Vec<Procedure>;
    fn extract_call_graph(&self, ast: &Tree) -> Vec<CallEdge>;
    fn schema_extensions(&self) -> Vec<&'static str>; // SQL CREATE TABLE для специфики
    fn additional_tools(&self) -> Vec<Box<dyn IndexTool>>; // MCP-tools
}

pub trait IndexTool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;        // "search procedures... For BSL/1C only"
    fn input_schema(&self) -> serde_json::Value;
    fn applicable_languages(&self) -> Option<Vec<&str>>;  // None = универсальный
    fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<serde_json::Value>;
}
```

#### `code-index` (публичный bin)

Тонкий wrapper вокруг `code-index-core`:
```rust
fn main() {
    let mut app = code_index_core::App::new();
    // Регистрируем универсальные процессоры
    app.register_processor(Box::new(PythonProcessor::new()));
    app.register_processor(Box::new(RustProcessor::new()));
    app.register_processor(Box::new(JavaProcessor::new()));
    // ...
    // BSL processor НЕ регистрируется здесь
    app.run(); // CLI: index, serve, etc.
}
```

Это и есть текущий code-index, только без 1С. Для opensource — выкладываем именно его.

#### `bsl-extension` (приватный lib)

1С-специфика:
- `BslLanguageProcessor` — реализует `LanguageProcessor`, использует tree-sitter-bsl
- XML-парсеры: `Configuration.xml`, `Forms/*.xml`, `EventSubscriptions/*.xml`, `CommonModules/*/Ext/Module.bsl`
- Call graph builder с 5 типами рёбер: `direct`, `subscription`, `form_event`, `extension_override`, `external_assignment`
- Дополнительные SQLite таблицы (миграции встроены): `metadata_objects`, `metadata_forms`, `event_subscriptions`, `proc_call_graph`, `extensions`, `attribute_overrides`, `extension_overrides`, `override_chains`
- MCP tools: `get_object_structure`, `get_form_handlers`, `get_event_subscriptions`, `find_path`
- Embed-стратегия hybrid: jina-v3 локально для процедур ≤ 30K знаков, summary через OpenRouter chat-модель → embed jina-v3 для процедур > 30K (≈280 штук, ~$1.25 разово)
- **Чанки не используются** — 99% процедур и так одночанковые, см. карточку [259]

Не публикуется в opensource.

#### `bsl-indexer` (приватный bin)

```rust
fn main() {
    let mut app = code_index_core::App::new();
    app.register_processor(Box::new(PythonProcessor::new()));   // если нужно
    app.register_processor(Box::new(RustProcessor::new()));     // если нужно
    app.register_processor(Box::new(bsl_extension::BslLanguageProcessor::new()));
    app.run();
}
```

Используется в проде на VM RAG. **Один процесс** обслуживает все репо смешанных языков.

### Расширение конфига `daemon.toml`

Добавить поле `language` к `[[paths]]`. Поле опционально на уровне TOML, но
после первого старта демона оно будет заполнено для всех записей — либо
руками оператором, либо auto-detect'ом самого демона.

```toml
[[paths]]
path = "/srv/repos/ut"
alias = "ut"
language = "bsl"        # явное указание оператором

[[paths]]
path = "/srv/repos/myproject"
alias = "myproject"
# language не указан — демон определит сам и допишет это поле в TOML
```

#### Auto-detect (один раз, с записью результата обратно в TOML)

Назначение auto-detect — **миграция уже работающих установок code-index**.
У существующих пользователей `daemon.toml` ещё не содержит `language`;
после обновления на новую версию они не должны терять работоспособность
и не должны быть вынуждены вручную править конфиг.

Алгоритм при старте демона:

1. Для каждого `[[paths]]` без `language`:
   * Простая эвристика по корню репо:
     1. `Configuration.xml` → `bsl`
     2. `pyproject.toml` или `setup.py` → `python`
     3. `Cargo.toml` → `rust`
     4. `package.json` → `javascript` (или `typescript`, если ещё и `tsconfig.json`)
     5. иначе по преобладанию расширений: `.bsl/.os` → `bsl`, `.py` → `python`, ...
   * Если эвристика не определила язык — `tracing::warn!` в лог
     и репо **пропускается** до явного указания оператором (никакого
     молчаливого фолбэка на «угадай»).
2. Определённое значение **записывается обратно в `daemon.toml`** через
   `toml_edit` — такая правка сохраняет комментарии и порядок ключей,
   в отличие от round-trip через `toml::Value`.
3. После записи демон сравнивает множество активных языков **до** и
   **после** auto-detect. Если в результате появился новый язык
   (например, добавилось `bsl`, которого раньше не было):
   * MCP-сервер шлёт всем подключённым клиентам JSON-RPC notification
     `notifications/tools/list_changed`;
   * параллельно `tracing::warn!` в лог с явным указанием:
     `Обнаружены новые активные языки: [bsl]. Если клиент не реагирует
     на tools/list_changed — переподключите его (/mcp в Claude Code или
     рестарт VS Code / LibreChat).`

#### File-watch на сам `daemon.toml`

Демон и MCP-сервер подписываются на `notify`-события файла `daemon.toml`
(не только на исходники в `[[paths]]`). При изменении файла:

* демон перечитывает конфиг, перестраивает реестр репо (стартует
  индексацию для новых, останавливает для удалённых);
* MCP-сервер перечитывает конфиг, пересобирает множество активных языков,
  и если оно изменилось — шлёт `notifications/tools/list_changed`.

Команда `daemon reload` остаётся доступной как явный fallback (например,
если правка пришла через атомарный rename, который notify пропустил).

#### Зависимости

Для записи обратно в TOML с сохранением форматирования — `toml_edit = "0.22"`
(можно держать рядом с `toml`, либо полностью перейти на него).

## Conditional registration MCP tools — главный экспериментальный момент

Это **ключевая часть ТЗ**, которую нужно проверить на практике из-за сомнений по поводу того, как Claude Code и LibreChat обрабатывают `tools/list`.

### Логика регистрации

При старте `bsl-indexer`:

1. Читает `daemon.toml`, для каждого `[[paths]]` определяет `language` (явно или auto-detect).
2. Собирает множество языков: `languages = {"bsl", "python", "rust"}` (например).
3. Для каждого зарегистрированного `LanguageProcessor`:
   - Если `processor.name() in languages` — собирает дополнительные tools через `processor.additional_tools()`.
4. Финальный список tools = универсальные tools (всегда) + tools от активных processor'ов.
5. MCP `tools/list` возвращает этот итоговый список.

**Пример** для `daemon.toml` с 4 BSL и 5 не-BSL репо:
- Универсальные: `search_function`, `search_class`, `find_symbol`, `get_callers`, `get_callees`, `grep_body`, `get_file_summary`, `search_text`, `get_stats`, `health` → ВСЕГДА
- BSL-специфичные: `get_object_structure`, `get_form_handlers`, `get_event_subscriptions`, `find_path` → потому что **есть BSL-репо**

Если BSL-репо нет — 1С-tools не появляются в `tools/list`.

### Гарантии при misuse

Каждый tool валидирует совместимость c repo при выполнении:

```rust
impl IndexTool for GetObjectStructureTool {
    fn applicable_languages(&self) -> Option<Vec<&str>> {
        Some(vec!["bsl"])
    }
    fn execute(&self, args: Value, ctx: &ToolContext) -> Result<Value> {
        let repo = args["repo"].as_str().ok_or_else(|| ...)?;
        let repo_lang = ctx.config.repo_language(repo);
        if repo_lang != "bsl" {
            return Err(McpError::structured(
                "language_mismatch",
                format!("tool '{}' requires language='bsl', but repo '{}' has language='{}'",
                        self.name(), repo, repo_lang)
            ));
        }
        // ... основная логика
    }
}
```

LLM получает структурированную ошибку и понимает что инструмент не подходит. В `description` каждого 1С-tool явно указано: `"For BSL/1C repositories only"`.

### Что нужно ПРОВЕРИТЬ при первом запуске

1. **Claude Code**: подключаемся к `bsl-indexer:8011/mcp`, выполняем `/mcp`, смотрим что в tools/list ожидаемое количество. Меняем daemon.toml (убираем все BSL-репо), рестартуем bsl-indexer, проверяем что 1С-tools исчезли из tools/list **после переподключения** клиента.

2. **Динамическое обновление**: если возможно — отправить notification `notifications/tools/list_changed` после правки daemon.toml без рестарта. Это часть MCP-протокола, но не все клиенты её обрабатывают. Если Claude Code не реагирует — приемлемо, требовать рестарта клиента.

3. **LibreChat**: подключаемся, проверяем что tools видны в агенте «БСЛ-Эксперт». Делаем тест с misuse (1С-tool на не-BSL репо) — должна прийти структурированная ошибка.

4. **Корректность заявленной информации**: при `tools/list` каждый tool имеет правильный `description`, `inputSchema`. Это критично для способности LLM правильно выбирать инструменты.

## Enrichment и эмбеддинги (этап 5, финальный дизайн)

Обе фичи — **только в `bsl-extension`**, не в core. Универсальный `code-index` ни про enrichment, ни про embeddings ничего не знает: для Python/Rust/Go код достаточно описательный, чтобы FTS на именах и комментариях покрывал семантический поиск. Бизнес-доменность (1С: товары/склады/проведения), для которой нужно дополнять код описаниями, — специфика 1С.

### Принцип: три независимых тумблера

```toml
# daemon.toml — обе секции по умолчанию ОТСУТСТВУЮТ.
# Если их нет — enrichment и embeddings не используются вовсе.

[enrichment]
enabled = true
provider = "openai_compatible"
url = "https://openrouter.ai/api/v1/chat/completions"
model = "anthropic/claude-haiku-4.5"
api_key_env = "OPENROUTER_API_KEY"
batch_size = 20
prompt_template = "..."

[embeddings]
enabled = false
provider = "openai_compatible"
url = "http://127.0.0.1:11434/v1/embeddings"   # Ollama локально
model = "bge-m3"
expected_dim = 1024
```

* `enabled` независимы: enrichment может быть включён без embeddings и наоборот.
* Семантический поиск использует ту же embedding-модель что и при индексации (жёсткий инвариант, защищается через `embedding_signature`).
* Запрос пользователя при семантическом поиске **не обогащается** — идёт прямо в embedder.

### Разделение terms и embeddings

Это ключевое решение, к которому пришли в обсуждении 2026-04-25:

**embedding строится ТОЛЬКО на сыром коде процедуры**. enrichment_terms хранятся отдельно и не входят в embedding-вектор.

Зачем:
- Если бы terms подмешивались в embedding, рассинхрон состояний enrichment (какие-то процедуры обогащены, какие-то нет) приводил бы к **рассинхрону качества внутри одного индекса**: часть векторов в одном пространстве смыслов, часть в другом. Поиск стал бы непредсказуемым.
- При раздельном хранении embeddings стабильны и не зависят от того, прошёл enrichment до конца или нет. Включение/выключение enrichment не требует пересчёта embeddings.
- enrichment-терms используются через **FTS на отдельной колонке** — это самостоятельный канал поиска, оффлайн, миллисекунды, без обращения к embedder.

```sql
ALTER TABLE procedures ADD COLUMN enrichment_terms TEXT;
-- NULL по умолчанию для всех процедур.
-- Заполняется LLM-моделью при enrichment-проходе.

CREATE VIRTUAL TABLE fts_enrichment_terms USING fts5(
    enrichment_terms,
    content='procedures',
    content_rowid='id'
);

CREATE TABLE IF NOT EXISTS embedding_meta (
    key TEXT PRIMARY KEY,
    value TEXT
);
-- Ключи:
--   'embedding_signature' = 'openai_compatible:bge-m3:1024'
--   'enrichment_signature' = 'openai_compatible:claude-haiku-4.5'

CREATE VIRTUAL TABLE procedures_vec USING vec0(
    proc_key TEXT PRIMARY KEY,
    embedding float[?]      -- размерность из expected_dim
);
```

### Три канала семантического поиска

| Канал | MCP-tool | Когда работает |
|---|---|---|
| FTS по именам/коду/комментариям | `search_function`, `grep_body` (core) | всегда |
| FTS по бизнес-терминам | `search_terms` (bsl-ext) | если enrichment когда-либо прогонялся для процедур; работает оффлайн |
| Векторный по сходству кода | `semantic_search` (bsl-ext) | если embeddings включены и embedder доступен |

Пустые `enrichment_terms` (NULL) просто не находятся через `search_terms` — это **не баг**, а progressive enhancement: по мере прогона enrichment покрытие растёт.

### Подэтапы реализации

**5a — enrichment + FTS на terms** (без embeddings):
- ALTER TABLE на `procedures.enrichment_terms` + FTS5 virtual table.
- HTTP-клиент к chat-completions через cargo feature `enrichment`.
- Конфиг-секция `[enrichment]`, проверка `enrichment_signature`.
- Команда `bsl-indexer enrich` (запуск отдельно от индексации).
- MCP-tool `search_terms`.
- Это **минимально-достаточно для большинства юзкейсов 1С** — закрывает запросы вида «найди процедуру про скидки».

**5b — embeddings** (опционально, отдельный полноценный этап):
- `procedures_vec` через sqlite-vec.
- HTTP-клиент к embeddings через cargo feature `embeddings`.
- Конфиг-секция `[embeddings]`, проверка `embedding_signature`, отказ старта при несовпадении.
- LRU-кеш запросов (~1000 записей, ~4 МБ).
- MCP-tool `semantic_search`.
- Может быть пропущен, если выяснится что 5a достаточно.

### Карта моделей

**Открытые веса (можно поднять локально через Ollama):**
- `nomic-embed-text` (768 dim), `mxbai-embed-large` (1024), `bge-m3` (1024), `multilingual-e5-large` (1024), `jina-embeddings-v2-base` (768).

**Только облако (требуется интернет для embed-запросов на каждый поиск):**
- `voyage-3-large`, `voyage-code-3` (1024) — Voyage AI.
- `text-embedding-3-large` (3072) — OpenAI.
- `embed-english-v3` (1024) — Cohere.

Гибрид моделей и автоматический fallback **не реализуем** — векторы разных моделей лежат в разных пространствах, fallback дал бы обманчиво похожий на правду мусор. Сменили модель → отказ старта или явная команда `bsl-indexer reindex`.

### Стоимость и сценарии

| Сценарий | Объём | Время | Деньги |
|---|---|---|---|
| Полный enrichment УТ-масштаба | 313k процедур | ~2-4 ч (OpenRouter Haiku/Flash, 50 параллельных запросов) | $5-25 разово |
| Полный enrichment локально GPU | 313k | ~5-10 дней (Qwen2.5-7B на RTX 3060) | бесплатно |
| Полный enrichment локально CPU | 313k | нереально (~50 дней) | — |
| Полный embeddings локально (Ollama bge-m3) | 313k | ~3-6 ч | бесплатно |
| Полный embeddings облако | 313k | ~30 мин | ~$1-3 |
| Один семантический запрос | 1 | 5-30 мс локально / 100-500 мс облако | 0 / ~$0.0001 |

Дальше всё инкрементально — по `proc_hash_fast` пере-enrich/re-embed только изменённые.

### Контроль изменений

Через `proc_hash_fast` в core. При обновлении процедуры:
- enrichment_terms помечается stale (или зануляется).
- procedures_vec для этого proc_key обновляется при следующем проходе.

## Граф вызовов (для bsl-extension)

5 типов рёбер:
- `direct` (~1.6M) — прямые вызовы из кода, через tree-sitter AST
- `subscription` (~344) — подписки на события, из EventSubscriptions/*.xml
- `form_event` (~60k) — обработчики событий форм, из Forms/*.xml
- `extension_override` — перехваты в расширениях (CFE)
- `external_assignment` — внешние назначения (если применимо)

## Граф вызовов (для bsl-extension)

5 типов рёбер:
- `direct` (~1.6M) — прямые вызовы из кода, через tree-sitter AST
- `subscription` (~344) — подписки на события, из EventSubscriptions/*.xml
- `form_event` (~60k) — обработчики событий форм, из Forms/*.xml
- `extension_override` — перехваты в расширениях (CFE)
- `external_assignment` — внешние назначения (если применимо)

```sql
CREATE TABLE proc_call_graph (
    repo TEXT NOT NULL,
    caller_proc_key TEXT NOT NULL,
    callee_proc_name TEXT NOT NULL,         -- может быть resolved или нет
    callee_proc_key TEXT,                    -- NULL если resolution неуспешен
    call_type TEXT NOT NULL,                 -- direct/subscription/form_event/...
    UNIQUE(repo, caller_proc_key, callee_proc_name, call_type)
);
CREATE INDEX idx_pcg_caller ON proc_call_graph(repo, caller_proc_key);
CREATE INDEX idx_pcg_callee ON proc_call_graph(repo, callee_proc_name);
```

Скорость на Rust:
- Полная пересборка: 1-2 минуты (rayon parallel + serial bulk INSERT)
- Инкрементальная (1% изменилось): 3-5 секунд

`find_path` — recursive CTE по proc_call_graph, max_depth=3.

## Этапы реализации (рекомендованный порядок)

### Этап 0: Подготовка workspace

1. Создать ветку `workspace-refactor` от `master`.
2. Превратить `Cargo.toml` в workspace root (`[workspace] members = ["crates/*"]`).
3. Перенести существующий `src/` в `crates/code-index-core/src/` и `crates/code-index/src/main.rs`.
4. Убедиться что `cargo build --release` собирает рабочий `code-index` (smoke test против существующих репо).
5. Коммит на ветке.

### Этап 1: Trait-API для extensions

1. Определить trait'ы `LanguageProcessor`, `IndexTool`, `ToolContext` в `code-index-core`.
2. Перевести существующий MCP-сервер на conditional registration (по списку зарегистрированных processor'ов).
3. Добавить парсинг `language` в `daemon.toml` + auto-detect.
4. Smoke test: `code-index` без процессоров регистрирует только базовые tools (или, наоборот, без processor'ов вообще не работает — решить).

### Этап 2: bsl-extension скелет

1. Создать `crates/bsl-extension/` с пустыми реализациями trait'ов.
2. Создать `crates/bsl-indexer/src/main.rs` — регистрирует BslLanguageProcessor.
3. Smoke test: `bsl-indexer` запускается на тестовом репо с одним BSL-файлом, парсит, кладёт в SQLite.

### Этап 3: Разбор XML 1С

1. Парсер `Configuration.xml` → metadata_objects.
2. Парсер `Forms/*.xml` → metadata_forms.
3. Парсер `EventSubscriptions/*.xml` → event_subscriptions.
4. Тест на `RepoUT`: проверить что количество объектов сравнимо с тем что в текущем Postgres pg_indexer.

### Этап 4: Граф вызовов

1. Построение `direct` рёбер через tree-sitter AST.
2. Построение `subscription` и `form_event` рёбер из XML.
3. Тест: для известной процедуры (например, `ОбновитьКассу` если есть) сравнить `search_callers` против текущего bsl-agent — должны быть похожие результаты (точная эквивалентность не требуется, current pg_indexer тоже неидеален).

### Этап 5: Эмбеддинги

1. Клиент к jina-v3 (HTTP к llama-server на :8081 на VM RAG).
2. Клиент к OpenRouter (для summary процедур >30K знаков).
3. sqlite-vec интеграция через `sqlite-vec` crate.
4. `procedures_vec` таблица + полное заполнение.

### Этап 6: 1С MCP-tools

`get_object_structure`, `get_form_handlers`, `get_event_subscriptions`, `find_path`.

Each tool — реализация `IndexTool` trait в bsl-extension.

### Этап 7: Conditional registration верификация

**Главное что мы хотим проверить.** Тестовые сценарии:

1. `daemon.toml` с 1 BSL и 1 Python репо — bsl-indexer выдаёт core + 1С tools при `tools/list`.
2. `daemon.toml` только с Python репо — bsl-indexer выдаёт только core tools.
3. Misuse: `get_object_structure(repo="python_repo", ...)` — структурированная ошибка.
4. Подключаем bsl-indexer к Claude Code, к LibreChat — оба видят правильный набор tools.
5. (Опционально) Изменение конфига on-the-fly + `notifications/tools/list_changed` — проверить, реагируют ли клиенты.

### Этап 8: Drop-in replacement для pg_indexer

Когда bsl-indexer отрабатывает на тестовом репо корректно — параллельно запускаем его на VM RAG в **отдельной БД** (не трогаем существующую Postgres). Сравниваем результаты:

- Количество процедур
- Количество рёбер графа по типам
- Sample-проверка: 10 случайных вопросов через bsl-agent, оба бэкенда — сравнить ответы.

Если bsl-indexer корректен — переключаем bsl-agent на SQLite, останавливаем pg_indexer.py.

## Что НЕ делаем сейчас

- Не выкладываем bsl-extension в публичный GitHub. Приватный crate, остаётся на машинах разработчика.
- Не трогаем `bsl-mcp` (BSL Language Server) — это другая система (статический анализ генерируемого BSL), к bsl-indexer она не относится.
- Не трогаем cnotedb — у него своя `multilingual-e5-large` модель, своя SQLite-база, свой scope.
- Не реализуем `enrich` (terms-обогащение) — отложено как отдельная фича. Колонка `terms` в `procedures` не создаётся в новой схеме (можно добавить позже миграцией).
- Не трогаем `agent_runs/queries/candidates/traces` — текущий bsl-agent пишет в Postgres, при переключении на SQLite-схему нужно перенести и эти таблицы. Отдельный sub-этап.
- Не трогаем publication code-index. Все изменения в ветке `workspace-refactor`. Слить в master — только после успешной верификации conditional registration.

## Решения по архитектурным вопросам (закрыты 2026-04-25)

| Вопрос | Решение |
|---|---|
| Tree-sitter-BSL crate | Не вопрос: `tree-sitter-onescript = "0.1"` уже подключён и работает в [src/parser/bsl.rs](file:///C:/MCP-Servers/code-index/src/parser/bsl.rs). При переезде в `bsl-extension` зависимость переедет туда. |
| Auto-detect cache | **Не нужен.** Результат пишется обратно в сам `daemon.toml` через `toml_edit`. Один источник истины — конфиг. |
| `notifications/tools/list_changed` | **Реализуем со стороны сервера** (отправка по сохранённому peer при rebuild active set). Поведение клиентов **не гарантируем** — см. блок «Поведение клиентов» ниже. |
| File-watch на `daemon.toml` | **Реализуем** в индексирующем демоне и в MCP-сервере. При сохранении файла демон/сервер перечитывают конфиг автоматически. |
| Если auto-detect не определил язык | warning в лог, репо пропускается до явного указания оператором. Никакого молчаливого фолбэка. |
| Множество активных языков при `tools/list` | Собирается на старте сервера и при file-watch / `daemon reload`. `tools/list` отдаёт базовые универсальные tools + tools от каждого `LanguageProcessor`, чьё `name()` входит в множество. |

### Поведение клиентов на `tools/list_changed` — эмпирически проверено

Сервер отправляет `notifications/tools/list_changed` корректно (это часть
протокола MCP). На этапе 7 мы прогнали живой тест на текущей версии
Claude Code, чтобы зафиксировать конкретное поведение клиента — без
этого решения о пользовательском флоу принимать наугад нельзя.

#### Тест 2026-04-26 (Claude Code 2.1.120, протокол MCP `2025-11-25`)

Setup: запущен `bsl-indexer serve --transport http --port 8911 --config daemon-with-bsl.toml`.
В TOML — два репо (`bsl`, `python`). Claude Code подключён к серверу через
`.mcp.json`, протокол согласован на `2025-11-25` (клиент сам поднял с
объявленного нами `2024-11-05`).

Симметричный двусторонний тест:

| Сценарий | `tools/list` без reconnect | `tools/list` после reconnect |
|---|---|---|
| Удалили BSL-репо из TOML, file-watch отправил `tools/list_changed` | **18** (старое значение, BSL остались) | **13** (BSL пропали) |
| Вернули BSL-репо обратно, file-watch снова отправил `tools/list_changed` | **13** (старое значение, BSL не вернулись) | **18** (BSL появились) |

Сервер при каждой правке корректно логировал отправку:

```
INFO config_watch: перечитан daemon.toml, активные языки: ["python"]
INFO Состав активных языков изменился: ["bsl","python"] → ["python"]. Отправляю tools/list_changed.
```

Вывод: **уведомление `notifications/tools/list_changed` Claude Code 2.1.120
игнорирует**. Это совпадает с расследованием в upstream issue
[anthropics/claude-code#13646](https://github.com/anthropics/claude-code/issues/13646)
— в minified `cli.js` определена Zod-схема для парсинга уведомления, но
`setNotificationHandler` для него не зарегистрирован, обработчик возвращается
сразу.

#### Что это значит для пользователя

* После правки `daemon.toml` (добавление/удаление репо, смена `language`)
  для появления/пропажи 1С-tools в Claude Code **нужен ручной
  `/mcp reconnect`** для нашего сервера. Через MCP UI → `code-index`
  (или `bsl-indexer`) → кнопка `Reconnect`.
* `/mcp reconnect` в Claude Code 2.1.120 **корректно перечитывает**
  `tools/list` (issue [#33779](https://github.com/anthropics/claude-code/issues/33779)
  здесь не воспроизвёлся — после reconnect клиент видит свежий список).
* Уведомление мы **продолжаем отправлять** — оно ничего не ломает, и если
  Anthropic в будущей версии добавит handler, всё заработает без правок
  с нашей стороны.

#### Для LibreChat / OpenWebUI / других клиентов

Поведение **не проверяли**. Если будете подключать к этим клиентам —
повторите сценарий теста 2026-04-26 и допишите результат сюда. Пока
гипотеза (по аналогии с Claude Code): `tools/list_changed` могут не
обрабатывать → workaround «переподключить вручную» универсален.

## Остающиеся открытые вопросы

1. **agent_runs migration**: при переключении bsl-agent на SQLite нужна миграция данных. Если жалко — настроить bsl-agent читать оба источника на переходный период. (Этап 8.)

## Файлы для проверки в текущем code-index

При реализации полезно посмотреть:

- [src/](file:///C:/MCP-Servers/code-index/src/) — текущая структура. Понять где file scanner, где tree-sitter integration, где MCP-сервер.
- [Cargo.toml](file:///C:/MCP-Servers/code-index/Cargo.toml) — текущие зависимости. Большинство переходит в `code-index-core`.
- [README.md](file:///C:/MCP-Servers/code-index/README.md) — публичная документация. Обновится при выпуске workspace-версии.
- [docs/](file:///C:/MCP-Servers/code-index/docs/) — текущая внутренняя документация (полезно для понимания архитектуры).

## Связанные карточки в памяти

- rag-query [259] — общий план миграции MCP-стека на VM RAG, рефакторинг bsl-agent-индексера. Включает обоснование sqlite-vec vs pgvector, hybrid embed strategy, отказ от чанков.
- rag-query [258] — креды GitLab oshisha.tech (для git pull репо 1С на VM RAG).
- rag-query [252] — bsl-platform-context удалён (не путать с bsl-extension в этом ТЗ — это разные системы).
- claude-note-db: знания про cnotedb embed-архитектуру (sqlite-vec, multilingual-e5-large, embed_server pattern на :8079).

## Поверхностный sanity-check перед стартом

```bash
cd C:\MCP-Servers\code-index

# 1. Что собирается прямо сейчас?
cargo build --release

# 2. Какая текущая версия?
git log --oneline -5

# 3. Какие зависимости?
cat Cargo.toml | head -50

# 4. Есть ли тесты?
cargo test --release
```

Дальше — приступать к Этапу 0 (workspace skeleton).

---

**Конец ТЗ.** Файл предназначен для следующей сессии Claude Code, открытой в директории `C:\MCP-Servers\code-index`. При работе следовать этапам последовательно, верифицировать каждый перед переходом к следующему.
