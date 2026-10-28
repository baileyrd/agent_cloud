//! Workspace backups (ADR-021) as pure rules. A backup is a `btrfs send` stream kept as a file:
//! a full one, or an incremental one sent against the backup before it. Restoring a backup
//! replays its chain, from the last full one up to it. These rules name the files, find chains,
//! pick what the next send is against, and choose what retention may delete without breaking a
//! chain that is kept.

use std::fmt;

use crate::checkpoint::{stamp_secs, CheckpointId, Volume};
use crate::AgentId;

/// A backup's id: the same UTC stamp as a checkpoint's ([`CheckpointId`]), so ids sort by age.
pub type BackupId = CheckpointId;

/// Backups kept per agent at the destination when none is asked for.
pub const DEFAULT_KEEP: usize = 7;

/// A full send at least this often (counting the full one) when none is asked for, so a chain
/// to replay is at most this long.
pub const DEFAULT_FULL_EVERY: usize = 7;

/// The ending of every stream file.
const SUFFIX: &str = ".btrfs";
/// Between an incremental stream's id and its parent's.
const AFTER: &str = ".after-";

/// One stream file: a backup and the backup it was sent against (`None`: in full).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stream {
    /// The backup.
    pub id: BackupId,
    /// What it was sent against.
    pub parent: Option<BackupId>,
}

impl Stream {
    /// Its file name: `<id>.full.btrfs`, or `<id>.after-<parent>.btrfs`.
    pub fn file_name(&self) -> String {
        match &self.parent {
            None => format!("{}.full{SUFFIX}", self.id),
            Some(p) => format!("{}{AFTER}{p}{SUFFIX}", self.id),
        }
    }

    /// The stream a file name stands for; `None` for any other file (a `.part` among them).
    pub fn parse(name: &str) -> Option<Self> {
        let stem = name.strip_suffix(SUFFIX)?;
        if let Some(id) = stem.strip_suffix(".full") {
            let id = BackupId::parse(id).ok()?;
            return Some(Self { id, parent: None });
        }
        let (id, parent) = stem.split_once(AFTER)?;
        let (id, parent) = (BackupId::parse(id).ok()?, BackupId::parse(parent).ok()?);
        (parent < id).then_some(Self {
            id,
            parent: Some(parent),
        })
    }
}

/// The id for a backup made at `unix_secs`: never at or before the newest in `taken`, so ids
/// sort by age even after retention frees one or the clock steps back. Two backups in one
/// second get ids a second apart, a second ahead of the clock.
pub fn next_id(unix_secs: u64, taken: &[BackupId]) -> BackupId {
    let floor = taken
        .iter()
        .filter_map(stamp_secs)
        .max()
        .map_or(0, |s| s + 1);
    CheckpointId::next(unix_secs.max(floor), taken)
}

/// The streams to replay to restore `id`, the full one first; `None` if `id` is not there or
/// its chain is broken (a stream missing).
pub fn chain<'a>(streams: &'a [Stream], id: &BackupId) -> Option<Vec<&'a Stream>> {
    let mut chain = Vec::new();
    let mut at = streams.iter().find(|s| &s.id == id)?;
    loop {
        chain.push(at);
        let Some(parent) = &at.parent else {
            chain.reverse();
            return Some(chain);
        };
        // Parents are older (`Stream::parse`), so this always ends.
        at = streams.iter().find(|s| &s.id == parent)?;
    }
}

/// The backups that can be restored (their chain is whole), oldest first.
pub fn restorable(streams: &[Stream]) -> Vec<BackupId> {
    let mut ids: Vec<BackupId> = streams
        .iter()
        .filter(|s| chain(streams, &s.id).is_some())
        .map(|s| s.id.clone())
        .collect();
    ids.sort();
    ids
}

/// What the next send is against: the newest backup kept on this host (`local`) whose chain at
/// the destination is whole, unless that chain already holds `full_every` streams. `None`:
/// send in full.
pub fn parent<'a>(
    local: &'a [BackupId],
    streams: &[Stream],
    full_every: usize,
) -> Option<&'a BackupId> {
    let newest = local.iter().filter(|b| chain(streams, b).is_some()).max()?;
    let length = chain(streams, newest).map_or(0, |c| c.len());
    (length < full_every.max(1)).then_some(newest)
}

