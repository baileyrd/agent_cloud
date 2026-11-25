//! Egress allowlist: which hosts an agent may reach.
//!
//! The file format also has two directives, for profiles (ADR-016 addendum): `endpoint <host>:<port>`
//! allows exactly that host on exactly that port, also where it resolves to a private address (the
//! tailnet, the LAN); `!exclusive` makes an agent that has the profile attached lose the base list,
//! so it reaches only what its profiles allow.

use std::fmt;

/// Invalid allowlist entry.
#[derive(Debug, PartialEq, Eq)]
pub struct AllowlistError(pub String);

impl fmt::Display for AllowlistError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid allowlist entry {:?}", self.0)
    }
}

impl std::error::Error for AllowlistError {}

/// One host on one port, which may resolve to a private address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// Lowercase hostname.
    pub host: String,
    /// The only port allowed.
    pub port: u16,
}

impl Endpoint {
    /// Parse `host:port`: a valid hostname (no IP literal, no numeric last label) and a port
    /// from 1 to 65535.
    pub fn parse(raw: &str) -> Result<Self, AllowlistError> {
        let bad = || AllowlistError(raw.to_owned());
        let (host, port) = raw.trim().rsplit_once(':').ok_or_else(bad)?;
        let host = host.to_ascii_lowercase();
        let numeric_tld = host
            .rsplit('.')
            .next()
            .is_some_and(|t| t.bytes().all(|b| b.is_ascii_digit()));
        if !valid_host(&host) || numeric_tld {
            return Err(bad());
        }
        match port.parse::<u16>() {
            Ok(port) if port > 0 => Ok(Self { host, port }),
            _ => Err(bad()),
        }
    }
}

/// Exact hostnames, plus `*.suffix` entries matching any subdomain (not the apex), plus
/// [`Endpoint`]s, and whether it is exclusive (see the module docs).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Allowlist {
    exact: Vec<String>,
    suffixes: Vec<String>,
    endpoints: Vec<Endpoint>,
    exclusive: bool,
}

impl Allowlist {
    /// Build from entries like `api.anthropic.com` or `*.github.com`.
    pub fn new<'a>(entries: impl IntoIterator<Item = &'a str>) -> Result<Self, AllowlistError> {
        let mut list = Self::default();
        for raw in entries {
            let e = raw.trim().to_ascii_lowercase();
            let (target, host) = match e.strip_prefix("*.") {
                Some(rest) => (&mut list.suffixes, rest),
                None => (&mut list.exact, e.as_str()),
            };
            if !valid_host(host) {
                return Err(AllowlistError(raw.to_owned()));
            }
            target.push(host.to_owned());
        }
        Ok(list)
    }

    /// Parse the file format: one entry per line, `#` starts a comment, blank lines skipped;
    /// `endpoint <host>:<port>` and `!exclusive` lines are directives.
    pub fn parse_file(text: &str) -> Result<Self, AllowlistError> {
        let lines = text
            .lines()
            .map(|l| l.split('#').next().unwrap_or("").trim())
            .filter(|l| !l.is_empty());
        let (mut endpoints, mut exclusive, mut hosts) = (Vec::new(), false, Vec::new());
        for line in lines {
            if line == "!exclusive" {
                exclusive = true;
            } else if let Some(rest) = line.strip_prefix("endpoint ") {
                endpoints.push(Endpoint::parse(rest)?);
            } else {
                hosts.push(line);
            }
        }
        let mut list = Self::new(hosts)?;
        list.endpoints = endpoints;
        list.exclusive = exclusive;
        Ok(list)
    }

    /// Parse the base allowlist file: [`Self::parse_file`] without its profile-only directives.
    /// An `endpoint` there would give every agent (and callers with no credential) a private
    /// address, and `!exclusive` means nothing, so both are refused.
    pub fn parse_base(text: &str) -> Result<Self, AllowlistError> {
        let list = Self::parse_file(text)?;
        if let Some(e) = list.endpoints.first() {
            let line = format!("endpoint {}:{} (only in a profile)", e.host, e.port);
            return Err(AllowlistError(line));
        }
        if list.exclusive {
            return Err(AllowlistError("!exclusive (only in a profile)".into()));
        }
        Ok(list)
    }

    /// How many entries: exact, wildcard and endpoints.
    pub fn len(&self) -> usize {
        self.exact.len() + self.suffixes.len() + self.endpoints.len()
    }

    /// Whether an agent with this profile loses the base list.
    pub fn exclusive(&self) -> bool {
        self.exclusive
    }

    /// Whether `host:port` is one of the endpoints.
    pub fn endpoint(&self, host: &str, port: u16) -> bool {
        let h = host.trim_end_matches('.').to_ascii_lowercase();
        self.endpoints.iter().any(|e| e.host == h && e.port == port)
    }

    /// Whether there are no entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Add every entry of `other`, its endpoints too. Whether this list is exclusive does not
    /// change: that is a property of a profile, read before lists are merged.
    pub fn merge(&mut self, other: &Allowlist) {
        self.exact.extend(other.exact.iter().cloned());
        self.suffixes.extend(other.suffixes.iter().cloned());
        self.endpoints.extend(other.endpoints.iter().cloned());
    }

    /// Whether `host` may be reached. Empty list denies everything.
    pub fn permits(&self, host: &str) -> bool {
        let h = host.trim_end_matches('.').to_ascii_lowercase();
        self.exact.contains(&h)
            || self
                .suffixes
                .iter()
                .any(|s| h.strip_suffix(s.as_str()).is_some_and(|p| p.ends_with('.')))
    }
}

