//! Recommended settings for a host, from its facts and a workload profile (ADR-014). Every rule
//! cites what it rests on in docs/sizing.md or docs/install.md, and says whether its figure was
//! measured on a host of this shape (`measured`) or carried over by a rule (`extrapolated`).
//! Pure: [`HostFacts`] in, [`Finding`]s out.

use super::facts::{
    Disk, Env, Fact, HostFacts, Kata, Kvm, Mode, Shm, Virt, AGENTS_ZONE, NFT_CHAINS,
};

/// The measured host (BaileyServ, i5-3550): every figure in docs/sizing.md comes from it.
pub const MEASURED_CORES: u32 = 4;
/// Its `MemTotal`, MiB.
pub const MEASURED_MEM_TOTAL_MIB: u64 = 23909;
/// A host within this many MiB of [`MEASURED_MEM_TOTAL_MIB`] counts as the measured RAM size.
pub const MEASURED_MEM_SLACK_MIB: u64 = 2048;
/// The physical address width at which `default_maxmemory = 0` failed (install.md, note 3).
pub const SHORT_PHYS_BITS: u32 = 36;
/// The cap that fixed it.
pub const SHORT_PHYS_MAXMEMORY: u64 = 8192;
/// Below this `MemTotal` the host is too small for agents.
pub const MIN_MEM_TOTAL_MIB: u64 = 8192;
/// Admission's default margin (`AC_SHM_MARGIN_MIB`, ADR-012).
pub const SHM_MARGIN_MIB: u64 = 512;
/// The job gate's default CPU PSI limit (ADR-013).
pub const DEFAULT_CPU_PSI_MAX: f64 = 40.0;
/// The limit for the measured shape: two 2-vCPU builds peaked at 23.7 (sizing.md).
pub const MEASURED_CPU_PSI_MAX: f64 = 20.0;
/// Kata's defaults for keys the configuration does not set.
pub const KATA_DEFAULT_VCPUS: u64 = 1;

/// What the host is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// Mostly waiting on the model API.
    Idle,
    /// Builds and tests of this repo's size.
    Build,
    /// Bigger builds than were measured.
    Heavy,
}

impl Profile {
    /// `idle`, `build` or `heavy`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "idle" => Some(Self::Idle),
            "build" => Some(Self::Build),
            "heavy" => Some(Self::Heavy),
            _ => None,
        }
    }

    /// Its name, as [`Self::parse`] takes it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Build => "build",
            Self::Heavy => "heavy",
        }
    }

    /// Guest memory (`default_memory`) for this profile, MiB.
    pub fn guest_mib(self) -> u64 {
        match self {
            Self::Idle | Self::Build => 2048,
            Self::Heavy => 4096,
        }
    }
}

/// How a finding stands. Ordered: the worst status decides the doctor's exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Status {
    /// As recommended.
    Ok,
    /// Worth knowing; nothing to change.
    Info,
    /// Differs from the recommendation.
    Warn,
    /// Agents cannot run, or will fail.
    Error,
}

impl Status {
    /// `ok`, `info`, `warn` or `error`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

/// Where a recommendation's figure comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    /// Reproduces what docs/sizing.md measured on the i5-3550 host, on a host of that shape.
    Measured,
    /// Derived from a rule; not measured on this shape of hardware.
    Extrapolated,
}

impl Confidence {
    /// `measured` or `extrapolated`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Measured => "measured",
            Self::Extrapolated => "extrapolated",
        }
    }
}

/// An edit `ac-ctl tune` can make for a finding.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    /// Set a key in the first `[hypervisor.*]` table of Kata's configuration.
    Kata(&'static str, u64),
    /// Set a key in `gateway.env`.
    Env(&'static str, String),
    /// Set `size=` of the `/dev/shm` line in `/etc/fstab` (adding the line if there is none).
    ShmFstab(u64),
}

/// One rule's verdict on this host.
#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    /// Stable rule id (`kata.default_memory`).
    pub rule: &'static str,
    /// What it is about (`default_memory`).
    pub setting: String,
    /// What the host has.
    pub current: String,
    /// What the rule recommends.
    pub recommended: String,
    /// How it stands.
    pub status: Status,
    /// Where the recommendation's figure comes from.
    pub confidence: Confidence,
    /// One line: why, with its source.
    pub reason: String,
    /// What `tune` would edit, if this is a setting it owns.
    pub change: Option<Change>,
}

