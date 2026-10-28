//! Workspace backups as `btrfs send` streams in files (ADR-021). A backup is a read-only snapshot
//! of the workspace, sent to `<dest>/<agent>/<stream>` in any directory: a local disk, or a
//! mount of remote or object storage. After the first, a send is incremental against the last
//! one, which stays on this host under `backup_root/<agent>/` as its parent, until a chain is
//! `full_every` long and the next is full again. Restore replays a chain into a staging
//! directory beside the workspace, which must be on btrfs; the destination need not be.
//!
//! A stream is written as `<name>.part`, flushed to disk and only then renamed, so a stream cut
//! short (a crash, a full disk) is never listed, and the next backup of that agent deletes it.
//!
//! Nothing here opens the registry: backups run beside `ac-gateway`.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};

use ac_domain::backup::{self, BackupId, Stream};
use ac_domain::{AgentId, CheckpointId, Label, Volume};
use ac_ports::PortError;
use platform::process::Spawner;

use crate::snap::{io_fail, now_unix, private_dir, sibling};
use crate::{failure, remove_dir_if_present, CtrRuntime};

/// Ending of a stream still being written.
const PART: &str = "part";

/// What one backup did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sent {
    /// The new backup.
    pub id: BackupId,
    /// The backup it was sent against; `None`: sent in full.
    pub parent: Option<BackupId>,
    /// Its size in bytes.
    pub bytes: u64,
    /// Older backups retention deleted from the destination.
    pub pruned: Vec<BackupId>,
}

/// Subvolume ids in `dir` that parse as backup ids, oldest first; nothing there is fine.
fn ids_in(op: &'static str, dir: &Path) -> Result<Vec<BackupId>, PortError> {
    let mut ids: Vec<BackupId> = names_in(op, dir, true)?
        .iter()
        .filter_map(|n| BackupId::parse(n).ok())
        .collect();
    ids.sort();
    Ok(ids)
}

/// The names of the directories (`dirs`) or files in `dir`; nothing there is fine.
fn names_in(op: &'static str, dir: &Path, dirs: bool) -> Result<Vec<String>, PortError> {
    let entries = match std::fs::read_dir(dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_fail(op, "list", dir, e)),
        Ok(entries) => entries,
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| io_fail(op, "list", dir, e))?;
        let kind_ok = entry
            .file_type()
            .is_ok_and(|t| if dirs { t.is_dir() } else { t.is_file() });
        if let (true, Some(name)) = (kind_ok, entry.file_name().to_str()) {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

/// The stream files in `dir`, oldest first.
fn streams_in(op: &'static str, dir: &Path) -> Result<Vec<Stream>, PortError> {
    let mut streams: Vec<Stream> = names_in(op, dir, false)?
        .iter()
        .filter_map(|n| Stream::parse(n))
        .collect();
    streams.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(streams)
}

/// Flush `path` (a file, or a directory after a rename in it) to disk.
fn sync(op: &'static str, path: &Path) -> Result<(), PortError> {
    std::fs::File::open(path)
        .and_then(|f| f.sync_all())
        .map_err(|e| io_fail(op, "fsync", path, e))
}

fn remove_file(op: &'static str, path: &Path) -> Result<(), PortError> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(io_fail(op, "rm", path, e)),
        _ => Ok(()),
    }
}

impl<S: Spawner> CtrRuntime<S> {
    fn backup_parents(&self, id: &AgentId) -> PathBuf {
        self.cfg.backup_root.join(id.as_str())
    }

    /// `btrfs <args>` with no time limit: a first send of a large workspace takes as long as it
    /// takes.
    fn btrfs_untimed(&self, op: &'static str, args: &[&OsStr]) -> Result<(), PortError> {
        let args: Vec<OsString> = args.iter().map(|a| a.to_os_string()).collect();
        match self.spawn_wait(op, OsStr::new(&self.cfg.btrfs_bin), &args, None)? {
            (status, _, _) if status.success() => Ok(()),
            (status, stdout, stderr) => Err(failure(op, status, &stdout, &stderr)),
        }
    }

