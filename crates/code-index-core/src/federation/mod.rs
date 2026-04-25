// Федеративная архитектура serve (rc6+).
//
// Когда подключён глобальный `serve.toml`, один процесс `code-index serve`
// обслуживает реестр репозиториев из нескольких машин. Если `repo.ip == own_ip`
// — читаем локальный SQLite, иначе HTTP-вызов к удалённому serve.
//
// Подмодули добавляются по этапам:
//   - `config`     — парсинг и валидация `serve.toml`.
//   - `repos`      — слияние `serve.toml` и `daemon.toml` в `Vec<FederatedRepo>`.
//   - `client`     — HTTP-клиент к удалённому `/federate/<tool>`.
//   - `dispatcher` — выбор local/remote и форвард запроса.
//   - `server`     — приёмная сторона `/federate/<tool>`.
//   - `whitelist`  — IP-фильтр для входящих запросов.

pub mod client;
pub mod config;
pub mod dispatcher;
pub mod repos;
pub mod server;
pub mod whitelist;
