# Деплой bsl-indexer на VM RAG (<vm-rag-ip>)

Этот документ — операционная инструкция для замены текущего `pg_indexer.py` на `bsl-indexer` на боевой VM RAG. Параллельный запуск, A/B-проверка, переключение `bsl-agent` — описаны в этапе 8 плана workspace-refactor.

> **Внимание.** На VM работает прод-индексатор и зависимый от него `bsl-agent`. Не удаляйте существующие данные `pg_indexer` до явного подтверждения корректности bsl-indexer на A/B-тесте.

---

## 1. Предусловия

* SSH-доступ `rag@<vm-rag-ip>` через ключ `~/.ssh/id_ed25519_vm`.
* Свободное место на VM: ≥ 30 ГБ (проверка: `df -h /`).
* RAM: ≥ 8 ГБ свободной (проверка: `free -h`).

Все 1С-репо лежат на VM в `/home/rag/data/bsl_local/`. Имя bare-remote — `windows-bare`, источник истины — bare-репозитории на Windows-host'е разработчика. Скрипт обновления — `/home/rag/update_all_repos.sh`.

## 2. Установка Rust toolchain (одноразово)

```bash
ssh rag@<vm-rag-ip>
curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y \
    --default-toolchain stable --profile minimal --no-modify-path
echo 'source $HOME/.cargo/env' >> ~/.bashrc
source $HOME/.cargo/env
rustc --version  # проверка
```

Установка занимает ~1 ГБ диска. Версия rustc должна быть ≥ 1.77.

## 3. Деплой исходников

С Windows-host'а (где лежит этот репо):

```bash
# Из корня репо code-index/
tar --exclude='target' --exclude='.git' --exclude='*/.code-index' \
    --exclude='__pycache__' --exclude='.cargo' \
    -czf - . | ssh rag@<vm-rag-ip> 'mkdir -p /home/rag/code-index && tar -xzf - -C /home/rag/code-index'
```

Размер архива ~1.5 МБ — занимает секунды.

> **Важно — `--exclude='.cargo'`.** На Windows-host'е может лежать `.cargo/config.toml` с Windows-специфичными настройками (например, путь к w64devkit-gcc для локальной сборки под x86_64-pc-windows-gnu). Этот файл cargo читает автоматически при сборке, и если попадёт на Linux — попытается использовать `C:\...` пути и упадёт. Исключаем его при копировании.

## 4. Сборка

```bash
ssh rag@<vm-rag-ip> '
cd /home/rag/code-index
source $HOME/.cargo/env
cargo build --release -p bsl-indexer --features enrichment
'
```

Первый билд: 3-7 минут (deps + компиляция). Артефакт — `/home/rag/code-index/target/release/bsl-indexer` (~30 МБ).

Бинарник копируется в `~/.local/bin/`:

```bash
ssh rag@<vm-rag-ip> '
mkdir -p ~/.local/bin
cp /home/rag/code-index/target/release/bsl-indexer ~/.local/bin/
echo "export PATH=\$HOME/.local/bin:\$PATH" >> ~/.bashrc
'
```

## 5. Конфигурация `daemon.toml`

```bash
mkdir -p ~/.code-index
cat > ~/.code-index/daemon.toml <<'EOF'
[daemon]
http_host = "127.0.0.1"
http_port = 0  # автовыбор; реальный порт пишется в daemon.json
log_level = "info"

[[paths]]
path = "/home/rag/data/bsl_local/RepoUT"
alias = "ut"
language = "bsl"

[[paths]]
path = "/home/rag/data/bsl_local/RepoBP_SS"
alias = "bp-ss"
language = "bsl"

[[paths]]
path = "/home/rag/data/bsl_local/RepoBP_TDK"
alias = "bp-tdk"
language = "bsl"

[[paths]]
path = "/home/rag/data/bsl_local/RepoZUP"
alias = "zup"
language = "bsl"

# Опциональная секция — обогащение через OpenRouter / Ollama.
# Включается отдельным запуском `bsl-indexer enrich`.
# [enrichment]
# enabled = true
# url = "https://openrouter.ai/api/v1/chat/completions"
# model = "meta-llama/llama-3.2-3b-instruct"  # самое дешёвое
# api_key_env = "OPENROUTER_API_KEY"
# batch_size = 20
EOF
```

