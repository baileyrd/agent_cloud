//! Per-agent egress (ADR-016): named profiles that add hosts to an agent's allowlist, and the
//! agents file that says which profiles each agent has and the digest of its current proxy
//! credential. Text formats only; reading and writing the files is the adapters' job.
//!
//! The agents file has one line per agent, `<agent> <sha256 hex or -> <profile,profile or ->`;
//! `#` starts a comment. The gateway writes it; the proxy only reads it.

use std::fmt;

use crate::id::{self, AgentId};

/// The marker for "none" in the agents file: no credential, no profiles.
pub const NONE: &str = "-";

/// Written at the top of the agents file.
pub const HEADER: &str =
    "# agent-cloud egress (ADR-016): agent sha256(proxy secret)|- profile,...|-\n\
     # Written by ac-gateway; attach profiles with PUT /api/v1/agents/<id>/egress.\n";

/// A profile name: the agent-id rule, so `-` (the file's "none") can never be one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProfileName(String);

/// A name that is not a valid profile name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadProfile(pub String);

impl fmt::Display for BadProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "profile name {:?}: want 1 to {} of [a-z0-9-], not starting or ending with -",
            self.0.chars().take(40).collect::<String>(),
            id::MAX_LEN
        )
    }
}

impl std::error::Error for BadProfile {}

impl ProfileName {
    /// Parse and validate external input.
    pub fn parse(s: &str) -> Result<Self, BadProfile> {
        id::check(s)
            .map(|()| Self(s.to_owned()))
            .map_err(|_| BadProfile(s.to_owned()))
    }

    /// The validated string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProfileName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Names, sorted and without duplicates.
pub fn profile_set(names: impl IntoIterator<Item = ProfileName>) -> Vec<ProfileName> {
    let mut v: Vec<ProfileName> = names.into_iter().collect();
    v.sort();
    v.dedup();
    v
}

/// One agent's line: its credential's digest (`None`: no VM, so no credential) and profiles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    /// The agent.
    pub agent: AgentId,
    /// SHA-256 of the secret of the credential its running VM holds.
    pub digest: Option<[u8; 32]>,
    /// Attached profiles, sorted, no duplicates.
    pub profiles: Vec<ProfileName>,
}

impl Attachment {
    /// An agent with nothing: base list only, no credential.
    pub fn bare(agent: AgentId) -> Self {
        Self {
            agent,
            digest: None,
            profiles: Vec::new(),
        }
    }
}

/// Parse the agents file. Strict: a malformed line or a repeated agent refuses the whole file,
/// naming the line, so a reader keeps what it had rather than guess.
pub fn parse_agents(text: &str) -> Result<Vec<Attachment>, String> {
    let mut out: Vec<Attachment> = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("");
        if line.trim().is_empty() {
            continue;
        }
        let a = parse_line(line).map_err(|e| format!("line {}: {e}", n + 1))?;
        if out.iter().any(|b| b.agent == a.agent) {
            return Err(format!("line {}: agent {} listed twice", n + 1, a.agent));
        }
        out.push(a);
    }
    Ok(out)
}

fn parse_line(line: &str) -> Result<Attachment, String> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let [agent, digest, profiles] = fields[..] else {
        return Err("want: agent sha256|- profile,...|-".into());
    };
    let agent = AgentId::parse(agent).map_err(|e| e.to_string())?;
    let digest = match digest {
        NONE => None,
        d => Some(unhex32(d).ok_or("digest: want 64 lowercase hex digits or -")?),
    };
    Ok(Attachment {
        agent,
        digest,
        profiles: parse_profiles(profiles).map_err(|e| e.to_string())?,
    })
}

/// A comma list of profile names, or `-` for none. Duplicates are refused.
pub fn parse_profiles(list: &str) -> Result<Vec<ProfileName>, BadProfile> {
    if list == NONE {
        return Ok(Vec::new());
    }
    let names = list
        .split(',')
        .map(ProfileName::parse)
        .collect::<Result<Vec<_>, _>>()?;
    let set = profile_set(names.iter().cloned());
    if set.len() != names.len() {
        return Err(BadProfile(list.to_owned()));
    }
    Ok(set)
}

