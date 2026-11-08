# Веб-панель администрирования баз

English: [README_EN.md](README_EN.md).

Необязательный компонент серверного развёртывания: когда `bsl-indexer` (или `code-index`) работает
на отдельной машине для команды, панель позволяет вести базы из браузера — без SSH и ручной правки
`daemon.toml`. Бинарники и формат индекса не меняются.

## Что умеет

- список баз из `[[paths]]` `daemon.toml`: статус из `/health` демона, число файлов, размер
  исходников и индекса (`.code-index`), для git-баз — ветка и последний коммит;
- свободное место на диске и память;
- добавление базы: `git clone` (приватный репозиторий — по URL с токеном), загрузка ZIP из браузера
  (например, XML-выгрузка конфигурации 1С), подключение каталога, уже скопированного на сервер;
- `git pull` для git-баз, удаление базы вместе с каталогом;
- хвост `daemon.log` и готовый блок `.mcp.json` для клиента.

## Устройство

- [`app.py`](app.py) — Python 3.11+, только стандартная библиотека; [`index.html`](index.html) —
  одна страница без сборки и внешних зависимостей.
- Правит только секции `[[paths]]` в `daemon.toml`: всё, что стоит до них, и прочие поля секций
  сохраняются. Демон сам замечает изменение файла.
- Каталоги баз панель создаёт и удаляет только внутри `ADMIN_REPOS_ROOT`; пути из ZIP проверяются
  на выход за его пределы.
- `serve` читает список баз при старте, поэтому после добавления или удаления базы панель
  перезапускает его юнит (`ADMIN_SERVE_UNIT`, через `sudo -n`). Клиентам MCP после этого нужно
  переподключиться (в Claude Code — `/mcp` → Reconnect).
- Собственной авторизации нет. Панель слушает внутренний адрес и рассчитана на работу за обратным
  прокси с TLS и паролем. Изменяющие запросы требуют заголовка `X-Admin: 1` — обычная HTML-форма
  с чужого сайта его не выставит (защита от CSRF).

## Установка (Linux, systemd)

Предполагается, что демон уже работает (см. [`../systemd`](../systemd/bsl-indexer-daemon.service))
под тем же пользователем, что будет у панели.

```bash
sudo mkdir -p /opt/code-index/admin-panel /srv/repos
sudo cp app.py index.html /opt/code-index/admin-panel/
sudo chown -R codeindex:codeindex /srv/repos

sudo cp bsl-indexer-serve.service code-index-admin.service /etc/systemd/system/   # поправьте пути
sudo cp sudoers.example /etc/sudoers.d/code-index-admin && sudo visudo -cf /etc/sudoers.d/code-index-admin
sudo systemctl daemon-reload
sudo systemctl enable --now bsl-indexer-serve code-index-admin
```

Обратный прокси — пример [`nginx.conf.example`](nginx.conf.example): basic auth для панели,
Bearer-токен для `/mcp`, потоковая загрузка больших ZIP.

### Переменные окружения

| Переменная | По умолчанию | Назначение |
|---|---|---|
| `CODE_INDEX_HOME` | — (обязательна) | каталог демона: `daemon.toml`, `daemon.json`, `daemon.log` |
| `ADMIN_REPOS_ROOT` | `/srv/repos` | где панель создаёт и удаляет каталоги баз |
| `ADMIN_HOST`, `ADMIN_PORT` | `127.0.0.1`, `8080` | адрес панели |
| `ADMIN_MCP_URL` | пусто | адрес MCP для блока подключения |
| `ADMIN_MCP_TOKEN_FILE` | пусто | файл с токеном для заголовка `Authorization` в блоке подключения |
| `ADMIN_SERVE_UNIT` | пусто | юнит `serve`, который перезапускается после изменения списка баз |

Порт демона панель берёт из `daemon.json`.

## Большие выгрузки на Linux

Демон следит за каждым каталогом базы через inotify. Лимит `fs.inotify.max_user_watches`
по умолчанию — порядка 60 тыс., а крупная выгрузка 1С (ERP) содержит больше 100 тыс. файлов.
При нехватке демон останавливает воркер с `create_watcher: OS file watch limit reached`,
и база переходит в статус «ошибка». Поднимите лимит:

```bash
cat <<'EOF' | sudo tee /etc/sysctl.d/90-code-index.conf
fs.inotify.max_user_watches = 2097152
fs.inotify.max_user_instances = 1024
EOF
sudo sysctl --system
sudo systemctl restart bsl-indexer-daemon
```

## Проверено

Ubuntu 24.04, Python 3.12, `bsl-indexer` 1.15.6; базы — XML-выгрузки типовых конфигураций 1С,
крупнейшая около 125 тыс. файлов.
