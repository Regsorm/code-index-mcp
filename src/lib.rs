// Публичные модули code-index-mcp
// Каждый модуль будет реализован в соответствующем шаге плана

pub mod storage;        // SQLite-хранилище индекса
pub mod parser;         // tree-sitter парсеры
pub mod indexer;        // Обход и индексация файлов
pub mod mcp;            // MCP-сервер (read-only, v0.5+)
pub mod watcher;        // File watcher на базе notify
pub mod daemon_core;    // Ядро фонового демона: конфиг, IPC, состояние, HTTP-сервер