    /// Delete every backup subvolume in `dir`.
    fn delete_subvolumes(&self, op: &'static str, dir: &Path) -> Result<(), PortError> {
        for b in ids_in(op, dir)? {
            self.remove_volume(op, &dir.join(b.as_str()))?;
        }
        Ok(())
    }

    /// The stream files of `id` at `dest`, oldest first.
    pub fn streams(&self, dest: &Path, id: &AgentId) -> Result<Vec<Stream>, PortError> {
        streams_in("backups", &dest.join(id.as_str()))
    }

    /// The backups of `id` at `dest` that can be restored, oldest first.
    pub fn backups(&self, dest: &Path, id: &AgentId) -> Result<Vec<BackupId>, PortError> {
        Ok(backup::restorable(&self.streams(dest, id)?))
    }

    /// Back up `id`'s workspace (a subvolume) to `dest`, against the last backup while its chain
    /// is shorter than `full_every`, then keep the newest `keep` there. A failure leaves `dest`
    /// and this host as they were.
    pub fn backup(
        &self,
        dest: &Path,
        id: &AgentId,
        keep: usize,
        full_every: usize,
    ) -> Result<Sent, PortError> {
        let op = "backup";
        if !dest.is_dir() {
            return Err(PortError {
                op,
                cause: format!("{} is not a directory", dest.display()),
            });
        }
        let remote_dir = private_dir(op, dest, id)?;
        let local_dir = private_dir(op, &self.cfg.backup_root, id)?;
        for name in names_in(op, &remote_dir, false)? {
            if name.ends_with(&format!(".{PART}")) {
                remove_file(op, &remote_dir.join(name))?;
            }
        }
        let streams = streams_in(op, &remote_dir)?;
        let local = ids_in(op, &local_dir)?;
        let parent = backup::parent(&local, &streams, full_every).cloned();
        let taken: Vec<BackupId> = local
            .iter()
            .cloned()
            .chain(streams.iter().map(|s| s.id.clone()))
            .collect();
        let stream = Stream {
            id: backup::next_id(now_unix(), &taken),
            parent,
        };
        let snap = local_dir.join(stream.id.as_str());
        self.snapshot(op, &self.workspace(id), &snap, true)?;
        let file = remote_dir.join(stream.file_name());
        let bytes = match self.send_to(op, &local_dir, &stream, &snap, &file) {
            Ok(bytes) => bytes,
            Err(e) => {
                let cleanup = remove_file(op, &sibling(&file, PART))
                    .and_then(|()| self.remove_volume(op, &snap));
                return Err(crate::with_cleanup(e, cleanup));
            }
        };
        for b in local.iter().filter(|b| **b != stream.id) {
            self.remove_volume(op, &local_dir.join(b.as_str()))?;
        }
        let mut streams = streams;
        streams.push(stream.clone());
        let pruned: Vec<Stream> = backup::prune(&streams, keep).into_iter().cloned().collect();
        for s in &pruned {
            remove_file(op, &remote_dir.join(s.file_name()))?;
        }
        Ok(Sent {
            id: stream.id,
            parent: stream.parent,
            bytes,
            pruned: pruned.into_iter().map(|s| s.id).collect(),
        })
    }

    /// `btrfs send [-p <parent>] -f <file>.part <snap>`, flushed, then renamed to `file`; its
    /// size.
    fn send_to(
        &self,
        op: &'static str,
        local_dir: &Path,
        stream: &Stream,
        snap: &Path,
        file: &Path,
    ) -> Result<u64, PortError> {
        let part = sibling(file, PART);
        let parent = stream.parent.as_ref().map(|p| local_dir.join(p.as_str()));
        let mut args = vec![OsStr::new("send"), OsStr::new("-q")];
        if let Some(p) = &parent {
            args.extend([OsStr::new("-p"), p.as_os_str()]);
        }
        args.extend([OsStr::new("-f"), part.as_os_str(), snap.as_os_str()]);
        self.btrfs_untimed(op, &args)?;
        sync(op, &part)?;
        std::fs::rename(&part, file).map_err(|e| io_fail(op, "rename", &part, e))?;
        if let Some(dir) = file.parent() {
            sync(op, dir)?;
        }
        std::fs::metadata(file)
            .map(|m| m.len())
            .map_err(|e| io_fail(op, "stat", file, e))
    }

