//! The per-agent egress files (ADR-016): the agents file the gateway writes and the operator's
//! profiles directory, re-read when any of them changes. The proxy stats them on every new
//! connection, so a change applies to the next request without a signal.
//!
//! Every file must be a regular file owned by the proxy's user and not group or world
//! accessible; the directory must not be writable by anyone else. Each file fails on its own: a
//! bad agents file keeps the last good agents, a bad profile keeps its last good version (or adds
//! nothing if it never had one), and every other change still applies, so one operator typo can
//! never freeze a detach. Nothing bad is ever half-read.

use std::collections::BTreeMap;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ac_domain::egress::parse_agents;
use ac_domain::{Allowlist, Attachment, ProfileName};

/// Largest agents or profile file accepted.
const MAX_BYTES: u64 = 64 * 1024;
/// Profiles in the directory, at most.
const MAX_PROFILES: usize = 256;
/// A profile's file name: `<name>.txt`.
const EXT: &str = ".txt";

/// One version of one path: absent, or a file as of its mtime, inode and length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stamp {
    Missing,
    File {
        mtime: SystemTime,
        ino: u64,
        len: u64,
    },
}

/// What was looked at: the agents file, and each profile file (or why the directory is unusable).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Seen {
    agents: Result<Stamp, String>,
    profiles: Result<Vec<(PathBuf, Stamp)>, String>,
}

/// The policy's per-agent part as it now stands.
#[derive(Debug, Clone, Default)]
pub struct Loaded {
    /// Profiles by name.
    pub profiles: BTreeMap<ProfileName, Allowlist>,
    /// One line per agent.
    pub agents: Vec<Attachment>,
    /// Attachments that name a profile with no usable file: each adds nothing.
    pub warnings: Vec<String>,
    /// Files refused this time; each kept its last good version.
    pub errors: Vec<String>,
}

/// The two paths, remembering which version was last read and the last good contents.
#[derive(Debug)]
pub struct EgressFiles {
    agents: PathBuf,
    profiles: PathBuf,
    owner: u32,
    seen: Option<Seen>,
    good: Loaded,
}

impl EgressFiles {
    /// Track the agents file and the profiles directory, which must be owned by `owner`.
    pub fn new(agents: impl Into<PathBuf>, profiles: impl Into<PathBuf>, owner: u32) -> Self {
        Self {
            agents: agents.into(),
            profiles: profiles.into(),
            owner,
            seen: None,
            good: Loaded::default(),
        }
    }

    /// The agents file.
    pub fn agents_path(&self) -> &Path {
        &self.agents
    }

    /// The profiles directory.
    pub fn profiles_path(&self) -> &Path {
        &self.profiles
    }

    /// Re-read everything if anything changed since the last look, or always when `force`, and
    /// return the result: what parsed, plus the last good version of anything refused (named in
    /// `errors`, reported once per change). `None`: unchanged. A missing file or directory is
    /// empty.
    pub fn reload(&mut self, force: bool) -> Option<Loaded> {
        let seen = Seen {
            agents: stamp(&self.agents),
            profiles: self.profile_stamps(),
        };
        if !force && self.seen.as_ref() == Some(&seen) {
            return None;
        }
        let mut errors = Vec::new();
        match self.read(&self.agents).and_then(|t| {
            parse_agents(&t.unwrap_or_default())
                .map_err(|e| format!("{}: {e}", self.agents.display()))
        }) {
            Ok(agents) => self.good.agents = agents,
            Err(e) => errors.push(e),
        }
        match &seen.profiles {
            Err(e) => errors.push(e.clone()),
            Ok(files) => {
                let mut next = BTreeMap::new();
                for (path, _) in files {
                    let name = match profile_name(path) {
                        Ok(name) => name,
                        Err(e) => {
                            errors.push(e);
                            continue;
                        }
                    };
                    let parsed = self.read(path).and_then(|t| {
                        Allowlist::parse_file(&t.unwrap_or_default())
                            .map_err(|e| format!("{}: {e}", path.display()))
                    });
                    match (parsed, self.good.profiles.remove(&name)) {
                        (Ok(list), _) => {
                            next.insert(name, list);
                        }
                        (Err(e), last) => {
                            errors.push(e);
                            if let Some(last) = last {
                                next.insert(name, last);
                            }
                        }
                    }
                }
                self.good.profiles = next;
            }
        }
        self.seen = Some(seen);
        let mut warnings = Vec::new();
        for a in &self.good.agents {
            for p in a
                .profiles
                .iter()
                .filter(|p| !self.good.profiles.contains_key(*p))
            {
                warnings.push(format!(
                    "agent {}: profile {p} has no usable file in {} (adds nothing)",
                    a.agent,
                    self.profiles.display()
                ));
            }
        }
        Some(Loaded {
            warnings,
            errors,
            ..self.good.clone()
        })
    }

