//! What the doctor knows about a host (ADR-014), and the parsers that read it from `/proc`, `/sys`,
//! `/etc/fstab`, Kata's `configuration.toml` and `gateway.env`. Pure: text in, facts out.

use std::collections::BTreeMap;

/// A fact that may not be known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fact<T> {
    /// Read from the host or the facts file.
    Known(T),
    /// Looked for and not there: a missing file, device or CPU flag.
    Absent,
    /// There, but not in the form expected, and why.
    Invalid(String),
    /// Not checked, and why: a root-only file, not in the facts file.
    Skipped(String),
}

impl<T> Fact<T> {
    /// The value, when known.
    pub fn known(&self) -> Option<&T> {
        match self {
            Self::Known(v) => Some(v),
            _ => None,
        }
    }
}

/// The CPU's hardware virtualization flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Virt {
    /// Intel VT-x.
    Vmx,
    /// AMD-V.
    Svm,
}

/// The class of the disk that holds the data directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disk {
    /// `queue/rotational` is 1: a spinning disk.
    Rotational,
    /// SSD or NVMe.
    Solid,
}

/// Whether `/dev/kvm` (when present) can be opened read-write by this process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kvm {
    /// Opened read-write.
    Usable,
    /// Present, but this process may not open it.
    NoAccess,
}

/// A tmpfs on `/dev/shm`: the running mount or the fstab line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shm {
    /// Its size in MiB (tmpfs without `size=` is half of RAM).
    pub size_mib: u64,
    /// Its other options, in order, without `size=`, `rw`, `seclabel`, `relatime` and `defaults`.
    pub options: Vec<String>,
}

/// The settings `ac-ctl tune` owns in the first `[hypervisor.*]` table of Kata's configuration,
/// plus the initrd the doctor inspects. `None`: not set, so Kata's default applies.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Kata {
    /// Guest memory in MiB (Kata's default 2048).
    pub default_memory: Option<u64>,
    /// vCPUs per guest (Kata's default 1).
    pub default_vcpus: Option<u64>,
    /// Most memory a guest can grow to, MiB (Kata's default 0: all of the host's).
    pub default_maxmemory: Option<u64>,
    /// Most vCPUs a guest can grow to (Kata's default 0: all of the host's).
    pub default_maxvcpus: Option<u64>,
    /// The guest initrd's path.
    pub initrd: Option<String>,
}

/// Owner and permission bits of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mode {
    /// Owner uid.
    pub uid: u32,
    /// Group gid.
    pub gid: u32,
    /// Permission bits (`0o600`).
    pub perm: u32,
}

/// The profiles the egress agents file attaches (ADR-016), as the proxy would read them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EgressProfiles {
    /// Distinct profiles attached to some agent, sorted.
    pub attached: Vec<String>,
    /// Of those, the ones with no file.
    pub missing: Vec<String>,
    /// Of those, the ones whose file the proxy refuses: `name: why`.
    pub refused: Vec<String>,
}

/// The agent firewall as `scripts/post-reboot-check.sh` checks it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Firewall {
    /// Rules in each of [`NFT_CHAINS`], in that order; `None` when the chain does not exist.
    pub chains: Vec<Option<u32>>,
    /// The `agents` zone as `interfaces|ports|services` (want [`AGENTS_ZONE`]).
    pub zone: String,
}

/// Each nftables chain of the agent firewall and the rules it must hold. The same list as
/// `scripts/post-reboot-check.sh` (a test keeps the two equal).
pub const NFT_CHAINS: [(&str, u32); 3] = [
    ("inet agentfw input", 3),
    ("inet agentfw forward", 3),
    ("bridge agentfw forward", 1),
];

/// firewalld's `agents` zone as the post-reboot check wants it: `acbr0` only, `3128/tcp` only.
pub const AGENTS_ZONE: &str = "acbr0|3128/tcp|";

/// `gateway.env` (or the gateway's own environment): key to value, the last assignment winning.
pub type Env = BTreeMap<String, String>;

