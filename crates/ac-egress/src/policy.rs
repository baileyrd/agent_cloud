//! The egress decision, sans I/O: who a request is from ([`identify`]), which `host:port` that
//! caller may reach ([`judge`]), and which resolved addresses the proxy may then connect to
//! ([`vet`]).

use std::collections::BTreeMap;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use ac_domain::allowlist::valid_host;
use ac_domain::{AgentId, Allowlist, Attachment, ProfileName};
use rusty_crypto_key::constant_time_eq;
use rusty_sha2::{Hash, Sha256};

use crate::auth::Presented;

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

/// What the proxy enforces (ADR-016): the base list every agent gets, the operator's profiles,
/// and each agent's credential digest and attached profiles (the agents file).
#[derive(Debug, Clone, Default)]
pub struct Policy {
    /// `/etc/agent-cloud/egress-allow.txt`.
    pub base: Allowlist,
    /// `/etc/agent-cloud/egress/profiles/<name>.txt`.
    pub profiles: BTreeMap<ProfileName, Allowlist>,
    /// `/etc/agent-cloud/egress/agents`.
    pub agents: Vec<Attachment>,
}

/// Who a request is from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caller {
    /// No credential (an agent booted before ADR-016, or a client that drops it): base only.
    Anonymous,
    /// A credential that matched: the base list plus the agent's profiles.
    Agent(AgentId),
}

impl Caller {
    /// For the log: the agent id, or `-`.
    pub fn label(&self) -> String {
        match self {
            Self::Anonymous => "-".to_owned(),
            Self::Agent(id) => id.to_string(),
        }
    }
}

/// Why a credential is refused. The proxy answers 407 and never falls back to the base list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// Not one well-formed `Basic` credential.
    Malformed(&'static str),
    /// No line for this agent in the agents file.
    UnknownAgent(AgentId),
    /// The agent has no credential now (its VM is not running, as far as the gateway knows).
    NoCredential(AgentId),
    /// The secret does not match (a stale credential from an earlier boot, or a guess).
    WrongSecret(AgentId),
}

impl Refused {
    /// For the log: the agent claimed, or `?` when there is none.
    pub fn label(&self) -> String {
        match self {
            Self::Malformed(_) => "?".to_owned(),
            Self::UnknownAgent(id) | Self::NoCredential(id) | Self::WrongSecret(id) => {
                format!("{id}?")
            }
        }
    }
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("proxy credential refused: ")?;
        match self {
            Self::Malformed(why) => f.write_str(why),
            Self::UnknownAgent(_) => f.write_str("unknown agent"),
            Self::NoCredential(_) => f.write_str("agent has no active credential"),
            Self::WrongSecret(_) => f.write_str("wrong secret"),
        }
    }
}

/// Who presented `presented`: [`Caller::Anonymous`] without a credential, the agent when its
/// secret's SHA-256 matches the agents file (compared in constant time), else refused.
pub fn identify(policy: &Policy, presented: &Presented) -> Result<Caller, Refused> {
    let (agent, secret) = match presented {
        Presented::None => return Ok(Caller::Anonymous),
        Presented::Malformed(why) => return Err(Refused::Malformed(why)),
        Presented::Basic { agent, secret } => (agent, secret),
    };
    let line = policy
        .agents
        .iter()
        .find(|a| &a.agent == agent)
        .ok_or_else(|| Refused::UnknownAgent(agent.clone()))?;
    let digest = line
        .digest
        .ok_or_else(|| Refused::NoCredential(agent.clone()))?;
    let presented = Sha256::digest(secret.expose().as_bytes());
    if !constant_time_eq(&presented, &digest) {
        return Err(Refused::WrongSecret(agent.clone()));
    }
    Ok(Caller::Agent(agent.clone()))
}

/// The list `caller` gets: the base list, plus each attached profile that exists. A profile
/// with no file adds nothing (the reader warns about it).
pub fn allowlist(policy: &Policy, caller: &Caller) -> Allowlist {
    let mut list = policy.base.clone();
    let Caller::Agent(id) = caller else {
        return list;
    };
    let attached = policy.agents.iter().find(|a| &a.agent == id);
    for name in attached.map_or(&[][..], |a| &a.profiles) {
        if let Some(profile) = policy.profiles.get(name) {
            list.merge(profile);
        }
    }
    list
}

/// Profiles that would allow `host`: for the server-side log of a deny, never the client.
pub fn would_allow<'a>(policy: &'a Policy, host: &str) -> Vec<&'a ProfileName> {
    policy
        .profiles
        .iter()
        .filter(|(_, list)| list.permits(host))
        .map(|(name, _)| name)
        .collect()
}

/// The decision for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The credential is refused: 407.
    Refused(Refused),
    /// The caller is known; may it reach the target?
    Decided(Caller, Decision),
}

