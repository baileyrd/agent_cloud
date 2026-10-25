//! Per-agent egress on the host (ADR-016), the gateway's side: the agents file it writes for the
//! proxy, and the operator's profiles it reads. [`EgressStore`] is the [`Egress`] port (a new
//! credential for every boot) and the [`Profiles`] the API attaches from.
//!
//! The agents file (`/etc/agent-cloud/egress/agents`) is the source of truth for attachments: it
//! is read at startup and on every change, never cached, and replaced atomically (temp file,
//! fsync, rename, mode 0600). Changes are serialized by one lock, and lifecycle calls already
//! hold the API's. A file that is not this user's, is group/world accessible or is malformed is
//! an error and is never rewritten, so a bad edit cannot be silently lost.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ac_domain::egress::{parse_agents, render_agents};
use ac_domain::{AgentId, Allowlist, Attachment, ProfileName};
use ac_ports::{Boot, Egress, PortError};
use rusty_sha2::{Hash, Sha256};

use crate::tokens::random_hex;

/// Largest agents or profile file accepted (as in the proxy).
const MAX_BYTES: u64 = 64 * 1024;
/// The variables a boot overrides: the image sets the same names without a credential.
pub const PROXY_VARS: [&str; 4] = ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"];

/// Every profile by name: its entry count, or why the proxy would refuse it.
pub type Listed = Vec<(ProfileName, Result<usize, String>)>;

/// Profiles and attachments, for the egress routes.
pub trait Profiles: Send + Sync {
    /// Every profile by name: its entry count, or why the proxy would refuse it.
    fn list(&self) -> Result<Listed, String>;
    /// Entries in the base list every agent gets.
    fn base_hosts(&self) -> Result<usize, String>;
    /// The profiles attached to `id` (none if it has no line).
    fn attached(&self, id: &AgentId) -> Result<Vec<ProfileName>, String>;
    /// Replace `id`'s profiles; its credential is kept.
    fn attach(&self, id: &AgentId, profiles: &[ProfileName]) -> Result<(), String>;
}

/// The agents file, the profiles directory and the base allowlist.
pub struct EgressStore {
    agents: PathBuf,
    profiles: PathBuf,
    base: PathBuf,
    proxy: String,
    owner: u32,
    lock: Mutex<()>,
}

impl EgressStore {
    /// Files owned by `owner` (the gateway's uid); credentials name the proxy at `proxy`
    /// (`10.89.0.1:3128`).
    pub fn new(
        agents: impl Into<PathBuf>,
        profiles: impl Into<PathBuf>,
        base: impl Into<PathBuf>,
        proxy: &str,
        owner: u32,
    ) -> Self {
        Self {
            agents: agents.into(),
            profiles: profiles.into(),
            base: base.into(),
            proxy: proxy.to_owned(),
            owner,
            lock: Mutex::new(()),
        }
    }

    /// The agents file.
    pub fn agents_path(&self) -> &Path {
        &self.agents
    }

    /// Every agent's line now; none if the file is missing.
    pub fn load(&self) -> Result<Vec<Attachment>, String> {
        let text = read_private(&self.agents, self.owner)?.unwrap_or_default();
        parse_agents(&text).map_err(|e| format!("{}: {e}", self.agents.display()))
    }

    /// Change the lines under the lock and write them back if `change` changed them.
    fn edit<T>(
        &self,
        change: impl FnOnce(&mut Vec<Attachment>) -> Result<T, String>,
    ) -> Result<T, String> {
        let _held = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut all = self.load()?;
        let before = all.clone();
        let out = change(&mut all)?;
        if all == before {
            return Ok(out);
        }
        let shown = self.agents.display();
        if let Some(dir) = self.agents.parent() {
            if !dir.exists() {
                std::fs::create_dir_all(dir)
                    .map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                    .map_err(|e| format!("chmod {}: {e}", dir.display()))?;
            }
        }
        rusty_atomic_file::write_private(&self.agents, render_agents(&all).as_bytes())
            .map_err(|e| format!("{shown}: {e}"))?;
        Ok(out)
    }

    /// The line for `id`, added bare if there is none.
    fn line<'a>(all: &'a mut Vec<Attachment>, id: &AgentId) -> &'a mut Attachment {
        let at = match all.iter().position(|a| &a.agent == id) {
            Some(at) => at,
            None => {
                all.push(Attachment::bare(id.clone()));
                all.len() - 1
            }
        };
        &mut all[at]
    }

    fn port_error(op: &'static str) -> impl Fn(String) -> PortError {
        move |cause| PortError { op, cause }
    }
}

impl Egress for EgressStore {
    fn issue(&self, id: &AgentId, profiles: Option<&[ProfileName]>) -> Result<Boot, PortError> {
        let fail = Self::port_error("egress credential");
        let secret = random_hex().map_err(&fail)?;
        let digest = Sha256::digest(secret.as_bytes());
        self.edit(|all| {
            let line = Self::line(all, id);
            line.digest = Some(digest);
            if let Some(p) = profiles {
                line.profiles = ac_domain::egress::profile_set(p.iter().cloned());
            }
            Ok(())
        })
        .map_err(&fail)?;
        let url = format!("http://{id}:{secret}@{}", self.proxy);
        Ok(PROXY_VARS
            .into_iter()
            .fold(Boot::default(), |b, name| b.with_env(name, url.clone())))
    }

