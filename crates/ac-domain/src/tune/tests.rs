//! The rules over the host shapes ADR-014 names, and the edits on fixture files.

use super::edit::{diff, env_set, fstab_shm, hunks, kata_set, stamp};
use super::facts::*;
use super::rules::*;

fn env(pairs: &[(&str, &str)]) -> Env {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

fn shm(size_mib: u64) -> Shm {
    Shm {
        size_mib,
        options: ["nosuid", "nodev", "inode64", "usrquota"]
            .map(String::from)
            .to_vec(),
    }
}

const ROOT_600: Mode = Mode {
    uid: 0,
    gid: 0,
    perm: 0o600,
};

/// BaileyServ as deployed: i5-3550, 24 GiB, rotational disk, the live settings.
fn this_host() -> HostFacts {
    HostFacts {
        mem_total_mib: 23909,
        cores: 4,
        phys_bits: Fact::Known(36),
        virt: Fact::Known(Virt::Vmx),
        disk: Fact::Known(Disk::Rotational),
        shm: Fact::Known(shm(15717)),
        shm_used_mib: Fact::Known(0),
        shm_fstab: Fact::Known(shm(15717)),
        kata: Fact::Known(Kata {
            default_memory: Some(2048),
            default_vcpus: Some(2),
            default_maxmemory: Some(8192),
            default_maxvcpus: Some(0),
            initrd: Some("/var/cache/kata-containers/kata-containers-initrd.img".into()),
        }),
        gateway: Fact::Known(env(&[
            ("AC_IMAGE", "localhost/agent-shell:dev"),
            ("AC_BIND", "127.0.0.1:8780"),
            ("AC_NETWORK", "1"),
            ("AC_JOB_CPU_PSI_MAX", "20"),
        ])),
        kvm: Fact::Known(Kvm::Usable),
        initrd_net: Fact::Known(true),
        firewall: Fact::Known(Firewall {
            chains: vec![Some(3), Some(3), Some(1)],
            zone: AGENTS_ZONE.into(),
        }),
        tokens_mode: Fact::Known(ROOT_600),
        allowlist_mode: Fact::Known(ROOT_600),
    }
}

/// A host of another size with this host's prerequisites and a fresh Kata config (no keys set).
fn shape(mem_total_mib: u64, cores: u32, phys_bits: u32, disk: Disk) -> HostFacts {
    let target = shm_target(mem_total_mib).unwrap_or(mem_total_mib / 2);
    HostFacts {
        mem_total_mib,
        cores,
        phys_bits: Fact::Known(phys_bits),
        disk: Fact::Known(disk),
        shm: Fact::Known(shm(target)),
        shm_fstab: Fact::Known(shm(target)),
        kata: Fact::Known(Kata::default()),
        gateway: Fact::Known(Env::new()),
        ..this_host()
    }
}

fn get<'a>(fs: &'a [Finding], rule: &str) -> &'a Finding {
    fs.iter()
        .find(|f| f.rule == rule)
        .unwrap_or_else(|| panic!("no finding {rule}"))
}

/// (rule, status, recommended, confidence) of one finding.
fn verdict(fs: &[Finding], rule: &str) -> (Status, String, Confidence) {
    let f = get(fs, rule);
    (f.status, f.recommended.clone(), f.confidence)
}

use Confidence::{Extrapolated as X, Measured as M, MeasuredInput as MI};
use Status::{Error as E, Info as I, Ok as K, Warn as W};

