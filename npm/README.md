# @regsorm/code-index-mcp

npm-обёртка для [code-index](https://github.com/Regsorm/code-index-mcp) — высокопроизводительного индексатора кода с MCP-протоколом для AI-моделей.

Rust + tree-sitter + SQLite. Индексация 62K файлов за ~43 сек, 282K функций, поиск < 1 мс.

## Установка

```bash
npm install -g @regsorm/code-index-mcp
```

При установке `postinstall` скачивает готовый нативный бинарник под вашу платформу из [GitHub Releases](https://github.com/Regsorm/code-index-mcp/releases). Сам код на Rust — обёртка ничего не компилирует.

Поддерживаемые платформы: Windows x64, Linux x64, macOS arm64.

## Запуск как MCP-сервера

```bash
npx @regsorm/code-index-mcp serve --path /path/to/your/repo
```

Транспорт по умолчанию — `stdio`. Для подключения к Claude Code / Cursor добавьте в MCP-конфигурацию:

```json
{
  "code-index": {
    "command": "npx",
    "args": ["-y", "@regsorm/code-index-mcp", "serve", "--path", "/path/to/your/repo"]
  }
}
```

## Документация

Полное описание инструментов (`search_function`, `get_function`, `grep_body`, `find_symbol`, `get_callers` и др.), режим демона и конфигурация — в [основном репозитории](https://github.com/Regsorm/code-index-mcp).

## Лицензия

MIT