    fn revoke(&self, id: &AgentId) -> Result<(), PortError> {
        self.edit(|all| {
            if let Some(a) = all.iter_mut().find(|a| &a.agent == id) {
                a.digest = None;
            }
            Ok(())
        })
        .map_err(Self::port_error("egress revoke"))
    }

    fn forget(&self, id: &AgentId) -> Result<(), PortError> {
        self.edit(|all| {
            all.retain(|a| &a.agent != id);
            Ok(())
        })
        .map_err(Self::port_error("egress forget"))
    }

    fn sweep(&self, live: &[AgentId]) -> Result<Vec<AgentId>, PortError> {
        self.edit(|all| {
            let mut swept = Vec::new();
            for a in all.iter_mut() {
                if a.digest.is_some() && !live.contains(&a.agent) {
                    a.digest = None;
                    swept.push(a.agent.clone());
                }
            }
            Ok(swept)
        })
        .map_err(Self::port_error("egress sweep"))
    }
}

impl Profiles for EgressStore {
    fn list(&self) -> Result<Listed, String> {
        let dir = &self.profiles;
        let entries = match std::fs::read_dir(dir) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("{}: {e}", dir.display())),
            Ok(entries) => entries,
        };
        let mut out = Vec::new();
        for entry in entries {
            let path = entry.map_err(|e| format!("{}: {e}", dir.display()))?.path();
            let file = path.file_name().map(|f| f.to_string_lossy().into_owned());
            let Some(stem) = file.as_deref().and_then(|f| f.strip_suffix(".txt")) else {
                continue;
            };
            // A bad name refuses the proxy's whole directory; it is listed by doctor, not here.
            let Ok(name) = ProfileName::parse(stem) else {
                continue;
            };
            let hosts = read_private(&path, self.owner)
                .and_then(|t| {
                    Allowlist::parse_file(&t.unwrap_or_default()).map_err(|e| e.to_string())
                })
                .map(|l| l.len());
            out.push((name, hosts));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    fn base_hosts(&self) -> Result<usize, String> {
        let text = read_private(&self.base, self.owner)?.unwrap_or_default();
        Allowlist::parse_file(&text)
            .map(|l| l.len())
            .map_err(|e| format!("{}: {e}", self.base.display()))
    }

    fn attached(&self, id: &AgentId) -> Result<Vec<ProfileName>, String> {
        let all = self.load()?;
        Ok(all
            .into_iter()
            .find(|a| &a.agent == id)
            .map(|a| a.profiles)
            .unwrap_or_default())
    }

    fn attach(&self, id: &AgentId, profiles: &[ProfileName]) -> Result<(), String> {
        self.edit(|all| {
            Self::line(all, id).profiles = ac_domain::egress::profile_set(profiles.iter().cloned());
            Ok(())
        })
    }
}

