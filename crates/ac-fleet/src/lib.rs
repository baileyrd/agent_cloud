//! Applies the lifecycle state machine through the ports. Pure orchestration:
//! every check happens before any side effect, so an illegal request never
//! reaches the runtime. With [`Fleet::with_admission`], a create or resume is also refused when
//! its guest would not fit `/dev/shm` ([`ac_domain::capacity::admit`]).

use std::fmt;

use ac_domain::{capacity, AgentId, Command, Shortfall, State, TransitionError};
use ac_ports::{Capacity, PortError, Runtime, SessionStore};

/// Why a fleet operation failed.
#[derive(Debug)]
pub enum FleetError {
    /// `create` for an id that already has a record.
    AlreadyExists(AgentId),
    /// Operation on an id with no record.
    NotFound(AgentId),
    /// The lifecycle forbids this command now.
    Illegal(TransitionError),
    /// Its guest would not fit `/dev/shm`; nothing was changed.
    NoCapacity(Shortfall),
    /// The runtime or store failed; state is left as last persisted.
    Port(PortError),
}

impl fmt::Display for FleetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyExists(id) => write!(f, "agent {id} already exists"),
            Self::NotFound(id) => write!(f, "agent {id} not found"),
            Self::Illegal(e) => e.fmt(f),
            Self::NoCapacity(e) => e.fmt(f),
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

/// Admission against `/dev/shm`: where to read it, and the margin to keep free.
struct Admission {
    capacity: Box<dyn Capacity + Send>,
    margin_mib: u64,
}

/// What admission sees now (`GET /api/v1/capacity`), in MiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    /// `/dev/shm` size.
    pub shm_total_mib: u64,
    /// `/dev/shm` free.
    pub shm_free_mib: u64,
    /// Configured memory of one guest.
    pub guest_mib: u64,
    /// Agents running or being created.
    pub running_agents: u64,
    /// Their guests at configured size.
    pub committed_mib: u64,
    /// How many more guests would be admitted now.
    pub max_more_agents: u64,
    /// The margin kept free.
    pub margin_mib: u64,
}

/// The set of agents, driven through a runtime and a store.
pub struct Fleet<R, S> {
    runtime: R,
    store: S,
    admission: Option<Admission>,
}

impl<R: Runtime, S: SessionStore> Fleet<R, S> {
    /// Compose a fleet from its two adapters.
    pub fn new(runtime: R, store: S) -> Self {
        Self {
            runtime,
            store,
            admission: None,
        }
    }

    /// Refuse a create or resume whose guest would not fit `/dev/shm` with `margin_mib` to spare.
    /// Every guest running or being created counts at its configured size, since touched guest
    /// pages are never given back. The caller serializes lifecycle calls (the gateway holds one
    /// lock across them), so the check and the start cannot interleave with another's.
    pub fn with_admission(mut self, capacity: Box<dyn Capacity + Send>, margin_mib: u64) -> Self {
        self.admission = Some(Admission {
            capacity,
            margin_mib,
        });
        self
    }

    /// What admission sees now; `None` without admission control.
    pub fn capacity(&self) -> Result<Option<Report>, FleetError> {
        match &self.admission {
            Some(a) => Ok(Some(self.report(a, None)?)),
            None => Ok(None),
        }
    }

    /// Current state of an agent, `None` if unknown.
    pub fn state(&self, id: &AgentId) -> Result<Option<State>, FleetError> {
        Ok(self.store.state(id)?)
    }

    /// Every known agent and its state, sorted by id.
    pub fn list(&self) -> Result<Vec<(AgentId, State)>, FleetError> {
        let mut all = self.store.list()?;
        all.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        Ok(all)
    }

    /// Provision a new agent, or re-create a destroyed one. A runtime failure leaves it `Creating`
    /// (retry with `create`, or clear it with `destroy`)
    /// (visible and destroyable) rather than silently forgotten.
    pub fn create(&self, id: &AgentId) -> Result<State, FleetError> {
        // Only a running or paused agent blocks the id. A destroyed one can be reused, and one
        // stuck in `Creating` (a previous create failed) is retried.
        if matches!(self.store.state(id)?, Some(State::Running | State::Paused)) {
            return Err(FleetError::AlreadyExists(id.clone()));
        }
        self.admit(id)?;
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
        let from = self
            .store
            .state(id)?
            .ok_or_else(|| FleetError::NotFound(id.clone()))?;
        from.apply(Command::Resume)?;
        self.admit(id)?;
        self.step(id, Command::Resume, |r| r.resume(id))
    }

    /// Tear the agent down, keeping its workspace; its record stays as `Destroyed`. `purge` also
    /// removes the workspace, and is allowed on an agent that is already destroyed.
    pub fn destroy(&self, id: &AgentId, purge: bool) -> Result<State, FleetError> {
        if purge && self.store.state(id)? == Some(State::Destroyed) {
            self.runtime.destroy(id, true)?;
            return Ok(State::Destroyed);
        }
        self.step(id, Command::Destroy, |r| r.destroy(id, purge))
    }

