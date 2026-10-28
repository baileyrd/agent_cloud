//! API tokens (ADR-011): which routes a caller may use, for which agents.
//!
//! The tokens file (`/etc/agent-cloud/tokens`, owned by the gateway's user, mode 0600) has one
//! token per line, `name scopes agents sha256 [created [expires]]`; `#` starts a comment. `scopes`
//! is a comma list of [`Scope`] names, `agents` is `*` or a comma list of agent ids, `sha256` is the
//! hex digest of the token (the token itself is shown once, by `ac-gateway token create`, and never
//! stored), `created` and `expires` are Unix seconds (ADR-022: from `expires` on, the token is
//! refused; without it, the token lasts until revoked). The old single token (`AC_TOKEN_FILE`) is
//! an implicit `admin` token for every agent named `legacy`.
//!
//! The file is re-read when it changes (as ac-egress's allowlist is): a missing file means no file
//! tokens; an unreadable, too open or malformed one is an error, so the gateway refuses to start
//! with it and keeps its previous tokens if it appears later.

use std::fmt;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use ac_domain::AgentId;
use rusty_crypto_key::constant_time_eq;
use rusty_sha2::{Hash, Sha256};

/// Largest tokens file accepted.
const MAX_BYTES: u64 = 64 * 1024;
/// Longest token name.
const MAX_NAME: usize = 32;
/// The implicit token from `AC_TOKEN_FILE`.
pub const LEGACY: &str = "legacy";
/// Written at the top of a new tokens file.
const HEADER: &str =
    "# agent-cloud API tokens (ADR-011, ADR-022): name scopes agents sha256(token) created [expires]\n\
    # Managed with `ac-gateway token create|list|revoke`; tokens themselves are never stored.\n";

/// What a token may do. `Admin` implies every other scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Agent status and the list.
    AgentsRead,
    /// Create, pause, resume, destroy, purge.
    AgentsWrite,
    /// Read jobs and their output.
    JobsRead,
    /// Submit and cancel jobs.
    JobsWrite,
    /// Watch the terminal and read its history.
    ViewerRead,
    /// Take over the terminal and type into it.
    ViewerControl,
    /// Attach egress profiles to agents (ADR-016): widens what they may reach.
    EgressWrite,
    /// Everything.
    Admin,
}

impl Scope {
    /// Every scope.
    pub const ALL: [Self; 8] = [
        Self::AgentsRead,
        Self::AgentsWrite,
        Self::JobsRead,
        Self::JobsWrite,
        Self::ViewerRead,
        Self::ViewerControl,
        Self::EgressWrite,
        Self::Admin,
    ];

    /// The name used in the file and the API.
    pub fn name(self) -> &'static str {
        match self {
            Self::AgentsRead => "agents:read",
            Self::AgentsWrite => "agents:write",
            Self::JobsRead => "jobs:read",
            Self::JobsWrite => "jobs:write",
            Self::ViewerRead => "viewer:read",
            Self::ViewerControl => "viewer:control",
            Self::EgressWrite => "egress:write",
            Self::Admin => "admin",
        }
    }

    fn bit(self) -> u8 {
        1 << self as u8
    }
}

/// A set of scopes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Scopes(u8);

impl Scopes {
    /// A comma list of scope names; at least one, none unknown.
    pub fn parse(list: &str) -> Result<Self, String> {
        let mut set = Self::default();
        for name in list.split(',') {
            let scope = Scope::ALL
                .into_iter()
                .find(|s| s.name() == name)
                .ok_or_else(|| format!("unknown scope {name:?}"))?;
            set.0 |= scope.bit();
        }
        Ok(set)
    }

    /// Whether the set grants `scope` (directly or through `admin`).
    pub fn allows(self, scope: Scope) -> bool {
        self.0 & (scope.bit() | Scope::Admin.bit()) != 0
    }

