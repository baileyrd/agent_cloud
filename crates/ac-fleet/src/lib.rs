//! Applies the lifecycle state machine through the ports. Pure orchestration:
//! every check happens before any side effect, so an illegal request never
//! reaches the runtime.

use std::fmt;

use ac_domain::{AgentId, Command, State, TransitionError};
use ac_ports::{PortError, Runtime, SessionStore};

/// Why a fleet operation failed.
#[derive(Debug)]
pub enum FleetError {
    /// `create` for an id that already has a record.
    AlreadyExists(AgentId),
    /// Operation on an id with no record.
    NotFound(AgentId),
    /// The lifecycle forbids this command now.
    Illegal(TransitionError),
    /// The runtime or store failed; state is left as last persisted.
    Port(PortError),
}

impl fmt::Display for FleetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyExists(id) => write!(f, "agent {id} already exists"),
            Self::NotFound(id) => write!(f, "agent {id} not found"),
            Self::Illegal(e) => e.fmt(f),
            Self::Port(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for FleetError {}

impl From<PortError> for FleetError {
    fn from(e: PortError) -> Self {
        Self::Port(e)
    }
}

impl From<TransitionError> for FleetError {
    fn from(e: TransitionError) -> Self {
        Self::Illegal(e)
    }
}

/// The set of agents, driven through a runtime and a store.
pub struct Fleet<R, S> {
    runtime: R,
    store: S,
}

impl<R: Runtime, S: SessionStore> Fleet<R, S> {
    /// Compose a fleet from its two adapters.
    pub fn new(runtime: R, store: S) -> Self {
        Self { runtime, store }
    }

    /// Provision a new agent. A runtime failure leaves it `Creating`
    /// (visible and destroyable) rather than silently forgotten.
    pub fn create(&self, id: &AgentId) -> Result<State, FleetError> {
        if self.store.state(id)?.is_some() {
            return Err(FleetError::AlreadyExists(id.clone()));
        }
        self.store.set_state(id, State::Creating)?;
        self.runtime.create(id)?;
        self.persist(id, State::Creating, Command::Ready)
    }

    /// Stop the agent's VM, keeping its volume.
    pub fn pause(&self, id: &AgentId) -> Result<State, FleetError> {
        self.step(id, Command::Pause, |r| r.pause(id))
    }

    /// Start a paused agent.
    pub fn resume(&self, id: &AgentId) -> Result<State, FleetError> {
        self.step(id, Command::Resume, |r| r.resume(id))
    }

    /// Tear the agent down; its record stays as `Destroyed`.
    pub fn destroy(&self, id: &AgentId) -> Result<State, FleetError> {
        self.step(id, Command::Destroy, |r| r.destroy(id))
    }

    fn step(
        &self,
        id: &AgentId,
        command: Command,
        act: impl FnOnce(&R) -> Result<(), PortError>,
    ) -> Result<State, FleetError> {
        let from = self
            .store
            .state(id)?
            .ok_or_else(|| FleetError::NotFound(id.clone()))?;
        let next = from.apply(command)?;
        act(&self.runtime)?;
        self.store.set_state(id, next)?;
        Ok(next)
    }

    fn persist(&self, id: &AgentId, from: State, command: Command) -> Result<State, FleetError> {
        let next = from.apply(command)?;
        self.store.set_state(id, next)?;
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use super::*;

    #[derive(Default)]
    struct FakeStore(RefCell<HashMap<String, State>>);

    impl SessionStore for FakeStore {
        fn state(&self, id: &AgentId) -> Result<Option<State>, PortError> {
            Ok(self.0.borrow().get(id.as_str()).copied())
        }
        fn set_state(&self, id: &AgentId, s: State) -> Result<(), PortError> {
            self.0.borrow_mut().insert(id.to_string(), s);
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeRuntime {
        calls: RefCell<Vec<&'static str>>,
        fail: bool,
    }

    impl FakeRuntime {
        fn call(&self, op: &'static str) -> Result<(), PortError> {
            self.calls.borrow_mut().push(op);
            if self.fail {
                return Err(PortError {
                    op,
                    cause: "boom".into(),
                });
            }
            Ok(())
        }
    }

    impl Runtime for &FakeRuntime {
        fn create(&self, _: &AgentId) -> Result<(), PortError> {
            self.call("create")
        }
        fn pause(&self, _: &AgentId) -> Result<(), PortError> {
            self.call("pause")
        }
        fn resume(&self, _: &AgentId) -> Result<(), PortError> {
            self.call("resume")
        }
        fn destroy(&self, _: &AgentId) -> Result<(), PortError> {
            self.call("destroy")
        }
    }

    fn id() -> AgentId {
        AgentId::parse("a1").unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn full_lifecycle() {
        let rt = FakeRuntime::default();
        let fleet = Fleet::new(&rt, FakeStore::default());
        assert!(matches!(fleet.create(&id()), Ok(State::Running)));
        assert!(matches!(fleet.pause(&id()), Ok(State::Paused)));
        assert!(matches!(fleet.resume(&id()), Ok(State::Running)));
        assert!(matches!(fleet.destroy(&id()), Ok(State::Destroyed)));
        assert_eq!(*rt.calls.borrow(), ["create", "pause", "resume", "destroy"]);
    }

    #[test]
    fn duplicate_create_refused_without_runtime_call() {
        let rt = FakeRuntime::default();
        let fleet = Fleet::new(&rt, FakeStore::default());
        let _ = fleet.create(&id());
        assert!(matches!(
            fleet.create(&id()),
            Err(FleetError::AlreadyExists(_))
        ));
        assert_eq!(rt.calls.borrow().len(), 1);
    }

    #[test]
    fn illegal_and_unknown_never_reach_runtime() {
        let rt = FakeRuntime::default();
        let fleet = Fleet::new(&rt, FakeStore::default());
        assert!(matches!(fleet.pause(&id()), Err(FleetError::NotFound(_))));
        let _ = fleet.create(&id());
        assert!(matches!(fleet.resume(&id()), Err(FleetError::Illegal(_))));
        assert_eq!(*rt.calls.borrow(), ["create"]);
    }

    #[test]
    fn runtime_failure_leaves_state_unchanged() {
        let rt = FakeRuntime {
            fail: true,
            ..Default::default()
        };
        let store = FakeStore::default();
        let fleet = Fleet::new(&rt, store);
        assert!(matches!(fleet.create(&id()), Err(FleetError::Port(_))));
        assert_eq!(
            fleet.store.state(&id()).ok().flatten(),
            Some(State::Creating)
        );
    }
}