impl Finding {
    /// Whether this is a setting (as opposed to a prerequisite or information).
    pub fn is_setting(&self) -> bool {
        !self.rule.starts_with("prereq.") && !self.rule.starts_with("disk.")
    }
}

/// What the recommended settings allow on this host.
#[derive(Debug, Clone, PartialEq)]
pub struct Forecast {
    /// Guest memory, MiB.
    pub guest_mib: u64,
    /// vCPUs per guest.
    pub vcpus: u64,
    /// `/dev/shm`, MiB.
    pub shm_mib: u64,
    /// Admission's margin, MiB.
    pub margin_mib: u64,
    /// Admission's ceiling: floor((`/dev/shm` − margin) / guest).
    pub max_agents: u64,
    /// floor(cores / vCPUs).
    pub builds_at_once: u64,
    /// Measured on this shape, or extrapolated.
    pub confidence: Confidence,
    /// What a boot costs on this disk.
    pub boot: &'static str,
}

/// A rule, for `ac-ctl doctor --explain`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuleDoc {
    /// Its id.
    pub id: &'static str,
    /// The rule.
    pub rule: &'static str,
    /// What it rests on.
    pub evidence: &'static str,
    /// The manual fix, for what `tune` does not edit.
    pub fix: &'static str,
}

/// Every rule [`recommend`] applies.
pub const RULES: &[RuleDoc] = &[
    RuleDoc {
        id: "shm.size",
        rule: "/dev/shm = MemTotal - reserve; reserve 8 GiB when MemTotal >= 16 GiB, else MemTotal/2; \
               error below 8 GiB MemTotal; ok within 1%",
        evidence: "sizing.md (Concurrency staircase: /dev/shm filled at N=6 and 4 VMs died with 9.6 GiB \
                   still available; ADR-012 resize to 15717 MiB on 24 GiB)",
        fix: "ac-ctl tune --apply (fstab), then sudo mount -o remount,size=<N>M /dev/shm",
    },
    RuleDoc {
        id: "kata.default_maxmemory",
        rule: "8192 when the CPU has 36 physical address bits or fewer; otherwise Kata's default (0)",
        evidence: "install.md Fedora note 3: QEMU 'phys-bits too low (36)' with the default on the \
                   i5-3550; 8192 fixed it",
        fix: "ac-ctl tune --apply",
    },
    RuleDoc {
        id: "kata.default_vcpus",
        rule: "2 when cores >= 4, else 1; never above 2 (only 4 cores were measured)",
        evidence: "sizing.md (Real builds, Builds at once): 2 vCPUs ~40% faster; 3 builds at 2 vCPUs on 4 \
                   cores 7% slower than at 1",
        fix: "ac-ctl tune --apply",
    },
    RuleDoc {
        id: "kata.default_memory",
        rule: "idle 2048, build 2048, heavy 4096",
        evidence: "sizing.md (Real builds): this repo's builds peak at 468 MiB used in a 2048 MiB guest; \
                   Claude Code ~475 MiB; nothing bigger was measured",
        fix: "ac-ctl tune --apply",
    },
    RuleDoc {
        id: "gateway.vm_memory",
        rule: "AC_VM_MEMORY_MIB, when set, equals default_memory",
        evidence: "ADR-012: admission counts AC_VM_MEMORY_MIB instead of default_memory",
        fix: "ac-ctl tune --apply",
    },
    RuleDoc {
        id: "gateway.max_running_jobs",
        rule: "AC_MAX_RUNNING_JOBS = cores (the default); warn above",
        evidence: "ADR-013 and sizing.md (Jobs at once): API-bound jobs use ~5% of a core each",
        fix: "ac-ctl tune --apply",
    },
    RuleDoc {
        id: "gateway.cpu_psi_max",
        rule: "AC_JOB_CPU_PSI_MAX = 20 on 4 cores with 2-vCPU guests (keeps builds at 2); else the \
               default 40, uncalibrated",
        evidence: "sizing.md (Builds at once): 2 builds at 2 vCPUs peaked at CPU PSI 23.7, 3 at 59.9; the \
                   gate at 20 itself is not yet exercised by a real-build burst (roadmap)",
        fix: "ac-ctl tune --apply; calibrate other hosts with scripts/real-build-bench.sh",
    },
    RuleDoc {
        id: "disk.class",
        rule: "information only",
        evidence: "sizing.md (Real builds, Disk class): on the rotational disk a cold boot took 18-19 s \
                   against 3.5 s warm, a cold build 38% longer; warm builds are CPU-bound",
        fix: "-",
    },
    RuleDoc {
        id: "prereq.kvm",
        rule: "/dev/kvm exists and opens read-write",
        evidence: "install.md Phase 0",
        fix: "enable VT-x / AMD-V in the firmware (install.md Fedora note 1); add the user to group kvm",
    },
    RuleDoc {
        id: "prereq.virt",
        rule: "the CPU flags include vmx or svm",
        evidence: "install.md Phase 0 and Fedora note 1",
        fix: "enable VT-x / AMD-V in the firmware, then cold-boot (install.md Fedora note 1)",
    },
    RuleDoc {
        id: "prereq.kata_config",
        rule: "Kata's configuration exists and parses",
        evidence: "install.md Fedora note 3",
        fix: "sudo mkdir -p /etc/kata-containers && sudo cp \
              /usr/share/kata-containers/defaults/configuration.toml /etc/kata-containers/configuration.toml",
    },
    RuleDoc {
        id: "prereq.initrd_net",
        rule: "the guest initrd has the kata-agent drop-in that loads virtio_net",
        evidence: "install.md Fedora note 4; scripts/fix-kata-initrd.sh",
        fix: "sudo scripts/fix-kata-initrd.sh",
    },
    RuleDoc {
        id: "prereq.shm_fstab",
        rule: "/etc/fstab has a /dev/shm line, and the running mount matches it",
        evidence: "install.md Fedora note 9",
        fix: "ac-ctl tune --apply adds the line; sudo mount -o remount,size=<N>M /dev/shm applies it now",
    },
    RuleDoc {
        id: "prereq.nft",
        rule: "the agentfw chains hold the rules scripts/post-reboot-check.sh counts",
        evidence: "install.md Host services (ADR-007)",
        fix: "sudo install -D -m 0644 deploy/agentfw.nft /etc/agent-cloud/agentfw.nft && sudo systemctl \
              enable --now ac-agentfw",
    },
    RuleDoc {
        id: "prereq.firewalld",
        rule: "firewalld zone agents: interface acbr0, port 3128/tcp, nothing else",
        evidence: "install.md Host services",
        fix: "sudo install -m 0644 deploy/firewalld-agents.xml /etc/firewalld/zones/agents.xml && sudo \
              firewall-cmd --reload",
    },
    RuleDoc {
        id: "prereq.tokens_mode",
        rule: "/etc/agent-cloud/tokens is root:root 0600",
        evidence: "install.md API tokens (ADR-011): the gateway refuses anything else",
        fix: "sudo chown root:root /etc/agent-cloud/tokens && sudo chmod 0600 /etc/agent-cloud/tokens",
    },
    RuleDoc {
        id: "prereq.allowlist_mode",
        rule: "/etc/agent-cloud/egress-allow.txt is root:root 0600",
        evidence: "install.md Egress proxy (ADR-007)",
        fix: "sudo chown root:root /etc/agent-cloud/egress-allow.txt && sudo chmod 0600 \
              /etc/agent-cloud/egress-allow.txt",
    },
];

