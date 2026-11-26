//! Backups over the API (ADR-031): what is at the destination and the backup unit's last run, a
//! run started through the unit, and a paused agent restored from one. A run is always
//! `ac-backup.service`'s (its nice, I/O class, sandbox and one run at a time), never a send of
//! the gateway's own; a restore is ADR-021's, under the same `backup.lock` as `ac-ctl`.

use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};
use std::process::Command;

use ac_ctr::facts::{kata_sandboxes, HostRead};
use ac_ctr::CtrRuntime;
use ac_domain::backup::{self, BackupId, Stream};
use ac_domain::{state, AgentId, CheckpointId};
use ac_ports::Snapshots;
use platform::process::Spawner;

use crate::admin::Done;
use crate::{error, escape, members, Hold, Reply};

/// The lock a backup and a restore hold against each other, in the data directory (`ac-ctl`'s).
const LOCK: &str = "backup.lock";

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

/// Why a restore did not happen.
#[derive(Debug, PartialEq, Eq)]
pub enum RestoreError {
    /// Refused as it stands (a run holds the lock, a plain-directory workspace): 409.
    Refused(String),
    /// It failed: 500.
    Failed(String),
}

/// Puts a backup in place of a held agent's workspace.
pub trait Restorer: Send + Sync {
    /// Restore backup `b` of `id` from `dest`; the checkpoint that keeps the replaced workspace,
    /// `None` if there was none.
    fn restore(
        &self,
        dest: &Path,
        id: &AgentId,
        b: &BackupId,
    ) -> Result<Option<CheckpointId>, RestoreError>;
}

/// [`Restorer`] through the runtime, under `<data>/backup.lock`; `host` tells whether the
/// agent's VM still runs (a sandbox left over after a pause, say).
pub struct HostRestorer<S, H> {
    rt: CtrRuntime<S>,
    host: H,
    data: PathBuf,
}

impl<S, H> HostRestorer<S, H> {
    /// Restore with `rt`, read processes from `host`, lock in `data`.
    pub fn new(rt: CtrRuntime<S>, host: H, data: impl Into<PathBuf>) -> Self {
        Self {
            rt,
            host,
            data: data.into(),
        }
    }
}

impl<S: Spawner + Send + Sync, H: HostRead + Send + Sync> Restorer for HostRestorer<S, H> {
    fn restore(
        &self,
        dest: &Path,
        id: &AgentId,
        b: &BackupId,
    ) -> Result<Option<CheckpointId>, RestoreError> {
        let _held = lock(&self.data)?;
        // Picked before the lock: a run may have pruned it since.
        let still = self
            .rt
            .backups(dest, id)
            .map_err(|e| RestoreError::Failed(e.to_string()))?;
        if !still.contains(b) {
            return Err(RestoreError::Refused(format!(
                "backup {b} of {id} is no longer restorable: list again"
            )));
        }
        let running = kata_sandboxes(&self.host.cmdlines())
            .iter()
            .any(|s| s == id.as_str());
        let volume = self
            .rt
            .volume(id)
            .map_err(|e| RestoreError::Failed(e.to_string()))?;
        backup::check_restore(id, running, volume)
            .map_err(|e| RestoreError::Refused(e.to_string()))?;
        self.rt
            .restore_backup(dest, id, b)
            .map_err(|e| RestoreError::Failed(e.to_string()))
    }
}

/// `ac-ctl`'s lock: an OS file lock on `<data>/backup.lock`, released when the file closes.
fn lock(data: &Path) -> Result<File, RestoreError> {
    let path = data.join(LOCK);
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| RestoreError::Failed(format!("{}: {e}", path.display())))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(RestoreError::Refused(
            "a backup or another restore is running".into(),
        )),
        Err(TryLockError::Error(e)) => Err(RestoreError::Failed(format!(
            "lock {}: {e}",
            path.display()
        ))),
    }
}

