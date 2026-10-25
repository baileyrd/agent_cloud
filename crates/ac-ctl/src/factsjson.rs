//! [`HostFacts`] as JSON, for `doctor --json` and `--facts-file` (ADR-014). A fact that was
//! looked for and is not there is `null`; one that could not be read is `{"invalid": why}` or
//! `{"skipped": why}`; a key left out of a facts file is skipped as "not in the facts file". A
//! facts file may also be a whole `doctor --json` output: its `facts` are used.

use ac_domain::tune::facts::{
    Disk, EgressProfiles, Env, Fact, Firewall, HostFacts, Kata, Kvm, Mode, Shm, Virt, NFT_CHAINS,
};
use rusty_json::{Map, Value};

const NOT_IN_FILE: &str = "not in the facts file";

fn object(pairs: impl IntoIterator<Item = (String, Value)>) -> Value {
    let mut map = Map::new();
    for (k, v) in pairs {
        map.insert(k, v);
    }
    Value::Object(map)
}

fn fact<T>(f: &Fact<T>, known: impl Fn(&T) -> Value) -> Value {
    match f {
        Fact::Known(v) => known(v),
        Fact::Absent => Value::Null,
        Fact::Invalid(why) => object([("invalid".to_owned(), Value::from(why.as_str()))]),
        Fact::Skipped(why) => object([("skipped".to_owned(), Value::from(why.as_str()))]),
    }
}

fn shm(s: &Shm) -> Value {
    object([
        ("size_mib".to_owned(), Value::from(s.size_mib)),
        (
            "options".to_owned(),
            Value::Array(s.options.iter().map(|o| Value::from(o.as_str())).collect()),
        ),
    ])
}

fn mode(m: &Mode) -> Value {
    object([
        ("uid".to_owned(), Value::from(m.uid)),
        ("gid".to_owned(), Value::from(m.gid)),
        ("mode".to_owned(), Value::from(format!("{:04o}", m.perm))),
    ])
}

fn strings(v: &[String]) -> Value {
    Value::Array(v.iter().map(|s| Value::from(s.as_str())).collect())
}

fn egress_profiles(p: &EgressProfiles) -> Value {
    object([
        ("attached".to_owned(), strings(&p.attached)),
        ("missing".to_owned(), strings(&p.missing)),
        ("refused".to_owned(), strings(&p.refused)),
    ])
}

/// `f` as a JSON object.
pub fn to_json(f: &HostFacts) -> Value {
    let kata = |k: &Kata| {
        let n = |v: Option<u64>| v.map(Value::from);
        object(
            [
                ("default_memory", n(k.default_memory)),
                ("default_vcpus", n(k.default_vcpus)),
                ("default_maxmemory", n(k.default_maxmemory)),
                ("default_maxvcpus", n(k.default_maxvcpus)),
                ("initrd", k.initrd.as_deref().map(Value::from)),
            ]
            .into_iter()
            .filter_map(|(key, v)| Some((key.to_owned(), v?))),
        )
    };
    let firewall = |fw: &Firewall| {
        let chains = NFT_CHAINS
            .iter()
            .zip(&fw.chains)
            .map(|((chain, _), n)| ((*chain).to_owned(), n.map_or(Value::Null, Value::from)));
        object([
            ("chains".to_owned(), object(chains)),
            ("zone".to_owned(), Value::from(fw.zone.as_str())),
        ])
    };
    let env = |e: &Env| object(e.iter().map(|(k, v)| (k.clone(), Value::from(v.as_str()))));
    object([
        ("mem_total_mib".to_owned(), Value::from(f.mem_total_mib)),
        ("cores".to_owned(), Value::from(f.cores)),
        (
            "phys_bits".to_owned(),
            fact(&f.phys_bits, |b| Value::from(*b)),
        ),
        (
            "virt".to_owned(),
            fact(&f.virt, |v| {
                Value::from(match v {
                    Virt::Vmx => "vmx",
                    Virt::Svm => "svm",
                })
            }),
        ),
        (
            "disk".to_owned(),
            fact(&f.disk, |d| {
                Value::from(match d {
                    Disk::Rotational => "rotational",
                    Disk::Solid => "solid",
                })
            }),
        ),
        ("shm".to_owned(), fact(&f.shm, shm)),
        (
            "shm_used_mib".to_owned(),
            fact(&f.shm_used_mib, |u| Value::from(*u)),
        ),
        ("shm_fstab".to_owned(), fact(&f.shm_fstab, shm)),
        ("kata".to_owned(), fact(&f.kata, kata)),
        ("gateway".to_owned(), fact(&f.gateway, env)),
        (
            "kvm".to_owned(),
            fact(&f.kvm, |k| {
                Value::from(match k {
                    Kvm::Usable => "usable",
                    Kvm::NoAccess => "no_access",
                })
            }),
        ),
        (
            "initrd_net".to_owned(),
            fact(&f.initrd_net, |b| Value::from(*b)),
        ),
        ("firewall".to_owned(), fact(&f.firewall, firewall)),
        ("tokens_mode".to_owned(), fact(&f.tokens_mode, mode)),
        ("allowlist_mode".to_owned(), fact(&f.allowlist_mode, mode)),
        (
            "egress_agents_mode".to_owned(),
            fact(&f.egress_agents_mode, mode),
        ),
        (
            "egress_profiles".to_owned(),
            fact(&f.egress_profiles, egress_profiles),
        ),
    ])
}