/// Everything the rules look at. Built by the collector, or loaded from a facts file to plan
/// hardware that is not there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostFacts {
    /// `MemTotal`, MiB.
    pub mem_total_mib: u64,
    /// Online logical CPUs.
    pub cores: u32,
    /// The CPU's physical address bits.
    pub phys_bits: Fact<u32>,
    /// `vmx` or `svm` in the CPU flags.
    pub virt: Fact<Virt>,
    /// The disk under the data directory.
    pub disk: Fact<Disk>,
    /// `/dev/shm` as mounted now.
    pub shm: Fact<Shm>,
    /// MiB of `/dev/shm` in use now.
    pub shm_used_mib: Fact<u64>,
    /// The `/dev/shm` line of `/etc/fstab`.
    pub shm_fstab: Fact<Shm>,
    /// Kata's configuration.
    pub kata: Fact<Kata>,
    /// The gateway's settings.
    pub gateway: Fact<Env>,
    /// `/dev/kvm`.
    pub kvm: Fact<Kvm>,
    /// Whether the guest initrd loads the NIC modules (the `fix-kata-initrd.sh` drop-in).
    pub initrd_net: Fact<bool>,
    /// The agent firewall.
    pub firewall: Fact<Firewall>,
    /// The tokens file.
    pub tokens_mode: Fact<Mode>,
    /// The egress allowlist.
    pub allowlist_mode: Fact<Mode>,
    /// The egress agents file (ADR-016).
    pub egress_agents_mode: Fact<Mode>,
    /// The profiles it attaches.
    pub egress_profiles: Fact<EgressProfiles>,
}

impl HostFacts {
    /// A host of `mem_total_mib` and `cores` about which nothing else is known, each other fact
    /// skipped for `why`.
    pub fn bare(mem_total_mib: u64, cores: u32, why: &str) -> Self {
        fn s<T>(why: &str) -> Fact<T> {
            Fact::Skipped(why.to_owned())
        }
        Self {
            mem_total_mib,
            cores,
            phys_bits: s(why),
            virt: s(why),
            disk: s(why),
            shm: s(why),
            shm_used_mib: s(why),
            shm_fstab: s(why),
            kata: s(why),
            gateway: s(why),
            kvm: s(why),
            initrd_net: s(why),
            firewall: s(why),
            tokens_mode: s(why),
            allowlist_mode: s(why),
            egress_agents_mode: s(why),
            egress_profiles: s(why),
        }
    }
}

/// `MemTotal` from `/proc/meminfo`, in MiB (rounded down).
pub fn mem_total_mib(meminfo: &str) -> Option<u64> {
    let rest = meminfo.lines().find_map(|l| l.strip_prefix("MemTotal:"))?;
    let mut parts = rest.split_whitespace();
    let n: u64 = parts.next()?.parse().ok()?;
    match parts.next() {
        Some("kB") | None => Some(n / 1024),
        Some(_) => None,
    }
}

/// How many CPUs a kernel CPU list (`/sys/devices/system/cpu/online`: `0-3`, `0,2-5,7`) names.
pub fn cpu_count(list: &str) -> Option<u32> {
    let mut n = 0u32;
    for part in list.trim().split(',') {
        let (lo, hi) = part.split_once('-').unwrap_or((part, part));
        let (lo, hi): (u32, u32) = (lo.trim().parse().ok()?, hi.trim().parse().ok()?);
        n = n.checked_add(hi.checked_sub(lo)?.checked_add(1)?)?;
    }
    (n > 0).then_some(n)
}

/// Physical address bits from the first `address sizes` line of `/proc/cpuinfo`
/// (`36 bits physical, 48 bits virtual`).
pub fn phys_bits(cpuinfo: &str) -> Option<u32> {
    let line = cpuinfo.lines().find(|l| l.starts_with("address sizes"))?;
    let (_, value) = line.split_once(':')?;
    let (bits, rest) = value.trim().split_once(' ')?;
    rest.starts_with("bits physical")
        .then(|| bits.parse().ok())
        .flatten()
}

/// The virtualization flag in the first `flags` line of `/proc/cpuinfo`; `None` when the CPU has
/// none (or the firmware hides it).
pub fn virt(cpuinfo: &str) -> Option<Virt> {
    let line = cpuinfo.lines().find(|l| l.starts_with("flags"))?;
    let (_, flags) = line.split_once(':')?;
    flags.split_whitespace().find_map(|f| match f {
        "vmx" => Some(Virt::Vmx),
        "svm" => Some(Virt::Svm),
        _ => None,
    })
}

/// A tmpfs `size=` value in MiB: bytes, or with a `k`, `m`, `g` suffix (either case), or a
/// percentage of `mem_total_mib`.
pub fn size_mib(value: &str, mem_total_mib: u64) -> Option<u64> {
    let v = value.trim();
    let (digits, unit) = v.split_at(v.find(|c: char| !c.is_ascii_digit()).unwrap_or(v.len()));
    let n: u64 = digits.parse().ok()?;
    match unit.to_ascii_lowercase().as_str() {
        "" => Some(n >> 20),
        "k" => Some(n >> 10),
        "m" => Some(n),
        "g" => n.checked_mul(1024),
        "%" => Some(mem_total_mib.saturating_mul(n) / 100),
        _ => None,
    }
}

