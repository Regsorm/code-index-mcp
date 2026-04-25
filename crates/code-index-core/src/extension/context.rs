// Контекст выполнения MCP-инструмента.
//
// Передаётся в `IndexTool::execute` каждый раз, когда внешний клиент
// вызвал tool через MCP. Из контекста tool получает доступ к локальному
// SQLite-хранилищу нужного репо и его метаданным (alias, корневой путь,
// определённый язык).
//
// `ToolContext` — read-only-обёртка вокруг уже открытого `Storage`. Он
// принципиально не предоставляет write-доступ к SQLite — индексацию ведёт
// отдельный демон (one-writer / many-readers). Tool, который попытается
// что-то записать, получит ошибку SQLITE_READONLY.

use std::path::Path;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::storage::Storage;

/// Контекст одного вызова MCP-инструмента. Лёгкая обёртка — в каждом
/// tool-call создаётся свежий, поля внутри уже шарятся через `Arc`.
pub struct ToolContext<'a> {
    /// Алиас репозитория (параметр `repo` из tool-call).
    pub repo: &'a str,
    /// Канонический корневой путь репо. None для удалённых (federated) репо;
    /// у них tool не должен исполняться локально, диспатчер форвардит запрос.
    pub root_path: Option<&'a Path>,
    /// Язык, под который репо классифицирован при загрузке конфига.
    /// Может быть `None` если auto-detect не сработал и оператор ещё не указал.
    pub language: Option<&'a str>,
    /// Подключение к SQLite. `Mutex` сериализует доступ — `rusqlite::Connection`
    /// не Sync.
    pub storage: &'a Arc<Mutex<Storage>>,
}

impl<'a> ToolContext<'a> {
    /// Сахар: проверить что язык репо совпадает с одним из ожидаемых.
    /// Используется в начале `IndexTool::execute` у языко-специфичных tools.
    pub fn language_is(&self, lang: &str) -> bool {
        self.language.map(|l| l == lang).unwrap_or(false)
    }

    /// Сахар: проверить что язык репо входит в любой из ожидаемых.
    pub fn language_in(&self, langs: &[&str]) -> bool {
        match self.language {
            Some(l) => langs.contains(&l),
            None => false,
        }
    }
}
