#!/bin/bash
# Запускает bsl-indexer в одном контейнере: демон (писатель) + serve (read-only
# MCP HTTP). Оба процесса делят loopback и $CODE_INDEX_HOME — serve находит демон
# через $CODE_INDEX_HOME/daemon.json (см. docs/bsl-indexer.md).
set -euo pipefail

: "${CODE_INDEX_HOME:=/data/code-index-home}"
: "${MCP_HTTP_HOST:=0.0.0.0}"
: "${MCP_HTTP_PORT:=9003}"
export CODE_INDEX_HOME
mkdir -p "$CODE_INDEX_HOME"

if [[ ! -f "$CODE_INDEX_HOME/daemon.toml" ]]; then
  echo "[bsl-indexer] ОШИБКА: нет $CODE_INDEX_HOME/daemon.toml (примонтируйте deploy/docker/daemon.toml)" >&2
  exit 1
fi

# Свежий контейнер = новый PID-namespace: любые daemon.pid/daemon.json на томе
# остались от прошлого (нечисто завершённого) инстанса — RAII-очистка PID-файла
# не срабатывает при SIGKILL. Проверка живости PID внутри демона ненадёжна: PID
# из прошлого контейнера (напр. 8) совпадает с живым процессом нового → ложное
# «демон уже запущен» → краш-луп. В контейнере ни один прошлый демон дожить не
# мог, поэтому runtime-состояние всегда безопасно удалить при старте.
rm -f "$CODE_INDEX_HOME/daemon.pid" "$CODE_INDEX_HOME/daemon.json"

echo "[bsl-indexer] запуск демона (писатель, начальная индексация)…"
bsl-indexer daemon run &
DAEMON_PID=$!

# Ждём, пока демон запишет daemon.json (готов; индексация может ещё идти в фоне).
echo "[bsl-indexer] ожидание готовности демона ($CODE_INDEX_HOME/daemon.json)…"
for _ in $(seq 1 120); do
  [[ -f "$CODE_INDEX_HOME/daemon.json" ]] && break
  if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
    echo "[bsl-indexer] демон завершился преждевременно" >&2
    wait "$DAEMON_PID" || true
    exit 1
  fi
  sleep 1
done

echo "[bsl-indexer] запуск MCP HTTP serve на ${MCP_HTTP_HOST}:${MCP_HTTP_PORT} (/mcp)…"
bsl-indexer serve --transport http --host "$MCP_HTTP_HOST" --port "${MCP_HTTP_PORT}" \
  --config "$CODE_INDEX_HOME/daemon.toml" &
SERVE_PID=$!

# Если любой из процессов упадёт — гасим контейнер, restart policy перезапустит.
wait -n "$DAEMON_PID" "$SERVE_PID"
echo "[bsl-indexer] процесс завершился — останавливаю контейнер" >&2
kill "$DAEMON_PID" "$SERVE_PID" 2>/dev/null || true
exit 1