API-ключ кладётся отдельно в `/etc/bsl-indexer/env` (читается systemd-unit'ом):

```bash
sudo mkdir -p /etc/bsl-indexer
sudo tee /etc/bsl-indexer/env > /dev/null <<EOF
OPENROUTER_API_KEY=sk-or-v1-...
CODE_INDEX_HOME=/home/rag/.code-index
EOF
sudo chmod 600 /etc/bsl-indexer/env
sudo chown rag:rag /etc/bsl-indexer/env
```

## 6. Первичная индексация (параллельно с pg_indexer)

**Не трогаем PostgreSQL pg_indexer'а.** Каждый репо индексируется отдельно, индекс пишется в `<repo>/.code-index/index.db`:

```bash
bsl-indexer index /home/rag/data/bsl_local/RepoUT
bsl-indexer index /home/rag/data/bsl_local/RepoBP_SS
bsl-indexer index /home/rag/data/bsl_local/RepoBP_TDK
bsl-indexer index /home/rag/data/bsl_local/RepoZUP
```

Ожидаемое время (с холодным кешем диска): УТ ~1 мин, БП SS ~1.5 мин, БП ТДК ~1 мин, ЗУП ~1.5-2 мин. Итого 5-7 минут на полный набор.

После завершения — проверка таблиц:

```bash
sqlite3 /home/rag/data/bsl_local/RepoUT/.code-index/index.db <<'SQL'
.headers on
.mode column
SELECT COUNT(*) AS files FROM files;
SELECT COUNT(*) AS funcs FROM functions;
SELECT COUNT(*) AS modules FROM metadata_modules;
SELECT call_type, COUNT(*) FROM proc_call_graph GROUP BY call_type;
SQL
```

## 7. Запуск daemon как systemd-service

```bash
sudo cp /home/rag/code-index/deploy/systemd/bsl-indexer-daemon.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now bsl-indexer-daemon
sudo systemctl status bsl-indexer-daemon
journalctl -u bsl-indexer-daemon -f  # логи в реальном времени
```

Health-проверка (порт из `~/.code-index/daemon.json`):

```bash
PORT=$(jq -r '.http_port' ~/.code-index/daemon.json)
curl -s http://127.0.0.1:$PORT/health | jq
```

## 8. A/B сравнение с pg_indexer

Пока **оба** индексатора работают параллельно (на одних и тех же репо, в разные БД). Сравнение делается на стороне `bsl-agent`:

1. Выбрать 10 типичных запросов пользователя (логи прода, или вручную).
2. Прогнать каждый через `bsl-agent` с `pg_indexer` backend → сохранить ответ.
3. Прогнать тот же запрос с `bsl-indexer` backend → сохранить.
4. Сравнить: количество найденных процедур, релевантность top-3, временные метрики.

Если метрики сравнимы — переключаем `bsl-agent` на bsl-indexer (см. §9).

## 9. Переключение bsl-agent на bsl-indexer

`bsl-agent` сейчас читает из PostgreSQL через свой коннектор. Два варианта переключения:

**Вариант A — bsl-agent дёргает MCP-tools `bsl-indexer`.** Не пишем новый коннектор, используем готовое. `bsl-agent.py` подключается к `127.0.0.1:<port>/mcp` и вызывает `search_function`/`grep_body`/`find_path`/`search_terms` напрямую. Меньше кода, чище архитектурно.

**Вариант B — bsl-agent открывает `index.db` SQLite read-only.** Своими SQL-запросами (как сейчас к PostgreSQL). Низкая латентность (нет HTTP), но дублирует логику BSL-tools.

Рекомендация — A. Подробности в `bsl-agent`-репо (отдельная задача).

## 10. Откат на pg_indexer

Если на A/B обнаружились проблемы:

```bash
sudo systemctl disable --now bsl-indexer-daemon
# pg_indexer работает как раньше, его не трогали
```

Удалять SQLite-индексы НЕ обязательно — они занимают единицы ГБ и не мешают.

## 11. Мониторинг и обслуживание

* **Размер индекса:** `du -sh /home/rag/data/bsl_local/*/.code-index/`. На УТ-масштаб ожидается ~3-5 ГБ суммарно.
* **WAL-checkpoint после большой переиндексации:**
  ```bash
  sqlite3 /home/rag/data/bsl_local/RepoUT/.code-index/index.db \
      "PRAGMA wal_checkpoint(TRUNCATE);"
  ```
* **Обновление при смене кода** — пересборка из той же `code-index/` (через `git pull` ваших исходников или `tar`-копию):
  ```bash
  ssh rag@<vm-rag-ip> '
  cd /home/rag/code-index
  source $HOME/.cargo/env
  cargo build --release -p bsl-indexer --features enrichment
  cp target/release/bsl-indexer ~/.local/bin/
  '
  sudo systemctl restart bsl-indexer-daemon
  ```

## 12. Связанные документы

* [bsl-indexer.md](bsl-indexer.md) — пользовательская инструкция (что умеет, как настроить enrichment).
* [bsl-indexer-architecture.md](bsl-indexer-architecture.md) — полное ТЗ с архитектурными решениями.
* `deploy/systemd/bsl-indexer-daemon.service` — systemd unit-файл (этот же).
