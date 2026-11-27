//! Control-plane state backups (ADR-021 addendum) as pure rules. Next to the workspaces, a
//! backup keeps what a lost host needs to come back with the same agents and tokens: the agent
//! list with each agent's state, the token digests file, the egress agents file and each agent's
//! recorded image. A state
//! backup is a directory `<dest>/_state/<id>/` holding those files (the ones the host has) and a
//! `MANIFEST` with each file's size and SHA-256. These rules name the files and read and write the
//! agent list and the manifest; hashing and file I/O are the caller's.

use std::fmt;

use crate::{AgentId, State};

/// The directory under the backup destination. Not a valid agent id, so it never collides with
/// an agent's stream directory.
pub const DIR: &str = "_state";
/// The agent list.
pub const AGENTS: &str = "agents";
/// The token digests file (ADR-011).
pub const TOKENS: &str = "tokens";
/// The egress agents file (ADR-016).
pub const EGRESS: &str = "egress-agents";
/// Each agent's recorded image (card #1627). Older backups lack it; their agents boot on the
/// host's default image.
pub const IMAGES: &str = "images";
/// The manifest.
pub const MANIFEST: &str = "MANIFEST";
/// Every file a state backup may hold besides the manifest.
pub const FILES: [&str; 4] = [AGENTS, TOKENS, EGRESS, IMAGES];

/// Why a state backup could not be read or restored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateError {
    /// A line of the agent list is not `<agent-id> <state>`.
    BadAgentLine(usize),
    /// A line of the manifest is not `<sha256> <bytes> <file>` for a known file, or names one
    /// twice.
    BadManifestLine(usize),
    /// The manifest has no agent list.
    NoAgents,
    /// A file does not match its manifest entry.
    Mismatch(String),
}

impl fmt::Display for StateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadAgentLine(n) => write!(f, "agent list line {n} is not `<agent-id> <state>`"),
            Self::BadManifestLine(n) => write!(f, "manifest line {n} is malformed"),
            Self::NoAgents => f.write_str("the manifest lists no agent list"),
            Self::Mismatch(name) => write!(f, "{name} does not match its manifest entry"),
        }
    }
}

impl std::error::Error for StateError {}

/// A state's name in the agent list.
pub fn state_name(state: State) -> &'static str {
    match state {
        State::Creating => "creating",
        State::Running => "running",
        State::Paused => "paused",
        State::Destroyed => "destroyed",
    }
}

fn state_from(name: &str) -> Option<State> {
    Some(match name {
        "creating" => State::Creating,
        "running" => State::Running,
        "paused" => State::Paused,
        "destroyed" => State::Destroyed,
        _ => return None,
    })
}

/// The agent list: one `<agent-id> <state>` line per agent, sorted by id.
pub fn render_agents(agents: &[(AgentId, State)]) -> String {
    let mut sorted: Vec<&(AgentId, State)> = agents.iter().collect();
    sorted.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    sorted
        .iter()
        .map(|(id, s)| format!("{id} {}\n", state_name(*s)))
        .collect()
}

/// Read an agent list written by [`render_agents`]; blank lines are skipped.
pub fn parse_agents(text: &str) -> Result<Vec<(AgentId, State)>, StateError> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let bad = || StateError::BadAgentLine(i + 1);
        let (id, state) = line.split_once(' ').ok_or_else(bad)?;
        let id = AgentId::parse(id).map_err(|_| bad())?;
        out.push((id, state_from(state).ok_or_else(bad)?));
    }
    Ok(out)
}

/// One file of a state backup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// One of [`FILES`].
    pub name: &'static str,
    /// Its size.
    pub bytes: u64,
    /// Its SHA-256, 64 lowercase hex digits.
    pub sha256: String,
}

/// The manifest: one `<sha256> <bytes> <file>` line per entry.
pub fn render_manifest(entries: &[Entry]) -> String {
    entries
        .iter()
        .map(|e| format!("{} {} {}\n", e.sha256, e.bytes, e.name))
        .collect()
}

