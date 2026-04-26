// Формат и чтение конфигурации serve (`serve.toml`).
//
// Этот файл — глобальный, одинаковый на всех машинах в федерации. Хранит
// собственный IP машины и плоский список репозиториев с их IP. Если `repo.ip`
// совпадает с `[me].ip` — серv обслуживает репо локально, иначе форвардит
// запрос к удалённому процессу.
//
// Пример:
//
// ```toml
// [me]
// ip = "192.0.2.10"
// # token = "..."   # опционально, в rc6 не валидируется (заготовка под rc7)
//
// [[paths]]
// alias = "ut"
// ip = "192.0.2.50"
//
// [[paths]]
// alias = "dev"
// ip = "192.0.2.10"
//
// [[paths]]
// alias = "wms"
// ip = "192.0.2.51"
// port = 8021    # опционально: per-host port, default = DEFAULT_REMOTE_PORT (8011)
// ```
//
// Локальный путь репозитория живёт в `daemon.toml` (см. daemon_core::config).

use std::collections::HashSet;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::client::DEFAULT_REMOTE_PORT;

/// Полная конфигурация serve, прочитанная из `serve.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServeFileConfig {
    /// Секция `[me]` — параметры собственной машины.
    pub me: MeSection,

    /// Секции `[[paths]]` — реестр репозиториев в федерации.
    #[serde(default, rename = "paths")]
    pub paths: Vec<ServePathEntry>,
}

/// Секция `[me]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeSection {
    /// IP-адрес собственной машины. По нему serve определяет, какие репо
    /// обслуживать локально, и на какой интерфейс биндить порт.
    pub ip: String,

    /// Shared secret для будущей авторизации. В rc6 поле парсится, но не
    /// проверяется ни в одном handler-е — заготовка под rc7.
    #[serde(default)]
    pub token: Option<String>,
}

/// Запись `[[paths]]`. Без `path` — путь к корню репозитория хранится только в
/// локальном `daemon.toml` той машины, где репо физически лежит.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServePathEntry {
    /// Глобально уникальный алиас репозитория (`repo` параметр в tool-call).
    pub alias: String,

    /// IP машины, на которой лежит репозиторий.
    pub ip: String,

    /// Опциональный порт удалённого `code-index serve` для этой записи.
    /// Если не указан — используется `DEFAULT_REMOTE_PORT` (8011).
    /// Полезно когда на одной машине поднято несколько serve-нод (разные
    /// деплои, тестовые окружения), либо когда port в проде смещён от дефолта.
    /// Для local-записей (ip == me.ip) поле игнорируется — местный serve
    /// слушает на собственном порту, заданном в daemon.toml/CLI.
    #[serde(default)]
    pub port: Option<u16>,
}

impl ServePathEntry {
    /// Эффективный порт для исходящих federate-запросов: явно указанный
    /// `port` либо `DEFAULT_REMOTE_PORT` если не задан.
    pub fn effective_port(&self) -> u16 {
        self.port.unwrap_or(DEFAULT_REMOTE_PORT)
    }
}

/// Прочитать `serve.toml` с указанного пути и провалидировать.
pub fn load_from(path: &Path) -> anyhow::Result<ServeFileConfig> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("Не удалось прочитать {}: {}", path.display(), e))?;
    let cfg = parse_str(&text)?;
    validate(&cfg)?;
    Ok(cfg)
}

/// Разобрать конфиг из строки. Используется в тестах.
pub fn parse_str(text: &str) -> anyhow::Result<ServeFileConfig> {
    toml::from_str(text)
        .map_err(|e| anyhow::anyhow!("Ошибка парсинга serve.toml: {}", e))
}

/// Если в `home/serve.toml` есть файл — загрузить и провалидировать. Иначе
/// `Ok(None)` — serve работает в моно-режиме (rc5-совместимо).
pub fn load_or_default_from(home: &Path) -> anyhow::Result<Option<ServeFileConfig>> {
    let path = home.join("serve.toml");
    if !path.exists() {
        return Ok(None);
    }
    load_from(&path).map(Some)
}