#[test]
fn this_host_is_as_recommended_and_measured() {
    for p in [Profile::Idle, Profile::Build] {
        let fs = recommend(&this_host(), p);
        for f in &fs {
            let want = if f.rule == "disk.class" { I } else { K };
            assert_eq!(f.status, want, "{p:?} {f:?}");
            // The PSI limit's input was measured; the gate at that limit was not.
            let conf = if f.rule == "gateway.cpu_psi_max" {
                MI
            } else {
                M
            };
            assert_eq!(f.confidence, conf, "{p:?} {f:?}");
            assert_eq!(f.change, None, "{p:?} {f:?}");
        }
        assert_eq!(worst(&fs), I);
    }
    let fs = recommend(&this_host(), Profile::Build);
    let rec = |rule| get(&fs, rule).recommended.clone();
    assert_eq!(rec("shm.size"), "15717 MiB");
    assert_eq!(rec("kata.default_memory"), "2048");
    assert_eq!(rec("kata.default_vcpus"), "2");
    assert_eq!(rec("kata.default_maxmemory"), "8192");
    assert_eq!(rec("gateway.cpu_psi_max"), "20");
    assert_eq!(rec("gateway.max_running_jobs"), "4");
    // Heavy wants 4096: a warning, extrapolated, with the edit.
    let heavy = recommend(&this_host(), Profile::Heavy);
    let m = get(&heavy, "kata.default_memory");
    assert_eq!((m.status, m.confidence), (W, X));
    assert_eq!(m.change, Some(Change::Kata("default_memory", 4096)));
}

#[test]
fn shapes() {
    // (name, facts, profile, [(rule, status, recommended, confidence)], forecast (agents, builds))
    type Row = (
        &'static str,
        HostFacts,
        Profile,
        Vec<(&'static str, Status, &'static str, Confidence)>,
        (u64, u64),
    );
    let rows: Vec<Row> = vec![
        (
            "32 GiB, 8 cores, NVMe",
            shape(31998, 8, 39, Disk::Solid),
            Profile::Build,
            vec![
                ("shm.size", K, "23806 MiB", X),
                ("kata.default_maxmemory", K, "Kata's default", X),
                ("kata.default_vcpus", W, "2", X),
                ("kata.default_memory", K, "2048", M),
                ("gateway.max_running_jobs", K, "8", X),
                ("gateway.cpu_psi_max", K, "40", X),
                ("disk.class", I, "-", X),
            ],
            (11, 4),
        ),
        (
            "16 GiB, 4 cores",
            shape(15700, 4, 39, Disk::Solid),
            Profile::Build,
            vec![
                ("shm.size", K, "7850 MiB", X),
                ("kata.default_vcpus", W, "2", M),
                ("gateway.cpu_psi_max", W, "20", MI),
            ],
            (3, 2),
        ),
        (
            "64 GiB, 16 cores, heavy",
            shape(64000, 16, 46, Disk::Solid),
            Profile::Heavy,
            vec![
                ("shm.size", K, "55808 MiB", X),
                ("kata.default_vcpus", W, "2", X),
                ("kata.default_memory", W, "4096", X),
                ("gateway.max_running_jobs", K, "16", X),
            ],
            (13, 8),
        ),
        (
            "8 GiB",
            shape(7800, 4, 36, Disk::Rotational),
            Profile::Idle,
            vec![
                ("shm.size", E, "-", X),
                ("kata.default_maxmemory", E, "8192", M),
            ],
            (0, 2),
        ),
        (
            "2 cores",
            shape(23909, 2, 36, Disk::Rotational),
            Profile::Build,
            vec![
                ("kata.default_vcpus", K, "1", X),
                ("gateway.cpu_psi_max", K, "40", X),
                ("gateway.max_running_jobs", K, "2", X),
            ],
            (7, 2),
        ),
    ];
    for (name, facts, p, want, (agents, builds)) in rows {
        let fs = recommend(&facts, p);
        for (rule, status, rec, conf) in want {
            assert_eq!(
                verdict(&fs, rule),
                (status, rec.to_owned(), conf),
                "{name}: {rule}"
            );
        }
        let fc = forecast(&facts, p, SHM_MARGIN_MIB);
        assert_eq!(
            (fc.max_agents, fc.builds_at_once),
            (agents, builds),
            "{name}"
        );
    }
    let fc = forecast(&this_host(), Profile::Build, SHM_MARGIN_MIB);
    assert_eq!(
        (fc.shm_mib, fc.max_agents, fc.builds_at_once, fc.confidence),
        (15717, 7, 2, M)
    );
    assert!(fc.boot.contains("18-19 s"), "{}", fc.boot);
    let nvme = forecast(&shape(31998, 8, 39, Disk::Solid), Profile::Build, 512);
    assert_eq!(nvme.confidence, X);
    assert!(nvme.boot.contains("not measured"), "{}", nvme.boot);
}

