//! Workspace backups over btrfs send and receive (ADR-021). A backup is a read-only snapshot of
//! the workspace, sent to `<dest>/<agent>/<backup>` on another btrfs filesystem. The last one
//! sent stays on this host under `backup_root/<agent>/` as the parent of the next send, so only
//! what changed crosses. Each received backup is a whole subvolume: retention deletes any of
//! them without breaking the others.
//!
//! `btrfs receive` marks a subvolume read-only only once the stream is complete, so a backup cut
//! short (a crash, a full disk) is a writable subvolume: never listed, and deleted by the next
//! backup of that agent. Received subvolumes cannot be renamed (they are read-only), so they are
//! received in place rather than staged.
//!
//! Nothing here opens the registry: backups run beside `ac-gateway`.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};

use ac_domain::backup::{self, BackupId};
use ac_domain::{AgentId, CheckpointId, Label, Volume};
use ac_ports::PortError;
use platform::process::{Command, Spawner, Stdio};

use crate::snap::{io_fail, now_unix, private_dir, sibling};
use crate::{failure, read_all, remove_dir_if_present, CtrRuntime};

/// What one backup did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sent {
    /// The new backup.
    pub id: BackupId,
    /// The backup it was sent against; `None`: sent in full.
    pub parent: Option<BackupId>,
    /// Older backups retention deleted from the destination.
    pub pruned: Vec<BackupId>,
}

/// Subvolume ids in `dir` that parse as backup ids, oldest first; nothing there is fine.
fn ids_in(op: &'static str, dir: &Path) -> Result<Vec<BackupId>, PortError> {
    let entries = match std::fs::read_dir(dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_fail(op, "list", dir, e)),
        Ok(entries) => entries,
    };
    let mut ids = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| io_fail(op, "list", dir, e))?;
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        let name = entry.file_name();
        if let (true, Some(b)) = (is_dir, name.to_str().and_then(|n| BackupId::parse(n).ok())) {
            ids.push(b);
        }
    }
    ids.sort();
    Ok(ids)
}

impl<S: Spawner> CtrRuntime<S> {
    fn backup_parents(&self, id: &AgentId) -> PathBuf {
        self.cfg.backup_root.join(id.as_str())
    }

    /// Whether the subvolume at `path` is read-only: `btrfs property get -ts <path> ro`.
    fn read_only(&self, op: &'static str, path: &Path) -> Result<bool, PortError> {
        let args = [
            OsString::from("property"),
            "get".into(),
            "-ts".into(),
            path.as_os_str().to_owned(),
            "ro".into(),
        ];
        let out = self.run(op, OsStr::new(&self.cfg.btrfs_bin), &args, None)?;
        Ok(String::from_utf8_lossy(&out).trim() == "ro=true")
    }