/// Семантическая валидация конфига:
/// - `me.ip` парсится как `IpAddr`;
/// - все `paths[].ip` парсятся как `IpAddr`;
/// - алиасы уникальны и непусты.
pub fn validate(cfg: &ServeFileConfig) -> anyhow::Result<()> {
    if cfg.me.ip.trim().is_empty() {
        anyhow::bail!("[me].ip пустой — укажите IP собственной машины.");
    }
    cfg.me.ip.parse::<IpAddr>().map_err(|e| {
        anyhow::anyhow!("[me].ip = {:?} не является IP-адресом: {}", cfg.me.ip, e)
    })?;

    let mut seen = HashSet::new();
    for (idx, entry) in cfg.paths.iter().enumerate() {
        if entry.alias.trim().is_empty() {
            anyhow::bail!("[[paths]] #{}: alias пустой.", idx);
        }
        if entry.ip.trim().is_empty() {
            anyhow::bail!("[[paths]] #{} (alias='{}'): ip пустой.", idx, entry.alias);
        }
        entry.ip.parse::<IpAddr>().map_err(|e| {
            anyhow::anyhow!(
                "[[paths]] #{} (alias='{}'): ip = {:?} не является IP-адресом: {}",
                idx,
                entry.alias,
                entry.ip,
                e
            )
        })?;
        if let Some(p) = entry.port {
            if p == 0 {
                anyhow::bail!(
                    "[[paths]] #{} (alias='{}'): port = 0 недопустим (зарезервирован).",
                    idx,
                    entry.alias
                );
            }
        }
        if !seen.insert(entry.alias.clone()) {
            anyhow::bail!(
                "Алиас '{}' встречается в [[paths]] более одного раза — алиасы должны быть уникальны.",
                entry.alias
            );
        }
    }
    Ok(())
}

