//! Backups over the API (ADR-031): what is at the destination and the backup unit's last run, and
//! a run started through the unit. The gateway only reads the destination; a run is always
//! `ac-backup.service`'s (its nice, I/O class, sandbox and one run at a time), never a send of
//! the gateway's own.

use std::path::{Path, PathBuf};
use std::process::Command;

use ac_domain::backup::{self, BackupId, Stream};
use ac_domain::{state, AgentId};

use crate::{error, escape, Reply};

/// The unit that runs backups.
pub const UNIT: &str = "ac-backup.service";

/// The marker a destination holds (ADR-031): see [`ac_domain::backup::Destination`].
pub use ac_domain::backup::MARKER;

/// The unit as systemd knows it.
pub trait Unit: Send + Sync {
    /// `Key=value` lines of `systemctl show`, timestamps as `@<unix seconds>`.
    fn show(&self) -> Result<String, String>;
    /// Start it without waiting for the run.
    fn start(&self) -> Result<(), String>;
}

/// [`Unit`] through `systemctl`.
pub struct Systemd(pub &'static str);

impl Systemd {
    fn systemctl(&self, args: &[&str]) -> Result<String, String> {
        let out = Command::new("systemctl")
            .args(args)
            .arg(self.0)
            .output()
            .map_err(|e| format!("systemctl: {e}"))?;
        let text = |b: &[u8]| String::from_utf8_lossy(b).trim().to_owned();
        match out.status.success() {
            true => Ok(text(&out.stdout)),
            false => Err(format!("systemctl {}: {}", args[0], text(&out.stderr))),
        }
    }
}

impl Unit for Systemd {
    fn show(&self) -> Result<String, String> {
        self.systemctl(&[
            "show",
            "--timestamp=unix",
            "-p",
            "ActiveState,Result,ExecMainStartTimestamp,ExecMainExitTimestamp",
        ])
    }

    fn start(&self) -> Result<(), String> {
        self.systemctl(&["start", "--no-block"]).map(drop)
    }
}

/// The unit's last run, from [`Unit::show`].
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Run {
    /// `active`, `activating`, `inactive`, `failed`...
    pub active: String,
    /// `success`, `exit-code`...
    pub result: String,
    /// When the last run started (Unix seconds), if it ever did.
    pub started: Option<u64>,
    /// When it ended, if it has.
    pub finished: Option<u64>,
}

impl Run {
    /// Read `Key=value` lines; unknown keys and unparsable values are left out.
    pub fn parse(text: &str) -> Self {
        let mut run = Self::default();
        let stamp = |v: &str| v.strip_prefix('@').and_then(|s| s.parse().ok());
        for (key, value) in text.lines().filter_map(|l| l.split_once('=')) {
            match key {
                "ActiveState" => run.active = value.to_owned(),
                "Result" => run.result = value.to_owned(),
                "ExecMainStartTimestamp" => run.started = stamp(value),
                "ExecMainExitTimestamp" => run.finished = stamp(value),
                _ => {}
            }
        }
        run
    }

    /// Whether a run is under way.
    pub fn running(&self) -> bool {
        matches!(self.active.as_str(), "active" | "activating" | "reloading")
    }

    fn json(&self) -> String {
        format!(
            r#"{{"active":"{}","result":"{}","started":{},"finished":{}}}"#,
            escape(&self.active),
            escape(&self.result),
            number(self.started),
            number(self.finished)
        )
    }
}

/// One stream file at the destination.
#[derive(Debug, PartialEq, Eq)]
struct Item {
    stream: Stream,
    bytes: u64,
}

/// The destination and the unit that writes to it.
pub struct Backups {
    dest: PathBuf,
    unit: Box<dyn Unit>,
}

impl Backups {
    /// Read `dest`; start runs through `unit`.
    pub fn new(dest: impl Into<PathBuf>, unit: Box<dyn Unit>) -> Self {
        Self {
            dest: dest.into(),
            unit,
        }
    }

    /// The destination.
    pub fn dest(&self) -> &Path {
        &self.dest
    }

    /// `{"dest","run","state":[ids],"agents":[{"id","backups":[{"id","parent","bytes",
    /// "restorable"}]}]}`, oldest first. 503 if the destination is not there.
    pub fn list(&self) -> Reply {
        if let Err(reply) = self.present() {
            return reply;
        }
        let listed = state_ids(&self.dest).and_then(|s| Ok((s, agents(&self.dest)?)));
        let (state, agents) = match listed {
            Ok(found) => found,
            Err(e) => return failed(&e),
        };
        let run = self.run().unwrap_or_else(|e| {
            eprintln!("ac-gateway: backups: {e}");
            Run::default()
        });
        let state: Vec<String> = state.iter().map(|id| format!(r#""{id}""#)).collect();
        let agents: Vec<String> = agents
            .iter()
            .map(|(id, items)| agent_json(id, items))
            .collect();
        Reply {
            status: 200,
            body: format!(
                r#"{{"dest":"{}","run":{},"state":[{}],"agents":[{}]}}"#,
                escape(&self.dest.to_string_lossy()),
                run.json(),
                state.join(","),
                agents.join(",")
            ),
        }
    }

    /// Start the unit: 202, or 409 while a run is under way, 503 without the destination.
    pub fn start(&self) -> Reply {
        if let Err(reply) = self.present() {
            return reply;
        }
        // Best effort: a run can start between `show` and `start`, or wait on its mount while
        // still inactive. systemd merges the two start jobs, so the worst case is a 202 for a run
        // already under way.
        match self.run() {
            Ok(run) if run.running() => return error(409, "a backup is already running"),
            Ok(_) => {}
            Err(e) => return failed(&e),
        }
        match self.unit.start() {
            Ok(()) => Reply {
                status: 202,
                body: format!(r#"{{"started":"{UNIT}"}}"#),
            },
            Err(e) => failed(&e),
        }
    }

    fn run(&self) -> Result<Run, String> {
        self.unit.show().map(|text| Run::parse(&text))
    }

    /// 503 unless the destination holds [`MARKER`] or earlier backups: an unmounted share's empty
    /// mount point does not count. The gateway only reads, so it never writes the marker.
    pub fn present(&self) -> Result<(), Reply> {
        match ac_ctr::backup::destination(&self.dest) {
            Ok(d) if d.usable() => Ok(()),
            Ok(_) => Err(error(
                503,
                &format!(
                    "backup destination {} holds no backups and no {MARKER}: is the share \
                     mounted? (if it is, create the file there)",
                    self.dest.display()
                ),
            )),
            Err(e) => Err(failed(&e.to_string())),
        }
    }
}

/// The names of the directories (`dirs`) or files in `dir`; nothing there is fine.
fn names(dir: &Path, dirs: bool) -> Result<Vec<String>, String> {
    let entries = match std::fs::read_dir(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{}: {e}", dir.display())),
        Ok(entries) => entries,
    };
    Ok(entries
        .filter_map(Result::ok)
        // Not followed: a symlink, FIFO or socket is neither.
        .filter(|e| {
            e.file_type()
                .is_ok_and(|t| if dirs { t.is_dir() } else { t.is_file() })
        })
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .collect())
}

/// State copies, oldest first; a `.part` is not one.
fn state_ids(dest: &Path) -> Result<Vec<BackupId>, String> {
    let mut ids: Vec<BackupId> = names(&dest.join(state::DIR), true)?
        .iter()
        .filter_map(|n| BackupId::parse(n).ok())
        .collect();
    ids.sort();
    Ok(ids)
}

/// Each agent directory's stream files, by agent id, then oldest first.
fn agents(dest: &Path) -> Result<Vec<(AgentId, Vec<Item>)>, String> {
    let mut ids: Vec<AgentId> = names(dest, true)?
        .iter()
        .filter_map(|n| AgentId::parse(n).ok())
        .collect();
    ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    ids.into_iter()
        .map(|id| {
            let items = items(&dest.join(id.as_str()))?;
            Ok((id, items))
        })
        .collect()
}

fn items(dir: &Path) -> Result<Vec<Item>, String> {
    let mut items: Vec<Item> = names(dir, false)?
        .iter()
        .filter_map(|n| {
            let stream = Stream::parse(n)?;
            let bytes = std::fs::symlink_metadata(dir.join(n)).map_or(0, |m| m.len());
            Some(Item { stream, bytes })
        })
        .collect();
    items.sort_by(|a, b| a.stream.id.cmp(&b.stream.id));
    Ok(items)
}

fn agent_json(id: &AgentId, items: &[Item]) -> String {
    let streams: Vec<Stream> = items.iter().map(|i| i.stream.clone()).collect();
    let restorable = backup::restorable(&streams);
    let rows: Vec<String> = items
        .iter()
        .map(|i| {
            let parent = i
                .stream
                .parent
                .as_ref()
                .map_or_else(|| "null".to_owned(), |p| format!(r#""{p}""#));
            format!(
                r#"{{"id":"{}","parent":{parent},"bytes":{},"restorable":{}}}"#,
                i.stream.id,
                i.bytes,
                restorable.contains(&i.stream.id)
            )
        })
        .collect();
    format!(r#"{{"id":"{id}","backups":[{}]}}"#, rows.join(","))
}

fn number(n: Option<u64>) -> String {
    n.map_or_else(|| "null".to_owned(), |n| n.to_string())
}

/// 500; the detail goes to the journal.
fn failed(e: &str) -> Reply {
    eprintln!("ac-gateway: backups: {e}");
    error(
        500,
        "backups could not be read or started (see the gateway's journal)",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct Fake {
        show: &'static str,
        starts: Arc<AtomicUsize>,
    }

    impl Unit for Fake {
        fn show(&self) -> Result<String, String> {
            Ok(self.show.to_owned())
        }
        fn start(&self) -> Result<(), String> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    const IDLE: &str = "ActiveState=inactive\nResult=success\n\
        ExecMainStartTimestamp=@1764127800\nExecMainExitTimestamp=@1764127860\n";

    fn backups(show: &'static str) -> (Backups, Arc<AtomicUsize>, PathBuf) {
        let dest = std::env::temp_dir().join(format!(
            "ac-backups-{}-{}",
            std::process::id(),
            crate::tokens::random_hex().unwrap_or_default()
        ));
        let starts = Arc::new(AtomicUsize::new(0));
        let unit = Box::new(Fake {
            show,
            starts: Arc::clone(&starts),
        });
        (Backups::new(&dest, unit), starts, dest)
    }

    fn touch(path: &Path, bytes: usize) {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).unwrap_or_else(|e| panic!("{e}"));
        }
        std::fs::write(path, vec![0u8; bytes]).unwrap_or_else(|e| panic!("{e}"));
    }

    /// The destination, mounted: the directory and its marker.
    fn mount(dest: &Path) {
        touch(&dest.join(MARKER), 0);
    }

    #[test]
    fn run_from_systemctl_show() {
        let run = Run::parse(IDLE);
        assert_eq!(
            run,
            Run {
                active: "inactive".into(),
                result: "success".into(),
                started: Some(1_764_127_800),
                finished: Some(1_764_127_860),
            }
        );
        assert!(!run.running());
        let never = Run::parse("ActiveState=inactive\nResult=success\nExecMainStartTimestamp=\n");
        assert_eq!((never.started, never.finished), (None, None));
        assert!(Run::parse("ActiveState=activating\n").running());
        assert!(Run::parse("ActiveState=active\n").running());
    }

    #[test]
    fn lists_streams_state_and_the_last_run() {
        let (b, _, dest) = backups(IDLE);
        mount(&dest);
        let a1 = dest.join("a1");
        touch(&a1.join("20261124-033000.full.btrfs"), 5);
        touch(&a1.join("20261125-033000.after-20261124-033000.btrfs"), 2);
        touch(&a1.join("20261126-033000.after-20261120-033000.btrfs"), 1);
        touch(&a1.join("20261127-033000.full.btrfs.part"), 1);
        std::fs::create_dir_all(dest.join("_state/20261125-033000"))
            .unwrap_or_else(|e| panic!("{e}"));
        std::fs::create_dir_all(dest.join("_state/20261126-033000.part"))
            .unwrap_or_else(|e| panic!("{e}"));
        std::fs::create_dir_all(dest.join("not an agent")).unwrap_or_else(|e| panic!("{e}"));
        let r = b.list();
        assert_eq!(r.status, 200, "{}", r.body);
        let want = format!(
            r#"{{"dest":"{}","run":{{"active":"inactive","result":"success","started":1764127800,"finished":1764127860}},"state":["20261125-033000"],"agents":[{{"id":"a1","backups":[{{"id":"20261124-033000","parent":null,"bytes":5,"restorable":true}},{{"id":"20261125-033000","parent":"20261124-033000","bytes":2,"restorable":true}},{{"id":"20261126-033000","parent":"20261120-033000","bytes":1,"restorable":false}}]}}]}}"#,
            dest.display()
        );
        assert_eq!(r.body, want);
        let _ = std::fs::remove_dir_all(dest);
    }

    #[test]
    fn a_missing_destination_is_503() {
        let (b, starts, _) = backups(IDLE);
        assert_eq!(b.list().status, 503);
        assert_eq!(b.start().status, 503);
        assert_eq!(starts.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn start_unless_running() {
        let (b, starts, dest) = backups(IDLE);
        std::fs::create_dir_all(&dest).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(b.start().status, 503, "an empty mount point");
        mount(&dest);
        assert_eq!(b.start().status, 202);
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        let (busy, starts, dest2) = backups("ActiveState=activating\n");
        mount(&dest2);
        assert_eq!(busy.start().status, 409);
        assert_eq!(starts.load(Ordering::SeqCst), 0);
        let _ = std::fs::remove_dir_all(dest);
        let _ = std::fs::remove_dir_all(dest2);
    }
}
