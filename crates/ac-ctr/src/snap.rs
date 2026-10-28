//! `Snapshots` over btrfs-progs (ADR-018): each workspace is a subvolume, a checkpoint a
//! read-only snapshot of it under `checkpoint_root/<agent>/<checkpoint>`, a restore or fork a
//! writable snapshot. Every step acts on whole subvolumes or renames them, so root never opens a
//! path inside a workspace. The one exception is `migrate`, which copies a plain-directory
//! workspace into a new subvolume once, with the VM stopped.
//!
//! A swap (restore, migrate) builds the new volume as `<id>.restore` or `<id>.migrate` beside the
//! workspace, renames the workspace to `<id>.old`, renames the new one into place and deletes
//! `<id>.old`. Agent ids have no `.`, so these names never collide with an agent. A swap cut short
//! by a crash is finished or undone by the next one ([`CtrRuntime::recover`]).

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use ac_domain::{changes, AgentId, Checkpoint, CheckpointId, JobId, Label, Volume};
use ac_ports::{PortError, Snapshots};
use platform::process::Spawner;

use crate::{os, remove_dir_if_present, CtrRuntime};

/// `f_type` of a btrfs filesystem in statfs(2).
const BTRFS_SUPER_MAGIC: i64 = 0x9123_683e;
/// The inode number of every btrfs subvolume's root directory.
const SUBVOLUME_INO: u64 = 256;
/// Where a checkpoint's label is kept: beside its snapshot, never inside it.
const LABEL_SUFFIX: &str = "label";
/// A job's snapshots beside the checkpoints, as `<job>.start` and `<job>.end`: names no checkpoint
/// can have, so they are neither listed nor counted under the cap.
const JOB_START: &str = "start";
const JOB_END: &str = "end";

/// How the adapter tells what is on disk. Tests swap in their own.
#[derive(Clone, Copy)]
pub(crate) struct Disk {
    /// Whether the filesystem holding a path is btrfs.
    pub(crate) btrfs: fn(&Path) -> io::Result<bool>,
    /// Whether a directory on btrfs is a subvolume's root.
    pub(crate) subvolume: fn(&Path) -> io::Result<bool>,
}

impl Disk {
    /// statfs(2) and the subvolume root's fixed inode number.
    pub(crate) const LINUX: Disk = Disk {
        btrfs: is_btrfs,
        subvolume: |p| Ok(std::fs::symlink_metadata(p)?.ino() == SUBVOLUME_INO),
    };
}

/// Whether the filesystem holding `path` is btrfs.
pub fn is_btrfs(path: &Path) -> io::Result<bool> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)?;
    let stats = rusty_libc::fs::statfs(&c).map_err(|e| io::Error::other(e.to_string()))?;
    Ok(stats.f_type == BTRFS_SUPER_MAGIC)
}

pub(crate) fn io_fail(op: &'static str, what: &str, path: &Path, e: io::Error) -> PortError {
    PortError {
        op,
        cause: format!("{what} {}: {e}", path.display()),
    }
}

/// `path` with `.suffix` appended to its last component.
pub(crate) fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(format!(".{suffix}"));
    PathBuf::from(s)
}

/// `root/<id>`, made 0700 (and `root` 0700) if missing: workspaces hold credentials.
pub(crate) fn private_dir(
    op: &'static str,
    root: &Path,
    id: &AgentId,
) -> Result<PathBuf, PortError> {
    let dir = root.join(id.as_str());
    for d in [root, &dir] {
        if !d.is_dir() {
            std::fs::create_dir_all(d).map_err(|e| io_fail(op, "mkdir", d, e))?;
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| io_fail(op, "chmod", d, e))?;
        }
    }
    Ok(dir)
}