/// The manual fix for rule `id`.
pub fn fix(id: &str) -> &'static str {
    RULES.iter().find(|r| r.id == id).map_or("-", |r| r.fix)
}

/// The recommended `/dev/shm` size for `mem_total_mib`; `None` below [`MIN_MEM_TOTAL_MIB`].
pub fn shm_target(mem_total_mib: u64) -> Option<u64> {
    let reserve = if mem_total_mib >= 16384 {
        8192
    } else {
        mem_total_mib / 2
    };
    (mem_total_mib >= MIN_MEM_TOTAL_MIB).then(|| mem_total_mib - reserve)
}

/// vCPUs per guest for `cores`.
pub fn vcpus_target(cores: u32) -> u64 {
    if cores >= 4 {
        2
    } else {
        1
    }
}

/// The job gate's CPU PSI limit for this shape, and whether it was measured.
pub fn psi_target(cores: u32) -> (f64, Confidence) {
    if cores == MEASURED_CORES && vcpus_target(cores) == 2 {
        (MEASURED_CPU_PSI_MAX, Confidence::Measured)
    } else {
        (DEFAULT_CPU_PSI_MAX, Confidence::Extrapolated)
    }
}

fn measured_if(yes: bool) -> Confidence {
    if yes {
        Confidence::Measured
    } else {
        Confidence::Extrapolated
    }
}

