# bsl-indexer в Docker

Развёртывание `bsl-indexer` (структурный поиск по коду, MCP по HTTP) в одном
контейнере: демон-писатель + read-only `serve`. Индексирует примонтированные
репозитории и отдаёт MCP на `http://localhost:9003/mcp`.

Альтернатива [systemd-деплою](../systemd/bsl-indexer-daemon.service) — когда
удобнее контейнер (изоляция toolchain, воспроизводимая сборка), а не установка
бинарника на хост.

## Архитектура контейнера

Один контейнер, два процесса (общий loopback и `$CODE_INDEX_HOME`):

- **демон** (`bsl-indexer daemon run`) — единственный писатель: делает начальную
  индексацию путей из `daemon.toml` и держит `<repo>/.code-index/index.db`
  актуальным при изменениях; пишет свой рантайм-порт в `$CODE_INDEX_HOME/daemon.json`.
- **serve** (`bsl-indexer serve --transport http`) — read-only MCP: отдаёт `/mcp`,
  читает те же `index.db`. Находит демон через `daemon.json`.

Оркестрация — [`entrypoint.sh`](entrypoint.sh) (очистка stale-состояния → демон в
фоне → ожидание `daemon.json` → serve).

### Почему entrypoint чистит `daemon.pid`/`daemon.json` при старте

Демон защищён PID-файлом (`daemon.pid`) и снимает его RAII-очисткой при штатном
завершении. Но при `SIGKILL` (например, `docker stop` по таймауту или падение)
очистка не срабатывает — PID-файл остаётся на именованном томе. У нового
контейнера **свежий PID-namespace**, где PID из прошлого инстанса (часто `8`)
почти наверняка занят другим процессом; проверка живости даёт ложное «демон уже
запущен» → демон падает → `restart: unless-stopped` → **краш-луп** (порт 9003 не
слушается). В контейнере ни один прошлый демон дожить не может, поэтому entrypoint
безусловно удаляет `daemon.pid`/`daemon.json` при старте — это разрывает цикл.

## Запуск

1. Отредактируйте [`daemon.toml`](daemon.toml) под свои репозитории (пути —
   внутри контейнера, в `/repos`).
2. Укажите каталог с репозиториями через `REPOS_DIR` (или `.env`) — он монтируется
   в `/repos`:

   ```bash
   REPOS_DIR=/path/to/repos \
     docker compose -f deploy/docker/docker-compose.yml up -d --build
   ```

   По умолчанию монтируется `deploy/docker/repos`.

Первый билд компилирует Rust (~5–10 мин). Начальная индексация идёт в фоне уже
после старта; пока индекс не готов, инструменты возвращают `{status, progress}`
вместо ошибки.

## Проверка

```bash
docker compose -f deploy/docker/docker-compose.yml logs -f bsl-indexer   # логи
docker compose -f deploy/docker/docker-compose.yml exec bsl-indexer \
  bsl-indexer daemon status --json                                       # статус индексации
curl -s http://localhost:9003/mcp -H 'Accept: text/event-stream'         # эндпоинт MCP
```

## Подключение к MCP-клиенту

```json
"code-index": { "type": "http", "url": "http://localhost:9003/mcp" }
```

## Файлы

- [`Dockerfile`](Dockerfile) — multi-stage сборка (`rust:1-slim-bookworm` + gcc →
  `debian:bookworm-slim`), бинарник `bsl-indexer` без feature `enrichment`.
- [`docker-compose.yml`](docker-compose.yml) — сервис `bsl-indexer` (build
  context = корень репо), том с репозиториями (`REPOS_DIR`), именованный том
  `bsl-indexer-home`, порт `9003`.
- [`entrypoint.sh`](entrypoint.sh) — очистка stale-состояния + демон + serve.
- [`daemon.toml`](daemon.toml) — список индексируемых путей (правится под себя).
- [`../../.dockerignore`](../../.dockerignore) — отсекает `target/`, `.git` и пр.
  из контекста сборки.

## Заметки

- Порт `9003` задаётся `MCP_HTTP_PORT` (проброс в compose менять синхронно).
- Feature `enrichment` (LLM-обогащение) не включён — для индекса/поиска не нужен.
- Пересобрать после обновления исходников:
  `docker compose -f deploy/docker/docker-compose.yml up -d --build`.
