#!/usr/bin/env node
// Тонкая обёртка: запускает нативный бинарник code-index, прозрачно
// пробрасывая аргументы и stdin/stdout/stderr. Для MCP stdio-транспорта
// критично, чтобы потоки шли напрямую (stdio: 'inherit').
//
// Пример запуска MCP-сервера клиентом:
//   npx @regsorm/code-index-mcp serve --path C:/MyRepo
// (транспорт stdio — по умолчанию).

'use strict';

const path = require('path');
const { spawn } = require('child_process');

const binName = process.platform === 'win32' ? 'code-index.exe' : 'code-index';
const binPath = path.join(__dirname, binName);

const child = spawn(binPath, process.argv.slice(2), { stdio: 'inherit' });

child.on('exit', (code, signal) => {
  if (signal) {
    process.kill(process.pid, signal);
  } else {
    process.exit(code == null ? 0 : code);
  }
});

child.on('error', (err) => {
  if (err.code === 'ENOENT') {
    console.error(
      '[code-index-mcp] Бинарник не найден. Похоже, postinstall не отработал.\n' +
      'Переустановите пакет: npm install -g @regsorm/code-index-mcp'
    );
  } else {
    console.error('[code-index-mcp] Не удалось запустить бинарник:', err.message);
  }
  process.exit(1);
});