/// The streams retention deletes: all but the newest `keep` restorable backups (at least one,
/// so the next send's parent stays) and the streams their chains need. Broken streams go too.
pub fn prune(streams: &[Stream], keep: usize) -> Vec<&Stream> {
    let kept_ids = restorable(streams);
    let newest = &kept_ids[kept_ids.len().saturating_sub(keep.max(1))..];
    let needed: Vec<&BackupId> = newest
        .iter()
        .filter_map(|id| chain(streams, id))
        .flatten()
        .map(|s| &s.id)
        .collect();
    streams
        .iter()
        .filter(|s| !needed.contains(&&s.id))
        .collect()
}

/// Why restoring a backup is refused. Nothing was changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The agent's VM is running: its bind mount would keep the old workspace.
    Running(AgentId),
    /// The agent's workspace is a plain directory, which cannot be kept as a checkpoint.
    Plain(AgentId),
    /// The destination holds no restorable backup of the agent (or not the one asked for).
    NoBackup(AgentId),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Running(id) => write!(
                f,
                "cannot restore a backup of agent {id}: its VM is running; pause it first"
            ),
            Self::Plain(id) => write!(
                f,
                "cannot restore a backup of agent {id}: its workspace is a plain directory; run \
                 `ac-ctl workspace migrate {id}` first"
            ),
            Self::NoBackup(id) => write!(f, "no such backup of agent {id}"),
        }
    }
}

impl std::error::Error for Refusal {}

/// Whether a backup may replace `agent`'s workspace: no VM, and a workspace that is gone (purged,
/// or a new host) or a subvolume, which is kept as a checkpoint first.
pub fn check_restore(agent: &AgentId, vm_running: bool, volume: Volume) -> Result<(), Refusal> {
    if vm_running {
        return Err(Refusal::Running(agent.clone()));
    }
    if volume == Volume::Directory {
        return Err(Refusal::Plain(agent.clone()));
    }
    Ok(())
}