/// Identify the caller, then decide `host:port` against its list.
pub fn judge(policy: &Policy, presented: &Presented, host: &str, port: u16) -> Verdict {
    match identify(policy, presented) {
        Err(refused) => Verdict::Refused(refused),
        Ok(caller) => {
            let decision = decide(host, port, &allowlist(policy, &caller));
            Verdict::Decided(caller, decision)
        }
    }
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
    use ac_domain::egress::hex;

    use super::*;
    use crate::auth::Secret;

    const S1: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const S2: &str = "2222222222222222222222222222222222222222222222222222222222222222";

    fn id(s: &str) -> AgentId {
        AgentId::parse(s).unwrap_or_else(|e| panic!("{e}"))
    }

    fn digest(secret: &str) -> [u8; 32] {
        Sha256::digest(secret.as_bytes())
    }

    fn profile(s: &str) -> ProfileName {
        ProfileName::parse(s).unwrap_or_else(|e| panic!("{e}"))
    }

    /// a1 has rust (and a profile with no file), a2 nothing, a3 no credential (paused).
    fn policy() -> Policy {
        let entries = |e: &[&str]| Allowlist::new(e.iter().copied()).unwrap_or_default();
        let line = |agent: &str, secret: Option<&str>, profiles: &[&str]| Attachment {
            agent: id(agent),
            digest: secret.map(digest),
            profiles: profiles.iter().map(|p| profile(p)).collect(),
        };
        Policy {
            base: entries(&["api.anthropic.com", "github.com"]),
            profiles: BTreeMap::from([
                (
                    profile("rust"),
                    entries(&["index.crates.io", "static.crates.io"]),
                ),
                (profile("node"), entries(&["registry.npmjs.org"])),
            ]),
            agents: vec![
                line("a1", Some(S1), &["missing", "rust"]),
                line("a2", Some(S2), &[]),
                line("a3", None, &["rust"]),
            ],
        }
    }

    fn basic(agent: &str, secret: &str) -> Presented {
        Presented::Basic {
            agent: id(agent),
            secret: Secret::new(secret.to_owned()),
        }
    }

    fn verdict(presented: &Presented, host: &str) -> Verdict {
        judge(&policy(), presented, host, 443)
    }

    #[test]
    fn no_credential_is_base_only() {
        let anon = Presented::None;
        let allowed = Verdict::Decided(Caller::Anonymous, Decision::Allow);
        assert_eq!(verdict(&anon, "api.anthropic.com"), allowed);
        assert_eq!(
            verdict(&anon, "index.crates.io"),
            Verdict::Decided(Caller::Anonymous, Decision::Deny(Reason::NotAllowed))
        );
    }

    #[test]
    fn a_matching_credential_gets_base_plus_its_profiles() {
        let a1 = basic("a1", S1);
        let me = || Caller::Agent(id("a1"));
        for host in ["api.anthropic.com", "index.crates.io", "static.crates.io"] {
            assert_eq!(
                verdict(&a1, host),
                Verdict::Decided(me(), Decision::Allow),
                "{host}"
            );
        }
        // Not attached (node), and the missing profile adds nothing.
        assert_eq!(
            verdict(&a1, "registry.npmjs.org"),
            Verdict::Decided(me(), Decision::Deny(Reason::NotAllowed))
        );
        assert_eq!(allowlist(&policy(), &me()).len(), 4);
        // a2 has no profiles: base only, but it is identified.
        assert_eq!(
            verdict(&basic("a2", S2), "index.crates.io"),
            Verdict::Decided(Caller::Agent(id("a2")), Decision::Deny(Reason::NotAllowed))
        );
        // The port and literal checks still apply to a widened list.
        assert_eq!(
            judge(&policy(), &a1, "index.crates.io", 22),
            Verdict::Decided(me(), Decision::Deny(Reason::Port(22)))
        );
    }

    #[test]
    fn a_bad_credential_is_refused_never_base() {
        let refused = |p: &Presented| match verdict(p, "api.anthropic.com") {
            Verdict::Refused(r) => r,
            other => panic!("{p:?} got {other:?}"),
        };
        assert_eq!(refused(&basic("a1", S2)), Refused::WrongSecret(id("a1")));
        assert_eq!(refused(&basic("a2", S1)), Refused::WrongSecret(id("a2")));
        assert_eq!(refused(&basic("a3", S1)), Refused::NoCredential(id("a3")));
        assert_eq!(refused(&basic("a9", S1)), Refused::UnknownAgent(id("a9")));
        let bad = Presented::Malformed("Proxy-Authorization: no colon");
        assert_eq!(
            refused(&bad),
            Refused::Malformed("Proxy-Authorization: no colon")
        );
        assert_eq!(Refused::WrongSecret(id("a1")).label(), "a1?");
        assert_eq!(
            Refused::NoCredential(id("a3")).to_string(),
            "proxy credential refused: agent has no active credential"
        );
    }

    #[test]
    fn the_secret_is_compared_by_digest() {
        // The file holds the digest; presenting the digest itself is not the secret.
        assert!(matches!(
            verdict(&basic("a1", &hex(&digest(S1))), "github.com"),
            Verdict::Refused(Refused::WrongSecret(_))
        ));
    }

    #[test]
    fn would_allow_names_profiles_for_the_log() {
        let p = policy();
        let names: Vec<&str> = would_allow(&p, "static.crates.io")
            .into_iter()
            .map(ProfileName::as_str)
            .collect();
        assert_eq!(names, ["rust"]);
        assert!(would_allow(&p, "example.com").is_empty());
    }

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