pub(crate) fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl<S: Spawner> CtrRuntime<S> {
    fn checkpoints_dir(&self, id: &AgentId) -> PathBuf {
        self.cfg.checkpoint_root.join(id.as_str())
    }

    fn checkpoint_path(&self, id: &AgentId, cp: &CheckpointId) -> PathBuf {
        self.checkpoints_dir(id).join(cp.as_str())
    }

    /// `btrfs <args>`, failing on a non-zero exit.
    pub(crate) fn btrfs(&self, op: &'static str, args: &[&OsStr]) -> Result<(), PortError> {
        let args: Vec<OsString> = args.iter().map(|a| a.to_os_string()).collect();
        self.run(op, OsStr::new(&self.cfg.btrfs_bin), &args, None)
            .map(drop)
    }

    pub(crate) fn snapshot(
        &self,
        op: &'static str,
        from: &Path,
        to: &Path,
        read_only: bool,
    ) -> Result<(), PortError> {
        let mut args = vec![OsStr::new("subvolume"), OsStr::new("snapshot")];
        if read_only {
            args.push(OsStr::new("-r"));
        }
        args.extend([from.as_os_str(), to.as_os_str()]);
        self.btrfs(op, &args)
    }

    /// What is at `dir`: nothing, a plain directory, or a subvolume. Anything but a directory
    /// (a symlink, say) is an error, never followed.
    pub(crate) fn volume_at(&self, op: &'static str, dir: &Path) -> Result<Volume, PortError> {
        let meta = match std::fs::symlink_metadata(dir) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Volume::Missing),
            Err(e) => return Err(io_fail(op, "stat", dir, e)),
            Ok(m) => m,
        };
        if !meta.is_dir() {
            return Err(PortError {
                op,
                cause: format!("{} is not a directory", dir.display()),
            });
        }
        let probe =
            |f: fn(&Path) -> io::Result<bool>| f(dir).map_err(|e| io_fail(op, "stat", dir, e));
        if probe(self.disk.btrfs)? && probe(self.disk.subvolume)? {
            return Ok(Volume::Subvolume);
        }
        Ok(Volume::Directory)
    }

    /// A new workspace at `dir`: a subvolume where its parent is on btrfs, else a directory. One
    /// already there is kept as it is.
    pub(crate) fn make_volume(&self, op: &'static str, dir: &Path) -> Result<(), PortError> {
        if dir.exists() {
            return Ok(());
        }
        let parent = dir.parent().unwrap_or(Path::new("/"));
        std::fs::create_dir_all(parent).map_err(|e| io_fail(op, "mkdir", parent, e))?;
        let btrfs = (self.disk.btrfs)(parent).map_err(|e| io_fail(op, "statfs", parent, e))?;
        if btrfs {
            return self.btrfs(
                op,
                &[
                    OsStr::new("subvolume"),
                    OsStr::new("create"),
                    dir.as_os_str(),
                ],
            );
        }
        std::fs::create_dir(dir).map_err(|e| io_fail(op, "mkdir", dir, e))
    }

    /// Delete whatever volume is at `dir`; nothing there is fine.
    pub(crate) fn remove_volume(&self, op: &'static str, dir: &Path) -> Result<(), PortError> {
        match self.volume_at(op, dir)? {
            Volume::Missing => Ok(()),
            Volume::Directory => remove_dir_if_present(op, dir),
            Volume::Subvolume => self.btrfs(
                op,
                &[
                    OsStr::new("subvolume"),
                    OsStr::new("delete"),
                    dir.as_os_str(),
                ],
            ),
        }
    }

    /// Finish or undo a swap of `id`'s workspace that a crash cut short: drop a half-built
    /// `<id>.<building>`; if `<id>.old` is left, the swap got past its first rename, so delete
    /// it when the workspace is in place and put it back when not.
    pub(crate) fn recover(
        &self,
        op: &'static str,
        id: &AgentId,
        building: &str,
    ) -> Result<(), PortError> {
        let ws = self.workspace(id);
        let old = sibling(&ws, "old");
        self.remove_volume(op, &sibling(&ws, building))?;
        match (self.volume_at(op, &old)?, self.volume_at(op, &ws)?) {
            (Volume::Missing, _) => Ok(()),
            (_, Volume::Missing) => {
                std::fs::rename(&old, &ws).map_err(|e| io_fail(op, "rename", &old, e))
            }
            _ => self.remove_volume(op, &old),
        }
    }

    /// Put the volume built at `new` in place of `id`'s workspace, then delete the old one. A
    /// failed second rename puts the old one back.
    pub(crate) fn swap_in(
        &self,
        op: &'static str,
        id: &AgentId,
        new: &Path,
    ) -> Result<(), PortError> {
        let ws = self.workspace(id);
        let old = sibling(&ws, "old");
        std::fs::rename(&ws, &old).map_err(|e| io_fail(op, "rename", &ws, e))?;
        if let Err(e) = std::fs::rename(new, &ws) {
            let undo = std::fs::rename(&old, &ws).map_err(|u| io_fail(op, "rename back", &old, u));
            return Err(crate::with_cleanup(io_fail(op, "rename", new, e), undo));
        }
        self.remove_volume(op, &old)
    }

    /// Job snapshots in `dir` (`<job>.start`, `<job>.end`) that a crash left behind.
    fn job_snapshots(&self, op: &'static str, dir: &Path) -> Result<Vec<PathBuf>, PortError> {
        let entries = match std::fs::read_dir(dir) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io_fail(op, "list", dir, e)),
            Ok(entries) => entries,
        };
        let mut found = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| io_fail(op, "list", dir, e))?;
            let name = entry.file_name();
            let suffix = name
                .to_str()
                .and_then(|n| n.rsplit_once('.'))
                .map(|(_, s)| s);
            if matches!(suffix, Some(JOB_START | JOB_END)) {
                found.push(entry.path());
            }
        }
        Ok(found)
    }

    /// `checkpoint_root/<id>`, made 0700 (and the root 0700) if missing.
    fn checkpoints_dir_made(&self, op: &'static str, id: &AgentId) -> Result<PathBuf, PortError> {
        private_dir(op, &self.cfg.checkpoint_root, id)
    }

    fn read_label(&self, op: &'static str, snap: &Path) -> Result<Option<Label>, PortError> {
        let path = sibling(snap, LABEL_SUFFIX);
        match std::fs::read_to_string(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_fail(op, "read", &path, e)),
            Ok(text) => Label::parse(&text)
                .map(Some)
                .map_err(|e| io_fail(op, "read", &path, io::Error::other(e))),
        }
    }

    /// The label beside `snap`, written to a temporary name and renamed into place.
    fn write_label(&self, op: &'static str, snap: &Path, label: &Label) -> Result<(), PortError> {
        let path = sibling(snap, LABEL_SUFFIX);
        let tmp = sibling(&path, "tmp");
        std::fs::write(&tmp, label.as_str()).map_err(|e| io_fail(op, "write", &tmp, e))?;
        std::fs::rename(&tmp, &path).map_err(|e| io_fail(op, "rename", &tmp, e))
    }

    pub(crate) fn take(
        &self,
        op: &'static str,
        id: &AgentId,
        label: Option<&Label>,
    ) -> Result<CheckpointId, PortError> {
        let dir = self.checkpoints_dir_made(op, id)?;
        let taken: Vec<CheckpointId> = self.checkpoints(id)?.into_iter().map(|c| c.id).collect();
        let cp = CheckpointId::next(now_unix(), &taken);
        let snap = dir.join(cp.as_str());
        self.snapshot(op, &self.workspace(id), &snap, true)?;
        if let Some(label) = label {
            self.write_label(op, &snap, label)?;
        }
        Ok(cp)
    }
}