/// Read a manifest written by [`render_manifest`]. It must list the agent list; each file once.
pub fn parse_manifest(text: &str) -> Result<Vec<Entry>, StateError> {
    let mut out: Vec<Entry> = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let bad = || StateError::BadManifestLine(i + 1);
        let mut parts = line.split(' ');
        let (Some(sha256), Some(bytes), Some(name), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(bad());
        };
        let hex = sha256.len() == 64
            && sha256
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c));
        let name = FILES.into_iter().find(|f| *f == name).ok_or_else(bad)?;
        let bytes = bytes.parse::<u64>().map_err(|_| bad())?;
        if !hex || out.iter().any(|e| e.name == name) {
            return Err(bad());
        }
        out.push(Entry {
            name,
            bytes,
            sha256: sha256.to_owned(),
        });
    }
    match out.iter().any(|e| e.name == AGENTS) {
        true => Ok(out),
        false => Err(StateError::NoAgents),
    }
}

/// Check a file read back against its entry.
pub fn verify(entry: &Entry, bytes: u64, sha256: &str) -> Result<(), StateError> {
    match entry.bytes == bytes && entry.sha256 == sha256 {
        true => Ok(()),
        false => Err(StateError::Mismatch(entry.name.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(s: &str) -> AgentId {
        AgentId::parse(s).unwrap_or_else(|e| panic!("{e}"))
    }

    const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn entry(name: &'static str) -> Entry {
        Entry {
            name,
            bytes: 12,
            sha256: SHA.to_owned(),
        }
    }

    #[test]
    fn agent_list_round_trips_sorted() {
        let all = [
            (a("b2"), State::Paused),
            (a("a1"), State::Running),
            (a("c3"), State::Destroyed),
            (a("d4"), State::Creating),
        ];
        let text = render_agents(&all);
        assert_eq!(text, "a1 running\nb2 paused\nc3 destroyed\nd4 creating\n");
        let mut back = parse_agents(&text).unwrap_or_else(|e| panic!("{e}"));
        back.sort_by(|x, y| x.0.as_str().cmp(y.0.as_str()));
        let mut want = all.to_vec();
        want.sort_by(|x, y| x.0.as_str().cmp(y.0.as_str()));
        assert_eq!(back, want);
        assert_eq!(parse_agents(""), Ok(Vec::new()));
    }

    #[test]
    fn bad_agent_lines_are_refused_by_number() {
        for (text, line) in [
            ("a1 running\nBAD running\n", 2),
            ("a1 asleep\n", 1),
            ("a1\n", 1),
            ("\na1 running extra\n", 2),
        ] {
            assert_eq!(
                parse_agents(text),
                Err(StateError::BadAgentLine(line)),
                "{text}"
            );
        }
    }

    #[test]
    fn manifest_round_trips() {
        let all = vec![entry(AGENTS), entry(TOKENS), entry(EGRESS)];
        assert_eq!(parse_manifest(&render_manifest(&all)), Ok(all));
        assert_eq!(
            parse_manifest(&render_manifest(&[entry(AGENTS)])).map(|v| v.len()),
            Ok(1)
        );
    }

    #[test]
    fn bad_manifests_are_refused() {
        let line = |name: &str| format!("{SHA} 12 {name}\n");
        let short = format!("{} 12 agents\n", &SHA[..63]);
        let upper = format!("{} 12 agents\n", SHA.to_uppercase());
        let twice = format!("{}{}", line(AGENTS), line(AGENTS));
        for text in [
            line("passwd"),
            short,
            upper,
            format!("{SHA} -1 agents\n"),
            format!("{SHA} 12 agents x\n"),
            twice,
        ] {
            assert!(
                matches!(parse_manifest(&text), Err(StateError::BadManifestLine(_))),
                "{text}"
            );
        }
        assert_eq!(parse_manifest(&line(TOKENS)), Err(StateError::NoAgents));
    }

    #[test]
    fn verify_needs_size_and_digest() {
        let e = entry(TOKENS);
        assert_eq!(verify(&e, 12, SHA), Ok(()));
        assert!(verify(&e, 13, SHA).is_err());
        assert_eq!(
            verify(&e, 12, &SHA.replace('0', "1")),
            Err(StateError::Mismatch("tokens".into()))
        );
    }

    #[test]
    fn the_state_dir_is_never_an_agent() {
        assert!(AgentId::parse(DIR).is_err());
    }
}
