//! `ac-ctl tune` (ADR-014): the edits doctor recommends, shown as diffs and, with `--apply`, made.
//! It edits only the keys it owns (Kata's `default_memory`, `default_vcpus`, `default_maxmemory`;
//! the gateway.env keys of the rules and `AC_PROFILE`; `size=` of the `/dev/shm` fstab line),
//! keeps a timestamped copy of every file it changes, and never reboots, remounts, rebuilds the
//! initrd or touches the firewall or units: those it prints as manual steps.

use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use ac_domain::tune::edit::{diff, env_set, fstab_shm, kata_set};
use ac_domain::tune::rules::{fix, Change};
use ac_domain::tune::{Finding, Profile, Status};

/// One file's edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEdit {
    /// The file.
    pub path: PathBuf,
    /// Its text now.
    pub old: String,
    /// Its text after the edit.
    pub new: String,
}

/// What tune would do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// The edits, one per file that changes.
    pub edits: Vec<FileEdit>,
    /// What needs a person, one line each.
    pub manual: Vec<String>,
    /// Whether Kata's configuration changes (running VMs keep the old settings).
    pub kata: bool,
    /// The new `/dev/shm` size, when the fstab line changes.
    pub shm_mib: Option<u64>,
    /// Whether the gateway's settings change.
    pub gateway: bool,
}

/// The texts of the files tune edits, or why one could not be read.
#[derive(Debug, Clone)]
pub struct Texts {
    /// Kata's configuration.
    pub kata: (PathBuf, Result<String, String>),
    /// gateway.env.
    pub env: (PathBuf, Result<String, String>),
    /// /etc/fstab.
    pub fstab: (PathBuf, Result<String, String>),
}