    /// The names in the set.
    pub fn names(self) -> Vec<&'static str> {
        Scope::ALL
            .into_iter()
            .filter(|s| self.0 & s.bit() != 0)
            .map(Scope::name)
            .collect()
    }
}

impl fmt::Display for Scopes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.names().join(","))
    }
}

/// The agents a token is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Agents {
    /// `*`: every agent, also ones created later.
    All,
    /// Only these.
    Only(Vec<AgentId>),
}

impl Agents {
    /// `*` or a comma list of agent ids.
    pub fn parse(list: &str) -> Result<Self, String> {
        if list == "*" {
            return Ok(Self::All);
        }
        list.split(',')
            .map(|id| AgentId::parse(id).map_err(|e| format!("agent {id:?}: {e}")))
            .collect::<Result<_, _>>()
            .map(Self::Only)
    }

    /// Whether `id` (as it appears in a path) is one of them.
    pub fn contains(&self, id: &str) -> bool {
        match self {
            Self::All => true,
            Self::Only(ids) => ids.iter().any(|a| a.as_str() == id),
        }
    }
}

impl fmt::Display for Agents {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::All => f.write_str("*"),
            Self::Only(ids) => {
                let ids: Vec<&str> = ids.iter().map(AgentId::as_str).collect();
                f.write_str(&ids.join(","))
            }
        }
    }
}

/// One token, by its digest.
#[derive(Clone, PartialEq, Eq)]
pub struct Token {
    /// Shown in logs and the audit trail.
    pub name: String,
    /// What it may do.
    pub scopes: Scopes,
    /// For which agents.
    pub agents: Agents,
    /// When it was created (Unix seconds), if recorded.
    pub created: Option<u64>,
    /// When it stops working (Unix seconds); `None`: when revoked.
    pub expires: Option<u64>,
    hash: [u8; 32],
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Token({} {} {})", self.name, self.scopes, self.agents)?;
        match self.expires {
            Some(e) => write!(f, " until {}", utc(e)),
            None => Ok(()),
        }
    }
}

impl Token {
    /// An `admin` token for every agent with this name and secret (the legacy token, tests).
    pub fn admin(name: &str, secret: &str) -> Self {
        Self {
            name: name.to_owned(),
            scopes: Scopes(Scope::Admin.bit()),
            agents: Agents::All,
            created: None,
            expires: None,
            hash: Sha256::digest(secret.as_bytes()),
        }
    }

    /// Whether it still works at `now` (Unix seconds): it has no expiry, or `now` is before it.
    pub fn live(&self, now: u64) -> bool {
        self.expires.is_none_or(|e| now < e)
    }
}

/// What an `Authorization` header presented.
#[derive(Debug, PartialEq, Eq)]
pub enum Presented<'a> {
    /// No `Bearer` token.
    Missing,
    /// A token that matches none; the first 4 bytes of its digest, for the log.
    Unknown(String),
    /// A known token.
    Known(&'a Token),
}

/// Every token the gateway accepts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tokens(Vec<Token>);

impl Tokens {
    /// Parse the file format (see the module docs). Errors name the line.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut tokens = Self::default();
        for (n, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let token = parse_line(line).map_err(|e| format!("line {}: {e}", n + 1))?;
            if token.name == LEGACY {
                return Err(format!("line {}: the name {LEGACY} is reserved", n + 1));
            }
            tokens
                .add(token)
                .map_err(|e| format!("line {}: {e}", n + 1))?;
        }
        Ok(tokens)
    }

    /// Add a token; its name and digest must be new.
    pub fn add(&mut self, token: Token) -> Result<(), String> {
        if self.0.iter().any(|t| t.name == token.name) {
            return Err(format!("duplicate token name {}", token.name));
        }
        if self.0.iter().any(|t| t.hash == token.hash) {
            return Err(format!(
                "token {} has the same digest as another",
                token.name
            ));
        }
        self.0.push(token);
        Ok(())
    }

    /// The tokens, in file order.
    pub fn iter(&self) -> impl Iterator<Item = &Token> {
        self.0.iter()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether `token` (name, scopes, agents and digest) is still one of them.
    pub fn holds(&self, token: &Token) -> bool {
        self.0.iter().any(|t| t == token)
    }

    /// Who an `Authorization` header (`Bearer <token>`, exactly) belongs to. The presented token
    /// is hashed and compared with every digest in constant time.
    pub fn find(&self, authorization: Option<&str>) -> Presented<'_> {
        let Some(secret) = authorization
            .and_then(|h| h.strip_prefix("Bearer "))
            .filter(|s| !s.is_empty())
        else {
            return Presented::Missing;
        };
        let digest = Sha256::digest(secret.as_bytes());
        let mut found = None;
        for t in &self.0 {
            if constant_time_eq(&digest, &t.hash) && found.is_none() {
                found = Some(t);
            }
        }
        found.map_or_else(|| Presented::Unknown(hex(&digest[..4])), Presented::Known)
    }
}

