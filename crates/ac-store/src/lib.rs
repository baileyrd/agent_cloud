//! `SessionStore` backed by `rusty_multimodal_db_engine`.
//!
//! One record per agent. Its engine id is a UUIDv5 of the agent slug (the
//! engine needs a `Copy` id; the slug is validated by [`AgentId`]). The
//! lifecycle state is the engine's scannable field, so a state change is a
//! single durable field write, not a record rewrite.
//!
//! Holds only bounded registry data; unbounded history belongs in `ac-history`.

use std::path::Path;

use ac_domain::{AgentId, State};
use ac_ports::{PortError, SessionStore};
use rusty_multimodal_db_engine::dir_lock::DirLock;
use rusty_multimodal_db_engine::generic::mmap_store::GenericMmapStore;
use rusty_multimodal_db_engine::generic::production::GenericProductionStore;
use rusty_multimodal_db_engine::generic::traits::{
    IndexedField, Record, ScannableField, SchemaTag,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const NAMESPACE: Uuid = Uuid::from_u128(0x6163_2d63_6c6f_7564_0000_0000_0000_0001);
const STORE_FILE: &str = "agents.mmap";

#[derive(Clone, Serialize, Deserialize)]
struct AgentRecord {
    id: Uuid,
    slug: String,
    state: u32,
}

struct BySlug;
struct ByState;

impl Record for AgentRecord {
    type Id = Uuid;
    fn id(&self) -> Uuid {
        self.id
    }
}

// On-disk format name: changing the record layout means a new tag and a migration.
impl SchemaTag for AgentRecord {
    const SCHEMA_TAG: &'static str = "agent_cloud::AgentRecord@1";
}

impl IndexedField<BySlug> for AgentRecord {
    type IndexValue = String;
    fn indexed_value(&self) -> &String {
        &self.slug
    }
}

impl ScannableField<ByState> for AgentRecord {
    type ScanValue = u32;
    fn scannable_value(&self) -> u32 {
        self.state
    }
    fn set_scannable_value(&mut self, value: u32) {
        self.state = value;
    }
}

fn code(state: State) -> u32 {
    match state {
        State::Creating => 0,
        State::Running => 1,
        State::Paused => 2,
        State::Destroyed => 3,
    }
}

fn from_code(code: u32) -> Option<State> {
    Some(match code {
        0 => State::Creating,
        1 => State::Running,
        2 => State::Paused,
        3 => State::Destroyed,
        _ => return None,
    })
}

fn err(op: &'static str, cause: impl std::fmt::Display) -> PortError {
    PortError {
        op,
        cause: cause.to_string(),
    }
}

fn uuid_of(id: &AgentId) -> Uuid {
    Uuid::new_v5(&NAMESPACE, id.as_str().as_bytes())
}

/// Engine-backed registry of agents and their lifecycle state.
pub struct EngineStore {
    store: GenericProductionStore<GenericMmapStore<AgentRecord, BySlug, ByState>>,
    // Held for its Drop: refuses a second process on the same data directory.
    _lock: DirLock,
}

impl EngineStore {
    /// Open (or create) the registry in `dir`. Fails if another process holds it.
    pub fn open(dir: &Path) -> Result<Self, PortError> {
        let lock = DirLock::acquire(dir, "agents.lock").map_err(|e| err("lock data dir", e))?;
        let path = dir.join(STORE_FILE);
        let core = if path.exists() {
            GenericMmapStore::open_portable(&path)
        } else {
            GenericMmapStore::create(Vec::new(), &path)
        }
        .map_err(|e| err("open store", e))?;
        Ok(Self {
            store: GenericProductionStore::new(core),
            _lock: lock,
        })
    }

    fn record(&self, id: &AgentId) -> Result<Option<AgentRecord>, PortError> {
        let Some(rec) = self.store.get::<AgentRecord>(uuid_of(id)) else {
            return Ok(None);
        };
        if rec.slug != id.as_str() {
            return Err(err(
                "lookup",
                format!("id collision between {} and {id}", rec.slug),
            ));
        }
        Ok(Some(rec))
    }
}

impl SessionStore for EngineStore {
    fn state(&self, id: &AgentId) -> Result<Option<State>, PortError> {
        let Some(rec) = self.record(id)? else {
            return Ok(None);
        };
        from_code(rec.state)
            .map(Some)
            .ok_or_else(|| err("decode state", format!("unknown state code {}", rec.state)))
    }

    fn set_state(&self, id: &AgentId, state: State) -> Result<(), PortError> {
        if self.record(id)?.is_some() {
            return self
                .store
                .update::<AgentRecord, ByState>(uuid_of(id), code(state))
                .map_err(|e| err("update state", format!("{e:?}")));
        }
        let rec = AgentRecord {
            id: uuid_of(id),
            slug: id.to_string(),
            state: code(state),
        };
        self.store
            .insert(rec)
            .map_err(|e| err("insert agent", format!("{e:?}")))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn tmp() -> std::path::PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        std::env::temp_dir().join(format!(
            "ac-store-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn id(s: &str) -> AgentId {
        AgentId::parse(s).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn unknown_then_insert_then_update() {
        let dir = tmp();
        let s = EngineStore::open(&dir).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(s.state(&id("a1")).ok().flatten(), None);
        assert!(s.set_state(&id("a1"), State::Creating).is_ok());
        assert!(s.set_state(&id("a1"), State::Running).is_ok());
        assert_eq!(s.state(&id("a1")).ok().flatten(), Some(State::Running));
        assert_eq!(s.state(&id("a2")).ok().flatten(), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn state_survives_reopen() {
        let dir = tmp();
        {
            let s = EngineStore::open(&dir).unwrap_or_else(|e| panic!("{e}"));
            assert!(s.set_state(&id("a1"), State::Creating).is_ok());
            assert!(s.set_state(&id("a1"), State::Paused).is_ok());
            assert!(s.set_state(&id("b2"), State::Running).is_ok());
        }
        let s = EngineStore::open(&dir).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(s.state(&id("a1")).ok().flatten(), Some(State::Paused));
        assert_eq!(s.state(&id("b2")).ok().flatten(), Some(State::Running));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn second_open_of_same_dir_is_refused() {
        let dir = tmp();
        let _first = EngineStore::open(&dir).unwrap_or_else(|e| panic!("{e}"));
        assert!(EngineStore::open(&dir).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn state_codes_roundtrip() {
        for s in [
            State::Creating,
            State::Running,
            State::Paused,
            State::Destroyed,
        ] {
            assert_eq!(from_code(code(s)), Some(s));
        }
        assert_eq!(from_code(99), None);
    }
}