fn measured_ram(f: &HostFacts) -> bool {
    f.mem_total_mib.abs_diff(MEASURED_MEM_TOTAL_MIB) <= MEASURED_MEM_SLACK_MIB
}

fn finding(rule: &'static str, setting: &str, current: String, recommended: String) -> Finding {
    Finding {
        rule,
        setting: setting.to_owned(),
        current,
        recommended,
        status: Status::Ok,
        confidence: Confidence::Measured,
        reason: String::new(),
        change: None,
    }
}

impl Finding {
    fn status(mut self, status: Status) -> Self {
        self.status = status;
        self
    }

    fn confidence(mut self, confidence: Confidence) -> Self {
        self.confidence = confidence;
        self
    }

    fn reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = reason.into();
        self
    }

    fn change(mut self, change: Change) -> Self {
        self.change = Some(change);
        self
    }
}

/// What a fact that is not known says in the `current` column.
fn unknown<T>(fact: &Fact<T>) -> String {
    match fact {
        Fact::Known(_) => String::new(),
        Fact::Absent => "absent".to_owned(),
        Fact::Invalid(why) => format!("invalid: {why}"),
        Fact::Skipped(why) => format!("skipped: {why}"),
    }
}

/// Every finding for this host and profile, in [`RULES`] order.
pub fn recommend(f: &HostFacts, p: Profile) -> Vec<Finding> {
    let mut out = vec![shm_size(f)];
    out.extend(kata_settings(f, p));
    out.extend(gateway_settings(f, p));
    out.push(disk(f));
    out.extend(prerequisites(f));
    out
}

/// The settings (not prerequisites) at warn or error, as `setting current -> recommended`
/// joined by `, `; `None` when every setting is as recommended. For the gateway's startup log.
pub fn drift(findings: &[Finding]) -> Option<String> {
    let differ: Vec<String> = findings
        .iter()
        .filter(|f| f.is_setting() && f.status >= Status::Warn)
        .map(|f| format!("{} {} -> {}", f.setting, f.current, f.recommended))
        .collect();
    (!differ.is_empty()).then(|| differ.join(", "))
}

/// The worst status among `findings` (`Ok` for none).
pub fn worst(findings: &[Finding]) -> Status {
    findings
        .iter()
        .map(|f| f.status)
        .max()
        .unwrap_or(Status::Ok)
}

fn shm_size(f: &HostFacts) -> Finding {
    let reserve = if f.mem_total_mib >= 16384 {
        "8 GiB"
    } else {
        "MemTotal/2"
    };
    let configured = match (&f.shm_fstab, &f.shm) {
        (Fact::Known(s), _) | (_, Fact::Known(s)) => Some(s.size_mib),
        _ => None,
    };
    let current = configured.map_or_else(|| unknown(&f.shm), |c| format!("{c} MiB"));
    let Some(target) = shm_target(f.mem_total_mib) else {
        return finding("shm.size", "/dev/shm size", current, "-".into())
            .status(Status::Error)
            .confidence(Confidence::Extrapolated)
            .reason(format!(
                "MemTotal {} MiB is below 8 GiB: too small for agents (sizing.md, What this implies \
                 for hardware)",
                f.mem_total_mib
            ));
    };
    let base = finding(
        "shm.size",
        "/dev/shm size",
        current,
        format!("{target} MiB"),
    )
    .confidence(measured_if(measured_ram(f)))
    .reason(format!(
        "MemTotal {} MiB - {reserve} host reserve; guests live in /dev/shm and die when it fills \
             (sizing.md, staircase N=6; install.md note 9)",
        f.mem_total_mib
    ));
    match configured {
        Some(c) if c.abs_diff(target) * 100 <= target => base,
        Some(_) => base.status(Status::Warn).change(Change::ShmFstab(target)),
        None => base.status(Status::Info),
    }
}

