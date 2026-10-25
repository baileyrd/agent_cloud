//! The per-agent egress files (ADR-016): the agents file the gateway writes and the operator's
//! profiles directory, re-read when any of them changes. The proxy stats them on every new
//! connection, so a change applies to the next request without a signal.
//!
//! Every file must be a regular file owned by the proxy's user and not group or world
//! accessible; the directory must not be writable by anyone else. Any bad file refuses the
//! whole version: the caller keeps the policy it has (never falling open to a half-read one).

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

/// The agents file and profiles, as read.
#[derive(Debug, Clone, Default)]
pub struct Loaded {
    /// Profiles by name.
    pub profiles: BTreeMap<ProfileName, Allowlist>,
    /// One line per agent.
    pub agents: Vec<Attachment>,
    /// Attachments that name a profile with no file: each adds nothing.
    pub warnings: Vec<String>,
}

/// The two paths, remembering which version was last read.
#[derive(Debug)]
pub struct EgressFiles {
    agents: PathBuf,
    profiles: PathBuf,
    owner: u32,
    seen: Option<Vec<(PathBuf, Stamp)>>,
}

impl EgressFiles {
    /// Track the agents file and the profiles directory, which must be owned by `owner`.
    pub fn new(agents: impl Into<PathBuf>, profiles: impl Into<PathBuf>, owner: u32) -> Self {
        Self {
            agents: agents.into(),
            profiles: profiles.into(),
            owner,
            seen: None,
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

    /// Read everything if anything changed since the last attempt, or always when `force`.
    /// `Ok(None)`: unchanged. A missing file or directory is empty. `Err`: a bad file; the same
    /// version is not retried until something changes again or `force`.
    pub fn reload(&mut self, force: bool) -> Result<Option<Loaded>, String> {
        let stamps = self.stamps()?;
        if !force && self.seen.as_ref() == Some(&stamps) {
            return Ok(None);
        }
        self.seen = Some(stamps.clone());
        let mut loaded = Loaded::default();
        if let Some(text) = self.read(&self.agents)? {
            loaded.agents = parse_agents(&text).map_err(|e| format!("{}: {e}", self.shown()))?;
        }
        for (path, _) in stamps.iter().filter(|(p, _)| p != &self.agents) {
            let name = profile_name(path)?;
            let text = self.read(path)?.unwrap_or_default();
            let list =
                Allowlist::parse_file(&text).map_err(|e| format!("{}: {e}", path.display()))?;
            loaded.profiles.insert(name, list);
        }
        for a in &loaded.agents {
            for p in a
                .profiles
                .iter()
                .filter(|p| !loaded.profiles.contains_key(*p))
            {
                loaded.warnings.push(format!(
                    "agent {}: profile {p} has no file in {} (adds nothing)",
                    a.agent,
                    self.profiles.display()
                ));
            }
        }
        Ok(Some(loaded))
    }

    fn shown(&self) -> String {
        self.agents.display().to_string()
    }

    /// The agents file's stamp, then each `*.txt` in the profiles directory, sorted.
    fn stamps(&self) -> Result<Vec<(PathBuf, Stamp)>, String> {
        let mut out = vec![(self.agents.clone(), stamp(&self.agents)?)];
        let dir = &self.profiles;
        let entries = match std::fs::read_dir(dir) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
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
        match f.reload(false) {
            Ok(Some(l)) => l,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn missing_files_are_empty() {
        let dir = tmp();
        let _ = std::fs::remove_dir_all(dir.join("profiles"));
        let l = loaded(&mut files(&dir));
        assert!(l.agents.is_empty() && l.profiles.is_empty() && l.warnings.is_empty());
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
        assert_eq!(l.warnings.len(), 1, "{:?}", l.warnings);
        assert!(l.warnings[0].starts_with("agent a1: profile go has no file"));
        assert!(matches!(f.reload(false), Ok(None)), "unchanged");
        assert!(matches!(f.reload(true), Ok(Some(_))), "forced");
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
    fn a_bad_file_refuses_the_version_once() {
        for (path, text, mode, want) in [
            ("agents", "a1 - -\na1 - -\n", 0o600, "listed twice"),
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
            let mut f = files(&dir);
            let err = f.reload(false).err().unwrap_or_default();
            assert!(err.contains(want), "{path} {text:?}: {err}");
            assert!(
                matches!(f.reload(false), Ok(None)),
                "not retried until it changes"
            );
            assert!(f.reload(true).is_err());
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn symlinks_other_owners_and_open_directories_are_refused() {
        let dir = tmp();
        write(&dir.join("real"), "a1 - -\n", 0o600);
        let _ = std::os::unix::fs::symlink(dir.join("real"), dir.join("agents"));
        let err = files(&dir).reload(false).err().unwrap_or_default();
        assert!(err.contains("not a regular file"), "{err}");
        let _ = std::fs::remove_file(dir.join("agents"));
        let mut other = EgressFiles::new(dir.join("real"), dir.join("none"), me().wrapping_add(1));
        let err = other.reload(false).err().unwrap_or_default();
        assert!(err.contains("owned by uid"), "{err}");
        let _ =
            std::fs::set_permissions(dir.join("profiles"), std::fs::Permissions::from_mode(0o777));
        let err = files(&dir).reload(false).err().unwrap_or_default();
        assert!(err.contains("not group/world writable"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