/// The agents file for `all`: the header, then one line per agent, sorted.
pub fn render_agents(all: &[Attachment]) -> String {
    let mut sorted: Vec<&Attachment> = all.iter().collect();
    sorted.sort_by(|a, b| a.agent.as_str().cmp(b.agent.as_str()));
    let mut out = HEADER.to_owned();
    for a in sorted {
        let digest = a.digest.map_or_else(|| NONE.to_owned(), |d| hex(&d));
        let profiles = match a.profiles.is_empty() {
            true => NONE.to_owned(),
            false => names(&a.profiles),
        };
        out.push_str(&format!("{} {digest} {profiles}\n", a.agent));
    }
    out
}

/// `rust,python`; empty for none.
pub fn names(profiles: &[ProfileName]) -> String {
    profiles
        .iter()
        .map(ProfileName::as_str)
        .collect::<Vec<_>>()
        .join(",")
}

/// Lowercase hex.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 64 lowercase hex digits as 32 bytes.
pub fn unhex32(s: &str) -> Option<[u8; 32]> {
    let digit = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    let s = s.as_bytes();
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (o, pair) in out.iter_mut().zip(s.chunks(2)) {
        *o = digit(pair[0])? << 4 | digit(pair[1])?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

    fn p(s: &str) -> ProfileName {
        ProfileName::parse(s).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn profile_names() {
        for ok in ["rust", "python", "node-22", "a", &"x".repeat(32)] {
            assert!(ProfileName::parse(ok).is_ok(), "{ok}");
        }
        for bad in [
            "-",
            "",
            "Rust",
            "a_b",
            "-a",
            "a-",
            "../x",
            "a b",
            "a,b",
            &"x".repeat(33),
            "é",
        ] {
            assert!(ProfileName::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn profile_lists() {
        assert_eq!(parse_profiles("-"), Ok(vec![]));
        assert_eq!(parse_profiles("rust,node"), Ok(vec![p("node"), p("rust")]));
        for bad in [
            "",
            ",",
            "rust,",
            "rust,rust",
            "rust, node",
            "-,rust",
            "rust,-",
        ] {
            assert!(parse_profiles(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn agents_file_round_trip() {
        let text = format!("# c\n\na2 - -\na1 {D} rust,python  # live\n");
        let all = parse_agents(&text).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(all.len(), 2);
        assert_eq!(all[1].digest.map(|d| hex(&d)).as_deref(), Some(D));
        assert_eq!(all[1].profiles, [p("python"), p("rust")]);
        let rendered = render_agents(&all);
        assert!(rendered.starts_with(HEADER));
        assert!(rendered.ends_with(&format!("a1 {D} python,rust\na2 - -\n")));
        assert_eq!(
            parse_agents(&rendered),
            Ok(vec![all[1].clone(), all[0].clone()])
        );
        assert_eq!(parse_agents(""), Ok(vec![]));
    }

    #[test]
    fn agents_file_is_strict() {
        let upper = D.to_uppercase();
        for (bad, why) in [
            ("a1 - -\na1 - rust\n", "line 2: agent a1 listed twice"),
            ("a1 -\n", "line 1: want"),
            ("a1 - - extra\n", "line 1: want"),
            ("A1 - -\n", "line 1: id must match"),
            ("a1 abc -\n", "line 1: digest"),
            (&format!("a1 {upper} -\n"), "line 1: digest"),
            (&format!("a1 {D}0 -\n"), "line 1: digest"),
            ("a1 - Rust\n", "line 1: profile name"),
            ("a1 - rust,rust\n", "line 1: profile name"),
            ("a1 - ,\n", "line 1: profile name"),
        ] {
            let err = parse_agents(bad).err().unwrap_or_default();
            assert!(err.starts_with(why), "{bad:?}: {err}");
        }
    }
}