    /// Each `*.txt` in the profiles directory, sorted; empty if there is no directory.
    fn profile_stamps(&self) -> Result<Vec<(PathBuf, Stamp)>, String> {
        let dir = &self.profiles;
        let entries = match std::fs::read_dir(dir) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("{}: {e}", dir.display())),
            Ok(entries) => entries,
        };
        self.check_dir(dir)?;
        let mut files = Vec::new();
        for entry in entries {
            let path = entry.map_err(|e| format!("{}: {e}", dir.display()))?.path();
            if path.to_string_lossy().ends_with(EXT) {
                files.push(path);
            }
        }
        if files.len() > MAX_PROFILES {
            return Err(format!(
                "{}: more than {MAX_PROFILES} profiles",
                dir.display()
            ));
        }
        files.sort();
        let mut out = Vec::new();
        for path in files {
            let s = stamp(&path)?;
            out.push((path, s));
        }
        Ok(out)
    }

    fn check_dir(&self, dir: &Path) -> Result<(), String> {
        let meta = std::fs::symlink_metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        if !meta.is_dir() || meta.uid() != self.owner || meta.permissions().mode() & 0o022 != 0 {
            return Err(format!(
                "{} must be a directory owned by uid {}, not group/world writable",
                dir.display(),
                self.owner
            ));
        }
        Ok(())
    }

    /// A checked file's text; `None` if it is missing.
    fn read(&self, path: &Path) -> Result<Option<String>, String> {
        let shown = path.display();
        let meta = match std::fs::symlink_metadata(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("{shown}: {e}")),
            Ok(m) => m,
        };
        let mode = meta.permissions().mode();
        if !meta.is_file() {
            return Err(format!("{shown} is not a regular file"));
        }
        if meta.uid() != self.owner {
            return Err(format!(
                "{shown} is owned by uid {}, not {}",
                meta.uid(),
                self.owner
            ));
        }
        if mode & 0o077 != 0 {
            return Err(format!(
                "{shown} must not be group/world accessible (mode {:o})",
                mode & 0o777
            ));
        }
        if meta.len() > MAX_BYTES {
            return Err(format!("{shown} is larger than {MAX_BYTES} bytes"));
        }
        std::fs::read_to_string(path)
            .map(Some)
            .map_err(|e| format!("{shown}: {e}"))
    }
}

fn stamp(path: &Path) -> Result<Stamp, String> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Stamp::Missing),
        Err(e) => Err(format!("{}: {e}", path.display())),
        Ok(m) => Ok(Stamp::File {
            mtime: m
                .modified()
                .map_err(|e| format!("{}: {e}", path.display()))?,
            ino: m.ino(),
            len: m.len(),
        }),
    }
}

