//! Control-plane state backups (ADR-021 addendum): `ac-ctl backup` copies the agent list the
//! gateway mirrors, the tokens file and the egress agents file to `<dest>/_state/<id>/` with a
//! manifest, and `ac-ctl restore-state` puts them back into a data directory with no agents.
//! The rules (names, formats, checks) are `ac_domain::state`; this is the file I/O.

use std::io::ErrorKind;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use ac_domain::backup::{next_id, BackupId};
use ac_domain::state::{self, Entry, AGENTS, MANIFEST, TOKENS};
use ac_ports::SessionStore;
use ac_store::mirror::Mirrored;
use ac_store::EngineStore;
use rusty_sha2::{Hash, Sha256};

/// Where the files a state backup copies live on this host.
#[derive(Debug, Clone)]
pub struct Paths {
    /// The registry's data directory.
    pub data: PathBuf,
    /// The tokens file (ADR-011).
    pub tokens: PathBuf,
    /// The egress agents file (ADR-016).
    pub egress: PathBuf,
}

impl Paths {
    /// `data`, and the two files from `AC_TOKENS` and `AC_EGRESS_AGENTS` (the gateway's
    /// defaults when unset).
    pub fn from_env(data: &Path) -> Self {
        let env = |k: &str, d: &str| std::env::var_os(k).map_or_else(|| d.into(), PathBuf::from);
        Self {
            data: data.to_owned(),
            tokens: env("AC_TOKENS", "/etc/agent-cloud/tokens"),
            egress: env("AC_EGRESS_AGENTS", "/etc/agent-cloud/egress/agents"),
        }
    }

    /// The host file a state backup's `name` comes from and goes back to.
    pub fn source(&self, name: &str) -> PathBuf {
        match name {
            AGENTS => ac_store::mirror::path(&self.data),
            TOKENS => self.tokens.clone(),
            _ => self.egress.clone(),
        }
    }
}

/// A state backup that was written.
#[derive(Debug)]
pub struct Saved {
    /// Its id.
    pub id: BackupId,
    /// The files it holds.
    pub files: Vec<&'static str>,
    /// Older state backups deleted to keep `keep`.
    pub pruned: usize,
}

fn io(what: &Path, e: std::io::Error) -> String {
    format!("{}: {e}", what.display())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn digest(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn private_dir(dir: &Path) -> Result<(), String> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| io(dir, e))
}

/// Read `path`, `None` if it does not exist.
fn read_if_there(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match std::fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io(path, e)),
    }
}

/// State backups at `dest`, oldest first. A directory still named `.part` is not one.
pub fn list(dest: &Path) -> Result<Vec<BackupId>, String> {
    let dir = dest.join(state::DIR);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io(&dir, e)),
    };
    let mut ids: Vec<BackupId> = entries
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().to_str().and_then(|n| BackupId::parse(n).ok()))
        .collect();
    ids.sort();
    Ok(ids)
}

/// Copy the state to `<dest>/_state/<id>.part/`, flush it, and rename it to `<id>`; then keep the
/// newest `keep` and remove any `.part` left by a run cut short. The agent list is required (the
/// gateway writes it at start); the tokens and egress files are copied when they exist.
pub fn save(paths: &Paths, dest: &Path, keep: usize, now_secs: u64) -> Result<Saved, String> {
    let dir = dest.join(state::DIR);
    private_dir(&dir)?;
    let taken = list(dest)?;
    let id = next_id(now_secs, &taken);
    let part = dir.join(format!("{id}.part"));
    let _ = std::fs::remove_dir_all(&part);
    private_dir(&part)?;
    let written = write_files(paths, &part);
    let files = match written {
        Ok(files) => files,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&part);
            return Err(e);
        }
    };
    let done = dir.join(id.as_str());
    std::fs::rename(&part, &done).map_err(|e| io(&done, e))?;
    sync_dir(&dir)?;
    let pruned = prune(&dir, &list(dest)?, keep)?;
    Ok(Saved { id, files, pruned })
}