/// A fact from `json[key]`: missing is skipped, `null` absent, `{"skipped"|"invalid": why}` as
/// said, anything else through `known`.
fn read<T>(
    json: &Value,
    key: &str,
    known: impl Fn(&Value) -> Option<T>,
) -> Result<Fact<T>, String> {
    let Some(v) = json.get(key) else {
        return Ok(Fact::Skipped(NOT_IN_FILE.into()));
    };
    if v.is_null() {
        return Ok(Fact::Absent);
    }
    if let Some(why) = v.get("skipped").and_then(Value::as_str) {
        return Ok(Fact::Skipped(why.to_owned()));
    }
    if let Some(why) = v.get("invalid").and_then(Value::as_str) {
        return Ok(Fact::Invalid(why.to_owned()));
    }
    known(v)
        .map(Fact::Known)
        .ok_or_else(|| format!("{key}: unexpected value {}", v.to_json_string()))
}

fn shm_of(v: &Value) -> Option<Shm> {
    let options = v
        .get("options")
        .and_then(Value::as_array)
        .map_or(Some(Vec::new()), |a| {
            a.iter().map(|o| o.as_str().map(str::to_owned)).collect()
        })?;
    Some(Shm {
        size_mib: v.get("size_mib")?.as_u64()?,
        options,
    })
}

fn mode_of(v: &Value) -> Option<Mode> {
    Some(Mode {
        uid: u32::try_from(v.get("uid")?.as_u64()?).ok()?,
        gid: u32::try_from(v.get("gid")?.as_u64()?).ok()?,
        perm: u32::from_str_radix(v.get("mode")?.as_str()?, 8).ok()?,
    })
}

fn strings_of(v: Option<&Value>) -> Option<Vec<String>> {
    v?.as_array()?
        .iter()
        .map(|s| s.as_str().map(str::to_owned))
        .collect()
}

fn egress_profiles_of(v: &Value) -> Option<EgressProfiles> {
    Some(EgressProfiles {
        attached: strings_of(v.get("attached"))?,
        missing: strings_of(v.get("missing"))?,
        refused: strings_of(v.get("refused"))?,
    })
}

fn kata_of(v: &Value) -> Option<Kata> {
    let n = |key| match v.get(key) {
        None | Some(Value::Null) => Some(None),
        Some(x) => x.as_u64().map(Some),
    };
    Some(Kata {
        default_memory: n("default_memory")?,
        default_vcpus: n("default_vcpus")?,
        default_maxmemory: n("default_maxmemory")?,
        default_maxvcpus: n("default_maxvcpus")?,
        initrd: v.get("initrd").and_then(Value::as_str).map(str::to_owned),
    })
}

fn firewall_of(v: &Value) -> Option<Firewall> {
    let chains = v.get("chains")?;
    let chains = NFT_CHAINS
        .iter()
        .map(|(chain, _)| match chains.get(chain) {
            None | Some(Value::Null) => Some(None),
            Some(n) => n.as_u64().and_then(|n| u32::try_from(n).ok()).map(Some),
        })
        .collect::<Option<Vec<_>>>()?;
    Some(Firewall {
        chains,
        zone: v.get("zone")?.as_str()?.to_owned(),
    })
}

fn env_of(v: &Value) -> Option<Env> {
    v.as_object()?
        .iter()
        .map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
        .collect()
}

/// The `i`th of `kinds` for a word read as its index.
fn pick<T: Copy>(f: Fact<usize>, kinds: [T; 2]) -> Fact<T> {
    match f {
        Fact::Known(i) => Fact::Known(kinds[i]),
        Fact::Absent => Fact::Absent,
        Fact::Invalid(e) => Fact::Invalid(e),
        Fact::Skipped(e) => Fact::Skipped(e),
    }
}