/// The backup to restore: `wanted` if it is among the restorable ones, else the newest.
pub fn pick<'a>(
    agent: &AgentId,
    remote: &'a [BackupId],
    wanted: Option<&BackupId>,
) -> Result<&'a BackupId, Refusal> {
    let found = match wanted {
        Some(w) => remote.iter().find(|b| *b == w),
        None => remote.iter().max(),
    };
    found.ok_or_else(|| Refusal::NoBackup(agent.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(s: &str) -> BackupId {
        BackupId::parse(s).unwrap_or_else(|e| panic!("{e}"))
    }

    fn a1() -> AgentId {
        AgentId::parse("a1").unwrap_or_else(|e| panic!("{e}"))
    }

    /// Day `d`'s backup id.
    fn day(d: u32) -> BackupId {
        b(&format!("202610{d:02}-030000"))
    }

    fn full(d: u32) -> Stream {
        Stream {
            id: day(d),
            parent: None,
        }
    }

    fn after(d: u32, p: u32) -> Stream {
        Stream {
            id: day(d),
            parent: Some(day(p)),
        }
    }

    fn ids(streams: &[&Stream]) -> Vec<BackupId> {
        streams.iter().map(|s| s.id.clone()).collect()
    }

    #[test]
    fn file_names_round_trip() {
        assert_eq!(full(1).file_name(), "20261001-030000.full.btrfs");
        assert_eq!(
            after(2, 1).file_name(),
            "20261002-030000.after-20261001-030000.btrfs"
        );
        for s in [full(1), after(2, 1)] {
            assert_eq!(Stream::parse(&s.file_name()), Some(s));
        }
        for other in [
            "20261001-030000.full.btrfs.part",
            "20261001-030000.btrfs",
            "notes.txt",
            "../x.full.btrfs",
            "20261001-030000.after-20261002-030000.btrfs",
        ] {
            assert_eq!(Stream::parse(other), None, "{other}");
        }
    }

    #[test]
    fn chains_run_from_the_full_one() {
        let all = [full(1), after(2, 1), after(3, 2), full(4)];
        assert_eq!(
            chain(&all, &day(3)).map(|c| ids(&c)),
            Some(vec![day(1), day(2), day(3)])
        );
        assert_eq!(chain(&all, &day(4)).map(|c| ids(&c)), Some(vec![day(4)]));
        assert!(chain(&all, &day(9)).is_none());
        let broken = [full(1), after(3, 2)];
        assert!(chain(&broken, &day(3)).is_none());
        assert_eq!(restorable(&broken), vec![day(1)]);
        assert_eq!(restorable(&all), vec![day(1), day(2), day(3), day(4)]);
    }

    #[test]
    fn the_next_send_is_against_the_newest_whole_chain_until_it_is_long_enough() {
        let all = [full(1), after(2, 1), after(3, 2)];
        assert_eq!(parent(&[day(3)], &all, 7), Some(&day(3)));
        assert_eq!(
            parent(&[day(3)], &all, 3),
            None,
            "three in the chain: full now"
        );
        assert_eq!(parent(&[day(3)], &all, 4), Some(&day(3)));
        assert_eq!(parent(&[day(5)], &all, 7), None, "destination lost it");
        assert_eq!(parent(&[], &all, 7), None);
        assert_eq!(parent(&[day(1)], &[full(1)], 1), None, "every send full");
    }

    #[test]
    fn prune_keeps_the_chains_of_the_newest() {
        // Two chains: 1..3 and 4..5.
        let all = [full(1), after(2, 1), after(3, 2), full(4), after(5, 4)];
        assert_eq!(ids(&prune(&all, 2)), vec![day(1), day(2), day(3)]);
        assert_eq!(ids(&prune(&all, 1)), vec![day(1), day(2), day(3)]);
        assert!(prune(&all, 3).is_empty(), "3 needs 1 and 2");
        assert_eq!(
            ids(&prune(&all, 0)),
            ids(&prune(&all, 1)),
            "keep 0 still keeps one"
        );
        let broken = [full(1), after(3, 2), full(4)];
        assert_eq!(
            ids(&prune(&broken, 2)),
            vec![day(3)],
            "a broken stream goes"
        );
        assert!(prune(&[], 3).is_empty());
    }

    #[test]
    fn ids_only_move_forward() {
        // 2026-10-28 13:05:12 UTC.
        let t = 1_793_192_712;
        assert_eq!(next_id(t, &[]).as_str(), "20261028-130512");
        let later = [b("20261028-130513"), b("20261028-130512")];
        assert_eq!(
            next_id(t, &later).as_str(),
            "20261028-130514",
            "retention freed one"
        );
        assert_eq!(next_id(t + 60, &later).as_str(), "20261028-130612");
        assert_eq!(next_id(t, &[b("odd")]).as_str(), "20261028-130512");
    }

    #[test]
    fn restore_needs_no_vm_and_no_plain_directory() {
        assert!(check_restore(&a1(), false, Volume::Missing).is_ok());
        assert!(check_restore(&a1(), false, Volume::Subvolume).is_ok());
        assert_eq!(
            check_restore(&a1(), true, Volume::Missing),
            Err(Refusal::Running(a1()))
        );
        let plain = check_restore(&a1(), false, Volume::Directory);
        let msg = plain.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(msg.contains("ac-ctl workspace migrate a1"), "{msg}");
    }

    #[test]
    fn pick_the_one_asked_for_or_the_newest() {
        let all = [day(1), day(2)];
        assert_eq!(pick(&a1(), &all, None), Ok(&all[1]));
        assert_eq!(pick(&a1(), &all, Some(&all[0])), Ok(&all[0]));
        assert_eq!(
            pick(&a1(), &all, Some(&day(9))),
            Err(Refusal::NoBackup(a1()))
        );
        assert_eq!(pick(&a1(), &[], None), Err(Refusal::NoBackup(a1())));
    }
}
