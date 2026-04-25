// Кроссплатформенные пути, которые демон использует для конфига,
// PID-файла, файла с портом и лога.
//
// Единый источник истины — переменная окружения `CODE_INDEX_HOME`.
// Она обязательна. В этой папке лежат:
//   daemon.toml  — конфиг, редактирует пользователь
//   daemon.pid   — runtime, пишет демон при старте
//   daemon.json  — runtime, порт HTTP-IPC (читают MCP и CLI)
//   daemon.log   — лог демона
//
// Способы задать переменную:
//   * Windows:  setx CODE_INDEX_HOME "C:\tools\code-index"
//   * Linux:    export CODE_INDEX_HOME="$HOME/.local/code-index"
//   * macOS:    launchctl setenv CODE_INDEX_HOME /Users/you/code-index
//   * MCP:      "env": { "CODE_INDEX_HOME": "..." } в .mcp.json

use std::path::PathBuf;

use anyhow::{anyhow, Result};

/// Имя env var, которая задаёт единый каталог демона.
pub const HOME_ENV: &str = "CODE_INDEX_HOME";

/// Прочитать `CODE_INDEX_HOME`. Возвращает ошибку с понятным сообщением,
/// если переменная не задана или пуста.
pub fn home_dir() -> Result<PathBuf> {
    match std::env::var(HOME_ENV) {
        Ok(v) if !v.trim().is_empty() => Ok(PathBuf::from(v)),
        _ => Err(anyhow!(
            "Переменная окружения {} не задана. Пример установки:\n\
             Windows:  setx {} \"C:\\tools\\code-index\"\n\
             Linux:    export {}=\"$HOME/.local/code-index\"\n\
             macOS:    launchctl setenv {} /Users/you/code-index\n\
             MCP:      \"env\": {{ \"{}\": \"...\" }} в .mcp.json",
            HOME_ENV, HOME_ENV, HOME_ENV, HOME_ENV, HOME_ENV
        )),
    }
}

/// Путь к файлу конфигурации демона: `$CODE_INDEX_HOME/daemon.toml`.
pub fn config_path() -> Result<PathBuf> {
    Ok(home_dir()?.join("daemon.toml"))
}

/// Путь к каталогу runtime-состояния: та же папка, что и `CODE_INDEX_HOME`.
pub fn state_dir() -> Result<PathBuf> {
    home_dir()
}

/// PID-файл демона.
pub fn pid_file() -> Result<PathBuf> {
    Ok(state_dir()?.join("daemon.pid"))
}

/// Файл с runtime-информацией (host/port HTTP-IPC).
pub fn runtime_info_file() -> Result<PathBuf> {
    Ok(state_dir()?.join("daemon.json"))
}

/// Лог-файл демона.
pub fn log_file() -> Result<PathBuf> {
    Ok(state_dir()?.join("daemon.log"))
}

/// Убедиться, что каталог `CODE_INDEX_HOME` существует.
pub fn ensure_state_dir() -> Result<PathBuf> {
    let dir = state_dir()?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| anyhow!("Не удалось создать {}: {}", dir.display(), e))?;
    Ok(dir)
}