/// A `/dev/shm` options field: its size (half of RAM without `size=`) and the options that say
/// something, as [`Shm::options`] keeps them.
fn shm_of(options: &str, mem_total_mib: u64) -> Option<Shm> {
    let mut size = mem_total_mib / 2;
    let mut kept = Vec::new();
    for o in options.split(',') {
        match o.split_once('=') {
            Some(("size", v)) => size = size_mib(v, mem_total_mib)?,
            _ if matches!(o, "rw" | "seclabel" | "relatime" | "defaults" | "") => {}
            _ => kept.push(o.to_owned()),
        }
    }
    Some(Shm {
        size_mib: size,
        options: kept,
    })
}

/// The `/dev/shm` entry of a mount table (`/proc/self/mounts` or `/etc/fstab`): the last active
/// line whose mount point is `/dev/shm`. `Err` when that line cannot be read.
pub fn shm_entry(table: &str, mem_total_mib: u64) -> Result<Option<Shm>, String> {
    let Some(line) = table
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.starts_with('#') && l.split_whitespace().nth(1) == Some("/dev/shm"))
    else {
        return Ok(None);
    };
    let options = line.split_whitespace().nth(3).unwrap_or("defaults");
    shm_of(options, mem_total_mib)
        .map(Some)
        .ok_or_else(|| format!("cannot read the size in {line:?}"))
}

