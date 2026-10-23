//! The host side of admission control: `/dev/shm` from statfs(2) (`rusty_libc`, first-party; std
//! has no statvfs) and the guest size from the Kata configuration.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use ac_domain::capacity::kata_default_memory;
use ac_domain::GuestMemory;
use ac_ports::{Capacity, PortError, Shm};
use rusty_libc::fs::Statfs;

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
pub struct HostCapacity {
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

impl HostCapacity {
    /// Read the host as `cfg` says.
    pub fn new(cfg: CapacityConfig) -> Self {
        Self { cfg }
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

/// (total, free) MiB of the filesystem holding `path`, from statfs(2).
pub fn statfs_total_free(path: &Path) -> Result<(u64, u64), PortError> {
    let fail = |cause: String| PortError {
        op: "statfs",
        cause: format!("{}: {cause}", path.display()),
    };
    let c = CString::new(path.as_os_str().as_bytes()).map_err(|e| fail(e.to_string()))?;
    let stats = rusty_libc::fs::statfs(&c).map_err(|e| fail(e.to_string()))?;
    Ok(statfs_mib(&stats))
}

impl Capacity for HostCapacity {
    fn shm(&self) -> Result<Shm, PortError> {
        let (total, free) = statfs_total_free(&self.cfg.shm)?;
        Ok(limit(total, free, self.cfg.shm_limit_mib))
    }

    fn guest_mib(&self) -> Result<u64, PortError> {
        self.guest().map(|(mib, _)| mib)
    }
}

/// (total, free) MiB of a filesystem: blocks of the fragment size (the block size where there is
/// none). Free counts every free block; tmpfs reserves none for root.
fn statfs_mib(s: &Statfs) -> (u64, u64) {
    let unit = if s.f_frsize > 0 {
        s.f_frsize
    } else {
        s.f_bsize
    };
    let unit = u128::from(u64::try_from(unit).unwrap_or(0));
    let mib = |blocks: u64| u64::try_from((u128::from(blocks) * unit) >> 20).unwrap_or(u64::MAX);
    (mib(s.f_blocks), mib(s.f_bfree))
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

    fn stats(frsize: i64, bsize: i64, blocks: u64, free: u64) -> Statfs {
        let mut s = Statfs::default();
        (s.f_frsize, s.f_bsize, s.f_blocks, s.f_bfree) = (frsize, bsize, blocks, free);
        s
    }

    #[test]
    fn statfs_figures() {
        // 4 KiB fragments: 16 GiB, 15 GiB free.
        assert_eq!(
            statfs_mib(&stats(4096, 4096, 4_194_304, 3_932_160)),
            (16384, 15360)
        );
        // No fragment size: the block size.
        assert_eq!(statfs_mib(&stats(0, 1 << 20, 10, 3)), (10, 3));
        // Nonsense sizes give 0; huge products saturate.
        assert_eq!(statfs_mib(&stats(-1, -1, 10, 3)), (0, 0));
        assert_eq!(statfs_mib(&stats(i64::MAX, 0, u64::MAX, 0)), (u64::MAX, 0));
    }

    #[test]
    fn reads_a_real_filesystem() {
        let host = HostCapacity::new(CapacityConfig {
            shm: "/".into(),
            kata_config: "/nonexistent".into(),
            guest_mib: Some(1),
            shm_limit_mib: None,
        });
        let shm = host.shm().unwrap_or_else(|e| panic!("{e}"));
        assert!(
            shm.total_mib > 0 && shm.free_mib <= shm.total_mib,
            "{shm:?}"
        );
        let missing = HostCapacity::new(CapacityConfig {
            shm: "/nonexistent/shm".into(),
            ..host.cfg.clone()
        });
        assert!(missing
            .shm()
            .is_err_and(|e| e.to_string().contains("/nonexistent/shm")));
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
