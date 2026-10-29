//! `ac-ctl backup`, `backups` and `restore-backup` (ADR-021). None of them opens the registry, so
//! they run beside `ac-gateway`; a backup and a restore hold `<data>/backup.lock` against each
//! other.

use std::path::Path;
use std::time::Duration;

use ac_ctr::facts::{kata_sandboxes, HostRead, LinuxHost};
use ac_ctr::{CtrConfig, CtrRuntime};
use ac_domain::backup::{self, BackupId};
use ac_domain::{checkpoint, AgentId, Volume};
use ac_ports::Snapshots;
use platform_linux::LinuxSpawner;
use rusty_multimodal_db_engine::dir_lock::{DirLock, DirLockError};

use crate::args::{Backup, BackupOp};

type Rt = CtrRuntime<LinuxSpawner>;

/// The adapter, for its volume operations only: no VM is started here.
fn runtime(b: &Backup) -> Rt {
    CtrRuntime::new(
        LinuxSpawner,
        CtrConfig {
            ctr_bin: "ctr".into(),
            namespace: "agents".into(),
            image: String::new(),
            workspace_root: b.workspaces.clone(),
            checkpoint_root: b.data.join("checkpoints"),
            backup_root: b.data.join("backups"),
            btrfs_bin: std::env::var("AC_BTRFS").unwrap_or_else(|_| "btrfs".to_owned()),
            env_dir: b.data.join("boot"),
            workspace_owner: None,
            network: None,
            vc_root: "/run/vc".into(),
            task_root: "/run/containerd/io.containerd.runtime.v2.task".into(),
            command_timeout: Some(Duration::from_secs(120)),
            trace: std::env::var_os("AC_TRACE").is_some(),
            limits: Default::default(),
            cgroup_root: "/sys/fs/cgroup".into(),
        },
    )
}

fn lock(data: &Path) -> Result<DirLock, String> {
    DirLock::acquire(data, "backup.lock").map_err(|e| match e {
        DirLockError::Held(_) => "another backup or restore-backup is running".to_owned(),
        e => format!("lock {}: {e}", data.display()),
    })
}

/// Run it; the exit code (0 ok, 1 something failed), or why it could not run at all.
pub fn run(b: &Backup, max_checkpoints: usize) -> Result<u8, String> {
    let rt = runtime(b);
    match &b.op {
        BackupOp::Send {
            agents,
            keep,
            full_every,
        } => {
            let _held = lock(&b.data)?;
            send(&rt, &b.dest, agents, *keep, *full_every)
        }
        BackupOp::List(agent) => list(&rt, &b.dest, agent.as_ref()),
        BackupOp::Restore(id, wanted) => {
            let _held = lock(&b.data)?;
            restore(&rt, &b.dest, id, wanted.as_ref(), max_checkpoints)
        }
    }
}

/// The agents to back up and what their workspaces are: those named, else every one there.
fn targets(rt: &Rt, named: &[AgentId]) -> Result<Vec<(AgentId, Volume)>, String> {
    if named.is_empty() {
        return rt.workspaces().map_err(|e| e.to_string());
    }
    named
        .iter()
        .map(|id| Ok((id.clone(), rt.volume(id).map_err(|e| e.to_string())?)))
        .collect()
}

fn send(
    rt: &Rt,
    dest: &Path,
    named: &[AgentId],
    keep: usize,
    full_every: usize,
) -> Result<u8, String> {
    let mut failed = false;
    for (id, volume) in targets(rt, named)? {
        let line = match volume {
            Volume::Subvolume => rt
                .backup(dest, &id, keep, full_every)
                .map(|s| {
                    let how = match &s.parent {
                        Some(p) => format!("incremental from {p}"),
                        None => "full".to_owned(),
                    };
                    let pruned = match s.pruned.len() {
                        0 => String::new(),
                        n => format!("; {n} older deleted"),
                    };
                    format!("{id} backed up {} ({how}, {}{pruned})", s.id, size(s.bytes))
                })
                .map_err(|e| e.to_string()),
            Volume::Directory => Err(format!(
                "its workspace is a plain directory; pause it and run `ac-ctl workspace \
                     migrate {id}`"
            )),
            Volume::Missing => Err("it has no workspace".to_owned()),
        };
        match line {
            Ok(line) => println!("{line}"),
            Err(e) => {
                failed = true;
                eprintln!("{id} not backed up: {e}");
            }
        }
    }
    Ok(u8::from(failed))
}

/// `bytes` for a person: KiB, MiB or GiB with one decimal.
fn size(bytes: u64) -> String {
    let units = [("GiB", 1u64 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)];
    match units.iter().find(|(_, n)| bytes >= *n) {
        Some((unit, n)) => format!("{:.1} {unit}", bytes as f64 / *n as f64),
        None => format!("{bytes} B"),
    }
}

/// Agent ids with a directory at `dest`.
fn agents_at(dest: &Path) -> Result<Vec<AgentId>, String> {
    let entries = std::fs::read_dir(dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    let mut ids: Vec<AgentId> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().to_str().and_then(|n| AgentId::parse(n).ok()))
        .collect();
    ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    Ok(ids)
}

fn list(rt: &Rt, dest: &Path, agent: Option<&AgentId>) -> Result<u8, String> {
    let ids = match agent {
        Some(id) => vec![id.clone()],
        None => agents_at(dest)?,
    };
    for id in ids {
        let streams = rt.streams(dest, &id).map_err(|e| e.to_string())?;
        for b in backup::restorable(&streams) {
            let how = match streams
                .iter()
                .find(|s| s.id == b)
                .and_then(|s| s.parent.as_ref())
            {
                Some(p) => format!("incremental from {p}"),
                None => "full".to_owned(),
            };
            println!("{id} {b} {how}");
        }
    }
    Ok(0)
}

fn restore(
    rt: &Rt,
    dest: &Path,
    id: &AgentId,
    wanted: Option<&BackupId>,
    max_checkpoints: usize,
) -> Result<u8, String> {
    let all = rt.backups(dest, id).map_err(|e| e.to_string())?;
    let b = backup::pick(id, &all, wanted).map_err(|e| e.to_string())?;
    let running = kata_sandboxes(&LinuxHost::new(LinuxSpawner).cmdlines())
        .iter()
        .any(|s| s == id.as_str());
    let volume = rt.volume(id).map_err(|e| e.to_string())?;
    backup::check_restore(id, running, volume).map_err(|e| e.to_string())?;
    if volume == Volume::Subvolume {
        let count = rt.checkpoints(id).map_err(|e| e.to_string())?.len();
        checkpoint::check_room(id, count, max_checkpoints).map_err(|e| e.to_string())?;
    }
    match rt.restore_backup(dest, id, b).map_err(|e| e.to_string())? {
        Some(kept) => {
            println!("{id} restored backup {b}; the workspace it replaced is checkpoint {kept}")
        }
        None => println!(
            "{id} restored backup {b} as a new workspace; create (or resume) {id} to boot it"
        ),
    }
    Ok(0)
}