/// A Kata key's value for the `current` column: as set, or Kata's default.
fn kata_current(value: Option<u64>, default: &str) -> String {
    value.map_or_else(|| format!("unset ({default})"), |v| v.to_string())
}

/// Findings judged on a fact that is not known: the recommendation stands, the current value
/// shows why it is unknown, and nothing is changed or warned about.
fn unjudged(findings: Vec<Finding>, why: &str) -> Vec<Finding> {
    findings
        .into_iter()
        .map(|f| Finding {
            current: why.to_owned(),
            status: Status::Info,
            change: None,
            ..f
        })
        .collect()
}

fn kata_settings(f: &HostFacts, p: Profile) -> Vec<Finding> {
    let none = Kata::default();
    let kata = f.kata.known().unwrap_or(&none);
    let out = vec![
        maxmemory(f, kata.default_maxmemory),
        vcpus(f, kata.default_vcpus),
        memory(p, kata.default_memory),
    ];
    match &f.kata {
        Fact::Known(_) => out,
        other => unjudged(out, &unknown(other)),
    }
}

fn maxmemory(f: &HostFacts, value: Option<u64>) -> Finding {
    let current = kata_current(value, "0: all host RAM");
    let set = value.filter(|v| *v > 0);
    let Some(bits) = f.phys_bits.known().copied() else {
        return finding(
            "kata.default_maxmemory",
            "default_maxmemory",
            current,
            "-".into(),
        )
        .status(Status::Info)
        .confidence(Confidence::Extrapolated)
        .reason(format!("physical address bits {}", unknown(&f.phys_bits)));
    };
    if bits > SHORT_PHYS_BITS {
        let base = finding(
            "kata.default_maxmemory",
            "default_maxmemory",
            current,
            "Kata's default".into(),
        )
        .confidence(Confidence::Extrapolated);
        return match set {
            None => base.reason(format!(
                "{bits} physical address bits: the 36-bit QEMU failure does not apply (install.md \
                 note 3)"
            )),
            Some(_) => base.status(Status::Info).reason(format!(
                "not needed at {bits} physical address bits; the cap still bounds one agent's \
                 growth (install.md note 3)"
            )),
        };
    }
    let base = finding(
        "kata.default_maxmemory",
        "default_maxmemory",
        current,
        SHORT_PHYS_MAXMEMORY.to_string(),
    )
    .confidence(measured_if(bits == SHORT_PHYS_BITS))
    .change(Change::Kata("default_maxmemory", SHORT_PHYS_MAXMEMORY));
    match set {
        Some(SHORT_PHYS_MAXMEMORY) => Finding {
            change: None,
            ..base.reason(format!(
                "{bits} physical address bits: QEMU fails with 'phys-bits too low' without a cap \
                 (install.md note 3)"
            ))
        },
        None => base.status(Status::Error).reason(format!(
            "{bits} physical address bits: QEMU fails with 'phys-bits too low (36)' at Kata's \
             default (install.md note 3)"
        )),
        Some(_) => base.status(Status::Warn).reason(format!(
            "{bits} physical address bits: only 8192 was verified (install.md note 3)"
        )),
    }
}

fn vcpus(f: &HostFacts, value: Option<u64>) -> Finding {
    let target = vcpus_target(f.cores);
    let effective = value.unwrap_or(KATA_DEFAULT_VCPUS);
    let base = finding(
        "kata.default_vcpus",
        "default_vcpus",
        kata_current(value, "Kata's default 1"),
        target.to_string(),
    )
    .confidence(measured_if(f.cores == MEASURED_CORES));
    let why = format!(
        "{} cores: 2 vCPUs build ~40% faster, 3 builds at 2 vCPUs on 4 cores 7% slower than at 1; \
         above 2 not calibrated (sizing.md, Builds at once)",
        f.cores
    );
    if effective == target {
        return base.reason(why);
    }
    base.status(Status::Warn)
        .reason(why)
        .change(Change::Kata("default_vcpus", target))
}