    /// Bring the registry in line with the runtime after a host reboot or a VM crash: an agent
    /// recorded `Running` whose VM is gone has what it left behind (network, Kata state) cleaned
    /// up and becomes `Paused`, so `resume` boots it again on its workspace. Returns those
    /// agents. Stops at the first failure; running it again is safe.
    pub fn reconcile(&self) -> Result<Vec<AgentId>, FleetError> {
        let live = self.runtime.running()?;
        let mut changed = Vec::new();
        for (id, state) in self.store.list()? {
            if state == State::Running && !live.contains(&id) {
                self.runtime.pause(&id)?;
                self.persist(&id, state, Command::Pause)?;
                changed.push(id);
            }
        }
        Ok(changed)
    }

    /// Admit `id`'s guest on top of every other running or creating one.
    fn admit(&self, id: &AgentId) -> Result<(), FleetError> {
        let Some(a) = &self.admission else {
            return Ok(());
        };
        let r = self.report(a, Some(id))?;
        capacity::admit(
            r.committed_mib,
            r.guest_mib,
            r.shm_total_mib,
            r.shm_free_mib,
            a.margin_mib,
        )
        .map_err(FleetError::NoCapacity)
    }

    /// The figures, counting every agent running or creating except `without`.
    fn report(&self, a: &Admission, without: Option<&AgentId>) -> Result<Report, FleetError> {
        let shm = a.capacity.shm()?;
        let guest_mib = a.capacity.guest_mib()?;
        let running = self
            .store
            .list()?
            .into_iter()
            .filter(|(id, s)| matches!(s, State::Running | State::Creating) && Some(id) != without)
            .count() as u64;
        let committed_mib = running.saturating_mul(guest_mib);
        Ok(Report {
            shm_total_mib: shm.total_mib,
            shm_free_mib: shm.free_mib,
            guest_mib,
            running_agents: running,
            committed_mib,
            max_more_agents: capacity::more(
                committed_mib,
                guest_mib,
                shm.total_mib,
                shm.free_mib,
                a.margin_mib,
            ),
            margin_mib: a.margin_mib,
        })
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
        fn list(&self) -> Result<Vec<(AgentId, State)>, PortError> {
            let all = self.0.borrow();
            Ok(all.iter().map(|(k, s)| (agent(k), *s)).collect())
        }
    }

    #[derive(Default)]
    struct FakeRuntime {
        calls: RefCell<Vec<&'static str>>,
        fail: bool,
        /// What `running` reports.
        live: Vec<AgentId>,
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
        fn destroy(&self, _: &AgentId, purge: bool) -> Result<(), PortError> {
            self.call(if purge { "purge" } else { "destroy" })
        }
        fn running(&self) -> Result<Vec<AgentId>, PortError> {
            self.call("running").map(|()| self.live.clone())
        }
    }

    fn agent(s: &str) -> AgentId {
        AgentId::parse(s).unwrap_or_else(|e| panic!("{e}"))
    }

    fn id() -> AgentId {
        agent("a1")
    }

    #[test]
    fn reconcile_pauses_running_agents_whose_vm_is_gone() {
        let store = FakeStore::default();
        for (name, state) in [
            ("up", State::Running),
            ("gone", State::Running),
            ("asleep", State::Paused),
            ("dead", State::Destroyed),
            ("half", State::Creating),
        ] {
            let _ = store.set_state(&agent(name), state);
        }
        let rt = FakeRuntime {
            live: vec![agent("up")],
            ..Default::default()
        };
        let fleet = Fleet::new(&rt, store);
        let changed = fleet.reconcile().unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(changed, [agent("gone")]);
        assert_eq!(*rt.calls.borrow(), ["running", "pause"]);
        let state = |n| fleet.state(&agent(n)).ok().flatten();
        assert_eq!(state("gone"), Some(State::Paused));
        assert_eq!(state("up"), Some(State::Running));
        assert_eq!(state("half"), Some(State::Creating));
        // Nothing left to do the second time; the agent can be resumed.
        assert!(matches!(fleet.reconcile().as_deref(), Ok([])));
        assert!(matches!(fleet.resume(&agent("gone")), Ok(State::Running)));
    }

    #[test]
    fn reconcile_failure_leaves_the_record_running() {
        let store = FakeStore::default();
        let _ = store.set_state(&id(), State::Running);
        let rt = FakeRuntime {
            fail: true,
            ..Default::default()
        };
        let fleet = Fleet::new(&rt, store);
        assert!(matches!(fleet.reconcile(), Err(FleetError::Port(_))));
        assert_eq!(fleet.state(&id()).ok().flatten(), Some(State::Running));
    }

    #[test]
    fn full_lifecycle() {
        let rt = FakeRuntime::default();
        let fleet = Fleet::new(&rt, FakeStore::default());
        assert!(matches!(fleet.create(&id()), Ok(State::Running)));
        assert!(matches!(fleet.pause(&id()), Ok(State::Paused)));
        assert!(matches!(fleet.resume(&id()), Ok(State::Running)));
        assert!(matches!(fleet.destroy(&id(), false), Ok(State::Destroyed)));
        assert_eq!(*rt.calls.borrow(), ["create", "pause", "resume", "destroy"]);
    }

