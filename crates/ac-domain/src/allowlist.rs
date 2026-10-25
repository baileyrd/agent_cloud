//! Egress allowlist: which hosts an agent may reach.

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

/// Exact hostnames, plus `*.suffix` entries matching any subdomain (not the apex).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Allowlist {
    exact: Vec<String>,
    suffixes: Vec<String>,
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

    /// Parse the file format: one entry per line, `#` starts a comment, blank lines skipped.
    pub fn parse_file(text: &str) -> Result<Self, AllowlistError> {
        Self::new(
            text.lines()
                .map(|l| l.split('#').next().unwrap_or("").trim())
                .filter(|l| !l.is_empty()),
        )
    }

    /// How many entries, exact and wildcard.
    pub fn len(&self) -> usize {
        self.exact.len() + self.suffixes.len()
    }

    /// Whether there are no entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Add every entry of `other`.
    pub fn merge(&mut self, other: &Allowlist) {
        self.exact.extend(other.exact.iter().cloned());
        self.suffixes.extend(other.suffixes.iter().cloned());
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
        let extra = Allowlist::new(["index.crates.io", "*.pythonhosted.org"]).unwrap_or_default();
        assert!(!l.permits("index.crates.io"));
        l.merge(&extra);
        assert_eq!(l.len(), 4);
        assert!(l.permits("index.crates.io") && l.permits("files.pythonhosted.org"));
        assert!(l.permits("api.anthropic.com") && !l.permits("pythonhosted.org"));
    }

    #[test]
    fn rejects_bad_entries() {
        for e in ["", "*.", "*", "a..b", "-a.com", "a b.com", "http://x.com"] {
            assert!(Allowlist::new([e]).is_err(), "{e:?}");
        }
    }
}