fn memory(p: Profile, value: Option<u64>) -> Finding {
    let target = p.guest_mib();
    let effective = value.unwrap_or(crate::capacity::KATA_DEFAULT_MEMORY_MIB);
    let (confidence, why) = match p {
        Profile::Idle => (
            Confidence::Measured,
            "Claude Code peaks at ~475 MiB used in the guest (sizing.md, Claude Code)",
        ),
        Profile::Build => (
            Confidence::Measured,
            "this repo's builds peak at 468 MiB used with >= 65% of 2048 free (sizing.md, Real builds)",
        ),
        Profile::Heavy => (
            Confidence::Extrapolated,
            "builds bigger than this repo's were not measured; 4096 is Claude Code's documented \
             minimum (sizing.md, Recommended settings)",
        ),
    };
    let base = finding(
        "kata.default_memory",
        "default_memory",
        kata_current(value, "Kata's default 2048"),
        target.to_string(),
    )
    .confidence(confidence)
    .reason(format!("profile {}: {why}", p.name()));
    if effective == target {
        return base;
    }
    base.status(Status::Warn)
        .change(Change::Kata("default_memory", target))
}

fn gateway_settings(f: &HostFacts, p: Profile) -> Vec<Finding> {
    let none = Env::new();
    let env = f.gateway.known().unwrap_or(&none);
    let mut out = Vec::new();
    out.extend(vm_memory(f, p, env));
    out.push(max_running(f, env));
    out.push(cpu_psi(f, env));
    match &f.gateway {
        Fact::Known(_) => out,
        other => unjudged(out, &unknown(other)),
    }
}

fn vm_memory(f: &HostFacts, p: Profile, env: &Env) -> Option<Finding> {
    let set = env.get("AC_VM_MEMORY_MIB")?;
    let guest = f
        .kata
        .known()
        .and_then(|k| k.default_memory)
        .unwrap_or(p.guest_mib());
    let base = finding(
        "gateway.vm_memory",
        "AC_VM_MEMORY_MIB",
        set.clone(),
        p.guest_mib().to_string(),
    )
    .reason("admission counts AC_VM_MEMORY_MIB instead of default_memory (ADR-012)");
    match set.parse::<u64>() {
        Ok(v) if v == p.guest_mib() && v == guest => Some(base),
        _ => Some(
            base.status(Status::Warn)
                .change(Change::Env("AC_VM_MEMORY_MIB", p.guest_mib().to_string())),
        ),
    }
}

fn max_running(f: &HostFacts, env: &Env) -> Finding {
    let cores = u64::from(f.cores);
    let set = env.get("AC_MAX_RUNNING_JOBS");
    let current = set.map_or_else(|| format!("unset (cores: {cores})"), String::clone);
    let base = finding(
        "gateway.max_running_jobs",
        "AC_MAX_RUNNING_JOBS",
        current,
        cores.to_string(),
    )
    .confidence(measured_if(f.cores == MEASURED_CORES))
    .reason("one job per core; API-bound jobs use ~5% of a core each (ADR-013; sizing.md, Jobs at once)");
    let Some(set) = set else {
        return base;
    };
    match set.parse::<u64>() {
        Ok(v) if v == cores => base,
        Ok(v) if (1..cores).contains(&v) => base.status(Status::Info).reason(
            "below the core count: fewer API-bound jobs at once than the host allows (ADR-013)",
        ),
        Ok(v) if v > cores => base
            .status(Status::Warn)
            .change(Change::Env("AC_MAX_RUNNING_JOBS", cores.to_string())),
        _ => base
            .status(Status::Error)
            .reason("not a whole number >= 1: the gateway refuses to start (ADR-013)")
            .change(Change::Env("AC_MAX_RUNNING_JOBS", cores.to_string())),
    }
}