fn profile_name(path: &Path) -> Result<ProfileName, String> {
    let file = path
        .file_name()
        .map(|f| f.to_string_lossy())
        .unwrap_or_default();
    let stem = file.strip_suffix(EXT).unwrap_or(&file);
    ProfileName::parse(stem).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    const D: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

    fn me() -> u32 {
        std::fs::metadata("/proc/self")
            .map(|m| m.uid())
            .unwrap_or(u32::MAX)
    }

    /// A fresh directory holding `agents` and `profiles/`.
    fn tmp() -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("ac-egressfile-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(dir.join("profiles"));
        let _ =
            std::fs::set_permissions(dir.join("profiles"), std::fs::Permissions::from_mode(0o700));
        dir
    }

    fn write(path: &Path, text: &str, mode: u32) {
        let _ = std::fs::write(path, text);
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }

    fn files(dir: &Path) -> EgressFiles {
        EgressFiles::new(dir.join("agents"), dir.join("profiles"), me())
    }

    fn loaded(f: &mut EgressFiles) -> Loaded {
        f.reload(false).unwrap_or_else(|| panic!("unchanged"))
    }

    #[test]
    fn missing_files_are_empty() {
        let dir = tmp();
        let _ = std::fs::remove_dir_all(dir.join("profiles"));
        let l = loaded(&mut files(&dir));
        assert!(l.agents.is_empty() && l.profiles.is_empty());
        assert!(l.warnings.is_empty() && l.errors.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reads_agents_and_profiles_and_warns_about_missing_ones() {
        let dir = tmp();
        write(
            &dir.join("agents"),
            &format!("a1 {D} go,rust\na2 - -\n"),
            0o600,
        );
        write(
            &dir.join("profiles/rust.txt"),
            "# crates\nindex.crates.io\n*.crates.io\n",
            0o600,
        );
        write(&dir.join("profiles/notes.md"), "ignored", 0o644);
        let mut f = files(&dir);
        let l = loaded(&mut f);
        assert_eq!(l.agents.len(), 2);
        let names: Vec<&str> = l.profiles.keys().map(ProfileName::as_str).collect();
        assert_eq!(names, ["rust"]);
        assert!(l.profiles.values().all(|p| p.len() == 2));
        assert!(l.errors.is_empty(), "{:?}", l.errors);
        assert_eq!(l.warnings.len(), 1, "{:?}", l.warnings);
        assert!(l.warnings[0].starts_with("agent a1: profile go has no usable file"));
        assert!(f.reload(false).is_none(), "unchanged");
        assert!(f.reload(true).is_some(), "forced");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn any_change_is_seen_on_the_next_check() {
        let dir = tmp();
        write(&dir.join("agents"), "a1 - -\n", 0o600);
        let mut f = files(&dir);
        assert!(loaded(&mut f).agents[0].profiles.is_empty());
        // Attach (a new file by rename, as the gateway writes it), add a profile, edit it.
        let next = dir.join("agents.new");
        write(&next, "a1 - rust\n", 0o600);
        let _ = std::fs::rename(&next, dir.join("agents"));
        assert_eq!(loaded(&mut f).agents[0].profiles.len(), 1);
        write(&dir.join("profiles/rust.txt"), "a.example\n", 0o600);
        assert_eq!(loaded(&mut f).profiles.len(), 1);
        write(
            &dir.join("profiles/rust.txt"),
            "a.example\nb.example\n",
            0o600,
        );
        assert!(loaded(&mut f).profiles.values().all(|p| p.len() == 2));
        let _ = std::fs::remove_file(dir.join("profiles/rust.txt"));
        assert!(loaded(&mut f).profiles.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_bad_file_keeps_its_last_good_version_and_the_rest_still_applies() {
        let dir = tmp();
        write(&dir.join("agents"), "a1 - rust\n", 0o600);
        write(&dir.join("profiles/rust.txt"), "a.example\n", 0o600);
        write(&dir.join("profiles/node.txt"), "n.example\n", 0o600);
        let mut f = files(&dir);
        assert_eq!(loaded(&mut f).profiles.len(), 2);
        // rust goes bad: its last good version stays, and a detach in the agents file applies.
        write(
            &dir.join("profiles/rust.txt"),
            "a.example\nhttp://bad\n",
            0o600,
        );
        write(&dir.join("agents"), "a1 - -\n", 0o600);
        let l = loaded(&mut f);
        assert!(l.agents[0].profiles.is_empty(), "the detach applied");
        assert!(l.profiles.values().all(|p| p.len() == 1), "rust as it was");
        assert_eq!(l.errors.len(), 1);
        assert!(
            l.errors[0].contains("invalid allowlist entry"),
            "{:?}",
            l.errors
        );
        // The agents file goes bad: the last good agents stay, profile edits still apply.
        write(&dir.join("agents"), "a1 - -\na1 - -\n", 0o600);
        write(
            &dir.join("profiles/node.txt"),
            "n.example\nm.example\n",
            0o600,
        );
        let l = loaded(&mut f);
        assert_eq!(l.agents.len(), 1);
        assert_eq!(
            l.profiles
                .get(&ProfileName::parse("node").unwrap_or_else(|e| panic!("{e}")))
                .map(Allowlist::len),
            Some(2)
        );
        assert!(
            l.errors.iter().any(|e| e.contains("listed twice")),
            "{:?}",
            l.errors
        );
        assert!(f.reload(false).is_none(), "reported once per change");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn bad_files_that_never_were_good_add_nothing() {
        for (path, text, mode, want) in [
            ("agents", "a1 - -\n", 0o640, "group/world"),
            (
                "profiles/rust.txt",
                "http://bad\n",
                0o600,
                "invalid allowlist entry",
            ),
            ("profiles/rust.txt", "a.example\n", 0o604, "group/world"),
            ("profiles/Rust.txt", "a.example\n", 0o600, "profile name"),
            ("profiles/-.txt", "a.example\n", 0o600, "profile name"),
        ] {
            let dir = tmp();
            write(&dir.join(path), text, mode);
            let l = loaded(&mut files(&dir));
            assert!(l.agents.is_empty() && l.profiles.is_empty(), "{path}");
            assert!(
                l.errors.iter().any(|e| e.contains(want)),
                "{path}: {:?}",
                l.errors
            );
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn symlinks_other_owners_and_open_directories_are_refused() {
        let dir = tmp();
        write(&dir.join("real"), "a1 - -\n", 0o600);
        let _ = std::os::unix::fs::symlink(dir.join("real"), dir.join("agents"));
        let errors = loaded(&mut files(&dir)).errors;
        assert!(
            errors.iter().any(|e| e.contains("not a regular file")),
            "{errors:?}"
        );
        let _ = std::fs::remove_file(dir.join("agents"));
        let mut other = EgressFiles::new(dir.join("real"), dir.join("none"), me().wrapping_add(1));
        let errors = loaded(&mut other).errors;
        assert!(
            errors.iter().any(|e| e.contains("owned by uid")),
            "{errors:?}"
        );
        write(&dir.join("profiles/rust.txt"), "a.example\n", 0o600);
        let mut f = files(&dir);
        assert_eq!(loaded(&mut f).profiles.len(), 1);
        let _ =
            std::fs::set_permissions(dir.join("profiles"), std::fs::Permissions::from_mode(0o777));
        let l = loaded(&mut f);
        assert!(
            l.errors
                .iter()
                .any(|e| e.contains("not group/world writable")),
            "{:?}",
            l.errors
        );
        assert_eq!(l.profiles.len(), 1, "the last good profiles stay");
        let _ = std::fs::remove_dir_all(dir);
    }
}