/// Facts from a facts file's text (or a `doctor --json` output). `mem_total_mib` and `cores` are
/// required.
pub fn from_json(text: &str) -> Result<HostFacts, String> {
    let doc = Value::parse(text).map_err(|e| format!("not JSON: {e}"))?;
    let json = doc.get("facts").unwrap_or(&doc);
    let size = |key| {
        json.get(key)
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("{key}: a whole number is required"))
    };
    let mem_total_mib = size("mem_total_mib")?;
    let cores = u32::try_from(size("cores")?).map_err(|e| format!("cores: {e}"))?;
    if mem_total_mib == 0 || cores == 0 {
        return Err("mem_total_mib and cores must be above 0".into());
    }
    let word = |key: &str, words: &[&str]| -> Result<Fact<usize>, String> {
        read(json, key, |v| {
            words.iter().position(|w| Some(*w) == v.as_str())
        })
    };
    Ok(HostFacts {
        mem_total_mib,
        cores,
        phys_bits: read(json, "phys_bits", |v| u32::try_from(v.as_u64()?).ok())?,
        virt: pick(word("virt", &["vmx", "svm"])?, [Virt::Vmx, Virt::Svm]),
        disk: pick(
            word("disk", &["rotational", "solid"])?,
            [Disk::Rotational, Disk::Solid],
        ),
        shm: read(json, "shm", shm_of)?,
        shm_used_mib: read(json, "shm_used_mib", Value::as_u64)?,
        shm_fstab: read(json, "shm_fstab", shm_of)?,
        kata: read(json, "kata", kata_of)?,
        gateway: read(json, "gateway", env_of)?,
        kvm: pick(
            word("kvm", &["usable", "no_access"])?,
            [Kvm::Usable, Kvm::NoAccess],
        ),
        initrd_net: read(json, "initrd_net", Value::as_bool)?,
        firewall: read(json, "firewall", firewall_of)?,
        tokens_mode: read(json, "tokens_mode", mode_of)?,
        allowlist_mode: read(json, "allowlist_mode", mode_of)?,
        egress_agents_mode: read(json, "egress_agents_mode", mode_of)?,
        egress_profiles: read(json, "egress_profiles", egress_profiles_of)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full() -> HostFacts {
        HostFacts {
            phys_bits: Fact::Known(36),
            virt: Fact::Known(Virt::Svm),
            disk: Fact::Known(Disk::Rotational),
            shm: Fact::Known(Shm {
                size_mib: 15717,
                options: vec!["nosuid".into(), "nodev".into()],
            }),
            shm_used_mib: Fact::Known(3),
            shm_fstab: Fact::Absent,
            kata: Fact::Known(Kata {
                default_memory: Some(2048),
                initrd: Some("/i".into()),
                ..Kata::default()
            }),
            gateway: Fact::Known([("AC_X".to_owned(), "1".to_owned())].into()),
            kvm: Fact::Known(Kvm::NoAccess),
            initrd_net: Fact::Invalid("broken".into()),
            firewall: Fact::Known(Firewall {
                chains: vec![Some(3), None, Some(1)],
                zone: "acbr0|3128/tcp|".into(),
            }),
            tokens_mode: Fact::Known(Mode {
                uid: 0,
                gid: 0,
                perm: 0o600,
            }),
            allowlist_mode: Fact::Skipped("root-only".into()),
            egress_agents_mode: Fact::Absent,
            egress_profiles: Fact::Known(EgressProfiles {
                attached: vec!["node".into(), "rust".into()],
                missing: vec!["node".into()],
                refused: vec!["rust: mode 0644".into()],
            }),
            ..HostFacts::bare(23909, 4, "x")
        }
    }

    #[test]
    fn round_trips() {
        let f = full();
        let text = to_json(&f).to_json_string_pretty();
        assert_eq!(from_json(&text), Ok(f.clone()));
        // A whole doctor --json output works too.
        let doc = object([
            ("facts".to_owned(), to_json(&f)),
            ("errors".to_owned(), Value::from(0u64)),
        ]);
        assert_eq!(from_json(&doc.to_json_string()), Ok(f));
        let bare = HostFacts::bare(1, 1, "x");
        assert_eq!(from_json(&to_json(&bare).to_json_string()), Ok(bare));
    }

    #[test]
    fn missing_keys_are_skipped_and_sizes_required() {
        let f = from_json(r#"{"mem_total_mib": 31998, "cores": 8, "disk": "solid"}"#);
        let f = f.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(f.disk, Fact::Known(Disk::Solid));
        assert_eq!(f.kata, Fact::Skipped(NOT_IN_FILE.into()));
        for bad in [
            r#"{"cores": 8}"#,
            r#"{"mem_total_mib": 1}"#,
            r#"{"mem_total_mib": 0, "cores": 1}"#,
            r#"{"mem_total_mib": 1, "cores": 1, "disk": "floppy"}"#,
            r#"{"mem_total_mib": 1, "cores": 1, "kata": {"default_memory": "2048"}}"#,
            "not json",
        ] {
            assert!(from_json(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn shipped_host_files_load() {
        for (name, text) in [
            (
                "baileyserv",
                include_str!("../../../deploy/hosts/baileyserv.json"),
            ),
            (
                "32g-8core-nvme",
                include_str!("../../../deploy/hosts/32g-8core-nvme.json"),
            ),
        ] {
            assert!(from_json(text).is_ok(), "{name}: {:?}", from_json(text));
        }
    }
}