impl<S: Spawner> CtrRuntime<S> {
    /// Every agent's workspace and what it is, by agent id. Entries that are no agent id (a
    /// swap's `<id>.old`, say) are skipped.
    pub fn workspaces(&self) -> Result<Vec<(AgentId, Volume)>, PortError> {
        let (op, root) = ("workspaces", &self.cfg.workspace_root);
        std::fs::create_dir_all(root).map_err(|e| io_fail(op, "mkdir", root, e))?;
        let entries = std::fs::read_dir(root).map_err(|e| io_fail(op, "list", root, e))?;
        let mut all = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| io_fail(op, "list", root, e))?;
            let Some(id) = entry
                .file_name()
                .to_str()
                .and_then(|n| AgentId::parse(n).ok())
            else {
                continue;
            };
            all.push((id, self.volume_at(op, &entry.path())?));
        }
        all.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        Ok(all)
    }

    /// Agents whose workspace is still a plain directory, for `ac-ctl workspace migrate`; `None`
    /// when the workspace root is not on btrfs, so no workspace can be snapshotted.
    pub fn plain_workspaces(&self) -> Result<Option<Vec<AgentId>>, PortError> {
        let (op, root) = ("plain workspaces", &self.cfg.workspace_root);
        std::fs::create_dir_all(root).map_err(|e| io_fail(op, "mkdir", root, e))?;
        if !(self.disk.btrfs)(root).map_err(|e| io_fail(op, "statfs", root, e))? {
            return Ok(None);
        }
        let plain = self
            .workspaces()?
            .into_iter()
            .filter(|(_, v)| *v == Volume::Directory)
            .map(|(id, _)| id)
            .collect();
        Ok(Some(plain))
    }
}

impl<S: Spawner> CtrRuntime<S> {
    /// Snapshot `id`'s workspace read-only as job `job` starts (ADR-019). A workspace that is not
    /// a subvolume is skipped: its job gets no change summary.
    pub fn job_begin(&self, id: &AgentId, job: &JobId) -> Result<(), PortError> {
        let op = "job snapshot";
        let ws = self.workspace(id);
        if self.volume_at(op, &ws)? != Volume::Subvolume {
            return Ok(());
        }
        let dir = self.checkpoints_dir_made(op, id)?;
        self.snapshot(op, &ws, &sibling(&dir.join(job.as_str()), JOB_START), true)
    }

    /// The workspace paths job `job` changed ([`changes::paths`]): its start snapshot compared
    /// with one taken now, by `btrfs send --no-data`, which reads metadata only and never opens a
    /// path inside the workspace. Both snapshots are deleted, also on failure. `None` when the job
    /// has no start snapshot.
    pub fn job_changes(&self, id: &AgentId, job: &JobId) -> Result<Option<Vec<String>>, PortError> {
        let op = "job changes";
        let base = self.checkpoints_dir(id).join(job.as_str());
        let (start, end) = (sibling(&base, JOB_START), sibling(&base, JOB_END));
        if self.volume_at(op, &start)? == Volume::Missing {
            return Ok(None);
        }
        let stream = sibling(&base, "send");
        let diffed = self.diff(op, &self.workspace(id), &start, &end, &stream);
        let cleanup = [
            self.remove_volume(op, &end),
            self.remove_volume(op, &start),
            match std::fs::remove_file(&stream) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(io_fail(op, "rm", &stream, e)),
                _ => Ok(()),
            },
        ]
        .into_iter()
        .collect::<Result<(), _>>();
        match diffed {
            Ok(paths) => cleanup.map(|()| Some(paths)),
            Err(e) => Err(crate::with_cleanup(e, cleanup)),
        }
    }

    /// Snapshot `ws` to `end`, write the incremental send from its earlier snapshot `start` to
    /// `stream`, and list what changed.
    fn diff(
        &self,
        op: &'static str,
        ws: &Path,
        start: &Path,
        end: &Path,
        stream: &Path,
    ) -> Result<Vec<String>, PortError> {
        self.snapshot(op, ws, end, true)?;
        let flags = ["send", "-q", "--no-data", "-p"].map(OsStr::new);
        let mut args = flags.to_vec();
        args.extend([
            start.as_os_str(),
            OsStr::new("-f"),
            stream.as_os_str(),
            end.as_os_str(),
        ]);
        self.btrfs(op, &args)?;
        let mut dump = os(&["receive", "--dump", "-f"]);
        dump.push(stream.as_os_str().to_owned());
        let out = self.run(op, OsStr::new(&self.cfg.btrfs_bin), &dump, None)?;
        Ok(changes::paths(&String::from_utf8_lossy(&out)))
    }
}

