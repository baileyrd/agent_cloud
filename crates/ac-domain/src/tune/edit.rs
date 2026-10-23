//! The edits `ac-ctl tune` makes (ADR-014): one key at a time, line by line, so comments, layout
//! and keys it does not own stay as they were. Each returns the new text, which equals the old
//! one when the key already has the value: applying an edit twice changes nothing. Pure.

use super::facts::{hypervisor_table, kata_assignment};

/// Lines of `text`, and whether it ended with a newline.
fn lines(text: &str) -> (Vec<String>, bool) {
    (
        text.lines().map(str::to_owned).collect(),
        text.ends_with('\n') || text.is_empty(),
    )
}

fn join(lines: &[String], newline: bool) -> String {
    let mut out = lines.join("\n");
    if newline && !lines.is_empty() {
        out.push('\n');
    }
    out
}

/// Set `key = value` in the first `[hypervisor.*]` table of a Kata configuration. An active line
/// is rewritten in place (its indent and trailing comment kept); otherwise the line goes after the
/// commented-out template (`#key = ...`) if the table has one, else after the table's last
/// non-blank line. Refuses a file with no hypervisor table or with the key set twice.
pub fn kata_set(text: &str, key: &str, value: u64) -> Result<String, String> {
    let (mut lines, newline) = lines(text);
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    let (start, end) = hypervisor_table(&refs).ok_or("no [hypervisor.*] table")?;
    let active: Vec<usize> = (start + 1..end)
        .filter(|&i| kata_assignment(&lines[i]).is_some_and(|(k, _)| k == key))
        .collect();
    let new = format!("{key} = {value}");
    match active.as_slice() {
        [] => {
            let template = (start + 1..end).find(|&i| {
                let l = lines[i].trim_start();
                l.starts_with('#')
                    && kata_assignment(l.trim_start_matches('#')).is_some_and(|(k, _)| k == key)
            });
            let at = template.map_or_else(
                || {
                    (start..end)
                        .rev()
                        .find(|&i| !lines[i].trim().is_empty())
                        .unwrap_or(start)
                        + 1
                },
                |i| i + 1,
            );
            lines.insert(at, new);
        }
        [i] => {
            let line = &lines[*i];
            if kata_assignment(line).is_some_and(|(_, v)| v == value.to_string()) {
                return Ok(text.to_owned());
            }
            let indent = &line[..line.len() - line.trim_start().len()];
            let comment = line.find('#').map_or("", |at| &line[at..]);
            let sep = if comment.is_empty() { "" } else { " " };
            lines[*i] = format!("{indent}{new}{sep}{comment}");
        }
        _ => return Err(format!("{key} is set twice")),
    }
    Ok(join(&lines, newline))
}

/// Set `KEY=value` in an environment file: the last active assignment of the key is rewritten
/// (it is the one that counts), or the line is appended.
pub fn env_set(text: &str, key: &str, value: &str) -> String {
    let (mut lines, _) = lines(text);
    let new = format!("{key}={value}");
    let last = lines.iter().rposition(|l| {
        let l = l.trim();
        !l.starts_with(['#', ';']) && l.split_once('=').is_some_and(|(k, _)| k.trim() == key)
    });
    match last {
        Some(i)
            if super::facts::env_file(&lines[i])
                .get(key)
                .map(String::as_str)
                == Some(value) =>
        {
            return text.to_owned();
        }
        Some(i) => lines[i] = new,
        None => lines.push(new),
    }
    join(&lines, true)
}

/// Set the `/dev/shm` line of an fstab to `size_mib`: its `size=` option is replaced (or added),
/// the rest of the line kept; without a line, one is appended in the style of
/// `deploy/dev-shm.fstab`, with `options` (the running mount's) before `size=`.
pub fn fstab_shm(text: &str, size_mib: u64, options: &[String]) -> String {
    let (mut lines, _) = lines(text);
    let size = format!("size={size_mib}M");
    let last = lines.iter().rposition(|l| {
        let l = l.trim();
        !l.starts_with('#') && l.split_whitespace().nth(1) == Some("/dev/shm")
    });
    match last {
        Some(i) => {
            let fields: Vec<&str> = lines[i].split_whitespace().collect();
            let old = fields.get(3).copied().unwrap_or("defaults");
            let mut opts: Vec<&str> = old.split(',').filter(|o| !o.starts_with("size=")).collect();
            opts.push(&size);
            let opts = opts.join(",");
            if old.split(',').any(|o| o == size) {
                return text.to_owned();
            }
            let dump = fields.get(4).copied().unwrap_or("0");
            let pass = fields.get(5).copied().unwrap_or("0");
            lines[i] = format!(
                "{}  {}  {}  {opts}  {dump}  {pass}",
                fields[0], fields[1], fields[2]
            );
        }
        None => {
            let mut opts: Vec<&str> = options.iter().map(String::as_str).collect();
            if opts.is_empty() {
                opts = vec!["nosuid", "nodev"];
            }
            opts.push(&size);
            lines.push("# /dev/shm sized for agent guests: MemTotal minus the host reserve (ADR-012, ADR-014)".into());
            lines.push(format!("tmpfs  /dev/shm  tmpfs  {}  0  0", opts.join(",")));
        }
    }
    join(&lines, true)
}

/// One change between two texts, as a `diff -U0` hunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// First old line (1-based) and how many lines were removed.
    pub old: (usize, usize),
    /// First new line (1-based) and how many lines were added.
    pub new: (usize, usize),
    /// Removed lines.
    pub removed: Vec<String>,
    /// Added lines.
    pub added: Vec<String>,
}

/// The hunks that turn `old` into `new`, for edits that change, add or remove whole lines in place
/// (the edits above): lines are matched from both ends, and what differs between is one hunk per
/// run of changed lines.
pub fn hunks(old: &str, new: &str) -> Vec<Hunk> {
    let (a, b): (Vec<&str>, Vec<&str>) = (old.lines().collect(), new.lines().collect());
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            (i, j) = (i + 1, j + 1);
            continue;
        }
        // Find the next line where the two sides line up again: the nearest (di, dj).
        let resync = (0..=(a.len() - i + b.len() - j))
            .flat_map(|d| (0..=d).map(move |di| (di, d - di)))
            .find(|&(di, dj)| {
                let (x, y) = (i + di, j + dj);
                (x == a.len() && y == b.len()) || (x < a.len() && y < b.len() && a[x] == b[y])
            })
            .unwrap_or((a.len() - i, b.len() - j));
        let (di, dj) = resync;
        out.push(Hunk {
            old: (if di == 0 { i } else { i + 1 }, di),
            new: (if dj == 0 { j } else { j + 1 }, dj),
            removed: a[i..i + di].iter().map(|s| (*s).to_owned()).collect(),
            added: b[j..j + dj].iter().map(|s| (*s).to_owned()).collect(),
        });
        (i, j) = (i + di, j + dj);
    }
    out
}

/// `old` to `new` as a unified diff with no context, headed by `path`; empty when they are equal.
pub fn diff(path: &str, old: &str, new: &str) -> String {
    let hunks = hunks(old, new);
    if hunks.is_empty() {
        return String::new();
    }
    let mut out = format!("--- {path}\n+++ {path} (proposed)\n");
    for h in hunks {
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            h.old.0, h.old.1, h.new.0, h.new.1
        ));
        for l in &h.removed {
            out.push_str(&format!("-{l}\n"));
        }
        for l in &h.added {
            out.push_str(&format!("+{l}\n"));
        }
    }
    out
}
