#!/usr/bin/env node
// postinstall: скачивает архив с готовым бинарником code-index из GitHub
// Releases под текущую платформу и распаковывает его системным `tar`.
//
// Архивы уже публикуются в каждом релизе (.github/workflows/release.yml),
// поэтому пакет совместим с любым существующим релизом — отдельные сырые
// бинарники не нужны.
//
// `tar` есть из коробки: Windows 10 1803+ (bsdtar понимает и .zip, и .tar.gz),
// все Linux/macOS. Сеть нужна только при установке. Нет сборки под платформу
// или нет tar — печатаем понятное сообщение и выходим с кодом 1.

'use strict';

const fs = require('fs');
const os = require('os');
const path = require('path');
const https = require('https');
const { execFileSync } = require('child_process');

const REPO = 'Regsorm/code-index-mcp';
const { version } = require('../package.json');

// platform-arch -> { archive: имя архива в релизе, bin: имя бинарника внутри }
const TARGETS = {
  'win32-x64': { archive: 'code-index-windows-x64.zip', bin: 'code-index.exe' },
  'linux-x64': { archive: 'code-index-linux-x64.tar.gz', bin: 'code-index' },
  'darwin-arm64': { archive: 'code-index-macos-arm64.tar.gz', bin: 'code-index' },
};

const key = `${process.platform}-${process.arch}`;
const target = TARGETS[key];

if (!target) {
  console.error(
    `[code-index-mcp] Нет готового бинарника для платформы "${key}".\n` +
    `Поддерживаются: ${Object.keys(TARGETS).join(', ')}.\n` +
    `Соберите из исходников: https://github.com/${REPO}`
  );
  process.exit(1);
}

const url = `https://github.com/${REPO}/releases/download/v${version}/${target.archive}`;
const binDir = path.join(__dirname, '..', 'bin');
// Архив качаем прямо в binDir, чтобы распаковывать tar-ом по относительному
// имени (cwd = binDir) — путь с двоеточием диска tar трактует как remote-хост.
const archivePath = path.join(binDir, target.archive);

fs.mkdirSync(binDir, { recursive: true });

// Скачивание с поддержкой редиректов (GitHub Releases отдаёт 302 на CDN).
function download(currentUrl, redirectsLeft, cb) {
  if (redirectsLeft < 0) {
    return cb(new Error('Слишком много редиректов'));
  }
  https
    .get(currentUrl, { headers: { 'User-Agent': 'code-index-mcp-postinstall' } }, (res) => {
      const { statusCode } = res;
      if ([301, 302, 307, 308].includes(statusCode) && res.headers.location) {
        res.resume();
        return download(res.headers.location, redirectsLeft - 1, cb);
      }
      if (statusCode !== 200) {
        res.resume();
        return cb(new Error(`HTTP ${statusCode} для ${currentUrl}`));
      }
      const file = fs.createWriteStream(archivePath);
      res.pipe(file);
      file.on('finish', () => file.close(() => cb(null)));
      file.on('error', (err) => cb(err));
    })
    .on('error', (err) => cb(err));
}

download(url, 5, (err) => {
  if (err) {
    console.error(
      `[code-index-mcp] Не удалось скачать архив (${target.archive} v${version}): ${err.message}\n` +
      `URL: ${url}`
    );
    process.exit(1);
  }

  // Распаковка системным tar по относительному имени в cwd=binDir.
  // На Windows зовём системный bsdtar явно (System32\tar.exe) — он понимает
  // .zip; полагаться на первый `tar` из PATH нельзя (там может оказаться GNU
  // tar, который .zip не читает и трактует диск C: как remote-хост).
  const tarCmd = process.platform === 'win32'
    ? path.join(process.env.SystemRoot || 'C:\\Windows', 'System32', 'tar.exe')
    : 'tar';
  try {
    execFileSync(tarCmd, ['-xf', target.archive], { cwd: binDir, stdio: 'inherit' });
  } catch (e) {
    console.error(
      `[code-index-mcp] Не удалось распаковать архив через tar: ${e.message}\n` +
      `Убедитесь, что в системе есть tar (Windows 10 1803+, Linux, macOS).\n` +
      `Архив: ${archivePath}`
    );
    process.exit(1);
  } finally {
    try { fs.unlinkSync(archivePath); } catch (_) { /* не критично */ }
  }

  const dest = path.join(binDir, target.bin);
  if (!fs.existsSync(dest)) {
    console.error(
      `[code-index-mcp] После распаковки не найден бинарник "${target.bin}" в ${binDir}.`
    );
    process.exit(1);
  }
  if (process.platform !== 'win32') {
    fs.chmodSync(dest, 0o755);
  }
  console.log(`[code-index-mcp] Бинарник установлен: ${dest}`);
});
