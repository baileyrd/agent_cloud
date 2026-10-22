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

#[cfg(test)]
mod tests {
    use super::*;

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