    /// `btrfs send <send...> | btrfs receive <receive...>`, the pipe made by the kernel. Not under
    /// `timeout`: a first send of a large workspace takes as long as it takes.
    fn send_receive(
        &self,
        op: &'static str,
        send: &[&OsStr],
        receive: &[&OsStr],
    ) -> Result<(), PortError> {
        let fail = |cause: String| PortError { op, cause };
        let bin = OsStr::new(&self.cfg.btrfs_bin);
        let command = |verb: &str, args: &[&OsStr]| {
            let argv = std::iter::once(OsStr::new(verb)).chain(args.iter().copied());
            let argv: Vec<OsString> = argv.map(OsStr::to_os_string).collect();
            if self.cfg.trace {
                let line: Vec<String> = argv
                    .iter()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect();
                eprintln!("ac-ctr: {op}: {} {}", bin.to_string_lossy(), line.join(" "));
            }
            let mut cmd = Command::new(bin, "/").args(argv);
            cmd.stdin = Stdio::Null;
            cmd.stdout = Stdio::Pipe;
            cmd.stderr = Stdio::Pipe;
            cmd
        };
        let spawn = |cmd: &Command| {
            self.spawner
                .spawn(cmd)
                .map_err(|e| fail(format!("spawn btrfs {:?}: {e:?}", cmd.argv.first())))
        };
        let mut sender = spawn(&command("send", send))?;
        let stream = sender
            .take_stdout()
            .ok_or_else(|| fail("btrfs send has no stdout".into()))?;
        let mut cmd = command("receive", receive);
        cmd.stdin = Stdio::File(stream);
        cmd.stdout = Stdio::Null;
        let receiver = spawn(&cmd);
        // Close this process's copy of the pipe, so a receive that dies stops the send (EPIPE)
        // instead of leaving it blocked on a full pipe.
        drop(cmd);
        let mut receiver = match receiver {
            Ok(r) => r,
            Err(e) => {
                // Its pipe has no reader now, so it ends with EPIPE; the spawn error is the news.
                let _ = sender.wait();
                return Err(e);
            }
        };
        // Both print a line or two on stderr, far below a pipe's buffer, so draining them one
        // after the other cannot block either.
        let drain = |pipe: Option<Box<dyn platform::fs::File>>| match pipe {
            Some(mut p) => read_all(p.as_mut()).map_err(|e| fail(format!("read: {e:?}"))),
            None => Ok(Vec::new()),
        };
        let received_err = drain(receiver.take_stderr())?;
        let sent_err = drain(sender.take_stderr())?;
        let received = receiver.wait().map_err(|e| fail(format!("wait: {e:?}")))?;
        let sent = sender.wait().map_err(|e| fail(format!("wait: {e:?}")))?;
        // Either side failing makes the other fail too (an empty stream, or a broken pipe), and
        // which went first is not known: quote both.
        let quote = |status, err: &[u8]| failure(op, status, &[], err).cause;
        match (sent.success(), received.success()) {
            (true, true) => Ok(()),
            (false, true) => Err(fail(format!("send: {}", quote(sent, &sent_err)))),
            (true, false) => Err(fail(format!("receive: {}", quote(received, &received_err)))),
            (false, false) => Err(fail(format!(
                "send: {}; receive: {}",
                quote(sent, &sent_err),
                quote(received, &received_err)
            ))),
        }
    }

    /// Delete every backup subvolume in `dir` that `keep` refuses.
    fn delete_backups(
        &self,
        op: &'static str,
        dir: &Path,
        keep: impl Fn(&BackupId) -> bool,
    ) -> Result<(), PortError> {
        for b in ids_in(op, dir)? {
            if !keep(&b) {
                self.remove_volume(op, &dir.join(b.as_str()))?;
            }
        }
        Ok(())
    }

    /// Delete the backups at `dir` that a crash or failure left half received: subvolumes that
    /// are not read-only.
    fn delete_partial(&self, op: &'static str, dir: &Path) -> Result<(), PortError> {
        for b in ids_in(op, dir)? {
            let path = dir.join(b.as_str());
            if self.volume_at(op, &path)? == Volume::Subvolume && !self.read_only(op, &path)? {
                self.remove_volume(op, &path)?;
            }
        }
        Ok(())
    }

    /// Refuse a destination that is not on btrfs: `btrfs receive` needs one.
    fn check_destination(&self, op: &'static str, dest: &Path) -> Result<(), PortError> {
        let btrfs = (self.disk.btrfs)(dest).map_err(|e| io_fail(op, "statfs", dest, e))?;
        if !btrfs {
            return Err(PortError {
                op,
                cause: format!("{} is not on btrfs; backups need one", dest.display()),
            });
        }
        Ok(())
    }

    /// The complete backups of `id` at `dest`, oldest first.
    pub fn backups(&self, dest: &Path, id: &AgentId) -> Result<Vec<BackupId>, PortError> {
        let op = "backups";
        let dir = dest.join(id.as_str());
        let mut done = Vec::new();
        for b in ids_in(op, &dir)? {
            let path = dir.join(b.as_str());
            if self.volume_at(op, &path)? == Volume::Subvolume && self.read_only(op, &path)? {
                done.push(b);
            }
        }
        Ok(done)
    }