#[test]
fn amd_host() {
    let f = HostFacts {
        virt: Fact::Known(Virt::Svm),
        ..shape(31998, 8, 48, Disk::Solid)
    };
    let fs = recommend(&f, Profile::Build);
    let v = get(&fs, "prereq.virt");
    assert_eq!((v.status, v.current.as_str()), (K, "svm"));
    assert_eq!(get(&fs, "kata.default_maxmemory").status, K);
}

#[test]
fn host_without_kvm() {
    let f = HostFacts {
        kvm: Fact::Absent,
        virt: Fact::Absent,
        ..this_host()
    };
    let fs = recommend(&f, Profile::Build);
    assert_eq!(get(&fs, "prereq.kvm").status, E);
    assert_eq!(get(&fs, "prereq.virt").status, E);
    assert_eq!(worst(&fs), E);
    let no_access = HostFacts {
        kvm: Fact::Known(Kvm::NoAccess),
        ..this_host()
    };
    assert_eq!(
        get(&recommend(&no_access, Profile::Build), "prereq.kvm").status,
        E
    );
}

#[test]
fn kata_config_missing_keys() {
    let f = HostFacts {
        kata: Fact::Known(Kata::default()),
        ..this_host()
    };
    let fs = recommend(&f, Profile::Build);
    let max = get(&fs, "kata.default_maxmemory");
    assert_eq!(
        (max.status, max.current.as_str()),
        (E, "unset (0: all host RAM)")
    );
    assert_eq!(max.change, Some(Change::Kata("default_maxmemory", 8192)));
    let vcpus = get(&fs, "kata.default_vcpus");
    assert_eq!(
        (vcpus.status, vcpus.change.clone()),
        (W, Some(Change::Kata("default_vcpus", 2)))
    );
    // Kata's default memory is the build profile's: nothing to change.
    let mem = get(&fs, "kata.default_memory");
    assert_eq!(
        (mem.status, mem.current.as_str()),
        (K, "unset (Kata's default 2048)")
    );
    // No config at all, or one Kata refuses: an error, and the settings are not judged.
    for kata in [
        Fact::Absent,
        Fact::Invalid("default_memory is set twice".into()),
    ] {
        let fs = recommend(
            &HostFacts {
                kata,
                ..this_host()
            },
            Profile::Build,
        );
        assert_eq!(get(&fs, "prereq.kata_config").status, E);
        assert_eq!(get(&fs, "kata.default_memory").status, I);
    }
}

#[test]
fn shm_boundaries() {
    assert_eq!(shm_target(8191), None);
    assert_eq!(shm_target(8192), Some(4096));
    assert_eq!(shm_target(16383), Some(8192));
    assert_eq!(shm_target(16384), Some(8192));
    assert_eq!(shm_target(23909), Some(15717));
    // Within 1% is ok; beyond, a warning with the fstab edit.
    let with = |size| {
        let f = HostFacts {
            shm_fstab: Fact::Known(shm(size)),
            shm: Fact::Known(shm(size)),
            ..this_host()
        };
        let s = recommend(&f, Profile::Build)
            .into_iter()
            .find(|f| f.rule == "shm.size");
        s.map(|s| (s.status, s.change))
    };
    assert_eq!(with(15717 - 157), Some((K, None)));
    assert_eq!(with(15717 + 157), Some((K, None)));
    assert_eq!(with(15717 - 158), Some((W, Some(Change::ShmFstab(15717)))));
    assert_eq!(with(11954), Some((W, Some(Change::ShmFstab(15717)))));
}

