//! The egress decision, sans I/O: which `host:port` an agent may reach, and which resolved
//! addresses the proxy may then connect to.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use ac_domain::allowlist::valid_host;
use ac_domain::Allowlist;

/// Ports an agent may reach: HTTPS and HTTP.
pub const PORTS: [u16; 2] = [443, 80];

/// Why a request is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// Port other than those in [`PORTS`].
    Port(u16),
    /// The target is an IP address, not a hostname.
    IpLiteral,
    /// Not a syntactically valid hostname.
    BadHost,
    /// Hostname not on the allowlist.
    NotAllowed,
    /// The name could not be resolved.
    Lookup,
    /// The name resolved to nothing.
    NoAddress,
    /// The name resolved to a non-public address.
    Private(IpAddr),
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Port(p) => write!(f, "port {p} not allowed"),
            Self::IpLiteral => f.write_str("IP literals not allowed"),
            Self::BadHost => f.write_str("invalid hostname"),
            Self::NotAllowed => f.write_str("not on allowlist"),
            Self::Lookup => f.write_str("DNS lookup failed"),
            Self::NoAddress => f.write_str("no address"),
            Self::Private(ip) => write!(f, "resolves to non-public address {ip}"),
        }
    }
}

/// Outcome of [`decide`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Resolve the name and connect (after [`vet`]).
    Allow,
    /// Refuse.
    Deny(Reason),
}

/// Whether an agent may reach `host:port`. `host` must be a name on `list`: IP literals are
/// refused, including forms like `127.1` or `2130706433` that resolvers also read as addresses.
pub fn decide(host: &str, port: u16, list: &Allowlist) -> Decision {
    let deny = Decision::Deny;
    if !PORTS.contains(&port) {
        return deny(Reason::Port(port));
    }
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.parse::<IpAddr>().is_ok() {
        return deny(Reason::IpLiteral);
    }
    if !valid_host(host) {
        return deny(Reason::BadHost);
    }
    // No top-level domain is numeric; a numeric last label means an address in disguise.
    if host
        .rsplit('.')
        .next()
        .is_some_and(|tld| tld.bytes().all(|b| b.is_ascii_digit()))
    {
        return deny(Reason::IpLiteral);
    }
    if !list.permits(host) {
        return deny(Reason::NotAllowed);
    }
    Decision::Allow
}

/// The addresses the proxy may connect to: all of `addrs`, IPv4 first, if every one is public.
/// One non-public address refuses the lot: an allowed name that points into the LAN is hostile.
pub fn vet(mut addrs: Vec<SocketAddr>) -> Result<Vec<SocketAddr>, Reason> {
    if let Some(bad) = addrs.iter().find(|a| !is_public(a.ip())) {
        return Err(Reason::Private(bad.ip()));
    }
    if addrs.is_empty() {
        return Err(Reason::NoAddress);
    }
    addrs.sort_by_key(SocketAddr::is_ipv6);
    Ok(addrs)
}

/// Whether `ip` is a public unicast address: not private, loopback, link-local, CGNAT (the
/// tailnet), multicast, documentation, benchmarking or reserved.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => public_v4(v4),
        IpAddr::V6(v6) => public_v6(v6),
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || ip.is_multicast()
        || a == 0
        || (a == 100 && (64..128).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 198 && (b == 18 || b == 19))
        || a >= 240)
}

/// Global unicast (2000::/3) only, minus documentation and the ranges that tunnel to or embed
/// other addresses (Teredo, 6to4). Mapped and NAT64 forms fall outside 2000::/3.
fn public_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    (s[0] & 0xe000) == 0x2000
        && !(s[0] == 0x2001 && (s[1] == 0x0db8 || s[1] == 0))
        && s[0] != 0x2002
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list() -> Allowlist {
        Allowlist::new(["api.anthropic.com", "*.githubusercontent.com", "localhost"])
            .unwrap_or_else(|e| panic!("{e}"))
    }

    fn reason(host: &str, port: u16) -> Option<Reason> {
        match decide(host, port, &list()) {
            Decision::Allow => None,
            Decision::Deny(r) => Some(r),
        }
    }

    #[test]
    fn allows_listed_names_on_https_and_http() {
        assert_eq!(reason("api.anthropic.com", 443), None);
        assert_eq!(reason("API.anthropic.com.", 80), None);
        assert_eq!(reason("raw.githubusercontent.com", 443), None);
    }

    #[test]
    fn denies_other_ports_names_and_literals() {
        assert_eq!(reason("api.anthropic.com", 22), Some(Reason::Port(22)));
        assert_eq!(reason("api.anthropic.com", 8443), Some(Reason::Port(8443)));
        assert_eq!(reason("example.com", 443), Some(Reason::NotAllowed));
        assert_eq!(
            reason("githubusercontent.com", 443),
            Some(Reason::NotAllowed)
        );
        for literal in [
            "1.1.1.1",
            "::1",
            "127.1",
            "2130706433",
            "0x7f.1",
            "10.0.0.1.",
        ] {
            assert_eq!(reason(literal, 443), Some(Reason::IpLiteral), "{literal}");
        }
        for bad in [
            "", "a..b", "a b.com", "a_b.com", "-a.com", "a.com..", "é.com",
        ] {
            assert_eq!(reason(bad, 443), Some(Reason::BadHost), "{bad:?}");
        }
    }

    #[test]
    fn localhost_on_the_list_still_cannot_reach_loopback() {
        // The name passes decide (someone listed it), but vet refuses what it resolves to.
        assert_eq!(reason("localhost", 443), None);
        let resolved = vec![SocketAddr::from(([127, 0, 0, 1], 443))];
        assert_eq!(vet(resolved), Err(Reason::Private([127, 0, 0, 1].into())));
    }

    #[test]
    fn vet_refuses_any_private_result_and_orders_ipv4_first() {
        let v6: SocketAddr = "[2606:4700::1]:443"
            .parse()
            .unwrap_or_else(|e| panic!("{e}"));
        let v4 = SocketAddr::from(([160, 79, 104, 10], 443));
        assert_eq!(vet(vec![v6, v4]), Ok(vec![v4, v6]));
        let lan = SocketAddr::from(([192, 168, 50, 1], 443));
        assert_eq!(vet(vec![v4, lan]), Err(Reason::Private(lan.ip())));
        assert_eq!(vet(Vec::new()), Err(Reason::NoAddress));
    }

    #[test]
    fn public_address_table() {
        let public = [
            "1.1.1.1",
            "160.79.104.10",
            "100.63.0.1",
            "2606:4700::1",
            "2a00:1450::1",
        ];
        let private = [
            "10.89.0.1",
            "172.16.0.1",
            "192.168.50.1",
            "127.0.0.53",
            "169.254.169.254",
            "100.64.0.1",
            "100.127.255.255",
            "0.0.0.0",
            "0.1.2.3",
            "192.0.0.1",
            "192.0.2.1",
            "198.18.0.1",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "::ffff:192.168.0.1",
            "64:ff9b::a00:1",
            "fe80::1",
            "fd00::1",
            "ff02::1",
            "2001:db8::1",
            "2001:0:4136:e378::1",
            "2002:c0a8:101::1",
        ];
        for ip in public {
            assert!(
                is_public(ip.parse().unwrap_or_else(|e| panic!("{e}"))),
                "{ip}"
            );
        }
        for ip in private {
            assert!(
                !is_public(ip.parse().unwrap_or_else(|e| panic!("{e}"))),
                "{ip}"
            );
        }
    }
}