    /// Back up `id`'s workspace (a subvolume) to `dest`, against the last backup when `dest`
    /// still holds it, then keep the newest `keep` there. A failure leaves `dest` and this host
    /// as they were.
    pub fn backup(&self, dest: &Path, id: &AgentId, keep: usize) -> Result<Sent, PortError> {
        let op = "backup";
        self.check_destination(op, dest)?;
        let remote_dir = private_dir(op, dest, id)?;
        let local_dir = private_dir(op, &self.cfg.backup_root, id)?;
        self.delete_partial(op, &remote_dir)?;
        let remote = self.backups(dest, id)?;
        let local = ids_in(op, &local_dir)?;
        let parent = backup::parent(&local, &remote).cloned();
        let taken: Vec<BackupId> = local.iter().chain(&remote).cloned().collect();
        let new = backup::next_id(now_unix(), &taken);
        let snap = local_dir.join(new.as_str());
        self.snapshot(op, &self.workspace(id), &snap, true)?;
        let parent_path = parent.as_ref().map(|p| local_dir.join(p.as_str()));
        let mut send: Vec<&OsStr> = Vec::new();
        if let Some(p) = &parent_path {
            send.extend([OsStr::new("-p"), p.as_os_str()]);
        }
        send.push(snap.as_os_str());
        let receive = [remote_dir.as_os_str()];
        if let Err(e) = self.send_receive(op, &send, &receive) {
            let cleanup = self
                .remove_volume(op, &remote_dir.join(new.as_str()))
                .and_then(|()| self.remove_volume(op, &snap));
            return Err(crate::with_cleanup(e, cleanup));
        }
        self.delete_backups(op, &local_dir, |b| *b == new)?;
        let mut remote = remote;
        remote.push(new.clone());
        let pruned = backup::prune(&remote, keep).to_vec();
        for b in &pruned {
            self.remove_volume(op, &remote_dir.join(b.as_str()))?;
        }
        Ok(Sent {
            id: new,
            parent,
            pruned,
        })
    }

    /// Put backup `b` of `id` from `dest` in place of its workspace (its VM stopped; checked by
    /// the caller). A workspace that is there is kept as a checkpoint first, whose id is
    /// returned; one that is gone (purged, or a new host) is simply created.
    pub fn restore_backup(
        &self,
        dest: &Path,
        id: &AgentId,
        b: &BackupId,
    ) -> Result<Option<CheckpointId>, PortError> {
        let op = "restore backup";
        let ws = self.workspace(id);
        let incoming = sibling(&ws, "receive");
        self.recover_incoming(op, &incoming)?;
        self.recover(op, id, "restore")?;
        std::fs::create_dir(&incoming).map_err(|e| io_fail(op, "mkdir", &incoming, e))?;
        let src = dest.join(id.as_str()).join(b.as_str());
        let received = self
            .send_receive(op, &[src.as_os_str()], &[incoming.as_os_str()])
            .and_then(|()| {
                let new = sibling(&ws, "restore");
                self.snapshot(op, &incoming.join(b.as_str()), &new, false)
            });
        let cleanup = self.recover_incoming(op, &incoming);
        if let Err(e) = received {
            return Err(crate::with_cleanup(e, cleanup));
        }
        cleanup?;
        let new = sibling(&ws, "restore");
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
        self.delete_backups(op, dir, |_| false)?;
        remove_dir_if_present(op, dir)
    }

    /// Delete the backup parents kept on this host for `id` (its workspace was purged). Its
    /// backups at the destination stay.
    pub(crate) fn purge_backup_parents(&self, id: &AgentId) -> Result<(), PortError> {
        let (op, dir) = ("purge backup parents", self.backup_parents(id));
        self.delete_backups(op, &dir, |_| false)?;
        remove_dir_if_present(op, &dir)
    }
}

#[cfg(test)]
mod tests {
    //! Against the stand-in `btrfs` of the snapshot tests (a send is a tar stream through a real
    //! pipe). `scripts/btrfs-checkpoint-test.sh` runs the same steps against real btrfs as root.

    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::snap::tests::{agent, ok, Host};
    use ac_ports::Snapshots;

    fn dest(h: &Host) -> PathBuf {
        h.root.join("dest")
    }

    fn at_dest(h: &Host, id: &AgentId, b: &BackupId) -> String {
        let path = dest(h).join(id.as_str()).join(b.as_str()).join("file");
        std::fs::read_to_string(path).unwrap_or_default()
    }

    fn local(h: &Host, id: &AgentId) -> Vec<BackupId> {
        ok(ids_in("test", &h.root.join("backups").join(id.as_str())))
    }