    #[test]
    fn state_reports_unknown_then_current() {
        let rt = FakeRuntime::default();
        let fleet = Fleet::new(&rt, FakeStore::default());
        assert!(matches!(fleet.state(&id()), Ok(None)));
        let _ = fleet.create(&id());
        assert!(matches!(fleet.state(&id()), Ok(Some(State::Running))));
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
    fn failed_create_can_be_retried() {
        let failing = FakeRuntime {
            fail: true,
            ..Default::default()
        };
        let store = FakeStore::default();
        let first = Fleet::new(&failing, store);
        assert!(matches!(first.create(&id()), Err(FleetError::Port(_))));
        let working = FakeRuntime::default();
        let retry = Fleet::new(&working, first.store);
        assert!(matches!(retry.create(&id()), Ok(State::Running)));
    }

    #[test]
    fn destroyed_id_can_be_recreated() {
        let rt = FakeRuntime::default();
        let fleet = Fleet::new(&rt, FakeStore::default());
        let _ = fleet.create(&id());
        let _ = fleet.destroy(&id(), false);
        assert!(matches!(fleet.create(&id()), Ok(State::Running)));
        assert_eq!(*rt.calls.borrow(), ["create", "destroy", "create"]);
    }

    #[test]
    fn purge_works_live_or_after_destroy_but_not_on_unknown() {
        let rt = FakeRuntime::default();
        let fleet = Fleet::new(&rt, FakeStore::default());
        assert!(matches!(
            fleet.destroy(&id(), true),
            Err(FleetError::NotFound(_))
        ));
        let _ = fleet.create(&id());
        assert!(matches!(fleet.destroy(&id(), true), Ok(State::Destroyed)));
        let _ = fleet.create(&id());
        let _ = fleet.destroy(&id(), false);
        assert!(matches!(fleet.destroy(&id(), true), Ok(State::Destroyed)));
        assert!(matches!(
            fleet.destroy(&id(), false),
            Err(FleetError::Illegal(_))
        ));
        assert_eq!(
            *rt.calls.borrow(),
            ["create", "purge", "create", "destroy", "purge"]
        );
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

    /// `/dev/shm` of 9000 MiB, all free, guests of 4096 MiB.
    struct Shm9000;
    impl Capacity for Shm9000 {
        fn shm(&self) -> Result<ac_ports::Shm, PortError> {
            Ok(ac_ports::Shm {
                total_mib: 9000,
                free_mib: 9000,
            })
        }
        fn guest_mib(&self) -> Result<u64, PortError> {
            Ok(4096)
        }
    }

    #[test]
    fn admission_counts_running_and_creating_guests_at_configured_size() {
        let rt = FakeRuntime::default();
        let fleet = Fleet::new(&rt, FakeStore::default()).with_admission(Box::new(Shm9000), 512);
        assert!(matches!(fleet.create(&agent("x1")), Ok(State::Running)));
        assert!(matches!(fleet.create(&agent("x2")), Ok(State::Running)));
        // 3 x 4096 + 512 > 9000: refused before any runtime call or state change.
        let third = fleet.create(&agent("x3"));
        assert!(
            matches!(
                third,
                Err(FleetError::NoCapacity(Shortfall {
                    needed_mib: 12800,
                    available_mib: 9000,
                    more: 0,
                    ..
                }))
            ),
            "{third:?}"
        );
        assert_eq!(fleet.state(&agent("x3")).ok().flatten(), None);
        // A paused agent does not count; resuming it is checked like a create.
        let _ = fleet.pause(&agent("x1"));
        assert!(matches!(fleet.create(&agent("x3")), Ok(State::Running)));
        assert!(matches!(
            fleet.resume(&agent("x1")),
            Err(FleetError::NoCapacity(_))
        ));
        assert_eq!(
            fleet.state(&agent("x1")).ok().flatten(),
            Some(State::Paused)
        );
        assert_eq!(*rt.calls.borrow(), ["create", "create", "pause", "create"]);
        let report = fleet.capacity().ok().flatten();
        assert_eq!(
            report.map(|r| (r.running_agents, r.committed_mib, r.max_more_agents)),
            Some((2, 8192, 0))
        );
    }

    #[test]
    fn a_failed_create_is_retried_without_counting_itself_twice() {
        let failing = FakeRuntime {
            fail: true,
            ..Default::default()
        };
        let first = Fleet::new(&failing, FakeStore::default());
        let _ = first.create(&agent("x1"));
        let _ = first.create(&agent("x2"));
        let working = FakeRuntime::default();
        let retry = Fleet::new(&working, first.store).with_admission(Box::new(Shm9000), 512);
        // x1 and x2 are both Creating (8192 committed): x1's retry counts only x2 besides itself.
        assert!(matches!(retry.create(&agent("x1")), Ok(State::Running)));
        assert!(matches!(
            retry.create(&agent("x3")),
            Err(FleetError::NoCapacity(_))
        ));
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