#[test]
fn shm_fstab_prerequisite() {
    let missing = HostFacts {
        shm_fstab: Fact::Absent,
        shm: Fact::Known(shm(11954)),
        ..this_host()
    };
    let fs = recommend(&missing, Profile::Build);
    let p = get(&fs, "prereq.shm_fstab");
    assert_eq!(
        (p.status, p.change.clone()),
        (E, Some(Change::ShmFstab(15717)))
    );
    // Without a line the running size is judged.
    assert_eq!(get(&fs, "shm.size").status, W);
    // A line the running mount does not match: an error naming the remount.
    let stale = HostFacts {
        shm: Fact::Known(shm(11954)),
        ..this_host()
    };
    let p = recommend(&stale, Profile::Build);
    let p = get(&p, "prereq.shm_fstab");
    assert_eq!(p.status, E);
    assert!(
        p.reason.contains("mount -o remount,size=15717M"),
        "{}",
        p.reason
    );
    let other_opts = HostFacts {
        shm: Fact::Known(Shm {
            options: vec!["nosuid".into()],
            ..shm(15717)
        }),
        ..this_host()
    };
    assert_eq!(
        get(&recommend(&other_opts, Profile::Build), "prereq.shm_fstab").status,
        E
    );
}

#[test]
fn maxmemory_boundaries() {
    let at = |bits, set: Option<u64>| {
        let f = HostFacts {
            phys_bits: Fact::Known(bits),
            kata: Fact::Known(Kata {
                default_maxmemory: set,
                ..Kata::default()
            }),
            ..this_host()
        };
        let fs = recommend(&f, Profile::Build);
        let m = get(&fs, "kata.default_maxmemory");
        (m.status, m.recommended.clone(), m.confidence)
    };
    assert_eq!(at(36, Some(8192)), (K, "8192".into(), M));
    assert_eq!(at(36, Some(0)), (E, "8192".into(), M));
    assert_eq!(at(36, Some(16384)), (W, "8192".into(), M));
    assert_eq!(at(35, None), (E, "8192".into(), X));
    assert_eq!(at(37, None), (K, "Kata's default".into(), X));
    assert_eq!(at(37, Some(8192)), (I, "Kata's default".into(), X));
}

#[test]
fn vcpus_and_psi_boundaries() {
    assert_eq!(
        (
            vcpus_target(1),
            vcpus_target(3),
            vcpus_target(4),
            vcpus_target(64)
        ),
        (1, 1, 2, 2)
    );
    assert_eq!(psi_target(4), (20.0, MI));
    assert_eq!(MI.name(), "measured input, gate unvalidated");
    assert_eq!(psi_target(3), (40.0, X));
    assert_eq!(psi_target(5), (40.0, X));
    // A Kata config above 2 vCPUs: warned down to 2.
    let f = HostFacts {
        kata: Fact::Known(Kata {
            default_vcpus: Some(4),
            ..Kata::default()
        }),
        ..this_host()
    };
    assert_eq!(
        get(&recommend(&f, Profile::Build), "kata.default_vcpus").status,
        W
    );
}

