// Публичные модули code-index-mcp
// Каждый модуль будет реализован в соответствующем шаге плана

pub mod storage;    // Шаг 2: SQLite хранилище
pub mod parser;     // Шаг 3: tree-sitter парсеры
pub mod indexer;    // Шаг 4: обход и индексация файлов
pub mod mcp;        // Шаг 7: MCP-сервер
