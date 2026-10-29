//! Per-agent resource limits (ADR-023) as pure rules: what the operator set, checked once, and
//! the cgroup v2 values the adapter writes. CPU and memory bind the agent's whole sandbox on the
//! host (QEMU, virtiofsd, the shim); disk is a btrfs quota on its workspace subvolume.

use std::fmt;

/// `cpu.max`'s period, in microseconds (the kernel's default).
pub const CPU_PERIOD_US: u64 = 100_000;

/// Least host memory a sandbox needs above its guest: QEMU, virtiofsd and the shim (sizing.md,
/// Real builds: VM PSS ran ~120 MiB above the guest memory in `/dev/shm`).
pub const MIN_OVERHEAD_MIB: u64 = 128;

/// Most cores a limit may name.
const MAX_CPUS: u32 = 1024;

/// A CPU limit in hundredths of a core (150 is 1.5 cores), so it is exact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cpus(u32);

impl Cpus {
    /// Parse `2`, `1.5` or `0.25`: at most two decimals, from 0.01 to 1024 cores.
    pub fn parse(s: &str) -> Result<Self, String> {
        let bad = || format!("{s:?}: want cores such as 2 or 1.5 (0.01 to {MAX_CPUS})");
        let (whole, frac) = s.split_once('.').unwrap_or((s, ""));
        let digits = |t: &str| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit());
        if !digits(whole) || (s.contains('.') && !digits(frac)) || frac.len() > 2 {
            return Err(bad());
        }
        let whole: u32 = whole.parse().map_err(|_| bad())?;
        let frac: u32 = format!("{frac:0<2}").parse().map_err(|_| bad())?;
        let hundredths = whole.checked_mul(100).and_then(|w| w.checked_add(frac));
        match hundredths {
            Some(h) if h > 0 && h <= MAX_CPUS * 100 => Ok(Self(h)),
            _ => Err(bad()),
        }
    }

    /// The `cpu.max` line: quota and period in microseconds.
    pub fn cpu_max(self) -> String {
        let quota = u64::from(self.0) * CPU_PERIOD_US / 100;
        format!("{quota} {CPU_PERIOD_US}")
    }
}

impl fmt::Display for Cpus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (whole, frac) = (self.0 / 100, self.0 % 100);
        match frac {
            0 => write!(f, "{whole}"),
            n if n % 10 == 0 => write!(f, "{whole}.{}", n / 10),
            n => write!(f, "{whole}.{n:02}"),
        }
    }
}

/// The limits every agent boots with; `None` leaves that resource unlimited.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AgentLimits {
    /// Cores the sandbox may use (`cpu.max`).
    pub cpus: Option<Cpus>,
    /// Host memory the sandbox may use, guest RAM included (`memory.max`), MiB.
    pub memory_mib: Option<u64>,
    /// Data the workspace may hold (`btrfs qgroup limit`), GiB.
    pub disk_gib: Option<u64>,
}

impl AgentLimits {
    /// From `AC_AGENT_CPUS`, `AC_AGENT_MEMORY_MIB` and `AC_AGENT_DISK_GIB`, read through `get`.
    /// Unset or empty means no limit; zero or anything malformed is an error.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let var = |key: &str| get(key).filter(|v| !v.trim().is_empty());
        let size = |key: &str| -> Result<Option<u64>, String> {
            var(key)
                .map(|v| match v.trim().parse::<u64>() {
                    Ok(n) if n > 0 => Ok(n),
                    _ => Err(format!("{key}={v}: want a whole number above 0")),
                })
                .transpose()
        };
        let cpus = var("AC_AGENT_CPUS")
            .map(|v| Cpus::parse(v.trim()).map_err(|e| format!("AC_AGENT_CPUS={e}")))
            .transpose()?;
        Ok(Self {
            cpus,
            memory_mib: size("AC_AGENT_MEMORY_MIB")?,
            disk_gib: size("AC_AGENT_DISK_GIB")?,
        })
    }

    /// Refuse a memory limit a guest of `guest_mib` cannot boot or run under: the guest's RAM is
    /// host memory too, so the limit must cover it plus [`MIN_OVERHEAD_MIB`].
    pub fn check(&self, guest_mib: u64) -> Result<(), String> {
        let least = guest_mib + MIN_OVERHEAD_MIB;
        match self.memory_mib {
            Some(m) if m < least => Err(format!(
                "AC_AGENT_MEMORY_MIB={m} is below the guest's {guest_mib} MiB plus \
                 {MIN_OVERHEAD_MIB} MiB for QEMU and virtiofsd; set at least {least}"
            )),
            _ => Ok(()),
        }
    }

    /// Whether CPU or memory is limited, so the sandbox's cgroup is written.
    pub fn has_cgroup(&self) -> bool {
        self.cpus.is_some() || self.memory_mib.is_some()
    }

    /// What to enable in the parent's `cgroup.subtree_control`, such as `+cpu +memory`.
    pub fn controllers(&self) -> String {
        let on = [
            self.cpus.map(|_| "+cpu"),
            self.memory_mib.map(|_| "+memory"),
        ];
        on.into_iter().flatten().collect::<Vec<_>>().join(" ")
    }

    /// The cgroup files to write and their values, in order.
    pub fn cgroup_files(&self) -> Vec<(&'static str, String)> {
        let cpu = self.cpus.map(|c| ("cpu.max", c.cpu_max()));
        let memory = self
            .memory_mib
            .map(|m| ("memory.max", (m << 20).to_string()));
        [cpu, memory].into_iter().flatten().collect()
    }

    /// The workspace quota in bytes, for `btrfs qgroup limit`.
    pub fn disk_bytes(&self) -> Option<u64> {
        self.disk_gib.map(|g| g << 30)
    }
}