    /// Put backup `b` of `id` from `dest` in place of its workspace (its VM stopped; checked by
    /// the caller), replaying its chain. A workspace that is there is kept as a checkpoint first,
    /// whose id is returned; one that is gone (purged, or a new host) is simply created.
    pub fn restore_backup(
        &self,
        dest: &Path,
        id: &AgentId,
        b: &BackupId,
    ) -> Result<Option<CheckpointId>, PortError> {
        let op = "restore backup";
        let streams = self.streams(dest, id)?;
        let chain = backup::chain(&streams, b).ok_or_else(|| PortError {
            op,
            cause: format!(
                "backup {b} of {id} has no whole chain at {}",
                dest.display()
            ),
        })?;
        let ws = self.workspace(id);
        let incoming = sibling(&ws, "receive");
        self.recover_incoming(op, &incoming)?;
        self.recover(op, id, "restore")?;
        std::fs::create_dir(&incoming).map_err(|e| io_fail(op, "mkdir", &incoming, e))?;
        let new = sibling(&ws, "restore");
        let received = chain
            .iter()
            .try_for_each(|s| {
                let file = dest.join(id.as_str()).join(s.file_name());
                let args = [OsStr::new("receive"), OsStr::new("-f"), file.as_os_str()];
                self.btrfs_untimed(op, &[&args[..], &[incoming.as_os_str()]].concat())
            })
            .and_then(|()| self.snapshot(op, &incoming.join(b.as_str()), &new, false));
        let cleanup = self.recover_incoming(op, &incoming);
        if let Err(e) = received {
            return Err(crate::with_cleanup(e, cleanup));
        }
        cleanup?;
        if self.volume_at(op, &ws)? == Volume::Missing {
            std::fs::rename(&new, &ws).map_err(|e| io_fail(op, "rename", &new, e))?;
            return Ok(None);
        }
        let label =
            Label::parse(&format!("before restore of backup {b}")).map_err(|e| PortError {
                op,
                cause: e.to_string(),
            })?;
        let kept = self.take(op, id, Some(&label))?;
        self.swap_in(op, id, &new)?;
        Ok(Some(kept))
    }

    /// Delete a restore's staging directory and whatever it received.
    fn recover_incoming(&self, op: &'static str, dir: &Path) -> Result<(), PortError> {
        self.delete_subvolumes(op, dir)?;
        remove_dir_if_present(op, dir)
    }

    /// Delete the backup parents kept on this host for `id` (its workspace was purged). Its
    /// backups at the destination stay.
    pub(crate) fn purge_backup_parents(&self, id: &AgentId) -> Result<(), PortError> {
        let (op, dir) = ("purge backup parents", self.backup_parents(id));
        self.delete_subvolumes(op, &dir)?;
        remove_dir_if_present(op, &dir)
    }
}

#[cfg(test)]
mod tests {
    //! Against the stand-in `btrfs` of the snapshot tests (a stream is a tar of the snapshot).
    //! `scripts/btrfs-backup-test.sh` runs the same steps against real btrfs as root.

    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::snap::tests::{agent, ok, Host};
    use ac_ports::Snapshots;

    fn dest(h: &Host) -> PathBuf {
        let d = h.root.join("dest");
        let _ = std::fs::create_dir_all(&d);
        d
    }