impl<S: Spawner> Snapshots for CtrRuntime<S> {
    fn volume(&self, id: &AgentId) -> Result<Volume, PortError> {
        self.volume_at("volume", &self.workspace(id))
    }

    fn checkpoints(&self, id: &AgentId) -> Result<Vec<Checkpoint>, PortError> {
        let op = "checkpoints";
        let dir = self.checkpoints_dir(id);
        let entries = match std::fs::read_dir(&dir) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io_fail(op, "list", &dir, e)),
            Ok(entries) => entries,
        };
        let mut all = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| io_fail(op, "list", &dir, e))?;
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            let name = entry.file_name();
            let Some(cp) = name.to_str().and_then(|n| CheckpointId::parse(n).ok()) else {
                continue;
            };
            if is_dir {
                let label = self.read_label(op, &entry.path())?;
                all.push(Checkpoint { id: cp, label });
            }
        }
        all.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(all)
    }

    fn checkpoint(&self, id: &AgentId, label: Option<&Label>) -> Result<CheckpointId, PortError> {
        self.take("checkpoint", id, label)
    }

    fn delete(&self, id: &AgentId, cp: &CheckpointId) -> Result<(), PortError> {
        let op = "delete checkpoint";
        let snap = self.checkpoint_path(id, cp);
        self.remove_volume(op, &snap)?;
        let label = sibling(&snap, LABEL_SUFFIX);
        match std::fs::remove_file(&label) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(io_fail(op, "rm", &label, e)),
            _ => Ok(()),
        }
    }

    fn restore(&self, id: &AgentId, cp: &CheckpointId) -> Result<CheckpointId, PortError> {
        let op = "restore";
        self.recover(op, id, "restore")?;
        let label = Label::parse(&format!("before restore of {cp}")).map_err(|e| PortError {
            op,
            cause: e.to_string(),
        })?;
        let pre = self.take(op, id, Some(&label))?;
        let new = sibling(&self.workspace(id), "restore");
        self.snapshot(op, &self.checkpoint_path(id, cp), &new, false)?;
        self.swap_in(op, id, &new)?;
        Ok(pre)
    }

    fn fork(
        &self,
        from: &AgentId,
        cp: Option<&CheckpointId>,
        to: &AgentId,
    ) -> Result<(), PortError> {
        let src = match cp {
            Some(cp) => self.checkpoint_path(from, cp),
            None => self.workspace(from),
        };
        self.snapshot("fork", &src, &self.workspace(to), false)
    }

    fn migrate(&self, id: &AgentId) -> Result<(), PortError> {
        let op = "migrate";
        self.recover(op, id, "migrate")?;
        let ws = self.workspace(id);
        let new = sibling(&ws, "migrate");
        self.btrfs(
            op,
            &[
                OsStr::new("subvolume"),
                OsStr::new("create"),
                new.as_os_str(),
            ],
        )?;
        // `cp -a ws/. new/` copies the contents and the root directory's owner, mode and times; on
        // btrfs, `--reflink=auto` clones the data instead of copying it.
        // Not under `timeout`: a large workspace on a rotational disk takes as long as it takes.
        let (mut src, mut dst) = (ws.as_os_str().to_owned(), new.as_os_str().to_owned());
        src.push("/.");
        dst.push("/");
        let mut args = os(&["-a", "--reflink=auto", "--"]);
        args.extend([src, dst]);
        let copied = match self.spawn_wait(op, OsStr::new("cp"), &args, None)? {
            (status, _, _) if status.success() => Ok(()),
            (status, stdout, stderr) => Err(crate::failure(op, status, &stdout, &stderr)),
        };
        if let Err(e) = copied {
            return Err(crate::with_cleanup(e, self.remove_volume(op, &new)));
        }
        self.swap_in(op, id, &new)
    }

    fn purge(&self, id: &AgentId) -> Result<(), PortError> {
        let op = "purge checkpoints";
        for c in self.checkpoints(id)? {
            self.delete(id, &c.id)?;
        }
        let dir = self.checkpoints_dir(id);
        for snap in self.job_snapshots(op, &dir)? {
            self.remove_volume(op, &snap)?;
        }
        remove_dir_if_present(op, &dir)?;
        self.purge_backup_parents(id)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    //! The adapter against a stand-in `btrfs` (a shell script run through the real spawner) on
    //! whatever filesystem the tests run on: a subvolume is a directory with the sticky bit, which
    //! `cp -a` and `rename` keep as btrfs keeps a subvolume. `scripts/btrfs-checkpoint-test.sh`
    //! runs the same steps against real btrfs as root.

    use std::sync::atomic::{AtomicUsize, Ordering};

    use platform::process::{Child, Command, GroupHandle, Stdio};
    use platform_linux::LinuxSpawner;

    use super::*;
    use crate::CtrConfig;

    /// `btrfs subvolume create|snapshot [-r]|delete`, `send [-p P] SNAP`, `receive DIR` and
    /// `property get -ts PATH ro`, each call logged to `$LOG`. Read-only subvolumes are listed in
    /// `$LOG.ro`; a send is a tar stream. With `$LOG.fail-receive` present, receive leaves a
    /// partial (writable) subvolume and fails, as a full disk would. `send -q --no-data` writes a
    /// listing of `file` as written, which `receive --dump` prints (job changes).
    const FAKE_BTRFS: &str = r#"set -eu
echo "$*" >> "$LOG"
ro="$LOG.ro"
touch "$ro"
case "$1 ${2:-}" in
"send -q") printf 'snapshot ./%s uuid=x\nwrite ./%s/file offset=0 len=1\n' "${8##*/}" "${8##*/}" > "$7" ;;
"receive --dump") cat "$4" ;;
"subvolume create") mkdir "$3"; chmod +t "$3" ;;
"subvolume snapshot")
    shift 2
    r=; if [ "$1" = -r ]; then r=1; shift; fi
    [ -e "$2" ] && { echo "ERROR: target path already exists: $2" >&2; exit 1; }
    cp -a "$1" "$2"
    if [ -n "$r" ]; then echo "$2" >> "$ro"; fi ;;
