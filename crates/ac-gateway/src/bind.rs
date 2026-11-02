//! The gateway has authentication but no TLS of its own, so it must only be
//! reachable on a private network (loopback, RFC 1918, or the Tailscale range).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

fn is_tailscale(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 100 && (64..128).contains(&o[1])
}

/// Whether `addr` is acceptable without an explicit override.
pub fn is_private(addr: &SocketAddr) -> bool {
    match addr.ip() {
        IpAddr::V4(ip) => ip.is_loopback() || ip.is_private() || is_tailscale(ip),
        IpAddr::V6(ip) => ip.is_loopback(),
    }
}

/// Whether the gateway sits behind `tailscale serve` (`AC_TAILSCALE_SERVE=1`, ADR-024), which
/// makes the `Tailscale-User-Login` header Serve's word, not the client's. Only on loopback:
/// on any other address a client reaches the gateway directly and writes the header itself.
pub fn behind_serve(addr: &SocketAddr, setting: &str) -> Result<bool, String> {
    match setting {
        "" | "0" => Ok(false),
        "1" if addr.ip().is_loopback() => Ok(true),
        "1" => Err(format!(
            "AC_TAILSCALE_SERVE=1 needs AC_BIND on loopback, not {addr}: tailscale serve \
             forwards to it there, and nothing else can reach it to forge who is calling"
        )),
        other => Err(format!("AC_TAILSCALE_SERVE: {other:?} is not 0 or 1")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn behind_serve_only_on_loopback() {
        let at = |s: &str| s.parse::<SocketAddr>().unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(behind_serve(&at("127.0.0.1:8780"), "1"), Ok(true));
        assert_eq!(behind_serve(&at("[::1]:8780"), "1"), Ok(true));
        assert_eq!(behind_serve(&at("100.64.0.2:8780"), ""), Ok(false));
        assert_eq!(behind_serve(&at("127.0.0.1:8780"), "0"), Ok(false));
        for (addr, setting) in [
            ("100.64.0.2:8780", "1"),
            ("10.0.0.1:1", "1"),
            ("127.0.0.1:1", "yes"),
        ] {
            assert!(
                behind_serve(&at(addr), setting).is_err(),
                "{addr} {setting}"
            );
        }
    }

    fn ok(s: &str) -> bool {
        s.parse().map(|a| is_private(&a)).unwrap_or(false)
    }

    #[test]
    fn private_ranges_allowed() {
        for s in [
            "127.0.0.1:1",
            "10.1.2.3:1",
            "192.168.0.5:1",
            "172.16.0.1:1",
            "100.64.0.1:1",
            "100.127.255.1:1",
            "[::1]:1",
        ] {
            assert!(ok(s), "{s}");
        }
    }

    #[test]
    fn public_and_wildcard_refused() {
        for s in [
            "0.0.0.0:1",
            "[::]:1",
            "8.8.8.8:1",
            "100.63.0.1:1",
            "100.128.0.1:1",
            "172.32.0.1:1",
        ] {
            assert!(!ok(s), "{s}");
        }
    }
}
