//! The host side of admission control: `/dev/shm` from statfs(2) (coreutils `stat -f`; std has no
//! statvfs and the crate takes no FFI dependency) and the guest size from the Kata configuration.

use std::ffi::OsStr;
use std::path::PathBuf;

use ac_domain::capacity::kata_default_memory;
use ac_domain::GuestMemory;
use ac_ports::{Capacity, PortError, Shm};
use platform::process::Spawner;

use crate::{os, CtrRuntime};

/// Where the figures come from.
#[derive(Debug, Clone)]
pub struct CapacityConfig {
    /// The filesystem guest RAM lives on (`/dev/shm`).
    pub shm: PathBuf,
    /// Kata's `configuration.toml`.
    pub kata_config: PathBuf,
    /// Guest size override (`AC_VM_MEMORY_MIB`), instead of the Kata configuration.
    pub guest_mib: Option<u64>,
    /// Testing only (`AC_SHM_LIMIT_MIB`): treat `/dev/shm` as this big. See [`limit`].
    pub shm_limit_mib: Option<u64>,
}

/// [`Capacity`] read from this host.
pub struct HostCapacity<S> {
    rt: CtrRuntime<S>,
    cfg: CapacityConfig,
}

/// Where the guest size came from, for the startup log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuestSource {
    /// `AC_VM_MEMORY_MIB`.
    Override,
    /// The Kata configuration, set or defaulted.
    Kata(GuestMemory),
}

impl<S: Spawner> HostCapacity<S> {
    /// Read `/dev/shm` with `rt`'s spawner.
    pub fn new(rt: CtrRuntime<S>, cfg: CapacityConfig) -> Self {
        Self { rt, cfg }
    }

    /// The guest size and where it came from.
    pub fn guest(&self) -> Result<(u64, GuestSource), PortError> {
        if let Some(mib) = self.cfg.guest_mib {
            return Ok((mib, GuestSource::Override));
        }
        let fail = |cause: String| PortError {
            op: "guest memory",
            cause,
        };
        let path = &self.cfg.kata_config;
        let text =
            std::fs::read_to_string(path).map_err(|e| fail(format!("{}: {e}", path.display())))?;
        let memory =
            kata_default_memory(&text).map_err(|e| fail(format!("{}: {e}", path.display())))?;
        Ok((memory.mib(), GuestSource::Kata(memory)))
    }
}

impl<S: Spawner> Capacity for HostCapacity<S> {
    fn shm(&self) -> Result<Shm, PortError> {
        let path = self.cfg.shm.to_string_lossy();
        let args = os(&["-f", "-c", "%S %b %f", &path]);
        let out = self.rt.run("statfs", OsStr::new("stat"), &args, None)?;
        let (total, free) =
            statfs_mib(&String::from_utf8_lossy(&out)).ok_or_else(|| PortError {
                op: "statfs",
                cause: format!("unexpected `stat -f` output for {path}: {out:?}"),
            })?;
        Ok(limit(total, free, self.cfg.shm_limit_mib))
    }

    fn guest_mib(&self) -> Result<u64, PortError> {
        self.guest().map(|(mib, _)| mib)
    }
}

/// `"<block size> <blocks> <free blocks>"` as (total, free) MiB.
fn statfs_mib(out: &str) -> Option<(u64, u64)> {
    let mut n = out.split_whitespace().map(str::parse::<u64>);
    let (size, blocks, free) = (n.next()?.ok()?, n.next()?.ok()?, n.next()?.ok()?);
    let mib = |b: u64| u64::try_from((u128::from(b) * u128::from(size)) >> 20).unwrap_or(u64::MAX);
    Some((mib(blocks), mib(free)))
}

/// The testing override: with a limit, `/dev/shm` counts as that big, and what is free shrinks by
/// what is already used (never more than the real free space).
pub fn limit(total_mib: u64, free_mib: u64, limit_mib: Option<u64>) -> Shm {
    match limit_mib {
        Some(l) => Shm {
            total_mib: l,
            free_mib: free_mib.min(l.saturating_sub(total_mib.saturating_sub(free_mib))),
        },
        None => Shm {
            total_mib,
            free_mib,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statfs_output() {
        // 4 KiB blocks: 16 GiB, 15 GiB free.
        assert_eq!(statfs_mib("4096 4194304 3932160\n"), Some((16384, 15360)));
        assert_eq!(statfs_mib("4096 4194304"), None);
        assert_eq!(statfs_mib("x 1 2"), None);
        let max = u64::MAX;
        assert_eq!(statfs_mib(&format!("{max} {max} 0")), Some((u64::MAX, 0)));
    }

    #[test]
    fn limit_replaces_total_and_keeps_used() {
        let real = (16384, 15360); // 1024 used
        assert_eq!(
            limit(real.0, real.1, None),
            Shm {
                total_mib: 16384,
                free_mib: 15360
            }
        );
        assert_eq!(
            limit(real.0, real.1, Some(9000)),
            Shm {
                total_mib: 9000,
                free_mib: 7976
            }
        );
        assert_eq!(
            limit(real.0, real.1, Some(500)),
            Shm {
                total_mib: 500,
                free_mib: 0
            }
        );
        assert_eq!(
            limit(100, 50, Some(9000)),
            Shm {
                total_mib: 9000,
                free_mib: 50
            }
        );
    }
}
