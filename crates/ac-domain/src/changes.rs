//! What a job changed in its workspace (ADR-019), read from `btrfs receive --dump` of an
//! incremental `btrfs send --no-data` between the snapshots taken as it started and as it ended.
//! Pure: the listing in, the paths out.

use std::collections::{BTreeSet, HashSet};

/// The workspace paths a `btrfs receive --dump` listing says changed: added, removed, renamed
/// (both names), written, truncated, or with a new owner, mode or extended attribute. Sorted,
/// unique, relative to the workspace root and still escaped as btrfs prints them (`\ ` for a
/// space, `\\` for a backslash, `\ooo` for a control byte), so none holds whitespace or a
/// control character.
///
/// Left out: changes to times only (`utimes`; a `touch` is not seen), the root itself, and the
/// temporary `o<ino>-<gen>-<seq>` names send gives new files before renaming them. A file the
/// agent itself names like that is left out only where it is created or renamed to.
pub fn paths(dump: &str) -> Vec<String> {
    let mut lines = dump.lines().map(words);
    // The first line names the received snapshot, `./<name>`, which every path starts with.
    let Some(prefix) = lines.next().and_then(|w| Some(format!("{}/", w.get(1)?))) else {
        return Vec::new();
    };
    let rel = |p: &str| {
        p.strip_prefix(prefix.as_str())
            .filter(|r| !r.is_empty())
            .map(str::to_owned)
    };
    let (mut changed, mut temps) = (BTreeSet::new(), HashSet::new());
    let mut note = |path: String, temps: &mut HashSet<String>| {
        if is_temp(&path) {
            temps.insert(path);
        } else {
            changed.insert(path);
        }
    };
    for w in lines {
        let (Some(cmd), Some(path)) = (w.first(), w.get(1).and_then(|p| rel(p))) else {
            continue;
        };
        match cmd.as_str() {
            "utimes" => {}
            "rename" => {
                if !temps.remove(&path) {
                    note(path, &mut temps);
                }
                let dest = w[2..].iter().find_map(|a| a.strip_prefix("dest="));
                if let Some(dest) = dest.and_then(rel) {
                    note(dest, &mut temps);
                }
            }
            _ => note(path, &mut temps),
        }
    }
    changed.into_iter().collect()
}

/// Whitespace-separated words, a backslash keeping the character after it (and itself) in the
/// word.
fn words(line: &str) -> Vec<String> {
    let (mut out, mut word, mut chars) = (Vec::new(), String::new(), line.chars());
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                word.push(c);
                word.extend(chars.next());
            }
            c if c.is_whitespace() => {
                if !word.is_empty() {
                    out.push(std::mem::take(&mut word));
                }
            }
            c => word.push(c),
        }
    }
    if !word.is_empty() {
        out.push(word);
    }
    out
}

/// Whether the last component is send's temporary name for a new inode: `o<ino>-<gen>-<seq>`.
fn is_temp(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    let Some(rest) = name.strip_prefix('o') else {
        return false;
    };
    let parts: Vec<&str> = rest.split('-').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from btrfs-progs 6.6.3 (kernel 6.18): a file appended to, one removed, one
    /// renamed, a mode changed, a directory's time changed, and a new file, directory, file in
    /// it, symlink and file with a control byte in its name.
    const DUMP: &str = r"snapshot        ./j1.end                        uuid=16708d1f-c905-2c41-b033-de76ff3da480 transid=8 parent_uuid=70667361-3e82-d94f-b465-8f44a578af0a parent_transid=7
chmod           ./j1.end/keep                   mode=700
utimes          ./j1.end/keep                   atime=2026-10-28T19:21:04+0000 mtime=2026-10-28T19:21:04+0000 ctime=2026-10-28T19:21:04+0000
update_extent   ./j1.end/edit                   offset=0 len=4
unlink          ./j1.end/gone
link            ./j1.end/dir/new                dest=dir/old\ name
unlink          ./j1.end/dir/old\ name
mkfile          ./j1.end/o263-8-0
rename          ./j1.end/o263-8-0               dest=./j1.end/new\ file
update_extent   ./j1.end/new\ file              offset=0 len=2
chown           ./j1.end/new\ file              gid=0 uid=0
mkdir           ./j1.end/o264-8-0
rename          ./j1.end/o264-8-0               dest=./j1.end/sub
chmod           ./j1.end/sub                    mode=755
mkfile          ./j1.end/o265-8-0
rename          ./j1.end/o265-8-0               dest=./j1.end/sub/f
symlink         ./j1.end/o266-8-0               dest=keep
rename          ./j1.end/o266-8-0               dest=./j1.end/link
mkfile          ./j1.end/o268-8-0
rename          ./j1.end/o268-8-0               dest=./j1.end/ctl\001x
utimes          ./j1.end/                       atime=2026-10-28T19:21:04+0000 mtime=2026-10-28T19:21:04+0000 ctime=2026-10-28T19:21:04+0000
utimes          ./j1.end/dir                    atime=2026-10-28T19:21:04+0000 mtime=2026-10-28T19:21:04+0000 ctime=2026-10-28T19:21:04+0000
";

    #[test]
    fn a_real_listing() {
        assert_eq!(
            paths(DUMP),
            [
                r"ctl\001x",
                "dir/new",
                r"dir/old\ name",
                "edit",
                "gone",
                "keep",
                "link",
                r"new\ file",
                "sub",
                "sub/f"
            ]
        );
    }

    #[test]
    fn a_rename_of_an_existing_file_shows_both_names() {
        let dump = "snapshot ./s uuid=x\nrename ./s/a dest=./s/b\n";
        assert_eq!(paths(dump), ["a", "b"]);
        // Send's own two-step rename through a temporary name.
        let dump =
            "snapshot ./s uuid=x\nrename ./s/a dest=./s/o9-1-0\nrename ./s/o9-1-0 dest=./s/b\n";
        assert_eq!(paths(dump), ["a", "b"]);
    }

    #[test]
    fn nothing_changed_or_nothing_readable() {
        assert!(paths("").is_empty());
        assert!(paths("snapshot ./s uuid=x\nutimes ./s/ atime=1\n").is_empty());
        assert!(paths("snapshot ./s uuid=x\nwrite ./other/f offset=0\n").is_empty());
    }

    #[test]
    fn temporary_names() {
        assert!(is_temp("o257-8-0") && is_temp("dir/o1-2-3"));
        for not in ["o257-8", "o-1-2", "x257-8-0", "o2a-1-1", "o1-2-3/f"] {
            assert!(!is_temp(not), "{not}");
        }
    }

    #[test]
    fn words_keep_escapes() {
        assert_eq!(words(r"a  b\ c \\ d"), ["a", r"b\ c", r"\\", "d"]);
    }
}
