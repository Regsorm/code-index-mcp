# code-index / bsl-indexer — операции администрирования

> Операторские процедуры индексатора. Вынесены из `~/.claude/rules/code-index.md`,
> чтобы не грузить их в каждую сессию (нужны редко). Поведенческие правила работы
> с инструментами — в самом `code-index.md`. Управление процессами под супервизором —
> `~/.claude/rules/mcp-supervisor.md`.

## Добавление нового репо в индекс — ПРАВКА ДВУХ ФАЙЛОВ

При добавлении нового репо ОБЯЗАТЕЛЬНО править оба конфига — иначе либо индексация не пойдёт, либо MCP-клиент не узнает про alias:

1. **`C:/tools/code-index/daemon.toml`** — что индексировать (даёт daemon знать путь, alias, язык):
   ```toml
   [[paths]]
   path = "C:/<repo-path>"
   alias = "<alias>"
   language = "javascript"  # или другой
   ```

2. **`C:/tools/code-index/serve.toml`** — какие алиасы отдавать MCP-клиентам через federation routing:
   ```toml
   [[paths]]
   alias = "<alias>"
   ip = "<win-host>"
   port = 8013
   ```

**После правки** — рестарт обоих процессов `bsl-indexer.exe`:
- `schtasks /End /TN CodeIndexDaemon && schtasks /Run /TN CodeIndexDaemon` поднимает оба (`daemon run` + `serve --port 8013`).
- Дополнительно `Stop-Process -Id <mcp-cache-ci-pid> -Force` (на 8011) — иначе у клиентов остаются stale MCP sessions; supervisor его респавнит за пару секунд.

**Симптомы пропуска `serve.toml`:** daemon индексирует, SQLite в `<repo>/.code-index/` собирается, `get_stats(repo=None)` через прямой curl на daemon-port даёт `status: ready` — но `mcp__code-index__get_stats(repo="<alias>")` отвечает `"Неизвестный repo '<alias>'"`, потому что serve его не зарегистрировал.

Этот пункт зафиксирован после реального инцидента 2026-05-14 при добавлении `librechat-src` (правил только daemon.toml). Карточка feedback `#1179` в rag-query.

## ⚡ HOT-RELOAD DAEMON CONFIG — НИКАКОГО РЕСТАРТА НЕ НУЖНО

**ПОСЛЕ ПРАВКИ `daemon.toml` (добавил новый репо, поменял языки, изменил лимиты) — НЕ ПЕРЕЗАПУСКАЙ DAEMON ЧЕРЕЗ `schtasks /End && /Run` И НЕ УБИВАЙ ПО PID. У DAEMON ЕСТЬ HOT-RELOAD CONFIG:**

```bash
"C:/tools/code-index/bsl-indexer.exe" daemon reload
```

Эта команда делает `POST http://127.0.0.1:<http_port>/reload` на живой daemon (порт берётся из `daemon.json`). Конфиг перечитывается **за миллисекунды**, без kill, без spawn, без stale PID-lock, без потерянных child-handle. Все ранее проиндексированные репо остаются ready, новые попадают в очередь индексации.

**Полный CLI daemon'a (`bsl-indexer.exe daemon --help`):**
- `daemon run` — foreground запуск (для Scheduled Task / systemd, не для ручного вызова)
- `daemon status` — `GET /health` живого daemon
- **`daemon reload` — `POST /reload`, перечитать `daemon.toml` без рестарта ← ИСПОЛЬЗОВАТЬ ЭТО**
- `daemon stop` — `POST /stop`, корректное завершение

`schtasks /End /TN CodeIndexDaemon` нужен **только** при необходимости полного рестарта (например, после `cargo build --release` пересборки бинарника). В сценарии «добавил/убрал репо» — **только `daemon reload`**.

Скилл `/setup-project` использует именно этот путь (см. `setup_project.py::step_restart()`).

## Если MCP не отвечает

1. Проверить что процессы живы:
   ```bash
   tasklist //FI "IMAGENAME eq code-index.exe"
   ```
   Должно быть минимум 2: daemon и serve (MCP).

2. Проверить порт MCP:
   ```bash
   curl -v http://127.0.0.1:8011/mcp
   ```
   На streamable HTTP вернётся что-то структурированное, не connection refused.