impl fmt::Display for AgentLimits {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let show = |v: Option<String>| v.unwrap_or_else(|| "unlimited".to_owned());
        write!(
            f,
            "cpu {}, memory {}, disk {}",
            show(self.cpus.map(|c| format!("{c} cores"))),
            show(self.memory_mib.map(|m| format!("{m} MiB"))),
            show(self.disk_gib.map(|g| format!("{g} GiB"))),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn cpus(s: &str) -> Cpus {
        Cpus::parse(s).unwrap_or_else(|e| panic!("{e}"))
    }

    fn from(pairs: &[(&str, &str)]) -> Result<AgentLimits, String> {
        let env: BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        AgentLimits::from_lookup(|k| env.get(k).cloned())
    }

    #[test]
    fn cpus_parse_and_render() {
        for (text, max, shown) in [
            ("2", "200000 100000", "2"),
            ("1.5", "150000 100000", "1.5"),
            ("0.25", "25000 100000", "0.25"),
            ("0.01", "1000 100000", "0.01"),
            ("1.50", "150000 100000", "1.5"),
            ("1024", "102400000 100000", "1024"),
        ] {
            assert_eq!(cpus(text).cpu_max(), max, "{text}");
            assert_eq!(cpus(text).to_string(), shown, "{text}");
        }
    }

    #[test]
    fn cpus_refuse_zero_junk_and_too_many() {
        for bad in [
            "",
            "0",
            "0.0",
            "0.001",
            "1.",
            ".5",
            "-1",
            "1e2",
            "two",
            "1.5.1",
            "1025",
            "99999999999",
        ] {
            assert!(Cpus::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn unset_and_empty_mean_unlimited() {
        let none = from(&[]).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(none, AgentLimits::default());
        assert!(!none.has_cgroup());
        assert!(none.cgroup_files().is_empty());
        assert_eq!(none.disk_bytes(), None);
        assert_eq!(
            none.to_string(),
            "cpu unlimited, memory unlimited, disk unlimited"
        );
        let empty = from(&[("AC_AGENT_CPUS", ""), ("AC_AGENT_MEMORY_MIB", " ")]);
        assert_eq!(empty, Ok(AgentLimits::default()));
    }

    #[test]
    fn all_three_set() {
        let l = from(&[
            ("AC_AGENT_CPUS", "2"),
            ("AC_AGENT_MEMORY_MIB", "2560"),
            ("AC_AGENT_DISK_GIB", "20"),
        ])
        .unwrap_or_else(|e| panic!("{e}"));
        assert!(l.has_cgroup());
        assert_eq!(l.controllers(), "+cpu +memory");
        assert_eq!(
            l.cgroup_files(),
            [
                ("cpu.max", "200000 100000".to_owned()),
                ("memory.max", (2560u64 * 1024 * 1024).to_string()),
            ]
        );
        assert_eq!(l.disk_bytes(), Some(20 * 1024 * 1024 * 1024));
        assert_eq!(l.to_string(), "cpu 2 cores, memory 2560 MiB, disk 20 GiB");
    }

    #[test]
    fn one_controller_alone() {
        let mem = from(&[("AC_AGENT_MEMORY_MIB", "3000")]).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(mem.controllers(), "+memory");
        assert_eq!(mem.cgroup_files().len(), 1);
        let disk = from(&[("AC_AGENT_DISK_GIB", "5")]).unwrap_or_else(|e| panic!("{e}"));
        assert!(!disk.has_cgroup());
    }

    #[test]
    fn malformed_values_name_their_variable() {
        for (key, value) in [
            ("AC_AGENT_CPUS", "lots"),
            ("AC_AGENT_MEMORY_MIB", "0"),
            ("AC_AGENT_MEMORY_MIB", "2G"),
            ("AC_AGENT_DISK_GIB", "-1"),
        ] {
            let err = from(&[(key, value)]).err().unwrap_or_default();
            assert!(err.starts_with(key), "{key}={value}: {err}");
        }
    }

    #[test]
    fn memory_must_cover_the_guest_and_overhead() {
        let at = |m: u64| AgentLimits {
            memory_mib: Some(m),
            ..AgentLimits::default()
        };
        assert!(at(2048 + MIN_OVERHEAD_MIB).check(2048).is_ok());
        assert!(at(2560).check(2048).is_ok());
        let err = at(2048).check(2048).err().unwrap_or_default();
        assert!(err.contains("set at least 2176"), "{err}");
        assert!(AgentLimits::default().check(2048).is_ok());
    }
}