/// Удобная обёртка: путь до `serve.toml` рядом с `daemon.toml`.
pub fn default_path() -> anyhow::Result<PathBuf> {
    let daemon_cfg_path = crate::daemon_core::paths::config_path()?;
    let home = daemon_cfg_path.parent().ok_or_else(|| {
        anyhow::anyhow!(
            "Не удалось определить родительский каталог для {}",
            daemon_cfg_path.display()
        )
    })?;
    Ok(home.join("serve.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_config() {
        let text = r#"
            [me]
            ip = "192.0.2.10"

            [[paths]]
            alias = "ut"
            ip = "192.0.2.50"

            [[paths]]
            alias = "dev"
            ip = "192.0.2.10"
        "#;
        let cfg = parse_str(text).unwrap();
        validate(&cfg).unwrap();

        assert_eq!(cfg.me.ip, "192.0.2.10");
        assert!(cfg.me.token.is_none());
        assert_eq!(cfg.paths.len(), 2);
        assert_eq!(cfg.paths[0].alias, "ut");
        assert_eq!(cfg.paths[0].ip, "192.0.2.50");
        assert_eq!(cfg.paths[1].alias, "dev");
    }

    #[test]
    fn empty_paths_section_is_allowed() {
        // Моно-режим с одним [me] и без репо — допустим (например, при
        // постепенном вводе федерации).
        let text = r#"
            [me]
            ip = "127.0.0.1"
        "#;
        let cfg = parse_str(text).unwrap();
        validate(&cfg).unwrap();
        assert!(cfg.paths.is_empty());
    }

    #[test]
    fn token_is_optional() {
        let text = r#"
            [me]
            ip = "127.0.0.1"
            token = "secret-rc7-placeholder"
        "#;
        let cfg = parse_str(text).unwrap();
        validate(&cfg).unwrap();
        assert_eq!(cfg.me.token.as_deref(), Some("secret-rc7-placeholder"));
    }

    #[test]
    fn missing_me_section_fails_at_parse() {
        // Без секции [me] парсер сериализации обязан упасть, так как `me`
        // не имеет дефолта.
        let text = r#"
            [[paths]]
            alias = "ut"
            ip = "192.0.2.50"
        "#;
        let err = parse_str(text).expect_err("должна быть ошибка");
        let msg = format!("{}", err);
        assert!(
            msg.contains("me") || msg.to_lowercase().contains("missing"),
            "ожидалось упоминание поля 'me' в ошибке, получили: {}",
            msg
        );
    }

    #[test]
    fn invalid_ip_fails_validation() {
        let text = r#"
            [me]
            ip = "not-an-ip"

            [[paths]]
            alias = "ut"
            ip = "192.0.2.50"
        "#;
        let cfg = parse_str(text).unwrap();
        let err = validate(&cfg).expect_err("должна быть ошибка");
        assert!(format!("{}", err).contains("[me].ip"));
    }

    #[test]
    fn duplicate_alias_fails_validation() {
        let text = r#"
            [me]
            ip = "127.0.0.1"

            [[paths]]
            alias = "ut"
            ip = "192.0.2.50"

            [[paths]]
            alias = "ut"
            ip = "192.0.2.51"
        "#;
        let cfg = parse_str(text).unwrap();
        let err = validate(&cfg).expect_err("должна быть ошибка");
        assert!(format!("{}", err).contains("'ut'"));
    }

    #[test]
    fn empty_alias_fails_validation() {
        let text = r#"
            [me]
            ip = "127.0.0.1"

            [[paths]]
            alias = ""
            ip = "192.0.2.50"
        "#;
        let cfg = parse_str(text).unwrap();
        let err = validate(&cfg).expect_err("должна быть ошибка");
        assert!(format!("{}", err).contains("alias"));
    }

    #[test]
    fn empty_path_ip_fails_validation() {
        let text = r#"
            [me]
            ip = "127.0.0.1"

            [[paths]]
            alias = "ut"
            ip = ""
        "#;
        let cfg = parse_str(text).unwrap();
        let err = validate(&cfg).expect_err("должна быть ошибка");
        assert!(format!("{}", err).contains("ip"));
    }

    #[test]
    fn port_field_is_optional_and_defaults_to_remote_port() {
        // port не указан — effective_port() возвращает DEFAULT_REMOTE_PORT.
        let text = r#"
            [me]
            ip = "127.0.0.1"

            [[paths]]
            alias = "ut"
            ip = "192.0.2.50"
        "#;
        let cfg = parse_str(text).unwrap();
        validate(&cfg).unwrap();
        assert_eq!(cfg.paths[0].port, None);
        assert_eq!(cfg.paths[0].effective_port(), DEFAULT_REMOTE_PORT);
    }

    #[test]
    fn port_field_parses_when_explicit() {
        let text = r#"
            [me]
            ip = "127.0.0.1"

            [[paths]]
            alias = "ut"
            ip = "192.0.2.50"
            port = 8021

            [[paths]]
            alias = "dev"
            ip = "192.0.2.10"
        "#;
        let cfg = parse_str(text).unwrap();
        validate(&cfg).unwrap();
        assert_eq!(cfg.paths[0].port, Some(8021));
        assert_eq!(cfg.paths[0].effective_port(), 8021);
        // Соседняя запись без port — дефолт сохраняется независимо.
        assert_eq!(cfg.paths[1].port, None);
        assert_eq!(cfg.paths[1].effective_port(), DEFAULT_REMOTE_PORT);
    }

    #[test]
    fn zero_port_fails_validation() {
        let text = r#"
            [me]
            ip = "127.0.0.1"

            [[paths]]
            alias = "ut"
            ip = "192.0.2.50"
            port = 0
        "#;
        let cfg = parse_str(text).unwrap();
        let err = validate(&cfg).expect_err("port=0 должен отклоняться");
        assert!(format!("{}", err).contains("port = 0"));
    }

    #[test]
    fn load_or_default_returns_none_when_file_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let result = load_or_default_from(tmp.path()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn load_or_default_loads_when_file_present() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("serve.toml");
        std::fs::write(
            &path,
            r#"
            [me]
            ip = "127.0.0.1"

            [[paths]]
            alias = "ut"
            ip = "127.0.0.1"
            "#,
        )
        .unwrap();
        let cfg = load_or_default_from(tmp.path()).unwrap().unwrap();
        assert_eq!(cfg.me.ip, "127.0.0.1");
        assert_eq!(cfg.paths.len(), 1);
    }
}