#[test]
fn gateway_values() {
    let with = |pairs: &[(&str, &str)]| {
        recommend(
            &HostFacts {
                gateway: Fact::Known(env(pairs)),
                ..this_host()
            },
            Profile::Build,
        )
    };
    let status = |fs: &[Finding], rule| get(fs, rule).status;
    // The PSI limit unset on the measured shape: warned to 20.
    let fs = with(&[]);
    let psi = get(&fs, "gateway.cpu_psi_max");
    assert_eq!(
        (psi.status, psi.change.clone()),
        (W, Some(Change::Env("AC_JOB_CPU_PSI_MAX", "20".into())))
    );
    assert_eq!(
        status(
            &with(&[("AC_JOB_CPU_PSI_MAX", "20.0")]),
            "gateway.cpu_psi_max"
        ),
        K
    );
    assert_eq!(
        status(
            &with(&[("AC_JOB_CPU_PSI_MAX", "abc")]),
            "gateway.cpu_psi_max"
        ),
        E
    );
    // The cap: above the core count warned, below informational, not a number an error.
    let jobs = |v| {
        status(
            &with(&[("AC_JOB_CPU_PSI_MAX", "20"), ("AC_MAX_RUNNING_JOBS", v)]),
            "gateway.max_running_jobs",
        )
    };
    assert_eq!(
        (jobs("4"), jobs("2"), jobs("8"), jobs("0"), jobs("x")),
        (K, I, W, E, E)
    );
    // AC_VM_MEMORY_MIB only shows when set.
    assert!(!with(&[]).iter().any(|f| f.rule == "gateway.vm_memory"));
    let vm = |v| status(&with(&[("AC_VM_MEMORY_MIB", v)]), "gateway.vm_memory");
    assert_eq!((vm("2048"), vm("4096")), (K, W));
    // Root-only and not readable: skipped, not judged.
    let skipped = HostFacts {
        gateway: Fact::Skipped("root-only".into()),
        ..this_host()
    };
    let fs = recommend(&skipped, Profile::Build);
    let psi = get(&fs, "gateway.cpu_psi_max");
    assert_eq!(
        (psi.status, psi.current.as_str()),
        (I, "skipped: root-only")
    );
}

#[test]
fn firewall_and_modes() {
    let f = HostFacts {
        firewall: Fact::Known(Firewall {
            chains: vec![Some(3), None, Some(1)],
            zone: "acbr0|3128/tcp|ssh".into(),
        }),
        tokens_mode: Fact::Known(Mode {
            perm: 0o644,
            ..ROOT_600
        }),
        allowlist_mode: Fact::Absent,
        ..this_host()
    };
    let fs = recommend(&f, Profile::Build);
    let nft: Vec<(Status, &str)> = fs
        .iter()
        .filter(|f| f.rule == "prereq.nft")
        .map(|f| (f.status, f.current.as_str()))
        .collect();
    assert_eq!(nft, vec![(K, "3 rules"), (E, "no chain"), (K, "1 rules")]);
    assert_eq!(get(&fs, "prereq.firewalld").status, E);
    let t = get(&fs, "prereq.tokens_mode");
    assert_eq!((t.status, t.current.as_str()), (E, "0:0 0644"));
    assert_eq!(get(&fs, "prereq.allowlist_mode").status, E);
    // Not root: skipped is information, not an error.
    let skipped = HostFacts {
        firewall: Fact::Skipped("needs root".into()),
        ..this_host()
    };
    let fs = recommend(&skipped, Profile::Build);
    assert_eq!((get(&fs, "prereq.nft").status, worst(&fs)), (I, I));
}

#[test]
fn hypothetical_host_with_only_size() {
    let f = HostFacts::bare(31998, 8, "not in the facts file");
    let fs = recommend(&f, Profile::Build);
    assert_eq!(worst(&fs), I, "{fs:#?}");
    // Every setting still has its recommendation, for planning.
    let rec = |rule| verdict(&fs, rule);
    assert_eq!(rec("shm.size"), (I, "23806 MiB".into(), X));
    assert_eq!(rec("kata.default_vcpus"), (I, "2".into(), X));
    assert_eq!(rec("kata.default_memory"), (I, "2048".into(), M));
    assert_eq!(rec("gateway.cpu_psi_max"), (I, "40".into(), X));
    assert_eq!(rec("gateway.max_running_jobs"), (I, "8".into(), X));
    assert_eq!(forecast(&f, Profile::Build, 512).max_agents, 11);
}

