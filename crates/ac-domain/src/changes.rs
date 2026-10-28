//! What a job changed in its workspace (ADR-019), read from `btrfs receive --dump` of an
//! incremental `btrfs send --no-data` between the snapshots taken as it started and as it ended.
//! Pure: the listing in, the paths out.

use std::collections::{BTreeSet, HashMap};

/// The workspace paths a `btrfs receive --dump` listing says changed: added, removed, renamed
/// (both names), written, truncated, or with a new owner, mode or extended attribute. Sorted,
/// unique, relative to the workspace root and still escaped as btrfs prints them (`\ ` for a
/// space, `\\` for a backslash, `\ooo` for a control byte), so none holds whitespace or a
/// control character.
///
/// Left out: changes to times only (`utimes`; a `touch` is not seen), the root itself, and the
/// temporary `o<ino>-<gen>-<seq>` names send gives inodes on their way in or out. A path under
/// such a name (a new directory's files, written before it is renamed into place, or a removed
/// directory's files, unlinked after it is renamed away) is shown under the directory's real
/// name. A file the agent itself names like that is left out.
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
    // Every path named, and each temporary name's real one: where it was renamed to, or for a
    // name something was renamed to (a removal), where it came from.
    let (mut named, mut real) = (Vec::new(), HashMap::new());
    for w in lines {
        let (Some(cmd), Some(path)) = (w.first(), w.get(1).and_then(|p| rel(p))) else {
            continue;
        };
        match cmd.as_str() {
            "utimes" => {}
            "rename" => {
                let dest = w[2..].iter().find_map(|a| a.strip_prefix("dest="));
                if let Some(dest) = dest.and_then(rel) {
                    if is_temp(&path) {
                        real.insert(path.clone(), dest.clone());
                    } else if is_temp(&dest) {
                        real.insert(dest.clone(), path.clone());
                    }
                    named.push(dest);
                }
                named.push(path);
            }
            _ => named.push(path),
        }
    }
    let changed: BTreeSet<String> = named
        .into_iter()
        .map(|p| resolve(p, &real))
        .filter(|p| !p.split('/').any(is_temp))
        .collect();
    changed.into_iter().collect()
}

/// `path` with its longest temporary prefix replaced by that name's real one, until none is left
/// (a temporary name can be renamed into another; bounded in case one names itself).
fn resolve(mut path: String, real: &HashMap<String, String>) -> String {
    for _ in 0..16 {
        let prefix = path
            .match_indices('/')
            .map(|(i, _)| &path[..i])
            .chain([path.as_str()])
            .rfind(|p| real.contains_key(*p));
        let Some(prefix) = prefix else { break };
        path = format!("{}{}", real[prefix], &path[prefix.len()..]);
    }
    path
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

    /// Shapes seen on BaileyServ (btrfs-progs 7.1, kernel 7.2): a new directory whose files are
    /// written before it is renamed into place, and a removed one renamed away before its files
    /// are unlinked.
    #[test]
    fn files_under_a_temporary_directory_get_its_real_name() {
        let dump = "snapshot ./s uuid=x
mkdir ./s/o20-1-0
mkfile ./s/o21-1-0
rename ./s/o21-1-0 dest=./s/o20-1-0/exit_code
update_extent ./s/o20-1-0/exit_code offset=0 len=2
mkfile ./s/o20-1-0/result.txt
rename ./s/o20-1-0 dest=./s/.jobs/j2
rename ./s/.jobs/j1 dest=./s/o9-1-0
unlink ./s/o9-1-0/prompt.txt
rmdir ./s/o9-1-0
mkfile ./s/o30-1-0
";
        assert_eq!(
            paths(dump),
            [
                ".jobs/j1",
                ".jobs/j1/prompt.txt",
                ".jobs/j2",
                ".jobs/j2/exit_code",
                ".jobs/j2/result.txt"
            ]
        );
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
