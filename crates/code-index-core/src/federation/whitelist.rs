// IP-whitelist middleware для axum-роутера.
//
// Принимаем запросы только от peer-IP, которые перечислены в `serve.toml`
// (плюс loopback-адреса для локальных клиентов на той же машине). Чужой
// peer-IP получает 403 с JSON `{"error":"forbidden","peer":"..."}`.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use axum::{
    extract::{ConnectInfo, Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};

use super::config::ServeFileConfig;

/// Собрать множество допустимых peer-IP из конфига. Loopback-адреса включены
/// безусловно — чтобы локальный клиент Claude Code на этой машине мог
/// дёргать MCP без явного перечисления `me.ip` через 127.0.0.1.
pub fn build(cfg: &ServeFileConfig) -> HashSet<IpAddr> {
    let mut set = HashSet::new();
    set.insert(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
    set.insert(IpAddr::V6(Ipv6Addr::LOCALHOST));

    if let Ok(ip) = cfg.me.ip.parse::<IpAddr>() {
        set.insert(ip);
    }
    for entry in &cfg.paths {
        if let Ok(ip) = entry.ip.parse::<IpAddr>() {
            set.insert(ip);
        }
    }
    set
}

/// axum-middleware: пропускает запрос, если peer-IP в whitelist; иначе 403.
///
/// Требует, чтобы listener запускался через
/// `axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())`,
/// иначе ConnectInfo не извлечётся.
pub async fn middleware(
    State(allowed): State<Arc<HashSet<IpAddr>>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    let mut peer = addr.ip();
    // IPv4-mapped IPv6 (::ffff:127.0.0.1) — нормализуем к чистому IPv4
    // для совпадения с заполнением whitelist.
    if let IpAddr::V6(v6) = peer {
        if let Some(v4) = v6.to_ipv4_mapped() {
            peer = IpAddr::V4(v4);
        }
    }
    if !allowed.contains(&peer) {
        let body = serde_json::json!({
            "error": "forbidden",
            "peer": peer.to_string(),
        });
        return (
            StatusCode::FORBIDDEN,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            body.to_string(),
        )
            .into_response();
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::federation::config::{MeSection, ServePathEntry};

    #[test]
    fn build_includes_loopback_and_me_and_paths() {
        let cfg = ServeFileConfig {
            me: MeSection { ip: "192.0.2.10".to_string(), token: None },
            paths: vec![
                ServePathEntry { alias: "ut".to_string(), ip: "192.0.2.50".to_string(), port: None },
                ServePathEntry { alias: "dev".to_string(), ip: "192.0.2.10".to_string(), port: None },
            ],
        };
        let set = build(&cfg);
        assert!(set.contains(&"127.0.0.1".parse::<IpAddr>().unwrap()));
        assert!(set.contains(&"::1".parse::<IpAddr>().unwrap()));
        assert!(set.contains(&"192.0.2.50".parse::<IpAddr>().unwrap()));
        assert!(set.contains(&"192.0.2.10".parse::<IpAddr>().unwrap()));
        // Дубликаты IP (me + одна из paths) не плодятся
        assert_eq!(set.len(), 4);
    }

    #[test]
    fn build_with_invalid_ip_in_paths_silently_skips() {
        // validate() в config.rs ловит невалидные IP до этого момента —
        // build не должен паниковать на edge-случае.
        let cfg = ServeFileConfig {
            me: MeSection { ip: "192.0.2.10".to_string(), token: None },
            paths: vec![
                ServePathEntry { alias: "ut".to_string(), ip: "not-ip".to_string(), port: None },
            ],
        };
        let set = build(&cfg);
        // Только loopback + me.ip (paths[0] не парсится — пропущена).
        assert_eq!(set.len(), 3);
    }
}