/// Whether `h` is a syntactically valid DNS hostname: dot-separated labels of letters, digits
/// and inner hyphens, at most 63 bytes each and 253 in all. No trailing dot.
pub fn valid_host(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 253
        && h.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list() -> Allowlist {
        Allowlist::new(["api.anthropic.com", "*.github.com"]).unwrap_or_default()
    }

    #[test]
    fn exact_and_wildcard() {
        let l = list();
        assert!(l.permits("api.anthropic.com"));
        assert!(l.permits("API.Anthropic.com."));
        assert!(l.permits("raw.github.com"));
        assert!(l.permits("a.b.github.com"));
    }

    #[test]
    fn denies_lookalikes_and_apex() {
        let l = list();
        assert!(!l.permits("github.com"));
        assert!(!l.permits("evilgithub.com"));
        assert!(!l.permits("api.anthropic.com.evil.io"));
        assert!(!l.permits("anthropic.com"));
    }

    #[test]
    fn empty_denies_all() {
        assert!(!Allowlist::default().permits("example.com"));
    }

    #[test]
    fn file_format_and_merge() {
        let mut l = Allowlist::parse_file("# base\n\napi.anthropic.com  # model\n  *.github.com\n")
            .unwrap_or_default();
        assert_eq!(l.len(), 2);
        assert!(Allowlist::parse_file("ok.com\nhttp://bad\n").is_err());
        assert!(Allowlist::parse_file("# nothing\n").is_ok_and(|l| l.is_empty()));
        assert!(Allowlist::parse_base("api.anthropic.com\n").is_ok());
        for bad in ["endpoint mcp.test:8443\n", "a.com\n!exclusive\n"] {
            let e = Allowlist::parse_base(bad).err().map(|e| e.to_string());
            assert!(e.is_some_and(|e| e.contains("only in a profile")), "{bad}");
        }
        let extra = Allowlist::new(["index.crates.io", "*.pythonhosted.org"]).unwrap_or_default();
        assert!(!l.permits("index.crates.io"));
        l.merge(&extra);
        assert_eq!(l.len(), 4);
        assert!(l.permits("index.crates.io") && l.permits("files.pythonhosted.org"));
        assert!(l.permits("api.anthropic.com") && !l.permits("pythonhosted.org"));
    }

    #[test]
    fn endpoints_and_exclusive_directives() {
        let l = Allowlist::parse_file(
            "!exclusive  # gateway only\nendpoint MCP.tail.ts.net:8443\nmirror.example\n",
        )
        .unwrap_or_default();
        assert!(l.exclusive());
        assert_eq!(l.len(), 2);
        assert!(l.endpoint("mcp.tail.ts.net", 8443) && l.endpoint("mcp.tail.ts.net.", 8443));
        assert!(!l.endpoint("mcp.tail.ts.net", 443));
        assert!(
            !l.permits("mcp.tail.ts.net"),
            "an endpoint is not a host entry"
        );
        assert!(l.permits("mirror.example"));
        let mut base = Allowlist::parse_file("api.anthropic.com\n").unwrap_or_default();
        assert!(!base.exclusive());
        base.merge(&l);
        assert!(!base.exclusive() && base.endpoint("mcp.tail.ts.net", 8443));
        for bad in [
            "endpoint mcp.tail.ts.net",
            "endpoint mcp.tail.ts.net:0",
            "endpoint mcp.tail.ts.net:70000",
            "endpoint 100.64.0.2:8443",
            "endpoint [::1]:8443",
            "endpoint *.ts.net:8443",
            "endpoint http://x:1",
            "!exclusive extra",
        ] {
            assert!(Allowlist::parse_file(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn rejects_bad_entries() {
        for e in ["", "*.", "*", "a..b", "-a.com", "a b.com", "http://x.com"] {
            assert!(Allowlist::new([e]).is_err(), "{e:?}");
        }
    }
}