"subvolume delete") rm -rf "$3"; grep -vxF "$3" "$ro" > "$ro.tmp" || true; mv "$ro.tmp" "$ro" ;;
send*)
    shift
    if [ "$1" = -p ]; then shift 2; fi
    grep -qxF "$1" "$ro" || { echo "ERROR: $1 is not read-only" >&2; exit 1; }
    tar -C "$(dirname "$1")" -cf - "$(basename "$1")" ;;
receive*)
    first=$(tar -C "$2" -xvf - | head -n 1)
    name="$2/${first%/}"
    chmod +t "$name"
    if [ -e "$LOG.fail-receive" ]; then echo "ERROR: No space left on device" >&2; exit 1; fi
    echo "$name" >> "$ro" ;;
"property get")
    if grep -qxF "$4" "$ro"; then echo ro=true; else echo ro=false; fi ;;
*) exit 2 ;;
esac"#;

    /// Runs `btrfs` as `sh -c FAKE_BTRFS`, everything else as asked.
    pub(crate) struct Fake {
        log: PathBuf,
    }

    /// The same wiring for the real spawner: a file (a pipe end) is duplicated, not moved.
    fn copy(s: &Stdio) -> platform::error::Result<Stdio> {
        Ok(match s {
            Stdio::Inherit => Stdio::Inherit,
            Stdio::Null => Stdio::Null,
            Stdio::Pipe => Stdio::Pipe,
            Stdio::File(f) => Stdio::File(f.try_clone()?),
        })
    }

    impl Spawner for Fake {
        fn spawn(&self, cmd: &Command) -> platform::error::Result<Box<dyn Child>> {
            let (program, mut argv) = match cmd.program.to_str() {
                Some("btrfs") => ("sh", os(&["-c", FAKE_BTRFS, "btrfs"])),
                _ => (cmd.program.to_str().unwrap_or("false"), Vec::new()),
            };
            argv.extend(cmd.argv.iter().cloned());
            let mut env = std::collections::BTreeMap::new();
            env.insert(OsString::from("LOG"), self.log.clone().into_os_string());
            env.insert(OsString::from("PATH"), OsString::from("/usr/bin:/bin"));
            let mut c = Command::new(program, "/")
                .args(argv)
                .env(platform::process::EnvSpec::Explicit(env));
            c.stdin = copy(&cmd.stdin)?;
            c.stdout = copy(&cmd.stdout)?;
            c.stderr = copy(&cmd.stderr)?;
            LinuxSpawner.spawn(&c)
        }
        fn resolve(&self, program: &OsStr) -> platform::error::Result<OsString> {
            LinuxSpawner.resolve(program)
        }
        fn adopt(&self, pid: u32) -> platform::error::Result<Box<dyn GroupHandle>> {
            LinuxSpawner.adopt(pid)
        }
        fn is_alive(&self, pid: u32) -> platform::error::Result<bool> {
            LinuxSpawner.is_alive(pid)
        }
        fn is_zombie(&self, pid: u32) -> platform::error::Result<bool> {
            LinuxSpawner.is_zombie(pid)
        }
    }

    const FAKE_DISK: Disk = Disk {
        btrfs: |_| Ok(true),
        subvolume: |p| Ok(std::fs::symlink_metadata(p)?.permissions().mode() & 0o1000 != 0),
    };

    pub(crate) struct Host {
        pub(crate) rt: CtrRuntime<Fake>,
        pub(crate) root: PathBuf,
    }

    impl Drop for Host {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    impl Host {
        pub(crate) fn new() -> Self {
            static N: AtomicUsize = AtomicUsize::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!("ac-snap-{}-{n}", std::process::id()));
            let _ = std::fs::create_dir_all(&root);
            let cfg = CtrConfig {
                ctr_bin: "ctr".into(),
                namespace: "agents".into(),
                image: "agent:latest".into(),
                workspace_root: root.join("workspaces"),
                checkpoint_root: root.join("checkpoints"),
                backup_root: root.join("backups"),
                btrfs_bin: "btrfs".into(),
                env_dir: root.join("boot"),
                workspace_owner: None,
                network: None,
                vc_root: root.join("vc"),
                task_root: root.join("task"),
                command_timeout: None,
                trace: false,
            };
            let mut rt = CtrRuntime::new(
                Fake {
                    log: root.join("log"),
                },
                cfg,
            );
            rt.disk = FAKE_DISK;
            Self { rt, root }
        }

        pub(crate) fn ws(&self, id: &AgentId) -> PathBuf {
            self.rt.workspace(id)
        }

        /// A subvolume workspace for `id` holding `file` = `text`.
        pub(crate) fn workspace(&self, id: &AgentId, text: &str) {
            let ws = self.ws(id);
            self.rt
                .make_volume("test", &ws)
                .unwrap_or_else(|e| panic!("{e}"));
            self.write(id, text);
        }

        pub(crate) fn write(&self, id: &AgentId, text: &str) {
            let _ = std::fs::write(self.ws(id).join("file"), text);
        }

        pub(crate) fn read(&self, id: &AgentId) -> String {
            std::fs::read_to_string(self.ws(id).join("file")).unwrap_or_default()
        }

        pub(crate) fn log(&self) -> Vec<String> {
            let text = std::fs::read_to_string(self.root.join("log")).unwrap_or_default();
            text.lines().map(str::to_owned).collect()
        }
    }

    pub(crate) fn agent(s: &str) -> AgentId {
        AgentId::parse(s).unwrap_or_else(|e| panic!("{e}"))
    }

    pub(crate) fn label(s: &str) -> Label {
        Label::parse(s).unwrap_or_else(|e| panic!("{e}"))
    }

    pub(crate) fn ok<T>(r: Result<T, PortError>) -> T {
        r.unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn new_workspaces_are_subvolumes_on_btrfs_and_directories_elsewhere() {
        let h = Host::new();
        let a1 = agent("a1");
        assert_eq!(ok(h.rt.volume(&a1)), Volume::Missing);
        h.workspace(&a1, "x");
        assert_eq!(ok(h.rt.volume(&a1)), Volume::Subvolume);
        assert_eq!(
            h.log(),
            [format!("subvolume create {}", h.ws(&a1).display())]
        );
        let mut plain = Host::new();
        plain.rt.disk.btrfs = |_| Ok(false);
        plain.workspace(&a1, "x");
        assert_eq!(ok(plain.rt.volume(&a1)), Volume::Directory);
        assert!(plain.log().is_empty());
        assert_eq!(ok(plain.rt.plain_workspaces()), None, "not btrfs");
        let _ = std::fs::create_dir(h.ws(&agent("old")));
        assert_eq!(ok(h.rt.plain_workspaces()), Some(vec![agent("old")]));
    }

    #[test]
    fn checkpoints_are_read_only_snapshots_with_labels_beside_them() {
        let h = Host::new();
        let a1 = agent("a1");
        h.workspace(&a1, "one");
        let first = ok(h.rt.checkpoint(&a1, Some(&label("before refactor"))));
        h.write(&a1, "two");
        let second = ok(h.rt.checkpoint(&a1, None));
        assert!(first < second, "same second gets -2: {first} {second}");
        let all = ok(h.rt.checkpoints(&a1));
        assert_eq!(all.len(), 2);
        assert_eq!(
            all[0].label.as_ref().map(Label::as_str),
            Some("before refactor")
        );
        assert_eq!(all[1].label, None);
        let snap = h.root.join("checkpoints/a1").join(first.as_str());
        assert_eq!(
            std::fs::read_to_string(snap.join("file")).ok().as_deref(),
            Some("one")
        );
        assert!(
            h.log()[1].starts_with("subvolume snapshot -r "),
            "{:?}",
            h.log()
        );
        let mode = std::fs::metadata(h.root.join("checkpoints/a1")).map(|m| m.permissions().mode());
        assert_eq!(mode.ok().map(|m| m & 0o777), Some(0o700));
        ok(h.rt.delete(&a1, &first));
        assert_eq!(ok(h.rt.checkpoints(&a1)).len(), 1);
        assert!(!sibling(&snap, LABEL_SUFFIX).exists());
    }

    #[test]
    fn restore_keeps_the_current_workspace_as_a_checkpoint_first() {
        let h = Host::new();
        let a1 = agent("a1");
        h.workspace(&a1, "good");
        let good = ok(h.rt.checkpoint(&a1, None));
        h.write(&a1, "broken");
        let pre = ok(h.rt.restore(&a1, &good));
        assert_eq!(h.read(&a1), "good");
        assert_eq!(ok(h.rt.volume(&a1)), Volume::Subvolume);
        let all = ok(h.rt.checkpoints(&a1));
        let kept = all
            .iter()
            .find(|c| c.id == pre)
            .and_then(|c| c.label.clone());
        assert_eq!(
            kept.map(|l| l.as_str().to_owned()),
            Some(format!("before restore of {good}"))
        );
        // Undo the restore by restoring what it kept.
        ok(h.rt.restore(&a1, &pre));
        assert_eq!(h.read(&a1), "broken");
        for leftover in ["old", "restore"] {
            assert!(!sibling(&h.ws(&a1), leftover).exists(), "{leftover}");
        }
    }

    #[test]
    fn a_swap_cut_short_is_finished_or_undone_next_time() {
        let h = Host::new();
        let a1 = agent("a1");
        h.workspace(&a1, "now");
        let cp = ok(h.rt.checkpoint(&a1, None));
        let (ws, old) = (h.ws(&a1), sibling(&h.ws(&a1), "old"));
        // Crashed between the two renames: only `.old` and a half-built `.restore` are left.
        let _ = std::fs::rename(&ws, &old);
        let _ = std::fs::create_dir(sibling(&ws, "restore"));
        ok(h.rt.restore(&a1, &cp));
        assert_eq!(h.read(&a1), "now");
        assert!(!old.exists());
        // Crashed after the second rename: `.old` is left beside the new workspace.
        let _ = std::fs::create_dir(&old);
        ok(h.rt.restore(&a1, &cp));
        assert!(!old.exists());
    }

    #[test]
    fn forks_snapshot_the_workspace_or_a_checkpoint() {
        let h = Host::new();
        let (a1, b1, c1) = (agent("a1"), agent("b1"), agent("c1"));
        h.workspace(&a1, "then");
        let cp = ok(h.rt.checkpoint(&a1, None));
        h.write(&a1, "now");
        ok(h.rt.fork(&a1, None, &b1));
        ok(h.rt.fork(&a1, Some(&cp), &c1));
        assert_eq!((h.read(&b1), h.read(&c1)), ("now".into(), "then".into()));
        assert_eq!(ok(h.rt.volume(&c1)), Volume::Subvolume);
        h.write(&b1, "mine");
        assert_eq!(h.read(&a1), "now", "a fork is a copy");
        let taken = h.rt.fork(&a1, None, &b1);
        assert!(taken.is_err_and(|e| e.cause.contains("already exists")));
    }

    #[test]
    fn migrate_copies_a_plain_workspace_into_a_subvolume() {
        let h = Host::new();
        let a1 = agent("a1");
        let ws = h.ws(&a1);
        let _ = std::fs::create_dir_all(ws.join("sub"));
        let _ = std::fs::set_permissions(&ws, std::fs::Permissions::from_mode(0o750));
        h.write(&a1, "kept");
        let _ = std::os::unix::fs::symlink("/etc/shadow", ws.join("link"));
        assert_eq!(ok(h.rt.volume(&a1)), Volume::Directory);
        ok(h.rt.migrate(&a1));
        assert_eq!(h.read(&a1), "kept");
        assert!(ws.join("sub").is_dir());
        let link = std::fs::read_link(ws.join("link")).ok();
        assert_eq!(link, Some(PathBuf::from("/etc/shadow")), "copied as a link");
        let mode = std::fs::metadata(&ws).map(|m| m.permissions().mode() & 0o777);
        assert_eq!(mode.ok(), Some(0o750), "root directory's mode kept");
        assert!(!sibling(&ws, "old").exists() && !sibling(&ws, "migrate").exists());
    }

    #[test]
    fn purge_deletes_every_checkpoint_and_destroy_purge_the_subvolume() {
        let h = Host::new();
        let a1 = agent("a1");
        h.workspace(&a1, "x");
        for _ in 0..3 {
            ok(h.rt.checkpoint(&a1, Some(&label("l"))));
        }
        ok(h.rt.purge(&a1));
        assert!(!h.root.join("checkpoints/a1").exists());
        ok(h.rt.remove_volume("destroy", &h.ws(&a1)));
        assert_eq!(ok(h.rt.volume(&a1)), Volume::Missing);
        let deletes = h
            .log()
            .iter()
            .filter(|l| l.starts_with("subvolume delete"))
            .count();
        assert_eq!(deletes, 4);
        ok(h.rt.purge(&agent("never")));
    }

    #[test]
    fn job_changes_compare_snapshots_then_delete_them() {
        let h = Host::new();
        let (a1, job) = (agent("a1"), JobId::from_random([1; 8]));
        h.workspace(&a1, "x");
        assert_eq!(ok(h.rt.job_changes(&a1, &job)), None, "no start snapshot");
        ok(h.rt.job_begin(&a1, &job));
        assert!(ok(h.rt.checkpoints(&a1)).is_empty(), "not a checkpoint");
        assert_eq!(ok(h.rt.job_changes(&a1, &job)), Some(vec!["file".into()]));
        let dir = h.root.join("checkpoints/a1");
        let left: Vec<_> = std::fs::read_dir(&dir)
            .map(|d| d.count())
            .into_iter()
            .collect();
        assert_eq!(left, [0], "snapshots and stream deleted");
        let start = dir.join(format!("{job}.start"));
        let log = h.log();
        assert!(
            log.iter()
                .any(|l| l.starts_with(&format!("send -q --no-data -p {} -f ", start.display()))),
            "{log:?}"
        );
        // A plain-directory workspace has no snapshots; a crash's leftover is purged.
        let mut plain = Host::new();
        plain.rt.disk.btrfs = |_| Ok(false);
        plain.workspace(&a1, "x");
        ok(plain.rt.job_begin(&a1, &job));
        assert_eq!(ok(plain.rt.job_changes(&a1, &job)), None);
        ok(h.rt.job_begin(&a1, &job));
        ok(h.rt.purge(&a1));
        assert!(!dir.exists());
    }

    #[test]
    fn a_symlink_where_a_volume_should_be_is_refused() {
        let h = Host::new();
        let a1 = agent("a1");
        let _ = std::fs::create_dir_all(h.root.join("workspaces"));
        let _ = std::os::unix::fs::symlink("/", h.ws(&a1));
        let r = h.rt.volume(&a1);
        assert!(r.is_err_and(|e| e.cause.contains("is not a directory")));
    }

    /// The real thing: `btrfs` from btrfs-progs, statfs(2) and inode 256, as root, in
    /// `AC_BTRFS_TEST_DIR` (a directory on a btrfs mount). `scripts/btrfs-checkpoint-test.sh`
    /// makes a loopback btrfs and runs this.
    #[test]
    #[ignore = "needs root and btrfs: scripts/btrfs-checkpoint-test.sh"]
    fn on_real_btrfs() {
        let Some(dir) = std::env::var_os("AC_BTRFS_TEST_DIR").map(PathBuf::from) else {
            panic!("set AC_BTRFS_TEST_DIR to a directory on btrfs");
        };
        assert!(ok_io(is_btrfs(&dir)), "{} is not on btrfs", dir.display());
        let cfg = CtrConfig {
            ctr_bin: "ctr".into(),
            namespace: "agents".into(),
            image: "agent:latest".into(),
            workspace_root: dir.join("workspaces"),
            checkpoint_root: dir.join("checkpoints"),
            backup_root: dir.join("backups"),
            btrfs_bin: "btrfs".into(),
            env_dir: dir.join("boot"),
            workspace_owner: None,
            network: None,
            vc_root: dir.join("vc"),
            task_root: dir.join("task"),
            command_timeout: Some(std::time::Duration::from_secs(60)),
            trace: true,
        };
        let rt = CtrRuntime::new(LinuxSpawner, cfg);
        let (a1, b1, old) = (agent("a1"), agent("b1"), agent("old"));
        let ws = rt.workspace(&a1);
        let file = |id: &AgentId| {
            std::fs::read_to_string(rt.workspace(id).join("file")).unwrap_or_default()
        };
        ok(rt.make_volume("test", &ws));
        assert_eq!(ok(rt.volume(&a1)), Volume::Subvolume);
        let _ = std::fs::write(ws.join("file"), "good");
        let good = ok(rt.checkpoint(&a1, Some(&label("good"))));
        let snap = rt.checkpoint_path(&a1, &good);
        assert!(
            std::fs::write(snap.join("file"), "x").is_err(),
            "checkpoints are read-only"
        );
        let _ = std::fs::write(ws.join("file"), "broken");
        let kept = ok(rt.restore(&a1, &good));
        assert_eq!(file(&a1), "good");
        assert_eq!(ok(rt.volume(&a1)), Volume::Subvolume);
        ok(rt.fork(&a1, Some(&kept), &b1));
        assert_eq!(
            (file(&b1), ok(rt.volume(&b1))),
            ("broken".into(), Volume::Subvolume)
        );
        let _ = std::fs::create_dir(rt.workspace(&old));
        let _ = std::fs::write(rt.workspace(&old).join("file"), "plain");
        assert_eq!(ok(rt.plain_workspaces()), Some(vec![old.clone()]));
        ok(rt.migrate(&old));
        assert_eq!(
            (file(&old), ok(rt.volume(&old))),
            ("plain".into(), Volume::Subvolume)
        );
        // A job's change summary: what changed between its two snapshots, none of them kept.
        let job = JobId::from_random([7; 8]);
        ok(rt.job_begin(&a1, &job));
        let _ = std::fs::write(ws.join("file"), "edited");
        let _ = std::fs::create_dir(ws.join("new dir"));
        let _ = std::fs::write(ws.join("new dir/f"), "n");
        let changed = ok(rt.job_changes(&a1, &job));
        assert_eq!(
            changed,
            Some(vec![
                "file".into(),
                r"new\ dir".into(),
                r"new\ dir/f".into()
            ])
        );
        assert_eq!(ok(rt.checkpoints(&a1)).len(), 2, "no job snapshot left");
        ok(rt.job_begin(&a1, &job));
        for id in [&a1, &b1, &old] {
            ok(rt.purge(id));
            ok(rt.remove_volume("destroy", &rt.workspace(id)));
            assert_eq!(ok(rt.volume(id)), Volume::Missing);
        }
        assert!(!rt.checkpoints_dir(&a1).exists());
    }

    fn ok_io<T>(r: io::Result<T>) -> T {
        r.unwrap_or_else(|e| panic!("{e}"))
    }
}
