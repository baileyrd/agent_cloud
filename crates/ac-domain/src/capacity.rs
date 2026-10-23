//! Admission against `/dev/shm`, which backs every guest's RAM (Kata's `memory-backend-file`).
//! Touched guest pages are never given back, so a guest is counted at its configured size, not
//! at what it uses now. Pure arithmetic; all inputs in MiB.

use std::fmt;

/// Kata's built-in `default_memory`, used when the configuration does not set it.
pub const KATA_DEFAULT_MEMORY_MIB: u64 = 2048;

/// Why a guest is not admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shortfall {
    /// Which rule refused it.
    pub rule: Rule,
    /// MiB the rule needs.
    pub needed_mib: u64,
    /// MiB the rule has.
    pub available_mib: u64,
    /// How many more guests of the requested size fit now.
    pub more: u64,
}

/// The two admission rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    /// Every committed guest plus the new one plus the margin must fit `/dev/shm`'s size.
    Total,
    /// The new guest plus the margin must fit what `/dev/shm` has free now.
    Free,
}

impl fmt::Display for Shortfall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = match self.rule {
            Rule::Total => "committed guest memory plus margin",
            Rule::Free => "this guest plus margin",
        };
        let of = match self.rule {
            Rule::Total => "/dev/shm size",
            Rule::Free => "/dev/shm free",
        };
        write!(
            f,
            "not enough /dev/shm for guest memory: {what} needs {} MiB, {of} is {} MiB; {} more \
             agent(s) of this size fit",
            self.needed_mib, self.available_mib, self.more
        )
    }
}

impl std::error::Error for Shortfall {}

/// Admit a guest of `request_mib` when guests of `running_guest_mib` in total are already
/// committed: `running + request + margin <= shm_total` and `request + margin <= shm_free`.
/// Saturating arithmetic: a sum past `u64::MAX` is refused, never wrapped.
pub fn admit(
    running_guest_mib: u64,
    request_mib: u64,
    shm_total_mib: u64,
    shm_free_mib: u64,
    margin_mib: u64,
) -> Result<(), Shortfall> {
    let more = more(
        running_guest_mib,
        request_mib,
        shm_total_mib,
        shm_free_mib,
        margin_mib,
    );
    let total = running_guest_mib
        .saturating_add(request_mib)
        .saturating_add(margin_mib);
    if total > shm_total_mib {
        return Err(Shortfall {
            rule: Rule::Total,
            needed_mib: total,
            available_mib: shm_total_mib,
            more,
        });
    }
    let free = request_mib.saturating_add(margin_mib);
    if free > shm_free_mib {
        return Err(Shortfall {
            rule: Rule::Free,
            needed_mib: free,
            available_mib: shm_free_mib,
            more,
        });
    }
    Ok(())
}

/// How many more guests of `guest_mib` [`admit`] would take one after another, counting each at
/// its full size against both limits; 0 for a zero-sized guest (no answer).
pub fn more(
    running_guest_mib: u64,
    guest_mib: u64,
    shm_total_mib: u64,
    shm_free_mib: u64,
    margin_mib: u64,
) -> u64 {
    let by_total = shm_total_mib
        .saturating_sub(margin_mib)
        .saturating_sub(running_guest_mib);
    let by_free = shm_free_mib.saturating_sub(margin_mib);
    by_total
        .min(by_free)
        .checked_div(guest_mib)
        .unwrap_or_default()
}

/// The guest size a Kata configuration sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestMemory {
    /// `default_memory` in a `[hypervisor.*]` table.
    Configured(u64),
    /// Not set: Kata uses [`KATA_DEFAULT_MEMORY_MIB`].
    KataDefault,
}

impl GuestMemory {
    /// The size in MiB.
    pub fn mib(self) -> u64 {
        match self {
            Self::Configured(mib) => mib,
            Self::KataDefault => KATA_DEFAULT_MEMORY_MIB,
        }
    }
}