3. Health daemon (порт из `daemon.json`):
   ```bash
   curl http://127.0.0.1:$(python -c "import json; print(json.load(open('C:/tools/code-index/daemon.json'))['http_port'])")/health
   ```

4. Логи: `C:/tools/code-index/daemon.log`.

## Обновление tools/list при изменении backend (новые базы 1С, новые tools)

**Проблема.** Все три инстанса универсального бинарника `mcp-cache-ci` (локальный `127.0.0.1:8011` перед `code-index serve`, `mcp-cache-1c` на ВМ rag `:8010` перед `1c-router`, `mcp-cache-rag` на ВМ rag `:8019` перед `rag-query`) **кэшируют ответ `tools/list`**. Когда в backend появляется новый tool ИЛИ меняется JSONSchema существующего (например, расширяется enum `base` в `mcp__1c__execute_query` после добавления новой базы в `bases.json` 1c-router) — клиенты MCP **продолжают видеть старую схему** через кэш-прокси, даже после рестарта самого backend.

Дополнительно: **MCP-клиент Claude Code** (VSCode-расширение, headless `claude -p`) читает `tools/list` ровно один раз при инициализации сессии и держит схему в памяти. После того как кэш-прокси обновился, клиент всё равно показывает старый enum — нужно перезапустить и его.

**Канонический симптом** (зафиксирован 2026-05-15 при добавлении `tdk-bp-shadow` в 1c-router):

- На стороне backend (`http://127.0.0.1:8014/mcp/`, прямой запрос к 1c-router) `tools/list` возвращает свежий enum со всеми базами.
- На стороне кэш-прокси (`http://127.0.0.1:8010/mcp/`, mcp-cache-1c) — старый enum без новой базы.
- В Claude Code вызов `mcp__1c__execute_query(base="новая-база", ...)` падает с `InputValidationError: ... must be one of [...]` ещё до отправки на сервер (валидация enum на стороне MCP-клиента).

**Корректная процедура обновления — три шага в строгом порядке:**

1. **Рестарт backend-сервера** (тот, кто реально пересоздаёт tools/list-схему):
   - Для 1c-router (после правки `bases.json`): `ssh rag@<vm-rag> "cd /home/rag/docker-mcp && docker compose restart 1c-router"`
   - Для rag-query (после нового tool): `ssh rag@<vm-rag> "cd /home/rag/docker-mcp && docker compose restart rag-query"`
   - Для code-index serve: рестарт `bsl-indexer.exe serve` (через mcp-supervisor: `Stop-Process -Id <pid>` + supervisor поднимет за пару секунд; либо `schtasks /End /TN CodeIndexDaemon && schtasks /Run /TN CodeIndexDaemon` — поднимет и daemon, и serve).

2. **Рестарт соответствующего mcp-cache-***: иначе он будет отдавать клиентам старый кэш `tools/list`:
   - `mcp-cache-1c` (ВМ rag, перед 1c-router): `ssh rag@<vm-rag> "cd /home/rag/docker-mcp && docker compose restart mcp-cache-1c"`
   - `mcp-cache-rag` (ВМ rag, перед rag-query): `ssh rag@<vm-rag> "cd /home/rag/docker-mcp && docker compose restart mcp-cache-rag"`
   - локальный `mcp-cache-ci` (перед code-index): `Stop-Process -Name mcp-cache-ci -Force` (mcp-supervisor поднимет за ~3 сек).

3. **Перезагрузить MCP-клиент Claude Code**: VSCode-расширение перечитает `tools/list` при инициализации новой сессии. Способы:
   - В VSCode-расширении: закрыть и снова открыть проект (или Reload Window).
   - В headless: следующий вызов `claude -p` уже стартует с новой схемой (новая сессия = новый init).

**Верификация после каждого шага** — без пересборки контекста сразу видно, обновилось ли:

```bash
# Backend (порт 8014 — 1c-router): отдаёт ли свежий enum?
ssh rag@<vm-rag> "curl -sS -X POST -H 'Content-Type: application/json' \
  -H 'Accept: application/json,text/event-stream' \
  -d '{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{},\"clientInfo\":{\"name\":\"x\",\"version\":\"1\"}}}' \
  http://127.0.0.1:8014/mcp/ -D /tmp/h.txt -o /dev/null && \
  SID=\$(awk '/mcp-session-id/{print \$2}' /tmp/h.txt | tr -d '\r') && \
  curl -sS -X POST -H \"mcp-session-id: \$SID\" -H 'Accept: application/json,text/event-stream' \
       -H 'Content-Type: application/json' \
       -d '{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}' \
       http://127.0.0.1:8014/mcp/ > /dev/null && \
  curl -sS -X POST -H \"mcp-session-id: \$SID\" -H 'Accept: application/json,text/event-stream' \
       -H 'Content-Type: application/json' \
       -d '{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}' \
       http://127.0.0.1:8014/mcp/ | sed -n 's/^data: //p'"

# Cache-прокси (порт 8010 — mcp-cache-1c, stateless с 0.3.0): без session-id
ssh rag@<vm-rag> "curl -sS -X POST -H 'Content-Type: application/json' \
  -H 'Accept: application/json,text/event-stream' \
  -d '{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}' \
  http://127.0.0.1:8010/mcp/"
```

Сравнить enum — должны совпадать.

**Альтернатива без рестарта прокси** — заголовок `X-Cache-Bypass: 1`:

```bash
curl -H "X-Cache-Bypass: 1" -X POST ... http://127.0.0.1:8010/mcp/
```

Работает только для разовых curl-проверок. **MCP-клиент Claude Code этот заголовок не проставляет** — для боевого подхвата новой схемы клиентом всё равно нужен рестарт прокси + перезагрузка клиента.

**Антипаттерны:**

- ❌ Рестарт только backend без рестарта кэш-прокси — клиенты получают старую схему через кэш.
- ❌ Рестарт только кэш-прокси без перезагрузки Claude Code — клиент держит схему в своей памяти, прокси-кэш не помогает.
- ❌ Ожидание «само пройдёт по TTL» — TTL `tools/list` в `cache_policy_*.toml` обычно длинный (часы), не дождёшься.
- ❌ Попытка обхода через `X-Cache-Bypass` для MCP-сессии Claude Code — клиент его не шлёт.

## Управление индексом

Бинарник: `C:/tools/code-index/code-index.exe` (и резерв `C:/MCP-Servers/code-index/target/release/code-index.exe`).

```bash
# Список режимов:
C:/tools/code-index/code-index.exe --help

# Статус daemon:
cat C:/tools/code-index/daemon.json

# Разовая переиндексация пути:
C:/tools/code-index/code-index.exe index C:/RepoUT
```

**Добавить новый репо:** отредактировать `C:/tools/code-index/daemon.toml` → добавить `[[paths]]` с `path` и опциональным `alias` → перезапустить daemon и serve (см. раздел «Добавление нового репо» выше).

## Ребилд индексатора

При пересборке бинарника (`cargo build --release`) — если изменилась схема SQLite или логика парсера, после рестарта daemon сделает полную переиндексацию всех путей.

**Порядок:**
1. Остановить daemon: `C:/tools/code-index/daemon-ctl.bat stop` (или `kill` из `daemon.pid`)
2. Остановить MCP serve (процесс на порту 8011) — через mcp-supervisor либо `tasklist | grep code-index` + `taskkill`
3. `cd C:/MCP-Servers/code-index && cargo build --release`
4. Скопировать новый бинарник в `C:/tools/code-index/code-index.exe`
5. Запустить daemon и serve обратно
6. Дождаться `ready` по всем путям через health

**WAL-контроль** после большого ребилда (сначала убедиться, что daemon и serve остановлены):
```bash
sqlite3 C:/<repo>/.code-index/index.db "PRAGMA wal_checkpoint(TRUNCATE);"
```

**Если MCP при запросе возвращает `{"status": "indexing"}`** — daemon сейчас применяет батч. Подождать или проверить health. Не интерпретировать как ошибку MCP.

## Лимиты на размер code-файла (oversize)

Конфигурация в `daemon.toml` — два уровня переопределения:
```toml
[indexer]
max_code_file_size_bytes = 5242880   # глобальный default 5 МБ

[[paths]]
path = "C:/RepoUT"
max_code_file_size_bytes = 10485760  # per-repo override (10 МБ)
```
Приоритет: per-path → `[indexer]` → hardcoded **5 МБ**. Файлы крупнее лимита получают `oversize=1`, `content_blob=NULL`. AST/FTS/граф вызовов работают как обычно. `read_file` отдаёт `oversize=true` + `hint`. `grep_code` пропускает.