/// Copy each file there is into `part` and write the manifest last.
fn write_files(paths: &Paths, part: &Path) -> Result<Vec<&'static str>, String> {
    let mut entries = Vec::new();
    for name in state::FILES {
        let from = paths.source(name);
        let Some(bytes) = read_if_there(&from)? else {
            if name == AGENTS {
                return Err(format!(
                    "{} is missing: start a gateway that mirrors the agent list first",
                    from.display()
                ));
            }
            continue;
        };
        let to = part.join(name);
        rusty_atomic_file::write_private(&to, &bytes).map_err(|e| io(&to, e))?;
        entries.push(Entry {
            name,
            bytes: bytes.len() as u64,
            sha256: digest(&bytes),
        });
    }
    let manifest = part.join(MANIFEST);
    rusty_atomic_file::write_private(&manifest, state::render_manifest(&entries).as_bytes())
        .map_err(|e| io(&manifest, e))?;
    sync_dir(part)?;
    Ok(entries.iter().map(|e| e.name).collect())
}

fn sync_dir(dir: &Path) -> Result<(), String> {
    std::fs::File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(|e| io(dir, e))
}

/// Delete all but the newest `keep` of `ids`, and any leftover `.part`.
fn prune(dir: &Path, ids: &[BackupId], keep: usize) -> Result<usize, String> {
    let old = ids.len().saturating_sub(keep.max(1));
    for id in &ids[..old] {
        let path = dir.join(id.as_str());
        std::fs::remove_dir_all(&path).map_err(|e| io(&path, e))?;
    }
    let entries = std::fs::read_dir(dir).map_err(|e| io(dir, e))?;
    for e in entries.filter_map(Result::ok) {
        if e.file_name().to_string_lossy().ends_with(".part") {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
    Ok(old)
}

/// A state backup's files, read back and checked against its manifest.
fn read_backup(dir: &Path) -> Result<Vec<(Entry, Vec<u8>)>, String> {
    let manifest = dir.join(MANIFEST);
    let text = std::fs::read_to_string(&manifest).map_err(|e| io(&manifest, e))?;
    let entries = state::parse_manifest(&text).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut out = Vec::new();
    for entry in entries {
        let path = dir.join(entry.name);
        let bytes = std::fs::read(&path).map_err(|e| io(&path, e))?;
        state::verify(&entry, bytes.len() as u64, &digest(&bytes))
            .map_err(|e| format!("{}: {e}", dir.display()))?;
        out.push((entry, bytes));
    }
    Ok(out)
}

/// Put state backup `wanted` (default the newest) back. Refused while another process (the
/// gateway) holds the data directory, when the registry has an agent the backup does not hold in
/// the same state (so a restore cut short can be run again), and when a tokens or egress file is
/// already there with other content; every check runs before the first write.
pub fn restore(paths: &Paths, dest: &Path, wanted: Option<&BackupId>) -> Result<String, String> {
    let ids = list(dest)?;
    let id = match wanted {
        Some(w) => ids.iter().find(|i| *i == w),
        None => ids.last(),
    }
    .ok_or_else(|| format!("no state backup {} at {}", shown(wanted), dest.display()))?;
    let files = read_backup(&dest.join(state::DIR).join(id.as_str()))?;
    let store = EngineStore::open(&paths.data).map_err(|e| match e.op {
        ac_store::IN_USE => format!("{}: stop ac-gateway first", e.cause),
        _ => e.to_string(),
    })?;
    let agents = match files.iter().find(|(e, _)| e.name == AGENTS) {
        Some((_, bytes)) => {
            state::parse_agents(&String::from_utf8_lossy(bytes)).map_err(|e| e.to_string())?
        }
        None => Vec::new(),
    };
    let present = store.list().map_err(|e| e.to_string())?;
    if let Some((agent, _)) = present.iter().find(|p| !agents.contains(p)) {
        return Err(format!(
            "the registry in {} already has {} agent(s), {agent} not as in this backup; \
             restore-state only fills an empty registry or finishes its own restore",
            paths.data.display(),
            present.len()
        ));
    }
    for (entry, bytes) in files.iter().filter(|(e, _)| e.name != AGENTS) {
        let to = paths.source(entry.name);
        if read_if_there(&to)?.is_some_and(|there| there != *bytes) {
            return Err(format!(
                "{} is already there with other content; move it aside to restore it",
                to.display()
            ));
        }
    }
    for (entry, bytes) in files.iter().filter(|(e, _)| e.name != AGENTS) {
        let to = paths.source(entry.name);
        if let Some(parent) = to.parent() {
            private_dir(parent)?;
        }
        rusty_atomic_file::write_private(&to, bytes).map_err(|e| io(&to, e))?;
    }
    let registry = Mirrored::new(store, ac_store::mirror::path(&paths.data));
    for (agent, s) in &agents {
        registry.set_state(agent, *s).map_err(|e| e.to_string())?;
    }
    registry.sync().map_err(|e| e.to_string())?;
    let names: Vec<&str> = files.iter().map(|(e, _)| e.name).collect();
    Ok(format!(
        "restored state {id}: {} agent(s); {}. Start ac-gateway: its reconcile marks agents \
         recorded running but with no VM paused. Restore workspaces with restore-backup.",
        agents.len(),
        names.join(", ")
    ))
}

fn shown(id: Option<&BackupId>) -> String {
    id.map_or_else(|| "(newest)".to_owned(), |i| i.to_string())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use ac_domain::state::EGRESS;
    use ac_domain::{AgentId, State};

    use super::*;

    fn tmp(name: &str) -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!(
            "ac-state-{name}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn a(s: &str) -> AgentId {
        AgentId::parse(s).unwrap_or_else(|e| panic!("{e}"))
    }

    /// A host: a registry with two agents (mirrored), a tokens file and an egress file.
    fn host(root: &Path, egress: bool) -> Paths {
        let paths = Paths {
            data: root.join("data"),
            tokens: root.join("etc/tokens"),
            egress: root.join("etc/egress/agents"),
        };
        let store = EngineStore::open(&paths.data).unwrap_or_else(|e| panic!("{e}"));
        let m = Mirrored::new(store, ac_store::mirror::path(&paths.data));
        assert!(m.set_state(&a("a1"), State::Running).is_ok());
        assert!(m.set_state(&a("a2"), State::Paused).is_ok());
        let _ = std::fs::create_dir_all(root.join("etc/egress"));
        let _ = std::fs::write(&paths.tokens, "owner sha256:ab admin *\n");
        if egress {
            let _ = std::fs::write(&paths.egress, "a1 cred rust\n");
        }
        paths
    }

    /// A clean host: same layout under another root, nothing in it.
    fn clean(root: &Path) -> Paths {
        Paths {
            data: root.join("data"),
            tokens: root.join("etc/tokens"),
            egress: root.join("etc/egress/agents"),
        }
    }

    fn agents(data: &Path) -> Vec<(AgentId, State)> {
        let store = EngineStore::open(data).unwrap_or_else(|e| panic!("{e}"));
        let mut all = store.list().unwrap_or_default();
        all.sort_by(|x, y| x.0.as_str().cmp(y.0.as_str()));
        all
    }

    #[test]
    fn backup_and_restore_into_an_empty_data_dir() {
        let (src, dst, dest) = (tmp("src"), tmp("dst"), tmp("dest"));
        let from = host(&src, true);
        let saved = save(&from, &dest, 7, 1_795_000_000).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(saved.files, [AGENTS, TOKENS, EGRESS]);
        let dir = dest.join(state::DIR).join(saved.id.as_str());
        for f in [AGENTS, TOKENS, EGRESS, MANIFEST] {
            let mode = std::fs::metadata(dir.join(f)).map(|m| m.permissions().mode() & 0o777);
            assert_eq!(mode.ok(), Some(0o600), "{f}");
        }
        let to = clean(&dst);
        let said = restore(&to, &dest, None).unwrap_or_else(|e| panic!("{e}"));
        assert!(said.contains("2 agent(s)"), "{said}");
        assert_eq!(
            agents(&to.data),
            [(a("a1"), State::Running), (a("a2"), State::Paused)]
        );
        for (x, y) in [(&from.tokens, &to.tokens), (&from.egress, &to.egress)] {
            assert_eq!(std::fs::read(x).ok(), std::fs::read(y).ok());
        }
        assert_eq!(
            std::fs::read(ac_store::mirror::path(&to.data)).ok(),
            std::fs::read(ac_store::mirror::path(&from.data)).ok()
        );
        for d in [src, dst, dest] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    /// Review of PR #41: a restore cut short after recording some agents can be run again.
    #[test]
    fn a_restore_cut_short_can_be_run_again() {
        let (src, dst, dest) = (tmp("src"), tmp("dst"), tmp("dest"));
        save(&host(&src, true), &dest, 7, 1_795_000_000).unwrap_or_else(|e| panic!("{e}"));
        let to = clean(&dst);
        {
            let store = EngineStore::open(&to.data).unwrap_or_else(|e| panic!("{e}"));
            let _ = store.set_state(&a("a1"), State::Running);
        }
        let said = restore(&to, &dest, None).unwrap_or_else(|e| panic!("{e}"));
        assert!(said.contains("2 agent(s)"), "{said}");
        assert!(restore(&to, &dest, None).is_ok(), "and again");
        assert_eq!(
            agents(&to.data),
            [(a("a1"), State::Running), (a("a2"), State::Paused)]
        );
        for d in [src, dst, dest] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn missing_egress_file_is_left_out_and_retention_keeps_the_newest() {
        let (src, dest) = (tmp("src"), tmp("dest"));
        let from = host(&src, false);
        let ids: Vec<BackupId> = (0..4)
            .map(|i| save(&from, &dest, 2, 1_795_000_000 + i).map(|s| s.id))
            .collect::<Result<_, _>>()
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(list(&dest).ok(), Some(ids[2..].to_vec()));
        let manifest =
            std::fs::read_to_string(dest.join(state::DIR).join(ids[3].as_str()).join(MANIFEST))
                .unwrap_or_default();
        assert!(!manifest.contains(EGRESS), "{manifest}");
        for d in [src, dest] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn no_agent_list_means_no_backup() {
        let (src, dest) = (tmp("src"), tmp("dest"));
        let e = save(&clean(&src), &dest, 7, 1).err().unwrap_or_default();
        assert!(e.contains("start a gateway"), "{e}");
        assert_eq!(list(&dest).ok(), Some(Vec::new()));
        for d in [src, dest] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn restore_refusals_change_nothing() {
        let (src, dst, dest) = (tmp("src"), tmp("dst"), tmp("dest"));
        let from = host(&src, true);
        let id = save(&from, &dest, 7, 1_795_000_000)
            .map(|s| s.id)
            .unwrap_or_else(|e| panic!("{e}"));
        // A registry with an agent the backup does not hold.
        {
            let store = EngineStore::open(&from.data).unwrap_or_else(|e| panic!("{e}"));
            let _ = store.set_state(&a("a3"), State::Running);
        }
        let e = restore(&from, &dest, None).err().unwrap_or_default();
        assert!(
            e.contains("already has 3 agent(s), a3 not as in this backup"),
            "{e}"
        );
        // A gateway holding the data directory.
        let to = clean(&dst);
        let held = EngineStore::open(&to.data);
        let e = restore(&to, &dest, None).err().unwrap_or_default();
        assert!(e.contains("stop ac-gateway first"), "{e}");
        drop(held);
        // A tokens file with other content.
        let _ = std::fs::create_dir_all(dst.join("etc"));
        let _ = std::fs::write(&to.tokens, "someone else\n");
        let e = restore(&to, &dest, None).err().unwrap_or_default();
        assert!(e.contains("move it aside"), "{e}");
        assert!(agents(&to.data).is_empty());
        assert!(!to.egress.exists());
        let _ = std::fs::remove_file(&to.tokens);
        // A file changed at the destination.
        let tokens = dest.join(state::DIR).join(id.as_str()).join(TOKENS);
        let _ = std::fs::write(&tokens, "owner sha256:cd admin *\n");
        let e = restore(&to, &dest, Some(&id)).err().unwrap_or_default();
        assert!(e.contains("does not match"), "{e}");
        assert!(agents(&to.data).is_empty());
        // An unknown id.
        let other = BackupId::parse("20200101-000000").unwrap_or_else(|e| panic!("{e}"));
        let e = restore(&to, &dest, Some(&other)).err().unwrap_or_default();
        assert!(e.contains("no state backup 20200101-000000"), "{e}");
        for d in [src, dst, dest] {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}