/// The backup a restore body names: `{"backup": "<id>"}`, or `None` (empty body or no member)
/// for the newest restorable.
pub fn wanted(body: &[u8]) -> Result<Option<BackupId>, Reply> {
    let Some(value) = members(body, &["backup"])? else {
        return Ok(None);
    };
    match value.get("backup") {
        None => Ok(None),
        Some(v) => {
            let text = v
                .as_str()
                .ok_or_else(|| error(400, "backup must be a string"))?;
            BackupId::parse(text)
                .map(Some)
                .map_err(|e| error(400, &format!("backup: {e}")))
        }
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
    restorer: Box<dyn Restorer>,
}

impl Backups {
    /// Read `dest`; start runs through `unit`; restore through `restorer`.
    pub fn new(dest: impl Into<PathBuf>, unit: Box<dyn Unit>, restorer: Box<dyn Restorer>) -> Self {
        Self {
            dest: dest.into(),
            unit,
            restorer,
        }
    }

    /// Restore the held agent from backup `wanted`, or its newest restorable: 200
    /// `{"id","restored","kept"}`; 404 without such a backup, 409 refused, 503 without the
    /// destination. The audit adds ` backup=<id>` once one is picked.
    pub fn restore(&self, hold: &Hold, wanted: Option<&BackupId>) -> Done {
        let id = hold.id();
        let done = |reply, audit: String| Done { reply, audit };
        if let Err(reply) = self.present() {
            return done(reply, String::new());
        }
        let streams: Vec<Stream> = match items(&self.dest.join(id.as_str())) {
            Ok(items) => items.into_iter().map(|i| i.stream).collect(),
            Err(e) => return done(failed(&e), String::new()),
        };
        let all = backup::restorable(&streams);
        let b = match backup::pick(id, &all, wanted) {
            Ok(b) => b,
            Err(e) => return done(error(404, &e.to_string()), String::new()),
        };
        let reply = match self.restorer.restore(&self.dest, id, b) {
            Ok(kept) => Reply {
                status: 200,
                body: format!(
                    r#"{{"id":"{id}","restored":"{b}","kept":{}}}"#,
                    kept.map_or_else(|| "null".to_owned(), |k| format!(r#""{k}""#))
                ),
            },
            Err(RestoreError::Refused(e)) => error(409, &e),
            Err(RestoreError::Failed(e)) => failed(&e),
        };
        done(reply, format!(" backup={b}"))
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

    /// Restores nothing; records what it was asked, and answers `answer`.
    struct FakeRestorer {
        asked: Arc<std::sync::Mutex<Vec<String>>>,
        answer: fn() -> Result<Option<CheckpointId>, RestoreError>,
    }

    impl Restorer for FakeRestorer {
        fn restore(
            &self,
            _: &Path,
            id: &AgentId,
            b: &BackupId,
        ) -> Result<Option<CheckpointId>, RestoreError> {
            if let Ok(mut asked) = self.asked.lock() {
                asked.push(format!("{id} {b}"));
            }
            (self.answer)()
        }
    }

    fn kept() -> Result<Option<CheckpointId>, RestoreError> {
        Ok(CheckpointId::parse("20261126-120000").ok())
    }

    fn backups(show: &'static str) -> (Backups, Arc<AtomicUsize>, PathBuf) {
        backups_with(show, kept).0
    }

    #[allow(clippy::type_complexity)]
    fn backups_with(
        show: &'static str,
        answer: fn() -> Result<Option<CheckpointId>, RestoreError>,
    ) -> (
        (Backups, Arc<AtomicUsize>, PathBuf),
        Arc<std::sync::Mutex<Vec<String>>>,
    ) {
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
        let asked = Arc::default();
        let restorer = Box::new(FakeRestorer {
            asked: Arc::clone(&asked),
            answer,
        });
        ((Backups::new(&dest, unit, restorer), starts, dest), asked)
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

    fn agent(id: &str) -> Hold {
        Hold::detached(AgentId::parse(id).unwrap_or_else(|e| panic!("{e}")))
    }

    #[test]
    fn restore_picks_the_newest_restorable_or_the_one_named() {
        let ((b, _, dest), asked) = backups_with(IDLE, kept);
        mount(&dest);
        let a1 = dest.join("a1");
        touch(&a1.join("20261124-033000.full.btrfs"), 1);
        touch(&a1.join("20261125-033000.after-20261124-033000.btrfs"), 1);
        touch(&a1.join("20261126-033000.after-20261120-033000.btrfs"), 1);
        let d = b.restore(&agent("a1"), None);
        assert_eq!(
            (d.reply.status, d.reply.body.as_str(), d.audit.as_str()),
            (
                200,
                r#"{"id":"a1","restored":"20261125-033000","kept":"20261126-120000"}"#,
                " backup=20261125-033000"
            ),
            "the broken chain is skipped"
        );
        let first = BackupId::parse("20261124-033000").ok();
        assert_eq!(b.restore(&agent("a1"), first.as_ref()).reply.status, 200);
        let broken = BackupId::parse("20261126-033000").ok();
        assert_eq!(b.restore(&agent("a1"), broken.as_ref()).reply.status, 404);
        assert_eq!(
            b.restore(&agent("a2"), None).reply.status,
            404,
            "no backups"
        );
        let asked = asked.lock().map(|a| a.clone()).unwrap_or_default();
        assert_eq!(asked, ["a1 20261125-033000", "a1 20261124-033000"]);
        let _ = std::fs::remove_dir_all(dest);
    }

    #[test]
    fn restore_refusals_and_failures() {
        let refused = || {
            Err(RestoreError::Refused(
                "a backup or another restore is running".into(),
            ))
        };
        let ((b, _, dest), _) = backups_with(IDLE, refused);
        assert_eq!(
            b.restore(&agent("a1"), None).reply.status,
            503,
            "no destination"
        );
        mount(&dest);
        touch(&dest.join("a1/20261124-033000.full.btrfs"), 1);
        let d = b.restore(&agent("a1"), None);
        assert_eq!(d.reply.status, 409, "{}", d.reply.body);
        assert_eq!(d.audit, " backup=20261124-033000");
        let failed = || Err(RestoreError::Failed("btrfs receive: no space".into()));
        let ((b, _, dest2), _) = backups_with(IDLE, failed);
        mount(&dest2);
        touch(&dest2.join("a1/20261124-033000.full.btrfs"), 1);
        assert_eq!(b.restore(&agent("a1"), None).reply.status, 500);
        let _ = std::fs::remove_dir_all(dest);
        let _ = std::fs::remove_dir_all(dest2);
    }

    #[test]
    fn restore_bodies() {
        assert_eq!(wanted(b""), Ok(None));
        assert_eq!(wanted(b"{}"), Ok(None));
        assert_eq!(
            wanted(br#"{"backup":"20261124-033000"}"#),
            Ok(BackupId::parse("20261124-033000").ok())
        );
        for bad in [
            &br#"{"backup":"../x"}"#[..],
            br#"{"backup":1}"#,
            br#"{"id":"a1"}"#,
            b"[]",
        ] {
            assert_eq!(
                wanted(bad).err().map(|r| r.status),
                Some(400),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn a_second_lock_is_refused_until_the_first_is_dropped() {
        let dir = std::env::temp_dir().join(format!("ac-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("{e}"));
        let held = lock(&dir).unwrap_or_else(|e| panic!("{e:?}"));
        assert!(matches!(lock(&dir), Err(RestoreError::Refused(_))));
        drop(held);
        assert!(lock(&dir).is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }
}