/// The edits for `findings` (those at warn or error that name a [`Change`]), plus `AC_PROFILE`
/// when `profile` differs from the configured one; `shm_options` are the running mount's, for a
/// new fstab line.
pub fn plan(
    findings: &[Finding],
    profile: Option<(Profile, Profile)>,
    texts: &Texts,
    shm_options: &[String],
) -> Plan {
    let mut plan = Plan::default();
    let mut kata: Vec<(&'static str, u64)> = Vec::new();
    let mut env: BTreeMap<&'static str, String> = BTreeMap::new();
    let mut shm = None;
    for f in findings.iter().filter(|f| f.status >= Status::Warn) {
        match &f.change {
            Some(Change::Kata(key, value)) => kata.push((key, *value)),
            Some(Change::Env(key, value)) => {
                env.insert(key, value.clone());
            }
            Some(Change::ShmFstab(size)) => shm = Some(*size),
            None => plan.manual.push(format!(
                "{} ({}): {} -> {}: {}\n    fix: {}",
                f.setting,
                f.rule,
                f.current,
                f.recommended,
                f.reason,
                fix(f.rule)
            )),
        }
    }
    if let Some((want, configured)) = profile.filter(|(w, c)| w != c) {
        env.insert("AC_PROFILE", want.name().to_owned());
        plan.manual.push(format!(
            "profile: {} -> {} (AC_PROFILE, so doctor and the gateway judge against it)",
            configured.name(),
            want.name()
        ));
    }
    let mut edit = |(path, text): &(PathBuf, Result<String, String>),
                    wanted: bool,
                    apply: &dyn Fn(&str) -> Result<String, String>| {
        if !wanted {
            return false;
        }
        let new = text
            .as_ref()
            .map_err(String::clone)
            .and_then(|old| Ok((old, apply(old)?)));
        match new {
            Ok((old, new)) if *old == new => false,
            Ok((old, new)) => {
                plan.edits.push(FileEdit {
                    path: path.clone(),
                    old: old.clone(),
                    new,
                });
                true
            }
            Err(why) => {
                plan.manual
                    .push(format!("cannot edit {}: {why}", path.display()));
                false
            }
        }
    };
    let kata = edit(&texts.kata, !kata.is_empty(), &|old| {
        kata.iter()
            .try_fold(old.to_owned(), |t, (k, v)| kata_set(&t, k, *v))
    });
    let gateway = edit(&texts.env, !env.is_empty(), &|old| {
        Ok(env
            .iter()
            .fold(old.to_owned(), |t, (k, v)| env_set(&t, k, v)))
    });
    let fstab = edit(&texts.fstab, shm.is_some(), &|old| {
        Ok(fstab_shm(old, shm.unwrap_or_default(), shm_options))
    });
    Plan {
        kata,
        gateway,
        shm_mib: shm.filter(|_| fstab),
        ..plan
    }
}

/// The plan as diffs, then the manual steps.
pub fn render(plan: &Plan) -> String {
    let mut out = String::new();
    for e in &plan.edits {
        out.push_str(&diff(&e.path.display().to_string(), &e.old, &e.new));
    }
    if plan.edits.is_empty() {
        out.push_str(
            "no changes: every setting tune owns, in the files it could read, is as recommended\n",
        );
    }
    for m in &plan.manual {
        out.push_str(&format!("manual: {m}\n"));
    }
    out
}

/// Why `--apply` must not run now, if it must not: a Kata VM runs (unless `force`), or the new
/// `/dev/shm` is smaller than what is in use.
pub fn refusal(
    plan: &Plan,
    running: &[String],
    force: bool,
    shm_used_mib: Option<u64>,
) -> Option<String> {
    if !running.is_empty() && !force && !plan.edits.is_empty() {
        return Some(format!(
            "Kata VMs are running ({}): pause them first, or --force (they keep the old settings until \
             paused and resumed)",
            running.join(", ")
        ));
    }
    match (plan.shm_mib, shm_used_mib) {
        (Some(size), Some(used)) if size < used => Some(format!(
            "/dev/shm would be {size} MiB, below the {used} MiB in use now"
        )),
        _ => None,
    }
}

/// What to do after applying, one line each.
pub fn afterwards(plan: &Plan, running: &[String], restart: bool) -> Vec<String> {
    let mut out = Vec::new();
    if plan.kata {
        out.push(if running.is_empty() {
            "kata: no Kata VM is running; the next create or resume uses the new settings"
                .to_owned()
        } else {
            format!(
                "kata: these agents keep the old settings until paused and resumed: {}",
                running.join(", ")
            )
        });
    }
    if let Some(size) = plan.shm_mib {
        out.push(format!(
            "/dev/shm: takes effect at the next boot, or now with (not run): sudo mount -o \
             remount,size={size}M /dev/shm"
        ));
    }
    if (plan.kata || plan.gateway) && !restart {
        out.push(
            "gateway: reads these at startup: sudo systemctl restart ac-gateway (or tune --apply \
             --restart-gateway)"
                .to_owned(),
        );
    }
    out
}

/// Make the edits: each file is checked unchanged since it was read, copied to
/// `<file>.ac-tune-<stamp>`, then replaced atomically with its mode and owner kept. Returns the
/// backups.
pub fn apply(plan: &Plan, stamp: &str) -> Result<Vec<PathBuf>, String> {
    let mut backups = Vec::new();
    for e in &plan.edits {
        let fail = |what: &str, err: std::io::Error| format!("{what} {}: {err}", e.path.display());
        let now = std::fs::read_to_string(&e.path).map_err(|err| fail("read", err))?;
        if now != e.old {
            return Err(format!(
                "{} changed while tune ran; nothing more written",
                e.path.display()
            ));
        }
        let meta = std::fs::metadata(&e.path).map_err(|err| fail("stat", err))?;
        let backup = PathBuf::from(format!("{}.ac-tune-{stamp}", e.path.display()));
        std::fs::copy(&e.path, &backup).map_err(|err| fail("back up", err))?;
        backups.push(backup);
        write_like(&e.path, &e.new, &meta).map_err(|err| fail("write", err))?;
    }
    Ok(backups)
}

/// Replace `path` atomically with `text`, keeping `meta`'s mode and owner. A file no one else may
/// read is never readable while it is written.
fn write_like(path: &Path, text: &str, meta: &std::fs::Metadata) -> std::io::Result<()> {
    let mode = meta.mode() & 0o7777;
    if mode & 0o077 == 0 {
        rusty_atomic_file::write_private(path, text.as_bytes())?;
    } else {
        rusty_atomic_file::write(path, text.as_bytes())?;
    }
    std::os::unix::fs::chown(path, Some(meta.uid()), Some(meta.gid()))?;
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(mode))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ac_domain::tune::facts::{self, Fact};
    use ac_domain::tune::recommend;

    const KATA: &str = "# Kata\n[hypervisor.qemu]\n# keep me\ninitrd = \"/i\"\n#default_vcpus = 1\n\
                        default_memory = 4096 # MiB\nunknown_key = \"x\"\n\n[agent.kata]\nfoo = 1\n";
    const ENV: &str = "# settings\nAC_IMAGE=localhost/agent-shell:dev\nAC_BIND=127.0.0.1:8780\n";
    const FSTAB: &str = "UUID=x / btrfs subvol=root,compress=zstd:1 0 0\n";

    /// This host's shape with a fresh Kata config, no PSI setting and no fstab line.
    fn untuned() -> ac_domain::tune::HostFacts {
        let mut f =
            crate::factsjson::from_json(include_str!("../../../deploy/hosts/baileyserv.json"))
                .unwrap_or_else(|e| panic!("{e}"));
        f.kata = Fact::Known(facts::kata(KATA).unwrap_or_default());
        f.gateway = Fact::Known(facts::env_file(ENV));
        f.shm_fstab = Fact::Absent;
        f
    }

    fn texts(dir: &Path) -> Texts {
        let at = |name: &str, text: &str| {
            let p = dir.join(name);
            std::fs::write(&p, text).unwrap_or_else(|e| panic!("{e}"));
            (p, Ok(text.to_owned()))
        };
        Texts {
            kata: at("configuration.toml", KATA),
            env: at("gateway.env", ENV),
            fstab: at("fstab", FSTAB),
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ac-tune-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap_or_else(|e| panic!("{e}"));
        d
    }

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn plans_applies_backs_up_and_is_idempotent() {
        let dir = scratch("apply");
        let t = texts(&dir);
        let opts = vec!["nosuid".to_owned(), "nodev".to_owned()];
        let f = untuned();
        let p = plan(&recommend(&f, Profile::Build), None, &t, &opts);
        assert_eq!(
            (p.edits.len(), p.kata, p.gateway, p.shm_mib),
            (3, true, true, Some(15717))
        );
        let shown = render(&p);
        for line in [
            "-default_memory = 4096 # MiB\n",
            "+default_memory = 2048 # MiB\n",
        ] {
            assert!(shown.contains(line), "{line}: {shown}");
        }
        assert!(shown.contains("+default_vcpus = 2\n"), "{shown}");
        assert!(shown.contains("+AC_JOB_CPU_PSI_MAX=20\n"), "{shown}");
        assert!(
            shown.contains("+tmpfs  /dev/shm  tmpfs  nosuid,nodev,size=15717M  0  0\n"),
            "{shown}"
        );
        std::fs::set_permissions(
            &t.env.0,
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let backups = apply(&p, "20261023T120000Z").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(backups.len(), 3);
        for (b, original) in backups.iter().zip([KATA, ENV, FSTAB]) {
            assert!(
                b.to_string_lossy().ends_with(".ac-tune-20261023T120000Z"),
                "{}",
                b.display()
            );
            assert_eq!(read(b), original);
        }
        // Comments and keys tune does not own are untouched; the mode is kept.
        let kata = read(&t.kata.0);
        for kept in [
            "# Kata\n",
            "# keep me\n",
            "unknown_key = \"x\"\n",
            "[agent.kata]\nfoo = 1\n",
            "#default_vcpus = 1\ndefault_vcpus = 2\n",
        ] {
            assert!(kata.contains(kept), "{kept}: {kata}");
        }
        let mode = std::fs::metadata(&t.env.0)
            .map(|m| m.mode() & 0o777)
            .unwrap_or_default();
        assert_eq!(mode, 0o600);
        // A second run on the result changes nothing.
        let again = Texts {
            kata: (t.kata.0.clone(), Ok(kata.clone())),
            env: (t.env.0.clone(), Ok(read(&t.env.0))),
            fstab: (t.fstab.0.clone(), Ok(read(&t.fstab.0))),
        };
        let mut f2 = untuned();
        f2.kata = Fact::Known(facts::kata(&kata).unwrap_or_default());
        f2.gateway = Fact::Known(facts::env_file(&read(&t.env.0)));
        f2.shm_fstab = Fact::Known(
            facts::shm_entry(&read(&t.fstab.0), f2.mem_total_mib)
                .ok()
                .flatten()
                .unwrap_or_else(|| panic!("no line")),
        );
        let p2 = plan(&recommend(&f2, Profile::Build), None, &again, &opts);
        assert_eq!(p2.edits, vec![], "{}", render(&p2));
        assert!(render(&p2).starts_with("no changes"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn profile_and_manual_steps() {
        let dir = scratch("profile");
        let t = texts(&dir);
        let mut f = untuned();
        f.initrd_net = Fact::Known(false);
        let p = plan(
            &recommend(&f, Profile::Heavy),
            Some((Profile::Heavy, Profile::Build)),
            &t,
            &[],
        );
        let env = p
            .edits
            .iter()
            .find(|e| e.path == t.env.0)
            .map(|e| e.new.clone())
            .unwrap_or_default();
        assert!(env.contains("AC_PROFILE=heavy\n"), "{env}");
        assert!(
            p.manual
                .iter()
                .any(|m| m.contains("sudo scripts/fix-kata-initrd.sh")),
            "{:?}",
            p.manual
        );
        // Same profile as configured: no AC_PROFILE line.
        let same = plan(
            &recommend(&f, Profile::Build),
            Some((Profile::Build, Profile::Build)),
            &t,
            &[],
        );
        assert!(!same.edits.iter().any(|e| e.new.contains("AC_PROFILE")));
        // An unreadable file is a manual step, not an edit.
        let blind = Texts {
            env: (t.env.0.clone(), Err("root-only: run with sudo".into())),
            ..t.clone()
        };
        let p = plan(&recommend(&f, Profile::Build), None, &blind, &[]);
        assert!(
            !p.gateway
                && p.manual
                    .iter()
                    .any(|m| m.contains("cannot edit") && m.contains("root-only"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refusals() {
        let dir = scratch("refuse");
        let t = texts(&dir);
        let p = plan(&recommend(&untuned(), Profile::Build), None, &t, &[]);
        let running = vec!["a1".to_owned()];
        assert!(refusal(&p, &running, false, Some(0)).is_some_and(|r| r.contains("a1")));
        assert_eq!(refusal(&p, &running, true, Some(0)), None);
        assert_eq!(refusal(&p, &[], false, Some(0)), None);
        assert!(refusal(&p, &[], false, Some(20000)).is_some_and(|r| r.contains("below")));
        // Nothing to change: nothing to refuse.
        assert_eq!(refusal(&Plan::default(), &running, false, None), None);
        // The file changed after it was read: nothing written.
        std::fs::write(&t.kata.0, "changed").unwrap_or_else(|e| panic!("{e}"));
        assert!(apply(&p, "s").is_err_and(|e| e.contains("changed while tune ran")));
        assert_eq!(read(&t.kata.0), "changed");
        assert!(!dir.join("configuration.toml.ac-tune-s").exists());
        let after = afterwards(&p, &running, false);
        assert!(after.iter().any(|l| l.contains("paused and resumed: a1")));
        assert!(after
            .iter()
            .any(|l| l.contains("mount -o remount,size=15717M /dev/shm")));
        assert!(after
            .iter()
            .any(|l| l.contains("systemctl restart ac-gateway")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