fn cpu_psi(f: &HostFacts, env: &Env) -> Finding {
    let (target, confidence) = psi_target(f.cores);
    let set = env.get("AC_JOB_CPU_PSI_MAX");
    let current = set.map_or_else(|| format!("unset ({DEFAULT_CPU_PSI_MAX})"), String::clone);
    let reason = match confidence {
        Confidence::Measured => {
            "4 cores, 2-vCPU guests: 2 builds peaked at CPU PSI 23.7, so 20 holds a burst at 2 \
             (sizing.md, Jobs at once)"
        }
        Confidence::Extrapolated => {
            "uncalibrated: run scripts/real-build-bench.sh to calibrate (only 4 cores were measured)"
        }
    };
    let base = finding(
        "gateway.cpu_psi_max",
        "AC_JOB_CPU_PSI_MAX",
        current,
        target.to_string(),
    )
    .confidence(confidence)
    .reason(reason);
    let effective = match set.map(|s| s.parse::<f64>()) {
        None => DEFAULT_CPU_PSI_MAX,
        Some(Ok(v)) if v.is_finite() && v >= 0.0 => v,
        Some(_) => {
            return base
                .status(Status::Error)
                .reason("not a number >= 0: the gateway refuses to start (ADR-013)")
                .change(Change::Env("AC_JOB_CPU_PSI_MAX", target.to_string()));
        }
    };
    if effective == target {
        return base;
    }
    match confidence {
        Confidence::Measured => base
            .status(Status::Warn)
            .change(Change::Env("AC_JOB_CPU_PSI_MAX", target.to_string())),
        Confidence::Extrapolated => base.status(Status::Info).reason(
            "set by hand on an uncalibrated shape: keep it if scripts/real-build-bench.sh calibrated it",
        ),
    }
}

fn disk(f: &HostFacts) -> Finding {
    let base =
        finding("disk.class", "data dir disk", String::new(), "-".into()).status(Status::Info);
    match &f.disk {
        Fact::Known(Disk::Rotational) => Finding {
            current: "rotational".into(),
            ..base.reason(
                "cold boot 18-19 s vs 3.5 s warm, cold builds 38% slower (sizing.md, Real builds); an \
                 SSD helps cold starts, not the CPU-bound warm limit",
            )
        },
        Fact::Known(Disk::Solid) => Finding {
            current: "ssd/nvme".into(),
            ..base.confidence(Confidence::Extrapolated).reason(
                "cold starts should approach the warm 3.5 s boot; no SSD was measured (sizing.md, \
                 Disk class)",
            )
        },
        other => Finding {
            current: unknown(other),
            ..base.confidence(Confidence::Extrapolated).reason("disk class unknown")
        },
    }
}

/// A prerequisite: ok when `check` passes on a known fact, `error` when it fails or the thing is
/// absent or invalid, `info` when it was not checked.
fn prereq<T>(
    rule: &'static str,
    setting: &str,
    fact: &Fact<T>,
    want: &str,
    show: impl Fn(&T) -> String,
    check: impl Fn(&T) -> Result<(), String>,
) -> Finding {
    let reason = RULES
        .iter()
        .find(|r| r.id == rule)
        .map_or("", |r| r.evidence);
    let base = finding(rule, setting, unknown(fact), want.to_owned()).reason(reason);
    match fact {
        Fact::Known(v) => match check(v) {
            Ok(()) => Finding {
                current: show(v),
                ..base
            },
            Err(why) => Finding {
                current: show(v),
                ..base.status(Status::Error).reason(why)
            },
        },
        Fact::Absent | Fact::Invalid(_) => base.status(Status::Error),
        Fact::Skipped(_) => base.status(Status::Info),
    }
}

fn mode_ok(m: &Mode) -> Result<(), String> {
    if (m.uid, m.gid, m.perm) == (0, 0, 0o600) {
        Ok(())
    } else {
        Err("must be root:root 0600".into())
    }
}

fn show_mode(m: &Mode) -> String {
    format!("{}:{} {:04o}", m.uid, m.gid, m.perm)
}

fn same_shm(fstab: &Shm, running: &Shm) -> bool {
    let sorted = |s: &Shm| {
        let mut o = s.options.clone();
        o.sort();
        o
    };
    fstab.size_mib == running.size_mib && sorted(fstab) == sorted(running)
}

