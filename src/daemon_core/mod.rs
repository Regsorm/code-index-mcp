// Ядро нового фонового демона индексации.
//
// Здесь собрана вся общая логика демона: конфиг, IPC-контракты, состояние
// отслеживаемых папок, кроссплатформенные пути. HTTP-сервер демона и MCP-клиент
// подключаются к этим примитивам и реализуют соответствующие роли.

pub mod client;
pub mod commands;
pub mod config;
pub mod ipc;
pub mod lock;
pub mod paths;
pub mod runner;
pub mod server;
pub mod state;
pub mod worker;