/// `default_memory` of the first `[hypervisor.*]` table of a Kata `configuration.toml`, read by
/// [`crate::tune::facts::kata`]. A value that is not a positive integer, or a key set twice, is an
/// error, not a fallback.
pub fn kata_default_memory(toml: &str) -> Result<GuestMemory, String> {
    match crate::tune::facts::kata(toml)?.default_memory {
        None => Ok(GuestMemory::KataDefault),
        Some(0) => Err("default_memory = 0 is not a positive number of MiB".into()),
        Some(mib) => Ok(GuestMemory::Configured(mib)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_boundary_is_admitted_and_one_mib_over_is_not() {
        // 2 x 4096 committed, 4096 more, 512 margin: 12800 exactly.
        assert_eq!(admit(8192, 4096, 12800, 12800, 512), Ok(()));
        let over = admit(8192, 4096, 12799, 12800, 512);
        assert_eq!(
            over,
            Err(Shortfall {
                rule: Rule::Total,
                needed_mib: 12800,
                available_mib: 12799,
                more: 0
            })
        );
        // The free rule's own boundary.
        assert_eq!(admit(0, 4096, 20000, 4608, 512), Ok(()));
        assert!(matches!(
            admit(0, 4096, 20000, 4607, 512),
            Err(Shortfall {
                rule: Rule::Free,
                needed_mib: 4608,
                available_mib: 4607,
                ..
            })
        ));
    }

    #[test]
    fn zero_agents() {
        assert_eq!(admit(0, 4096, 16384, 16384, 512), Ok(()));
        assert_eq!(more(0, 4096, 16384, 16384, 512), 3);
        assert_eq!(more(0, 4096, 0, 0, 0), 0);
    }

    #[test]
    fn overflow_saturates_and_refuses() {
        let r = admit(u64::MAX, u64::MAX, u64::MAX - 1, u64::MAX, u64::MAX);
        assert!(matches!(
            r,
            Err(Shortfall {
                rule: Rule::Total,
                needed_mib: u64::MAX,
                ..
            })
        ));
        assert_eq!(more(u64::MAX, 1, u64::MAX, u64::MAX, 0), 0);
        assert_eq!(more(0, 0, u64::MAX, u64::MAX, 0), 0);
    }

    #[test]
    fn margin_larger_than_shm_refuses_everything() {
        let r = admit(0, 1, 512, 512, 1024);
        assert!(matches!(
            r,
            Err(Shortfall {
                rule: Rule::Total,
                more: 0,
                ..
            })
        ));
    }

    #[test]
    fn free_below_total_limits_the_new_guest_and_the_count() {
        // The size allows two more; what is free right now allows one.
        assert_eq!(more(4096, 4096, 16896, 5000, 512), 1);
        assert_eq!(admit(4096, 4096, 16896, 5000, 512), Ok(()));
        let r = admit(4096, 4096, 16896, 4000, 512);
        assert_eq!(
            r,
            Err(Shortfall {
                rule: Rule::Free,
                needed_mib: 4608,
                available_mib: 4000,
                more: 0
            })
        );
        let text = r.err().map(|s| s.to_string()).unwrap_or_default();
        assert!(text.contains("needs 4608 MiB"), "{text}");
        assert!(text.contains("is 4000 MiB"), "{text}");
        assert!(text.contains("0 more agent(s)"), "{text}");
    }

    #[test]
    fn kata_default_memory_forms() {
        let t = |s: &str| kata_default_memory(s);
        let qemu = |body: &str| format!("[hypervisor.qemu]\npath = \"/usr/bin/qemu\"\n{body}\n");
        assert_eq!(
            t(&qemu("default_memory = 4096")),
            Ok(GuestMemory::Configured(4096))
        );
        assert_eq!(
            t(&qemu("  default_memory=3072   # MiB")),
            Ok(GuestMemory::Configured(3072))
        );
        assert_eq!(
            t(&qemu("default_memory = \"2048\"")),
            Ok(GuestMemory::Configured(2048))
        );
        assert_eq!(
            t(&qemu("#default_memory = 1\ndefault_memory = 4096")),
            Ok(GuestMemory::Configured(4096))
        );
        // Outside a hypervisor table, or only commented out: Kata's default, and it says so.
        assert_eq!(
            t("[runtime]\ndefault_memory = 9\n"),
            Ok(GuestMemory::KataDefault)
        );
        assert_eq!(
            t(&qemu("# default_memory = 4096")),
            Ok(GuestMemory::KataDefault)
        );
        assert_eq!(t(""), Ok(GuestMemory::KataDefault));
        assert_eq!(GuestMemory::KataDefault.mib(), 2048);
        assert!(t(&qemu("default_memory = 0")).is_err());
        assert!(t(&qemu("default_memory = 4G")).is_err());
    }
}