    fn files(h: &Host, id: &AgentId) -> Vec<String> {
        let mut names = ok(names_in("test", &dest(h).join(id.as_str()), false));
        names.sort();
        names
    }

    fn local(h: &Host, id: &AgentId) -> Vec<BackupId> {
        ok(ids_in("test", &h.root.join("backups").join(id.as_str())))
    }

    #[test]
    fn the_first_backup_is_full_and_the_next_against_it_until_the_chain_is_long_enough() {
        let h = Host::new();
        let a1 = agent("a1");
        h.workspace(&a1, "one");
        let first = ok(h.rt.backup(&dest(&h), &a1, 7, 3));
        assert_eq!(first.parent, None);
        assert!(first.bytes > 0);
        h.write(&a1, "two");
        let second = ok(h.rt.backup(&dest(&h), &a1, 7, 3));
        assert_eq!(second.parent.as_ref(), Some(&first.id));
        let third = ok(h.rt.backup(&dest(&h), &a1, 7, 3));
        assert_eq!(third.parent.as_ref(), Some(&second.id));
        let fourth = ok(h.rt.backup(&dest(&h), &a1, 7, 3));
        assert_eq!(fourth.parent, None, "a chain of 3: full again");
        assert_eq!(
            files(&h, &a1),
            [
                format!("{}.full.btrfs", first.id),
                format!("{}.after-{}.btrfs", second.id, first.id),
                format!("{}.after-{}.btrfs", third.id, second.id),
                format!("{}.full.btrfs", fourth.id),
            ]
        );
        assert_eq!(
            local(&h, &a1),
            std::slice::from_ref(&fourth.id),
            "only the last stays here"
        );
        let sends: Vec<String> = h
            .log()
            .into_iter()
            .filter(|l| l.starts_with("send"))
            .collect();
        assert!(sends[1].starts_with("send -q -p "), "{sends:?}");
        let mode = std::fs::metadata(dest(&h).join("a1")).map(|m| m.permissions().mode() & 0o777);
        assert_eq!(mode.ok(), Some(0o700));
    }

    #[test]
    fn retention_drops_whole_chains_only() {
        let h = Host::new();
        let a1 = agent("a1");
        h.workspace(&a1, "x");
        // Chains of 2: [0 1] [2 3] [4].
        let sent: Vec<Sent> = (0..5)
            .map(|_| ok(h.rt.backup(&dest(&h), &a1, 2, 2)))
            .collect();
        assert!(
            sent[2].pruned.is_empty(),
            "keeping 1 and 2 needs 0, 1's parent"
        );
        assert_eq!(sent[3].pruned, [sent[0].id.clone(), sent[1].id.clone()]);
        assert!(
            sent[4].pruned.is_empty(),
            "keeping 3 and 4 needs 2, 3's parent"
        );
        let ids: Vec<BackupId> = sent[2..].iter().map(|s| s.id.clone()).collect();
        assert_eq!(ok(h.rt.backups(&dest(&h), &a1)), ids);
    }

    #[test]
    fn a_failed_send_leaves_both_sides_as_they_were() {
        let h = Host::new();
        let a1 = agent("a1");
        h.workspace(&a1, "one");
        let first = ok(h.rt.backup(&dest(&h), &a1, 7, 7));
        let flag = h.root.join("log.fail-send");
        let _ = std::fs::write(&flag, "");
        let r = h.rt.backup(&dest(&h), &a1, 7, 7);
        assert!(r.is_err_and(|e| e.cause.contains("No space left")));
        assert_eq!(files(&h, &a1), [format!("{}.full.btrfs", first.id)]);
        assert_eq!(
            local(&h, &a1),
            std::slice::from_ref(&first.id),
            "the parent is kept"
        );
        let _ = std::fs::remove_file(&flag);
        let next = ok(h.rt.backup(&dest(&h), &a1, 7, 7));
        assert_eq!(next.parent, Some(first.id));
    }