/// A file's text if it is a regular file owned by `owner`, not group/world accessible and not too
/// large (the proxy's rules); `None` if it is missing.
fn read_private(path: &Path, owner: u32) -> Result<Option<String>, String> {
    let shown = path.display();
    let meta = match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{shown}: {e}")),
        Ok(m) => m,
    };
    let mode = meta.permissions().mode();
    if !meta.is_file() {
        Err(format!("{shown} is not a regular file"))
    } else if meta.uid() != owner {
        Err(format!(
            "{shown} is owned by uid {}, not {owner}",
            meta.uid()
        ))
    } else if mode & 0o077 != 0 {
        Err(format!(
            "{shown} must not be group/world accessible (mode {:o})",
            mode & 0o777
        ))
    } else if meta.len() > MAX_BYTES {
        Err(format!("{shown} is larger than {MAX_BYTES} bytes"))
    } else {
        std::fs::read_to_string(path)
            .map(Some)
            .map_err(|e| format!("{shown}: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use ac_domain::egress::hex;

    use super::*;

    fn me() -> u32 {
        std::fs::metadata("/proc/self")
            .map(|m| m.uid())
            .unwrap_or(u32::MAX)
    }

    fn tmp() -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("ac-gw-egress-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(dir.join("profiles"));
        dir
    }

    fn write(path: &Path, text: &str, mode: u32) {
        let _ = std::fs::write(path, text);
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }

    /// The agents file in a directory that does not exist yet (the gateway makes it).
    fn store(dir: &Path) -> EgressStore {
        EgressStore::new(
            dir.join("egress/agents"),
            dir.join("profiles"),
            dir.join("allow.txt"),
            "10.89.0.1:3128",
            me(),
        )
    }

    fn id(s: &str) -> AgentId {
        AgentId::parse(s).unwrap_or_else(|e| panic!("{e}"))
    }

    fn p(s: &str) -> ProfileName {
        ProfileName::parse(s).unwrap_or_else(|e| panic!("{e}"))
    }

    fn text(s: &EgressStore) -> String {
        std::fs::read_to_string(s.agents_path()).unwrap_or_default()
    }

    fn secret_of(boot: &Boot) -> String {
        let url = &boot.env()[0].1;
        url.trim_start_matches("http://a1:")
            .split('@')
            .next()
            .unwrap_or_default()
            .to_owned()
    }

    #[test]
    fn issue_stores_only_the_digest_and_rotates_every_boot() {
        let dir = tmp();
        let s = store(&dir);
        let first = s
            .issue(&id("a1"), Some(&[p("rust")]))
            .unwrap_or_else(|e| panic!("{e}"));
        let names: Vec<&str> = first.env().iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, PROXY_VARS);
        let secret = secret_of(&first);
        assert_eq!(secret.len(), 64);
        assert!(first
            .env()
            .iter()
            .all(|(_, v)| v == &format!("http://a1:{secret}@10.89.0.1:3128")));
        let file = text(&s);
        let digest = hex(&Sha256::digest(secret.as_bytes()));
        assert!(file.contains(&format!("a1 {digest} rust\n")), "{file}");
        assert!(!file.contains(&secret));
        let meta = std::fs::metadata(s.agents_path()).ok();
        assert_eq!(meta.map(|m| m.permissions().mode() & 0o777), Some(0o600));
        let dir_mode =
            std::fs::metadata(dir.join("egress")).map(|m| m.permissions().mode() & 0o777);
        assert_eq!(dir_mode.ok(), Some(0o700));
        // The next boot: a new secret and digest, the profiles kept.
        let second = s.issue(&id("a1"), None).unwrap_or_else(|e| panic!("{e}"));
        assert_ne!(secret_of(&second), secret);
        assert!(!text(&s).contains(&digest) && text(&s).contains(" rust\n"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn revoke_forget_and_sweep() {
        let dir = tmp();
        let s = store(&dir);
        for a in ["a1", "a2", "a3"] {
            let _ = s.issue(&id(a), Some(&[p("rust")]));
        }
        assert!(s.revoke(&id("a1")).is_ok());
        assert!(text(&s).contains("a1 - rust\n"));
        assert!(s.revoke(&id("zz")).is_ok() && !text(&s).contains("zz"));
        assert!(s.forget(&id("a2")).is_ok());
        assert!(!text(&s).contains("a2 "));
        let swept = s.sweep(&[id("a1")]).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(swept, [id("a3")]);
        assert!(text(&s).ends_with("a1 - rust\na3 - rust\n"), "{}", text(&s));
        assert_eq!(s.sweep(&[]).ok(), Some(vec![]));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn attach_profiles_and_counts() {
        let dir = tmp();
        let s = store(&dir);
        write(
            &dir.join("allow.txt"),
            "api.anthropic.com\ngithub.com\n",
            0o600,
        );
        write(
            &dir.join("profiles/rust.txt"),
            "index.crates.io\nstatic.crates.io\n",
            0o600,
        );
        write(
            &dir.join("profiles/node.txt"),
            "registry.npmjs.org\n",
            0o644,
        );
        write(&dir.join("profiles/Bad.txt"), "x.example\n", 0o600);
        write(&dir.join("profiles/README"), "", 0o600);
        let listed = s.list().unwrap_or_default();
        let names: Vec<&str> = listed.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["node", "rust"]);
        assert!(listed[0]
            .1
            .as_ref()
            .is_err_and(|e| e.contains("group/world")));
        assert_eq!(listed[1].1, Ok(2));
        assert_eq!(s.base_hosts(), Ok(2));
        assert_eq!(s.attached(&id("a1")), Ok(vec![]));
        assert!(s.attach(&id("a1"), &[p("rust")]).is_ok());
        assert_eq!(s.attached(&id("a1")), Ok(vec![p("rust")]));
        assert!(text(&s).contains("a1 - rust\n"));
        // Attaching keeps the credential; detaching keeps the line.
        let _ = s.issue(&id("a1"), None);
        assert!(s.attach(&id("a1"), &[]).is_ok());
        let line = text(&s).lines().last().unwrap_or_default().to_owned();
        assert!(
            line.starts_with("a1 ") && line.ends_with(" -") && !line.contains(" - "),
            "{line}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_bad_agents_file_is_never_rewritten() {
        let dir = tmp();
        let s = store(&dir);
        let _ = std::fs::create_dir_all(dir.join("egress"));
        for (body, mode, want) in [
            ("a1 - -\na1 - -\n", 0o600, "listed twice"),
            ("a1 - -\n", 0o644, "group/world"),
        ] {
            write(s.agents_path(), body, mode);
            let err = s
                .issue(&id("a1"), None)
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default();
            assert!(err.contains(want), "{err}");
            assert!(s.attach(&id("a1"), &[]).is_err() && s.revoke(&id("a1")).is_err());
            assert_eq!(text(&s), body, "untouched");
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
