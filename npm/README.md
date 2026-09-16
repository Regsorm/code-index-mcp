# @regsorm/code-index-mcp

npm-обёртка для [code-index](https://github.com/Regsorm/code-index-mcp) — высокопроизводительного индексатора кода с MCP-протоколом для AI-моделей.

Rust + tree-sitter + SQLite. Индексация 57 тыс. файлов с нуля за 2 мин 41 с, запуск на готовом индексе — 2,3 с, ответ на запрос — около 10 мс.

## Установка

```bash
npm install -g @regsorm/code-index-mcp
```

При установке `postinstall` скачивает готовый нативный бинарник под вашу платформу из [GitHub Releases](https://github.com/Regsorm/code-index-mcp/releases). Сам код на Rust — обёртка ничего не компилирует.

Поддерживаемые платформы: Windows x64, Linux x64, macOS arm64.

## Запуск

Программа работает двумя процессами: фоновый индексатор строит индекс, MCP-сервер его читает. Без индексатора инструменты отвечают `daemon_offline`.

1. Задайте переменную `CODE_INDEX_HOME` — папку для настроек и служебных файлов (например `C:\tools\code-index`).
2. Создайте в ней `daemon.toml` со списком папок:

   ```toml
   [daemon]
   http_host = "127.0.0.1"
   http_port = 8015

   [[paths]]
   path = "C:/path/to/your/repo"
   alias = "main"
   ```

3. Запустите индексатор:

   ```bash
   npx @regsorm/code-index-mcp daemon run
   ```

4. Подключите MCP-сервер. Транспорт по умолчанию — `stdio`; для Claude Code / Cursor добавьте в MCP-конфигурацию:

   ```json
   {
     "code-index": {
       "command": "npx",
       "args": ["-y", "@regsorm/code-index-mcp", "serve", "--path", "main=C:/path/to/your/repo"],
       "env": { "CODE_INDEX_HOME": "C:\\tools\\code-index" }
     }
   }
   ```

   Папка в `--path` должна быть перечислена в `daemon.toml`.

## Документация

Полное описание инструментов (`search_function`, `get_function`, `grep_body`, `find_symbol`, `get_callers` и др.), режим демона и конфигурация — в [основном репозитории](https://github.com/Regsorm/code-index-mcp).

## Лицензия

MIT