    #[test]
    fn a_stream_cut_short_is_never_listed_and_is_deleted_next_time() {
        let h = Host::new();
        let a1 = agent("a1");
        h.workspace(&a1, "x");
        let first = ok(h.rt.backup(&dest(&h), &a1, 7, 7));
        let part = dest(&h).join("a1/20991231-235959.full.btrfs.part");
        let _ = std::fs::write(&part, "half");
        assert_eq!(ok(h.rt.backups(&dest(&h), &a1)), [first.id]);
        ok(h.rt.backup(&dest(&h), &a1, 7, 7));
        assert!(!part.exists());
    }

    #[test]
    fn restore_replays_the_chain_and_keeps_the_workspace_or_makes_a_missing_one() {
        let h = Host::new();
        let a1 = agent("a1");
        h.workspace(&a1, "one");
        ok(h.rt.backup(&dest(&h), &a1, 7, 7));
        h.write(&a1, "good");
        let good = ok(h.rt.backup(&dest(&h), &a1, 7, 7)).id;
        h.write(&a1, "broken");
        let kept = ok(h.rt.restore_backup(&dest(&h), &a1, &good));
        assert_eq!(h.read(&a1), "good");
        assert_eq!(ok(h.rt.volume(&a1)), Volume::Subvolume);
        let receives = h
            .log()
            .iter()
            .filter(|l| l.starts_with("receive -f"))
            .count();
        assert_eq!(receives, 2, "the full one, then the incremental");
        let cps = ok(h.rt.checkpoints(&a1));
        let label = cps
            .iter()
            .find(|c| Some(&c.id) == kept.as_ref())
            .and_then(|c| c.label.clone());
        assert_eq!(
            label.map(|l| l.as_str().to_owned()),
            Some(format!("before restore of backup {good}"))
        );
        // A new host, or a purged agent: nothing to keep.
        ok(h.rt.purge(&a1));
        ok(h.rt.remove_volume("destroy", &h.ws(&a1)));
        assert_eq!(ok(h.rt.restore_backup(&dest(&h), &a1, &good)), None);
        assert_eq!(h.read(&a1), "good");
        for leftover in ["receive", "restore", "old"] {
            assert!(!sibling(&h.ws(&a1), leftover).exists(), "{leftover}");
        }
    }

    #[test]
    fn a_broken_chain_is_not_restored() {
        let h = Host::new();
        let a1 = agent("a1");
        h.workspace(&a1, "x");
        let first = ok(h.rt.backup(&dest(&h), &a1, 7, 7));
        let second = ok(h.rt.backup(&dest(&h), &a1, 7, 7));
        let _ = std::fs::remove_file(dest(&h).join("a1").join(format!("{}.full.btrfs", first.id)));
        assert!(ok(h.rt.backups(&dest(&h), &a1)).is_empty());
        let r = h.rt.restore_backup(&dest(&h), &a1, &second.id);
        assert!(r.is_err_and(|e| e.cause.contains("no whole chain")));
        // The next backup is full: nothing at the destination to send against.
        assert_eq!(ok(h.rt.backup(&dest(&h), &a1, 7, 7)).parent, None);
    }

    #[test]
    fn purge_drops_the_parent_here_and_keeps_the_backups_there() {
        let h = Host::new();
        let a1 = agent("a1");
        h.workspace(&a1, "x");
        let sent = ok(h.rt.backup(&dest(&h), &a1, 7, 7));
        ok(h.rt.purge(&a1));
        assert!(!h.root.join("backups/a1").exists());
        assert_eq!(ok(h.rt.backups(&dest(&h), &a1)), [sent.id]);
    }

    #[test]
    fn a_missing_destination_is_refused() {
        let h = Host::new();
        let a1 = agent("a1");
        h.workspace(&a1, "x");
        let r = h.rt.backup(&h.root.join("nowhere"), &a1, 7, 7);
        assert!(r.is_err_and(|e| e.cause.contains("is not a directory")));
        assert!(local(&h, &a1).is_empty());
    }
}
