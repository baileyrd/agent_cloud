//! Per-agent limits (ADR-023) on each boot. Disk first: a btrfs quota on the workspace subvolume,
//! set before the VM starts and again on every boot, so an agent created before the limit, or
//! restored or migrated into a new subvolume, gets it too. CPU and memory once the VM runs:
//! `cpu.max` and `memory.max` on the sandbox's cgroup. `ctr run --cgroup /<namespace>/<id>` names
//! the parent; Kata (3.26, cgroupfs) makes `<namespace>/kata_<id>` beside it and, with
//! `sandbox_cgroup_only = true`, puts the shim, QEMU and virtiofsd there (seen on BaileyServ
//! 2026-10-29). A cgroup that holds no process is refused, never left unlimited.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use ac_domain::{AgentId, Volume};
use ac_ports::PortError;
use platform::process::Spawner;

use crate::CtrRuntime;

impl<S: Spawner> CtrRuntime<S> {
    /// What `ctr run --cgroup` gets: `/<namespace>/<id>`. Kata's sandbox cgroup goes beside it.
    pub(crate) fn cgroup_path(&self, id: &AgentId) -> String {
        format!("/{}/{id}", self.cfg.namespace)
    }

    fn cgroup_dir(&self, id: &AgentId) -> PathBuf {
        self.cfg
            .cgroup_root
            .join(&self.cfg.namespace)
            .join(format!("kata_{id}"))
    }

    /// Set the workspace's btrfs quota, if there is a disk limit. It needs a subvolume.
    pub(crate) fn limit_disk(&self, op: &'static str, dir: &Path) -> Result<(), PortError> {
        let Some(bytes) = self.cfg.limits.disk_bytes() else {
            return Ok(());
        };
        if self.volume_at(op, dir)? != Volume::Subvolume {
            return Err(PortError {
                op,
                cause: format!(
                    "AC_AGENT_DISK_GIB needs a btrfs subvolume workspace and {} is not one \
                     (pause it and run ac-ctl workspace migrate)",
                    dir.display()
                ),
            });
        }
        let bytes = bytes.to_string();
        self.btrfs(
            op,
            &[
                OsStr::new("qgroup"),
                OsStr::new("limit"),
                OsStr::new(&bytes),
                dir.as_os_str(),
            ],
        )
    }

    /// Write `cpu.max` and `memory.max` on the running sandbox's cgroup, if either is limited.
    pub(crate) fn limit_cgroup(&self, op: &'static str, id: &AgentId) -> Result<(), PortError> {
        let limits = &self.cfg.limits;
        if !limits.has_cgroup() {
            return Ok(());
        }
        let fail = |what: &str, path: &Path, e: std::io::Error| PortError {
            op,
            cause: format!("{what} {}: {e}", path.display()),
        };
        let dir = self.cgroup_dir(id);
        let control = dir
            .parent()
            .unwrap_or(&self.cfg.cgroup_root)
            .join("cgroup.subtree_control");
        std::fs::write(&control, limits.controllers())
            .map_err(|e| fail("enable controllers in", &control, e))?;
        let procs = dir.join("cgroup.procs");
        let held = std::fs::read_to_string(&procs).map_err(|e| fail("read", &procs, e))?;
        if held.trim().is_empty() {
            return Err(PortError {
                op,
                cause: format!(
                    "{} holds no process, so limits there would bind nothing: set \
                     sandbox_cgroup_only = true in Kata's [runtime] table",
                    dir.display()
                ),
            });
        }
        for (name, value) in limits.cgroup_files() {
            let file = dir.join(name);
            std::fs::write(&file, value).map_err(|e| fail("write", &file, e))?;
        }
        Ok(())
    }
}
