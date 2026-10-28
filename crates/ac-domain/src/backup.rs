//! Workspace backups (ADR-019) as pure rules: which earlier backup a new one is sent against,
//! which backups retention removes, and when a backup may be restored over a workspace.

use std::fmt;

use crate::checkpoint::{stamp_secs, CheckpointId, Volume};
use crate::AgentId;

/// A backup's id: the same UTC stamp as a checkpoint's ([`CheckpointId`]), so ids sort by age.
pub type BackupId = CheckpointId;

/// Backups kept per agent on the destination when none is asked for.
pub const DEFAULT_KEEP: usize = 7;

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

/// The parent for an incremental send: the newest backup kept on this host (`local`) that the
/// destination also holds (`remote`). `None`: send in full.
pub fn parent<'a>(local: &'a [BackupId], remote: &[BackupId]) -> Option<&'a BackupId> {
    local.iter().filter(|b| remote.contains(b)).max()
}

/// The backups retention removes from `remote` (sorted oldest first) to keep the newest `keep`.
/// At least one is always kept, so the parent of the next send stays.
pub fn prune(remote: &[BackupId], keep: usize) -> &[BackupId] {
    let excess = remote.len().saturating_sub(keep.max(1));
    &remote[..excess]
}

/// Why restoring a backup is refused. Nothing was changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The agent's VM is running: its bind mount would keep the old workspace.
    Running(AgentId),
    /// The agent's workspace is a plain directory, which cannot be kept as a checkpoint.
    Plain(AgentId),
    /// The destination holds no backup of the agent (or not the one asked for).
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

/// The backup to restore: `wanted` if `remote` holds it, else the newest.
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

    #[test]
    fn parent_is_the_newest_backup_both_sides_hold() {
        let (one, two, three) = (
            b("20261028-010000"),
            b("20261029-010000"),
            b("20261030-010000"),
        );
        let remote = [one.clone(), two.clone()];
        assert_eq!(parent(&[one.clone(), two.clone()], &remote), Some(&two));
        assert_eq!(parent(&[one.clone(), three.clone()], &remote), Some(&one));
        assert_eq!(
            parent(&[three], &remote),
            None,
            "destination lost it: full send"
        );
        assert_eq!(parent(&[], &remote), None);
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
    fn prune_keeps_the_newest_and_never_the_last() {
        let all: Vec<BackupId> = (1..=5).map(|d| b(&format!("2026100{d}-000000"))).collect();
        assert_eq!(prune(&all, 3), &all[..2]);
        assert!(prune(&all, 5).is_empty() && prune(&all, 9).is_empty());
        assert_eq!(prune(&all, 0), &all[..4], "keep 0 still keeps one");
        assert!(prune(&[], 3).is_empty());
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
        let all = [b("20261028-010000"), b("20261029-010000")];
        assert_eq!(pick(&a1(), &all, None), Ok(&all[1]));
        assert_eq!(pick(&a1(), &all, Some(&all[0])), Ok(&all[0]));
        let missing = b("20261027-010000");
        assert_eq!(
            pick(&a1(), &all, Some(&missing)),
            Err(Refusal::NoBackup(a1()))
        );
        assert_eq!(pick(&a1(), &[], None), Err(Refusal::NoBackup(a1())));
    }
}
