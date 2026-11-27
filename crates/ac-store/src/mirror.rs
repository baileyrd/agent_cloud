//! A [`SessionStore`] that keeps a plain-text copy of the agent list beside the registry, so a
//! backup can copy it while the gateway holds the data directory (ADR-021 addendum). The copy is
//! rewritten whole after every state change, atomically (temp file, fsync, rename, mode 0600), so
//! a reader always sees one consistent list.

use std::path::{Path, PathBuf};

use ac_domain::state::render_agents;
use ac_domain::{AgentId, State};
use ac_ports::{PortError, SessionStore};

use crate::err;

/// Where the gateway keeps the copy, under its data directory.
pub fn path(data: &Path) -> PathBuf {
    data.join("state").join(ac_domain::state::AGENTS)
}

/// `inner`, with the agent list mirrored to a file.
pub struct Mirrored<S> {
    inner: S,
    path: PathBuf,
}

impl<S: SessionStore> Mirrored<S> {
    /// Mirror `inner` to `path`; its directory is made (0700) on the first write.
    pub fn new(inner: S, path: impl Into<PathBuf>) -> Self {
        Self {
            inner,
            path: path.into(),
        }
    }

    /// Write the copy now, from the registry.
    pub fn sync(&self) -> Result<(), PortError> {
        let all = self.inner.list()?;
        if let Some(dir) = self.path.parent() {
            make_private_dir(dir)?;
        }
        rusty_atomic_file::write_private(&self.path, render_agents(&all).as_bytes())
            .map_err(|e| err("mirror agent list", format!("{}: {e}", self.path.display())))
    }
}

pub(crate) fn make_private_dir(dir: &Path) -> Result<(), PortError> {
    use std::os::unix::fs::DirBuilderExt;
    match std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
    {
        Ok(()) => Ok(()),
        Err(e) => Err(err("mirror agent list", format!("{}: {e}", dir.display()))),
    }
}

impl<S: SessionStore> SessionStore for Mirrored<S> {
    fn state(&self, id: &AgentId) -> Result<Option<State>, PortError> {
        self.inner.state(id)
    }

    /// The registry write is what counts: a failed copy is logged, not returned, since the state
    /// did change. The next change, or the gateway's next start, writes the whole list again.
    fn set_state(&self, id: &AgentId, state: State) -> Result<(), PortError> {
        self.inner.set_state(id, state)?;
        if let Err(e) = self.sync() {
            eprintln!(
                "ac-store: {e}; the backup copy of the agent list is stale until the next change"
            );
        }
        Ok(())
    }

    fn list(&self) -> Result<Vec<(AgentId, State)>, PortError> {
        self.inner.list()
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::tests::tmp;
    use crate::EngineStore;

    fn a(s: &str) -> AgentId {
        AgentId::parse(s).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn every_change_rewrites_the_whole_list_privately() {
        let dir = tmp();
        let store = EngineStore::open(&dir).unwrap_or_else(|e| panic!("{e}"));
        let m = Mirrored::new(store, path(&dir));
        assert!(m.set_state(&a("b2"), State::Creating).is_ok());
        assert!(m.set_state(&a("a1"), State::Running).is_ok());
        assert!(m.set_state(&a("b2"), State::Paused).is_ok());
        let text = std::fs::read_to_string(path(&dir)).unwrap_or_default();
        assert_eq!(text, "a1 running\nb2 paused\n");
        let mode = |p: &Path| std::fs::metadata(p).map(|m| m.permissions().mode() & 0o777);
        assert_eq!(mode(&path(&dir)).ok(), Some(0o600));
        assert_eq!(mode(&dir.join("state")).ok(), Some(0o700));
        assert_eq!(m.state(&a("a1")).ok().flatten(), Some(State::Running));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sync_writes_an_empty_registry_too() {
        let dir = tmp();
        let store = EngineStore::open(&dir).unwrap_or_else(|e| panic!("{e}"));
        assert!(Mirrored::new(store, path(&dir)).sync().is_ok());
        assert_eq!(
            std::fs::read_to_string(path(&dir)).ok().as_deref(),
            Some("")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