/// A systemd `EnvironmentFile`: `KEY=value` lines; lines starting with `#` or `;` are comments;
/// whitespace around key and value is dropped, as is one pair of matching quotes around the value;
/// a later assignment of a key replaces an earlier one. `#` inside a value is part of it.
pub fn env_file(text: &str) -> Env {
    let mut env = Env::new();
    for line in text.lines().map(str::trim) {
        if line.starts_with(['#', ';']) {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        let unquoted = ['"', '\'']
            .iter()
            .find_map(|q| value.strip_prefix(*q)?.strip_suffix(*q))
            .unwrap_or(value);
        env.insert(key.trim().to_owned(), unquoted.to_owned());
    }
    env
}

/// The keys of [`Kata`], as they appear in the configuration.
pub const KATA_KEYS: [&str; 5] = [
    "default_memory",
    "default_vcpus",
    "default_maxmemory",
    "default_maxvcpus",
    "initrd",
];

/// The line range (start, end) of the first `[hypervisor.*]` table: from its header line to the
/// next table header or the end.
pub fn hypervisor_table(lines: &[&str]) -> Option<(usize, usize)> {
    let start = lines
        .iter()
        .position(|l| l.trim().starts_with("[hypervisor."))?;
    let end = lines[start + 1..]
        .iter()
        .position(|l| l.trim().starts_with('['))
        .map_or(lines.len(), |n| start + 1 + n);
    Some((start, end))
}

/// An active `key = value` line: the key and the value without its comment, quotes or spaces.
pub fn kata_assignment(line: &str) -> Option<(&str, &str)> {
    let line = line.split('#').next().unwrap_or_default().trim();
    let (key, value) = line.split_once('=')?;
    Some((key.trim(), value.trim().trim_matches('"')))
}

/// [`Kata`] from a Kata `configuration.toml`: the first `[hypervisor.*]` table only. Takes
/// `key = 4096`, `key=4096 # note` and `key = "4096"`; commented-out lines are not settings. A
/// number that is not a non-negative integer, or a key set twice in the table (TOML, and so
/// Kata, refuse that), is an error.
pub fn kata(toml: &str) -> Result<Kata, String> {
    let lines: Vec<&str> = toml.lines().collect();
    let mut kata = Kata::default();
    let Some((start, end)) = hypervisor_table(&lines) else {
        return Ok(kata);
    };
    for line in &lines[start + 1..end] {
        let Some((key, value)) = kata_assignment(line) else {
            continue;
        };
        let number = || {
            value
                .parse::<u64>()
                .map_err(|_| format!("{key} = {value:?} is not a whole number"))
        };
        let (slot, parsed) = match key {
            "default_memory" => (&mut kata.default_memory, number()?),
            "default_vcpus" => (&mut kata.default_vcpus, number()?),
            "default_maxmemory" => (&mut kata.default_maxmemory, number()?),
            "default_maxvcpus" => (&mut kata.default_maxvcpus, number()?),
            "initrd" if kata.initrd.is_some() => return Err("initrd is set twice".into()),
            "initrd" => {
                kata.initrd = Some(value.to_owned());
                continue;
            }
            _ => continue,
        };
        if slot.replace(parsed).is_some() {
            return Err(format!("{key} is set twice"));
        }
    }
    Ok(kata)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meminfo_forms() {
        let real = "MemTotal:       24483408 kB\nMemFree:         1234 kB\n";
        assert_eq!(mem_total_mib(real), Some(23909));
        assert_eq!(mem_total_mib("MemTotal: 2048"), Some(2));
        assert_eq!(mem_total_mib("MemFree: 1 kB\n"), None);
        assert_eq!(mem_total_mib("MemTotal: x kB"), None);
        assert_eq!(mem_total_mib("MemTotal: 1 MB"), None);
        assert_eq!(mem_total_mib(""), None);
    }

    #[test]
    fn cpu_lists() {
        assert_eq!(cpu_count("0-3\n"), Some(4));
        assert_eq!(cpu_count("0"), Some(1));
        assert_eq!(cpu_count("0,2-5,7"), Some(6));
        assert_eq!(cpu_count("0-15"), Some(16));
        for bad in ["", "3-1", "a-b", "0-"] {
            assert_eq!(cpu_count(bad), None, "{bad}");
        }
    }

    const INTEL: &str = "processor\t: 0\nvendor_id\t: GenuineIntel\nflags\t\t: fpu vme sse2 vmx \
                         smx est\naddress sizes\t: 36 bits physical, 48 bits virtual\n\nprocessor\t: \
                         1\naddress sizes\t: 40 bits physical, 48 bits virtual\n";
    const AMD: &str = "vendor_id\t: AuthenticAMD\nflags\t\t: fpu svm sse4a\naddress sizes\t: 48 \
                       bits physical, 48 bits virtual\n";

    #[test]
    fn cpuinfo_forms() {
        assert_eq!((phys_bits(INTEL), virt(INTEL)), (Some(36), Some(Virt::Vmx)));
        assert_eq!((phys_bits(AMD), virt(AMD)), (Some(48), Some(Virt::Svm)));
        // A VM without nested virtualization: no flag. No address line: unknown.
        assert_eq!(virt("flags\t: fpu sse2 hypervisor\n"), None);
        assert_eq!(phys_bits("flags\t: vmx\n"), None);
        assert_eq!(phys_bits("address sizes\t: many bits physical\n"), None);
        // `vmx` must be a whole flag, not part of one.
        assert_eq!(virt("flags\t: vmxnope\n"), None);
    }

    #[test]
    fn sizes() {
        assert_eq!(size_mib("16094208k", 0), Some(15717));
        assert_eq!(size_mib("15717M", 0), Some(15717));
        assert_eq!(size_mib("15g", 0), Some(15360));
        assert_eq!(size_mib("1073741824", 0), Some(1024));
        assert_eq!(size_mib("50%", 23909), Some(11954));
        assert_eq!(size_mib("12x", 0), None);
        assert_eq!(size_mib("M", 0), None);
    }

    #[test]
    fn shm_from_mounts_and_fstab() {
        let mounts = "proc /proc proc rw 0 0\ntmpfs /dev/shm tmpfs \
                      rw,seclabel,nosuid,nodev,size=16094208k,inode64,usrquota 0 0\n";
        let want = Shm {
            size_mib: 15717,
            options: ["nosuid", "nodev", "inode64", "usrquota"]
                .map(String::from)
                .to_vec(),
        };
        assert_eq!(shm_entry(mounts, 23909), Ok(Some(want.clone())));
        let fstab = include_str!("../../../../deploy/dev-shm.fstab");
        assert_eq!(shm_entry(fstab, 23909), Ok(Some(want)));
        // Commented out, absent, the default size, the last line winning, an unreadable size.
        assert_eq!(
            shm_entry("#tmpfs /dev/shm tmpfs size=1G 0 0\n", 8000),
            Ok(None)
        );
        assert_eq!(shm_entry("", 8000), Ok(None));
        let d = shm_entry("tmpfs /dev/shm tmpfs defaults 0 0", 8000);
        assert_eq!(d.map(|s| s.map(|s| s.size_mib)), Ok(Some(4000)));
        let two = "tmpfs /dev/shm tmpfs size=1G 0 0\ntmpfs /dev/shm tmpfs size=2G 0 0\n";
        assert_eq!(
            shm_entry(two, 0).map(|s| s.map(|s| s.size_mib)),
            Ok(Some(2048))
        );
        assert!(shm_entry("tmpfs /dev/shm tmpfs size=lots 0 0", 0).is_err());
        let no_opts = shm_entry("tmpfs /dev/shm tmpfs", 8000);
        assert_eq!(no_opts.map(|s| s.map(|s| s.size_mib)), Ok(Some(4000)));
    }

    #[test]
    fn env_file_forms() {
        let env = env_file(
            "# comment\n; also a comment\nAC_BIND=127.0.0.1:8780\n  AC_JOB_CPU_PSI_MAX = 20  \n\
             AC_IMAGE=\"localhost/agent-shell:dev\"\nAC_Q='x y'\nAC_HASH=a#b\n#AC_NETWORK=1\n\
             AC_BIND=10.0.0.1:1\nnot a line\nAC_EMPTY=\n",
        );
        let get = |k: &str| env.get(k).map(String::as_str);
        assert_eq!(get("AC_BIND"), Some("10.0.0.1:1"));
        assert_eq!(get("AC_JOB_CPU_PSI_MAX"), Some("20"));
        assert_eq!(get("AC_IMAGE"), Some("localhost/agent-shell:dev"));
        assert_eq!(get("AC_Q"), Some("x y"));
        assert_eq!(get("AC_HASH"), Some("a#b"));
        assert_eq!(get("AC_EMPTY"), Some(""));
        assert_eq!(get("AC_NETWORK"), None);
        assert_eq!(env.len(), 6);
        // The shipped file parses, with its comments ignored.
        let shipped = env_file(include_str!("../../../../deploy/gateway.env"));
        assert_eq!(shipped.get("AC_NETWORK").map(String::as_str), Some("1"));
        assert!(!shipped.contains_key("AC_TOKENS"));
    }

    fn qemu(body: &str) -> String {
        format!("[runtime]\ndefault_memory = 9\n[hypervisor.qemu]\npath = \"/usr/bin/qemu\"\n{body}\n[agent.kata]\ndefault_vcpus = 7\n")
    }

    #[test]
    fn kata_forms() {
        let k = kata(&qemu(
            "initrd = \"/var/cache/kata-containers/kata-containers-initrd.img\"\n\
             # default_memory = 1\ndefault_memory = 2048\n  default_vcpus=2 # two\n\
             default_maxmemory = \"8192\"\ndefault_maxvcpus = 0",
        ));
        assert_eq!(
            k,
            Ok(Kata {
                default_memory: Some(2048),
                default_vcpus: Some(2),
                default_maxmemory: Some(8192),
                default_maxvcpus: Some(0),
                initrd: Some("/var/cache/kata-containers/kata-containers-initrd.img".into()),
            })
        );
        // Missing keys are Kata's defaults; keys outside the hypervisor table do not count.
        assert_eq!(kata(&qemu("#default_vcpus = 4")), Ok(Kata::default()));
        assert_eq!(kata(""), Ok(Kata::default()));
        assert_eq!(
            kata("[runtime]\ndefault_memory = 4096\n"),
            Ok(Kata::default())
        );
        // Only the first hypervisor table.
        let two = "[hypervisor.qemu]\ndefault_vcpus = 2\n[hypervisor.clh]\ndefault_vcpus = 4\n";
        assert_eq!(kata(two).map(|k| k.default_vcpus), Ok(Some(2)));
        // Errors: bad numbers, duplicates.
        assert!(kata(&qemu("default_memory = 4G")).is_err());
        assert!(kata(&qemu("default_vcpus = -1")).is_err());
        let dup = kata(&qemu("default_memory = 2048\ndefault_memory = 4096"));
        assert_eq!(dup, Err("default_memory is set twice".into()));
        assert!(kata(&qemu("initrd = \"a\"\ninitrd = \"b\"")).is_err());
    }

    #[test]
    fn post_reboot_check_uses_the_same_firewall_list() {
        let script = include_str!("../../../../scripts/post-reboot-check.sh");
        for (chain, rules) in NFT_CHAINS {
            assert!(script.contains(&format!("\"{chain} {rules}\"")), "{chain}");
        }
        assert!(script.contains(&format!("\"{AGENTS_ZONE}\"")));
    }
}