#[test]
fn every_rule_is_documented() {
    let mut ids: Vec<&str> = recommend(&this_host(), Profile::Build)
        .iter()
        .map(|f| f.rule)
        .collect();
    ids.push("gateway.vm_memory");
    for id in ids {
        assert!(RULES.iter().any(|r| r.id == id), "{id}");
    }
    assert!(RULES
        .iter()
        .all(|r| r.evidence.contains(".md") || r.evidence.contains("ADR-")));
}

const KATA: &str = "\
# Copyright (c) 2017-2019 Intel Corporation
[hypervisor.qemu]
path = \"/usr/bin/qemu-kvm\"
initrd = \"/var/cache/kata-containers/kata-containers-initrd.img\"

# Default number of vCPUs per SB/VM:
#default_vcpus = 1
default_vcpus = 1

# Default memory size in MiB for SB/VM.
default_memory = 4096   # MiB
#default_maxmemory = 0
memory_slots = 10

[factory]
enable_template = false
";

#[test]
fn kata_edits() {
    // In place: indent and comment kept, nothing else touched.
    let out = kata_set(KATA, "default_memory", 2048).unwrap_or_else(|e| panic!("{e}"));
    assert!(out.contains("default_memory = 2048 # MiB\n"), "{out}");
    assert_eq!(out.lines().count(), KATA.lines().count());
    assert_eq!(
        diff("k", KATA, &out)
            .lines()
            .filter(|l| l.starts_with(['-', '+']))
            .count(),
        4
    );
    // After its commented template.
    let out = kata_set(KATA, "default_maxmemory", 8192).unwrap_or_else(|e| panic!("{e}"));
    assert!(
        out.contains("#default_maxmemory = 0\ndefault_maxmemory = 8192\nmemory_slots"),
        "{out}"
    );
    // No template: after the table's last non-blank line, before the next table.
    let out = kata_set(KATA, "default_maxvcpus", 2).unwrap_or_else(|e| panic!("{e}"));
    assert!(
        out.contains("memory_slots = 10\ndefault_maxvcpus = 2\n\n[factory]"),
        "{out}"
    );
    // Idempotent: a second run changes nothing; so does a value already set.
    let once = kata_set(KATA, "default_vcpus", 2).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        kata_set(&once, "default_vcpus", 2).as_deref(),
        Ok(once.as_str())
    );
    assert_eq!(kata_set(KATA, "default_vcpus", 1).as_deref(), Ok(KATA));
    // The result reads back as set; the other table is untouched.
    assert_eq!(kata(&once).map(|k| k.default_vcpus), Ok(Some(2)));
    assert!(once.ends_with("[factory]\nenable_template = false\n"));
    // Refusals.
    assert!(kata_set("[runtime]\nx = 1\n", "default_memory", 1).is_err());
    let dup = KATA.replace("memory_slots = 10", "default_vcpus = 3");
    assert_eq!(
        kata_set(&dup, "default_vcpus", 2),
        Err("default_vcpus is set twice".into())
    );
}

#[test]
fn env_edits() {
    let shipped = include_str!("../../../../deploy/gateway.env");
    let out = env_set(shipped, "AC_JOB_CPU_PSI_MAX", "20");
    assert!(
        out.starts_with(shipped) && out.ends_with("\nAC_JOB_CPU_PSI_MAX=20\n"),
        "{out}"
    );
    assert_eq!(env_set(&out, "AC_JOB_CPU_PSI_MAX", "20"), out);
    // The last assignment is the one rewritten; comments stay.
    let two = "# c\nAC_X=1\n#AC_X=9\nAC_X=2\nAC_Y=\"a b\"\n";
    assert_eq!(
        env_set(two, "AC_X", "3"),
        "# c\nAC_X=1\n#AC_X=9\nAC_X=3\nAC_Y=\"a b\"\n"
    );
    assert_eq!(env_set(two, "AC_Y", "a b"), two);
    assert_eq!(env_set("AC_A=1", "AC_B", "2"), "AC_A=1\nAC_B=2\n");
    assert_eq!(env_set("", "AC_B", "2"), "AC_B=2\n");
}