fn parse_line(line: &str) -> Result<Token, String> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let (name, scopes, agents, hash, created, expires) = match fields[..] {
        [n, s, a, h] => (n, s, a, h, None, None),
        [n, s, a, h, c] => (n, s, a, h, Some(c), None),
        [n, s, a, h, c, e] => (n, s, a, h, Some(c), Some(e)),
        _ => return Err("want: name scopes agents sha256 [created [expires]]".into()),
    };
    check_name(name)?;
    let created = created.map(|c| secs("created", c)).transpose()?;
    let expires = expires.map(|e| secs("expires", e)).transpose()?;
    if let (Some(c), Some(e)) = (created, expires) {
        if e <= c {
            return Err(format!("expires {e} is not after created {c}"));
        }
    }
    Ok(Token {
        name: name.to_owned(),
        scopes: Scopes::parse(scopes)?,
        agents: Agents::parse(agents)?,
        created,
        expires,
        hash: unhex(hash).ok_or("sha256 must be 64 lowercase hex digits")?,
    })
}

fn secs(field: &str, value: &str) -> Result<u64, String> {
    value
        .parse()
        .map_err(|_| format!("{field} {value:?} is not Unix seconds"))
}

/// A lifetime as `ac-gateway token create --expires` takes it: a whole number of `m`inutes,
/// `h`ours or `d`ays, more than zero and at most 3650 days.
pub fn parse_lifetime(text: &str) -> Result<u64, String> {
    const MAX: u64 = 3650 * 86_400;
    let bad = || format!("bad lifetime {text:?}: want a number and m, h or d (e.g. 90d)");
    let unit = match text.as_bytes().last() {
        Some(b'm') => 60,
        Some(b'h') => 3_600,
        Some(b'd') => 86_400,
        _ => return Err(bad()),
    };
    // The last byte is ASCII, so this is a char boundary.
    let n = &text[..text.len() - 1];
    if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    n.parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(unit))
        .filter(|s| (1..=MAX).contains(s))
        .ok_or_else(bad)
}

/// Now, in Unix seconds (0 if the clock is before 1970).
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// A token name: 1 to 32 of `a-z 0-9 _ -`, starting with a letter or digit.
pub fn check_name(name: &str) -> Result<(), String> {
    let ok = name.len() <= MAX_NAME
        && name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric())
        && name
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "bad token name {name:?}: 1 to {MAX_NAME} of a-z 0-9 _ -, starting with a letter or digit"
        ))
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<[u8; 32]> {
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

/// The tokens in force, swapped whole on reload. Cheap to clone.
#[derive(Clone, Default)]
pub struct Keys(Arc<RwLock<Arc<Tokens>>>);

impl Keys {
    /// Start with `tokens`.
    pub fn new(tokens: Tokens) -> Self {
        Self(Arc::new(RwLock::new(Arc::new(tokens))))
    }

    /// The current set.
    pub fn get(&self) -> Arc<Tokens> {
        Arc::clone(&self.0.read().unwrap_or_else(|e| e.into_inner()))
    }

    /// Replace the set.
    pub fn set(&self, tokens: Tokens) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(tokens);
    }
}

