//! The allowlist file: one entry per line (`api.anthropic.com`, `*.githubusercontent.com`), `#`
//! starts a comment. It must not be group or world accessible, and is re-read when it changes.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ac_domain::{Allowlist, AllowlistError};

/// Largest file accepted; an allowlist is a few dozen lines.
const MAX_BYTES: u64 = 64 * 1024;

/// Parse the file format. Returns the list and how many entries it has.
pub fn parse(text: &str) -> Result<(Allowlist, usize), AllowlistError> {
    let entries: Vec<&str> = text
        .lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .collect();
    Ok((Allowlist::new(entries.iter().copied())?, entries.len()))
}

/// What identifies one version of the file: a rewrite in place changes the mtime or length, a
/// replace-by-rename changes the inode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    mtime: SystemTime,
    ino: u64,
    len: u64,
}

/// The allowlist file, remembering which version was last read.
#[derive(Debug)]
pub struct AllowFile {
    path: PathBuf,
    seen: Option<Stamp>,
}

impl AllowFile {
    /// Track `path`; nothing is read until [`Self::reload`].
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            seen: None,
        }
    }

    /// The tracked path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the file if it changed since the last attempt, or always when `force` (SIGHUP).
    /// `Ok(None)`: unchanged. `Err`: unreadable, too open or invalid; the caller keeps its current
    /// list, and the same version is not retried until it changes again or `force`.
    pub fn reload(&mut self, force: bool) -> Result<Option<(Allowlist, usize)>, String> {
        let shown = self.path.display();
        let meta = std::fs::metadata(&self.path).map_err(|e| format!("{shown}: {e}"))?;
        let stamp = Stamp {
            mtime: meta.modified().map_err(|e| format!("{shown}: {e}"))?,
            ino: meta.ino(),
            len: meta.len(),
        };
        if !force && self.seen == Some(stamp) {
            return Ok(None);
        }
        self.seen = Some(stamp);
        let mode = meta.permissions().mode();
        if mode & 0o077 != 0 {
            return Err(format!(
                "{shown} must not be group/world accessible (mode {:o})",
                mode & 0o777
            ));
        }
        if meta.len() > MAX_BYTES {
            return Err(format!("{shown} is larger than {MAX_BYTES} bytes"));
        }
        let text = std::fs::read_to_string(&self.path).map_err(|e| format!("{shown}: {e}"))?;
        parse(&text).map(Some).map_err(|e| format!("{shown}: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn tmp() -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("ac-egress-{}-{n}.txt", std::process::id()))
    }

    fn write(path: &Path, text: &str, mode: u32) {
        let _ = std::fs::write(path, text);
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }

    #[test]
    fn parse_skips_comments_and_blank_lines() {
        let (list, n) = parse("# agents\n\napi.anthropic.com  # model API\n  *.github.com\n")
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(n, 2);
        assert!(list.permits("api.anthropic.com") && list.permits("raw.github.com"));
        assert!(parse("ok.com\nhttp://bad\n").is_err());
        assert_eq!(parse("# nothing\n").map(|(_, n)| n), Ok(0));
    }

    #[test]
    fn reload_reads_only_changed_versions() {
        let path = tmp();
        write(&path, "a.com\n", 0o600);
        let mut file = AllowFile::new(&path);
        let first = file.reload(false).unwrap_or_else(|e| panic!("{e}"));
        assert!(first.is_some_and(|(l, _)| l.permits("a.com")));
        assert!(matches!(file.reload(false), Ok(None)));
        assert!(matches!(file.reload(true), Ok(Some(_))));
        write(&path, "a.com\nb.com\n", 0o600);
        let second = file.reload(false).unwrap_or_else(|e| panic!("{e}"));
        assert!(second.is_some_and(|(l, n)| n == 2 && l.permits("b.com")));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_bad_version_is_reported_once_and_kept_out() {
        let path = tmp();
        write(&path, "a.com\n", 0o600);
        let mut file = AllowFile::new(&path);
        assert!(file.reload(false).is_ok());
        write(&path, "a.com\nnot a host\n", 0o600);
        assert!(file.reload(false).is_err());
        assert!(matches!(file.reload(false), Ok(None)));
        assert!(file.reload(true).is_err());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn group_or_world_access_is_refused() {
        let path = tmp();
        write(&path, "a.com\n", 0o644);
        let err = AllowFile::new(&path)
            .reload(false)
            .err()
            .unwrap_or_default();
        assert!(err.contains("group/world"), "{err}");
        let _ = std::fs::remove_file(path);
        assert!(AllowFile::new(tmp()).reload(false).is_err());
    }
}