#[test]
fn fstab_edits() {
    let fstab = "UUID=1 / btrfs subvol=root 0 0\n# tmpfs /dev/shm tmpfs size=1G 0 0\n";
    let opts = shm(0).options;
    let out = fstab_shm(fstab, 15717, &opts);
    assert!(out.starts_with(fstab), "{out}");
    assert!(
        out.ends_with("tmpfs  /dev/shm  tmpfs  nosuid,nodev,inode64,usrquota,size=15717M  0  0\n"),
        "{out}"
    );
    assert_eq!(
        shm_entry(&out, 23909).map(|s| s.map(|s| s.size_mib)),
        Ok(Some(15717))
    );
    assert_eq!(fstab_shm(&out, 15717, &opts), out);
    // An existing line: only size= changes.
    let deploy = include_str!("../../../../deploy/dev-shm.fstab");
    assert_eq!(fstab_shm(deploy, 15717, &[]), deploy);
    let resized = fstab_shm(deploy, 23806, &[]);
    assert!(
        resized.contains("tmpfs  /dev/shm  tmpfs  nosuid,nodev,inode64,usrquota,size=23806M  0  0"),
        "{resized}"
    );
    let h = hunks(deploy, &resized);
    assert_eq!(h.len(), 1);
    assert_eq!((h[0].old.1, h[0].new.1), (1, 1));
    // No size= yet: added at the end of the options.
    assert_eq!(
        fstab_shm("tmpfs /dev/shm tmpfs defaults 0 0\n", 100, &[]),
        "tmpfs  /dev/shm  tmpfs  defaults,size=100M  0  0\n"
    );
}

#[test]
fn diffs() {
    assert_eq!(diff("f", "a\nb\n", "a\nb\n"), "");
    assert_eq!(
        diff("f", "a\nb\nc\n", "a\nB\nc\n"),
        "--- f\n+++ f (proposed)\n@@ -2,1 +2,1 @@\n-b\n+B\n"
    );
    assert_eq!(
        diff("f", "a\nc\n", "a\nb\nc\n"),
        "--- f\n+++ f (proposed)\n@@ -1,0 +2,1 @@\n+b\n"
    );
    assert_eq!(
        diff("f", "a\n", "a\nb\n"),
        "--- f\n+++ f (proposed)\n@@ -1,0 +2,1 @@\n+b\n"
    );
    assert_eq!(
        diff("f", "a\nb\n", "a\n"),
        "--- f\n+++ f (proposed)\n@@ -2,1 +1,0 @@\n-b\n"
    );
    let two = hunks("1\n2\n3\n4\n5\n", "1\nx\n3\n4\ny\n");
    assert_eq!(two.len(), 2);
    assert_eq!((two[1].old, two[1].new), ((5, 1), (5, 1)));
}

#[test]
fn stamps() {
    assert_eq!(stamp(0), "19700101T000000Z");
    assert_eq!(stamp(1_761_221_715), "20251023T121515Z");
    assert_eq!(stamp(951_782_400), "20000229T000000Z");
}

#[test]
fn drift_names_only_settings_that_differ() {
    assert_eq!(drift(&recommend(&this_host(), Profile::Build)), None);
    let f = HostFacts {
        kvm: Fact::Absent,
        gateway: Fact::Known(Env::new()),
        kata: Fact::Known(Kata {
            default_memory: Some(4096),
            ..Kata::default()
        }),
        ..this_host()
    };
    let line = drift(&recommend(&f, Profile::Build)).unwrap_or_default();
    assert_eq!(
        line,
        "default_maxmemory unset (0: all host RAM) -> 8192, default_vcpus unset (Kata's default 1) -> 2, \
         default_memory 4096 -> 2048, AC_JOB_CPU_PSI_MAX unset (40) -> 20"
    );
}