fn prerequisites(f: &HostFacts) -> Vec<Finding> {
    let mut out = vec![
        prereq(
            "prereq.kvm",
            "/dev/kvm",
            &f.kvm,
            "usable",
            |k| format!("{k:?}").to_lowercase(),
            |k| match k {
                Kvm::Usable => Ok(()),
                Kvm::NoAccess => Err("present but not openable: add the user to group kvm".into()),
            },
        ),
        prereq(
            "prereq.virt",
            "vmx/svm flag",
            &f.virt,
            "vmx or svm",
            |v| match v {
                Virt::Vmx => "vmx".into(),
                Virt::Svm => "svm".into(),
            },
            |_| Ok(()),
        ),
        prereq(
            "prereq.kata_config",
            "Kata configuration",
            &f.kata,
            "present",
            |_| "present".into(),
            |_| Ok(()),
        ),
        prereq(
            "prereq.initrd_net",
            "initrd NIC drop-in",
            &f.initrd_net,
            "present",
            |has| if *has { "present" } else { "missing" }.into(),
            |has| {
                has.then_some(()).ok_or_else(|| {
                    "missing: networked agents never get eth0 (install.md note 4)".into()
                })
            },
        ),
    ];
    let running = f.shm.known();
    let fstab = prereq(
        "prereq.shm_fstab",
        "/dev/shm fstab line",
        &f.shm_fstab,
        "present, = running mount",
        |s| format!("{} MiB", s.size_mib),
        |s| {
            match running {
            Some(r) if !same_shm(s, r) => Err(format!(
                "running mount ({} MiB, {}) differs: sudo mount -o remount,size={}M /dev/shm (install.md \
                 note 9)",
                r.size_mib,
                r.options.join(","),
                s.size_mib
            )),
            _ => Ok(()),
        }
        },
    );
    let fstab = match (&f.shm_fstab, shm_target(f.mem_total_mib)) {
        (Fact::Absent, Some(target)) => Finding {
            reason: "no /dev/shm line: a reboot resets /dev/shm to half of RAM (install.md note 9)"
                .into(),
            ..fstab.change(Change::ShmFstab(target))
        },
        _ => fstab,
    };
    out.push(fstab);
    match &f.firewall {
        Fact::Known(fw) => {
            for ((chain, want), have) in NFT_CHAINS.iter().zip(&fw.chains) {
                let fact = Fact::Known(*have);
                out.push(prereq(
                    "prereq.nft",
                    &format!("nft {chain}"),
                    &fact,
                    &format!("{want} rules"),
                    |n| n.map_or("no chain".into(), |n| format!("{n} rules")),
                    |n| {
                        (*n == Some(*want))
                            .then_some(())
                            .ok_or_else(|| "differs (ADR-007)".into())
                    },
                ));
            }
            let fact = Fact::Known(fw.zone.clone());
            out.push(prereq(
                "prereq.firewalld",
                "firewalld zone agents",
                &fact,
                AGENTS_ZONE,
                String::clone,
                |z| {
                    (z == AGENTS_ZONE)
                        .then_some(())
                        .ok_or_else(|| "differs (ADR-007)".into())
                },
            ));
        }
        other => {
            out.push(prereq(
                "prereq.nft",
                "nft agentfw",
                other,
                "rules",
                |_| String::new(),
                |_| Ok(()),
            ));
            out.push(prereq(
                "prereq.firewalld",
                "firewalld zone agents",
                other,
                AGENTS_ZONE,
                |_| String::new(),
                |_| Ok(()),
            ));
        }
    }
    out.push(prereq(
        "prereq.tokens_mode",
        "tokens file mode",
        &f.tokens_mode,
        "0:0 0600",
        show_mode,
        mode_ok,
    ));
    out.push(prereq(
        "prereq.allowlist_mode",
        "allowlist mode",
        &f.allowlist_mode,
        "0:0 0600",
        show_mode,
        mode_ok,
    ));
    out
}

/// What the recommended settings allow on this host, with admission's `margin_mib`.
pub fn forecast(f: &HostFacts, p: Profile, margin_mib: u64) -> Forecast {
    let guest_mib = p.guest_mib();
    let vcpus = vcpus_target(f.cores);
    let shm_mib = shm_target(f.mem_total_mib).unwrap_or(0);
    let boot = match f.disk {
        Fact::Known(Disk::Rotational) => "rotational: cold boot 18-19 s, warm 3.5 s (measured)",
        Fact::Known(Disk::Solid) => {
            "ssd/nvme: cold boot expected near the warm 3.5 s (not measured)"
        }
        _ => "unknown disk class",
    };
    Forecast {
        guest_mib,
        vcpus,
        shm_mib,
        margin_mib,
        max_agents: shm_mib.saturating_sub(margin_mib) / guest_mib,
        builds_at_once: u64::from(f.cores) / vcpus,
        confidence: measured_if(f.cores == MEASURED_CORES && measured_ram(f)),
        boot,
    }
}