/// Which version of the file was last read: a rewrite in place changes the mtime or length, a
/// replace-by-rename the inode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Version {
    Missing,
    File {
        mtime: SystemTime,
        ino: u64,
        len: u64,
    },
}

/// The tokens file, remembering which version was last read.
#[derive(Debug)]
pub struct TokenFile {
    path: PathBuf,
    owner: u32,
    seen: Option<Version>,
}

impl TokenFile {
    /// Track `path`, which must be owned by `owner` (the gateway's uid).
    pub fn new(path: impl Into<PathBuf>, owner: u32) -> Self {
        Self {
            path: path.into(),
            owner,
            seen: None,
        }
    }

    /// The tracked path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the file if it changed since the last attempt, or always when `force` (startup,
    /// SIGHUP). `Ok(None)`: unchanged. A missing file is an empty set. `Err`: not a regular file,
    /// another owner, group/world accessible, too large or malformed; the same version is not
    /// retried until it changes again or `force`.
    pub fn reload(&mut self, force: bool) -> Result<Option<Tokens>, String> {
        let shown = self.path.display();
        let meta = match std::fs::symlink_metadata(&self.path) {
            Ok(m) => Some(m),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("{shown}: {e}")),
        };
        let version = match &meta {
            None => Version::Missing,
            Some(m) => Version::File {
                mtime: m.modified().map_err(|e| format!("{shown}: {e}"))?,
                ino: m.ino(),
                len: m.len(),
            },
        };
        if !force && self.seen == Some(version) {
            return Ok(None);
        }
        self.seen = Some(version);
        let Some(meta) = meta else {
            return Ok(Some(Tokens::default()));
        };
        self.check(&meta)?;
        let mut text = String::new();
        std::fs::File::open(&self.path)
            .and_then(|f| f.take(MAX_BYTES + 1).read_to_string(&mut text))
            .map_err(|e| format!("{shown}: {e}"))?;
        Tokens::parse(&text)
            .map(Some)
            .map_err(|e| format!("{shown}: {e}"))
    }

    fn check(&self, meta: &std::fs::Metadata) -> Result<(), String> {
        let shown = self.path.display();
        let mode = meta.permissions().mode();
        if !meta.is_file() {
            Err(format!("{shown} is not a regular file"))
        } else if meta.uid() != self.owner {
            Err(format!(
                "{shown} is owned by uid {}, not {}",
                meta.uid(),
                self.owner
            ))
        } else if mode & 0o077 != 0 {
            Err(format!(
                "{shown} must not be group/world accessible (mode {:o})",
                mode & 0o777
            ))
        } else if meta.len() > MAX_BYTES {
            Err(format!("{shown} is larger than {MAX_BYTES} bytes"))
        } else {
            Ok(())
        }
    }

    /// Add a token and return its secret (32 random bytes as hex), which is shown once and never
    /// stored. It expires `lifetime` seconds after `now`, or never. The line is appended; the rest
    /// of the file, comments included, is kept.
    pub fn create(
        &self,
        name: &str,
        scopes: Scopes,
        agents: Agents,
        now: u64,
        lifetime: Option<u64>,
    ) -> Result<String, String> {
        check_name(name)?;
        let expires = match lifetime {
            None => String::new(),
            Some(l) => format!(" {}", now.checked_add(l).ok_or("lifetime too long")?),
        };
        let secret = random_hex()?;
        let line = format!(
            "{name} {scopes} {agents} {} {now}{expires}\n",
            hex(&Sha256::digest(secret.as_bytes()))
        );
        self.edit(|text| {
            let mut text = if text.is_empty() {
                HEADER.to_owned()
            } else {
                text.to_owned()
            };
            if !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&line);
            Ok(text)
        })?;
        Ok(secret)
    }

    /// Remove the token named `name`.
    pub fn revoke(&self, name: &str) -> Result<(), String> {
        self.edit(|text| {
            let kept: Vec<&str> = text
                .lines()
                .filter(|l| {
                    l.split('#').next().unwrap_or("").split_whitespace().next() != Some(name)
                })
                .collect();
            if kept.len() == text.lines().count() {
                return Err(format!("no token named {name:?}"));
            }
            Ok(kept.iter().map(|l| format!("{l}\n")).collect())
        })
    }

    /// The tokens now in the file (none if it is missing).
    pub fn list(&self) -> Result<Tokens, String> {
        Self::new(&self.path, self.owner)
            .reload(true)
            .map(Option::unwrap_or_default)
    }

    /// Rewrite the file through `change`, under an exclusive lock on its directory, so concurrent
    /// edits do not lose each other. The current file must pass every check, and so must the
    /// result; it replaces the file atomically (temp file, fsync, rename), mode 0600.
    fn edit(&self, change: impl FnOnce(&str) -> Result<String, String>) -> Result<(), String> {
        let dir = self.path.parent().unwrap_or(Path::new("."));
        let lock = std::fs::File::open(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        lock.lock()
            .map_err(|e| format!("lock {}: {e}", dir.display()))?;
        let shown = self.path.display();
        let text = match std::fs::symlink_metadata(&self.path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(format!("{shown}: {e}")),
            Ok(meta) => {
                self.check(&meta)?;
                let text =
                    std::fs::read_to_string(&self.path).map_err(|e| format!("{shown}: {e}"))?;
                Tokens::parse(&text).map_err(|e| format!("{shown}: {e}"))?;
                text
            }
        };
        let next = change(&text)?;
        Tokens::parse(&next)?;
        if next.len() as u64 > MAX_BYTES {
            return Err(format!("{shown} would be larger than {MAX_BYTES} bytes"));
        }
        rusty_atomic_file::write_private(&self.path, next.as_bytes())
            .map_err(|e| format!("{shown}: {e}"))
    }
}