    #[test]
    fn the_first_backup_is_full_and_the_next_against_it() {
        let h = Host::new();
        let a1 = agent("a1");
        h.workspace(&a1, "one");
        let first = ok(h.rt.backup(&dest(&h), &a1, 7));
        assert_eq!(first.parent, None);
        h.write(&a1, "two");
        let second = ok(h.rt.backup(&dest(&h), &a1, 7));
        assert_eq!(second.parent.as_ref(), Some(&first.id));
        assert!(first.id < second.id);
        assert_eq!(
            ok(h.rt.backups(&dest(&h), &a1)),
            [first.id.clone(), second.id.clone()]
        );
        assert_eq!(at_dest(&h, &a1, &first.id), "one");
        assert_eq!(at_dest(&h, &a1, &second.id), "two");
        assert_eq!(
            local(&h, &a1),
            std::slice::from_ref(&second.id),
            "only the last stays here"
        );
        let sends: Vec<String> = h
            .log()
            .into_iter()
            .filter(|l| l.starts_with("send"))
            .collect();
        assert!(sends[1].starts_with("send -p "), "{sends:?}");
        let mode = std::fs::metadata(dest(&h).join("a1")).map(|m| m.permissions().mode() & 0o777);
        assert_eq!(mode.ok(), Some(0o700));
    }

    #[test]
    fn retention_keeps_the_newest() {
        let h = Host::new();
        let a1 = agent("a1");
        h.workspace(&a1, "x");
        let sent: Vec<Sent> = (0..4).map(|_| ok(h.rt.backup(&dest(&h), &a1, 2))).collect();
        assert_eq!(sent[2].pruned, [sent[0].id.clone()]);
        assert_eq!(sent[3].pruned, [sent[1].id.clone()]);
        let ids: Vec<BackupId> = sent[2..].iter().map(|s| s.id.clone()).collect();
        assert_eq!(ok(h.rt.backups(&dest(&h), &a1)), ids);
    }

    #[test]
    fn a_failed_receive_leaves_both_sides_as_they_were() {
        let h = Host::new();
        let a1 = agent("a1");
        h.workspace(&a1, "one");
        let first = ok(h.rt.backup(&dest(&h), &a1, 7));
        let flag = h.root.join("log.fail-receive");
        let _ = std::fs::write(&flag, "");
        let r = h.rt.backup(&dest(&h), &a1, 7);
        assert!(r.is_err_and(|e| e.cause.contains("No space left")));
        assert_eq!(
            ok(ids_in("test", &dest(&h).join("a1"))),
            std::slice::from_ref(&first.id)
        );
        assert_eq!(
            local(&h, &a1),
            std::slice::from_ref(&first.id),
            "the parent is kept"
        );
        let _ = std::fs::remove_file(&flag);
        let next = ok(h.rt.backup(&dest(&h), &a1, 7));
        assert_eq!(next.parent, Some(first.id));
    }

    #[test]
    fn a_backup_cut_short_is_never_listed_and_is_deleted_next_time() {
        let h = Host::new();
        let a1 = agent("a1");
        h.workspace(&a1, "x");
        let first = ok(h.rt.backup(&dest(&h), &a1, 7));
        // A crash mid-receive: a writable subvolume where the next backup would go.
        let partial = dest(&h).join("a1").join("20991231-235959");
        ok(h.rt.make_volume("test", &partial));
        assert_eq!(
            ok(h.rt.backups(&dest(&h), &a1)),
            std::slice::from_ref(&first.id)
        );
        ok(h.rt.backup(&dest(&h), &a1, 7));
        assert!(!partial.exists());
    }

    #[test]
    fn restore_keeps_the_workspace_as_a_checkpoint_or_makes_a_missing_one() {
        let h = Host::new();
        let a1 = agent("a1");
        h.workspace(&a1, "good");
        let good = ok(h.rt.backup(&dest(&h), &a1, 7)).id;
        h.write(&a1, "broken");
        let kept = ok(h.rt.restore_backup(&dest(&h), &a1, &good));
        assert_eq!(h.read(&a1), "good");
        assert_eq!(ok(h.rt.volume(&a1)), Volume::Subvolume);
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
    fn purge_drops_the_parent_here_and_keeps_the_backups_there() {
        let h = Host::new();
        let a1 = agent("a1");
        h.workspace(&a1, "x");
        let sent = ok(h.rt.backup(&dest(&h), &a1, 7));
        ok(h.rt.purge(&a1));
        assert!(!h.root.join("backups/a1").exists());
        assert_eq!(ok(h.rt.backups(&dest(&h), &a1)), [sent.id]);
    }

    #[test]
    fn a_destination_off_btrfs_is_refused() {
        let mut h = Host::new();
        let a1 = agent("a1");
        h.workspace(&a1, "x");
        h.rt.disk.btrfs = |_| Ok(false);
        let r = h.rt.backup(&dest(&h), &a1, 7);
        assert!(r.is_err_and(|e| e.cause.contains("is not on btrfs")));
        assert!(local(&h, &a1).is_empty());
    }
}