/// 32 bytes from the kernel's random source, as hex. No fallback: a token must be unguessable.
pub(crate) fn random_hex() -> Result<String, String> {
    let mut b = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .map_err(|e| format!("/dev/urandom: {e}"))?;
    Ok(hex(&b))
}

/// Unix seconds as `YYYY-MM-DDTHH:MM:SSZ` (`rusty_time`); past its range (a year beyond `i32`),
/// the raw seconds as `@<secs>`.
pub fn utc(secs: u64) -> String {
    i64::try_from(secs)
        .ok()
        .and_then(|s| rusty_time::DateTime::from_unix_secs(s).ok())
        .map_or_else(|| format!("@{secs}"), |t| t.to_iso8601())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    const DIGEST: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"; // "test"

    fn tmp() -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("ac-tokens-{}-{n}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        dir.join("tokens")
    }

    fn me() -> u32 {
        std::fs::metadata("/proc/self").map_or(0, |m| m.uid())
    }

    fn write(path: &Path, text: &str, mode: u32) {
        let _ = std::fs::write(path, text);
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }

    fn parse(text: &str) -> Tokens {
        Tokens::parse(text).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn parses_lines_comments_and_created() {
        let t = parse(&format!(
            "# tokens\n\nci jobs:read,jobs:write a1,a2 {DIGEST} 1761004800  # CI\nops admin * {} 5 9\n",
            "0".repeat(64)
        ));
        let all: Vec<&Token> = t.iter().collect();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].name, "ci");
        assert_eq!(all[0].scopes.to_string(), "jobs:read,jobs:write");
        assert_eq!(all[0].agents.to_string(), "a1,a2");
        assert_eq!(
            (all[0].created, all[0].expires),
            (Some(1_761_004_800), None)
        );
        assert_eq!(
            (all[1].agents.clone(), all[1].created, all[1].expires),
            (Agents::All, Some(5), Some(9))
        );
        assert!(Tokens::parse("# only comments\n\n").is_ok_and(|t| t.is_empty()));
    }

    #[test]
    fn hostile_files_are_refused_with_the_line() {
        let ok = format!("ci jobs:read a1 {DIGEST}");
        for (text, want) in [
            ("ci jobs:read a1".to_owned(), "line 1: want"),
            (format!("{ok} 1 2 extra"), "line 1: want"),
            (
                format!("ci jobs:read,root a1 {DIGEST}"),
                "unknown scope \"root\"",
            ),
            (format!("ci jobs:read, a1 {DIGEST}"), "unknown scope \"\""),
            (format!("ci jobs:read a1,../x {DIGEST}"), "agent \"../x\""),
            (format!("ci jobs:read ** {DIGEST}"), "agent \"**\""),
            (
                format!("ci jobs:read a1 {}", &DIGEST[1..]),
                "64 lowercase hex",
            ),
            (
                format!("ci jobs:read a1 {}", DIGEST.to_uppercase()),
                "64 lowercase hex",
            ),
            (
                format!("ci jobs:read a1 {}g", &DIGEST[1..]),
                "64 lowercase hex",
            ),
            (
                format!("ci jobs:read a1 {DIGEST} yesterday"),
                "Unix seconds",
            ),
            (
                format!("ci jobs:read a1 {DIGEST} 1 soon"),
                "expires \"soon\" is not Unix seconds",
            ),
            (format!("ci jobs:read a1 {DIGEST} 5 5"), "not after created"),
            (format!("ci jobs:read a1 {DIGEST} 5 4"), "not after created"),
            (format!("Ci jobs:read a1 {DIGEST}"), "bad token name"),
            (format!("-ci jobs:read a1 {DIGEST}"), "bad token name"),
            (
                format!("{} jobs:read a1 {DIGEST}", "a".repeat(33)),
                "bad token name",
            ),
            (format!("legacy admin * {DIGEST}"), "reserved"),
            (format!("{ok}\n{ok}"), "line 2: duplicate token name"),
            (
                format!("{ok}\nother admin * {DIGEST}"),
                "line 2: token other has the same digest",
            ),
            (format!("#\n\nci jobs:read a1 {DIGEST}\u{0}"), "line 3"),
        ] {
            let err = Tokens::parse(&text).err().unwrap_or_default();
            assert!(err.contains(want), "{text:?}: {err}");
        }
    }

    #[test]
    fn scopes_admin_implies_all_and_others_only_themselves() {
        let admin = Scopes::parse("admin").unwrap_or_default();
        assert!(Scope::ALL.into_iter().all(|s| admin.allows(s)));
        let jobs = Scopes::parse("jobs:write,jobs:read").unwrap_or_default();
        for s in Scope::ALL {
            assert_eq!(
                jobs.allows(s),
                matches!(s, Scope::JobsRead | Scope::JobsWrite),
                "{s:?}"
            );
        }
        assert!(!Scopes::parse("viewer:read")
            .unwrap_or_default()
            .allows(Scope::ViewerControl));
        // Widening egress is its own scope: agents:write does not imply it, admin does.
        let egress = Scopes::parse("egress:write").unwrap_or_default();
        assert!(egress.allows(Scope::EgressWrite) && !egress.allows(Scope::AgentsWrite));
        assert!(!Scopes::parse("agents:write")
            .unwrap_or_default()
            .allows(Scope::EgressWrite));
        assert_eq!(
            Scopes::parse("admin,egress:write").map(|s| s.to_string()),
            Ok("egress:write,admin".into())
        );
        assert!(Scopes::parse("").is_err() && Scopes::parse("Admin").is_err());
    }

    #[test]
    fn agent_restriction() {
        let only = Agents::parse("a1,b2").unwrap_or(Agents::All);
        assert!(only.contains("a1") && only.contains("b2"));
        for other in ["a2", "a1 ", "", "*", "a1,b2"] {
            assert!(!only.contains(other), "{other:?}");
        }
        assert!(Agents::All.contains("anything"));
        assert!(Agents::parse("").is_err() && Agents::parse("a1,").is_err());
    }

    #[test]
    fn find_needs_the_exact_bearer_token() {
        let mut t = parse(&format!("ci jobs:read a1 {DIGEST}"));
        let _ = t.add(Token::admin(LEGACY, "s3cret"));
        assert!(matches!(t.find(Some("Bearer test")), Presented::Known(k) if k.name == "ci"));
        assert!(matches!(t.find(Some("Bearer s3cret")), Presented::Known(k) if k.name == LEGACY));
        for missing in [
            None,
            Some(""),
            Some("Bearer "),
            Some("bearer test"),
            Some("test"),
        ] {
            assert_eq!(t.find(missing), Presented::Missing, "{missing:?}");
        }
        for wrong in [
            "Bearer tes",
            "Bearer test ",
            "Bearer  test",
            "Bearer TEST",
            "Bearer s3cret\n",
        ] {
            assert!(
                matches!(t.find(Some(wrong)), Presented::Unknown(p) if p.len() == 8),
                "{wrong:?}"
            );
        }
        // The digest of a stored token does not work as the token.
        assert!(matches!(
            t.find(Some(&format!("Bearer {DIGEST}"))),
            Presented::Unknown(_)
        ));
        let huge = format!("Bearer {}", "x".repeat(100_000));
        assert!(matches!(t.find(Some(&huge)), Presented::Unknown(_)));
    }

    #[test]
    fn create_list_revoke_and_reload() {
        let path = tmp();
        let file = TokenFile::new(&path, me());
        let mut watch = TokenFile::new(&path, me());
        assert_eq!(
            watch.reload(true),
            Ok(Some(Tokens::default())),
            "missing: none"
        );
        assert_eq!(watch.reload(false), Ok(None));

        let jobs = Scopes::parse("jobs:read,jobs:write").unwrap_or_default();
        let secret = file
            .create(
                "ci",
                jobs,
                Agents::parse("a1").unwrap_or(Agents::All),
                1,
                None,
            )
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(secret.len() == 64 && secret.bytes().all(|b| b.is_ascii_hexdigit()));
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            text.starts_with("# agent-cloud") && !text.contains(&secret),
            "{text}"
        );
        let mode = std::fs::metadata(&path).map_or(0, |m| m.permissions().mode());
        assert_eq!(mode & 0o777, 0o600);

        let now = watch.reload(false).unwrap_or_else(|e| panic!("{e}"));
        let now = now.unwrap_or_else(|| panic!("change not seen"));
        assert!(
            matches!(now.find(Some(&format!("Bearer {secret}"))), Presented::Known(t) if t.name == "ci")
        );

        let other = file.create(
            "ops",
            Scopes::parse("admin").unwrap_or_default(),
            Agents::All,
            2,
            Some(100),
        );
        assert!(other.is_ok());
        assert!(
            file.create("ci", jobs, Agents::All, 3, None).is_err(),
            "duplicate name"
        );
        assert!(file.create("bad name", jobs, Agents::All, 3, None).is_err());
        let names: Vec<String> = file
            .list()
            .unwrap_or_default()
            .iter()
            .map(|t| t.name.clone())
            .collect();
        assert_eq!(names, ["ci", "ops"]);
        let ops = file.list().unwrap_or_default();
        let ops = ops
            .iter()
            .find(|t| t.name == "ops")
            .map(|t| (t.created, t.expires));
        assert_eq!(
            ops,
            Some((Some(2), Some(102))),
            "expires = created + lifetime"
        );

        assert!(file.revoke("ci").is_ok());
        assert!(file.revoke("ci").is_err(), "already gone");
        let after = watch.reload(false).unwrap_or_else(|e| panic!("{e}"));
        let after = after.unwrap_or_else(|| panic!("revoke not seen"));
        assert_eq!(
            after.find(Some(&format!("Bearer {secret}"))),
            Presented::Unknown(hex(&Sha256::digest(secret.as_bytes())[..4]))
        );
        assert!(std::fs::read_to_string(&path)
            .is_ok_and(|t| t.starts_with("# agent-cloud") && t.contains("ops ")));

        let _ = std::fs::remove_file(&path);
        assert_eq!(
            watch.reload(false),
            Ok(Some(Tokens::default())),
            "deleted: none"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap_or(&path));
    }

    #[test]
    fn a_bad_file_is_refused_and_not_edited() {
        let path = tmp();
        let file = TokenFile::new(&path, me());
        write(&path, &format!("ci jobs:read a1 {DIGEST}\n"), 0o644);
        let err = TokenFile::new(&path, me())
            .reload(true)
            .err()
            .unwrap_or_default();
        assert!(err.contains("group/world"), "{err}");
        assert!(file
            .create(
                "x",
                Scopes::parse("admin").unwrap_or_default(),
                Agents::All,
                1,
                None
            )
            .is_err());

        write(&path, "garbage\n", 0o600);
        let mut watch = TokenFile::new(&path, me());
        assert!(watch.reload(true).is_err());
        assert_eq!(
            watch.reload(false),
            Ok(None),
            "same bad version not retried"
        );
        assert!(file.revoke("ci").is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap_or_default(),
            "garbage\n"
        );

        let err = TokenFile::new(&path, me() + 1)
            .reload(true)
            .err()
            .unwrap_or_default();
        assert!(err.contains("owned by uid"), "{err}");

        let _ = std::fs::remove_file(&path);
        let _ = std::os::unix::fs::symlink("/etc/passwd", &path);
        let err = TokenFile::new(&path, me())
            .reload(true)
            .err()
            .unwrap_or_default();
        assert!(err.contains("not a regular file"), "{err}");

        let _ = std::fs::remove_file(&path);
        write(&path, &"#".repeat(70_000), 0o600);
        let err = TokenFile::new(&path, me())
            .reload(true)
            .err()
            .unwrap_or_default();
        assert!(err.contains("larger than"), "{err}");
        let _ = std::fs::remove_dir_all(path.parent().unwrap_or(&path));
    }

    #[test]
    fn expiry_is_exclusive_and_optional() {
        let mut t = parse(&format!("ci jobs:read a1 {DIGEST} 10 20"))
            .iter()
            .next()
            .cloned()
            .unwrap_or_else(|| Token::admin("x", "y"));
        assert!(t.live(0) && t.live(19), "before");
        assert!(!t.live(20) && !t.live(u64::MAX), "at and after");
        t.expires = None;
        assert!(t.live(u64::MAX), "no expiry: until revoked");
        assert!(Token::admin(LEGACY, "s").live(u64::MAX));
    }

    #[test]
    fn lifetimes() {
        for (text, want) in [
            ("30m", 1_800),
            ("12h", 43_200),
            ("90d", 7_776_000),
            ("3650d", 315_360_000),
        ] {
            assert_eq!(parse_lifetime(text), Ok(want), "{text}");
        }
        for bad in [
            "",
            "d",
            "0d",
            "30",
            "30s",
            "1w",
            "-1d",
            "+1d",
            " 1d",
            "1.5d",
            "3651d",
            "1D",
            "99999999999999999999d",
            "١d",
            "1١",
        ] {
            assert!(parse_lifetime(bad).is_err(), "{bad:?}");
        }
        let file = TokenFile::new(tmp(), me());
        let admin = Scopes::parse("admin").unwrap_or_default();
        assert!(file
            .create("x", admin, Agents::All, u64::MAX, Some(1))
            .is_err_and(|e| e.contains("too long")));
    }

    #[test]
    fn utc_dates() {
        assert_eq!(utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(utc(1_761_004_800 + 3_723), "2025-10-21T01:02:03Z");
    }
}
