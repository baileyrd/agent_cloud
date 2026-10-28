//! Applies the lifecycle state machine through the ports. Pure orchestration:
//! every check happens before any side effect, so an illegal request never
//! reaches the runtime. With [`Fleet::with_admission`], a create or resume is also refused when
//! its guest would not fit `/dev/shm` ([`ac_domain::capacity::admit`]). With
//! [`Fleet::with_egress`], every boot gets a new proxy credential (ADR-016): issued before the VM
//! starts (a failure to store it fails the boot), revoked before it stops and whenever a start
//! fails, forgotten on purge, and swept by the reconcile for agents without a VM. With
//! [`Fleet::with_snapshots`], workspaces can be checkpointed, restored and forked (ADR-018), and a
//! purge deletes the agent's checkpoints too.

use std::fmt;
use std::sync::Arc;

use ac_domain::checkpoint::{self, Op};
use ac_domain::{
    capacity, AgentId, Checkpoint, CheckpointId, Command, Label, ProfileName, Refusal, Shortfall,
    State, TransitionError,
};
use ac_ports::{Boot, Capacity, Egress, PortError, Runtime, SessionStore, Snapshots};

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
    /// A workspace operation is not allowed now (ADR-018); nothing was changed.
    Refused(Refusal),
    /// The agent has no checkpoint with this id.
    NoCheckpoint(AgentId, CheckpointId),
    /// The fleet was built without [`Fleet::with_snapshots`].
    SnapshotsOff,
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
            Self::Refused(e) => e.fmt(f),
            Self::NoCheckpoint(id, cp) => write!(f, "agent {id} has no checkpoint {cp}"),
            Self::SnapshotsOff => f.write_str("checkpoints are off"),
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

impl From<Refusal> for FleetError {
    fn from(e: Refusal) -> Self {
        Self::Refused(e)
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

/// Workspace snapshots, and how many checkpoints each agent may keep.
struct Snapshotting {
    port: Arc<dyn Snapshots + Send + Sync>,
    max: usize,
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
    egress: Option<Arc<dyn Egress + Send + Sync>>,
    snapshots: Option<Snapshotting>,
}

impl<R: Runtime, S: SessionStore> Fleet<R, S> {
    /// Compose a fleet from its two adapters.
    pub fn new(runtime: R, store: S) -> Self {
        Self {
            runtime,
            store,
            admission: None,
            egress: None,
            snapshots: None,
        }
    }

    /// Checkpoint, restore and fork workspaces through `port`, keeping at most `max` checkpoints
    /// per agent (a restore's automatic one counts). Without it those calls fail with
    /// [`FleetError::SnapshotsOff`].
    pub fn with_snapshots(mut self, port: Arc<dyn Snapshots + Send + Sync>, max: usize) -> Self {
        self.snapshots = Some(Snapshotting { port, max });
        self
    }

    /// Give every boot a new proxy credential from `egress`, and take it away when the VM goes.
    /// Without it, VMs boot with the image's credential-less proxy settings (the base list).
    pub fn with_egress(mut self, egress: Arc<dyn Egress + Send + Sync>) -> Self {
        self.egress = Some(egress);
        self
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
        self.create_attached(id, None)
    }

    /// [`Self::create`], attaching `profiles` (when given) in the same write as the credential,
    /// before the VM starts.
    pub fn create_attached(
        &self,
        id: &AgentId,
        profiles: Option<&[ProfileName]>,
    ) -> Result<State, FleetError> {
        // Only a running or paused agent blocks the id. A destroyed one can be reused, and one
        // stuck in `Creating` (a previous create failed) is retried.
        if matches!(self.store.state(id)?, Some(State::Running | State::Paused)) {
            return Err(FleetError::AlreadyExists(id.clone()));
        }
        self.admit(id)?;
        self.boot_new(id, profiles)
    }

    /// Issue `id`'s credential, record it `Creating` and boot it on its workspace (made if
    /// missing). A failed boot leaves it `Creating`.
    fn boot_new(
        &self,
        id: &AgentId,
        profiles: Option<&[ProfileName]>,
    ) -> Result<State, FleetError> {
        let boot = self.issue(id, profiles)?;
        self.store.set_state(id, State::Creating)?;
        self.started(id, self.runtime.create(id, &boot))?;
        self.persist(id, State::Creating, Command::Ready)
    }

    /// Stop the agent's VM, keeping its volume. Its credential is revoked first.
    pub fn pause(&self, id: &AgentId) -> Result<State, FleetError> {
        self.step(id, Command::Pause, |r| {
            self.revoke(id)?;
            r.pause(id)
        })
    }

    /// Start a paused agent.
    pub fn resume(&self, id: &AgentId) -> Result<State, FleetError> {
        let from = self
            .store
            .state(id)?
            .ok_or_else(|| FleetError::NotFound(id.clone()))?;
        from.apply(Command::Resume)?;
        self.admit(id)?;
        let boot = self.issue(id, None)?;
        self.step(id, Command::Resume, |r| {
            self.started(id, r.resume(id, &boot))
        })
    }

    /// Tear the agent down, keeping its workspace; its record stays as `Destroyed`. `purge` also
    /// removes the workspace, and is allowed on an agent that is already destroyed; it also
    /// forgets the agent's egress line.
    pub fn destroy(&self, id: &AgentId, purge: bool) -> Result<State, FleetError> {
        if purge && self.store.state(id)? == Some(State::Destroyed) {
            self.runtime.destroy(id, true)?;
            self.purged(id)?;
            return Ok(State::Destroyed);
        }
        let state = self.step(id, Command::Destroy, |r| {
            self.revoke(id)?;
            r.destroy(id, purge)
        })?;
        if purge {
            self.purged(id)?;
        }
        Ok(state)
    }

    /// `id`'s workspace is gone: delete its checkpoints and forget its egress line.
    fn purged(&self, id: &AgentId) -> Result<(), PortError> {
        if let Some(s) = &self.snapshots {
            s.port.purge(id)?;
        }
        self.forget(id)
    }

    /// `id`'s checkpoints, oldest first, and the most it may keep.
    pub fn checkpoints(&self, id: &AgentId) -> Result<(Vec<Checkpoint>, usize), FleetError> {
        let s = self.snaps()?;
        self.known(id)?;
        Ok((s.port.checkpoints(id)?, s.max))
    }

    /// Snapshot `id`'s workspace read-only. Running or paused; refused at the cap.
    pub fn checkpoint(
        &self,
        id: &AgentId,
        label: Option<&Label>,
    ) -> Result<CheckpointId, FleetError> {
        let s = self.snaps()?;
        checkpoint::check(id, Op::Checkpoint, self.known(id)?, s.port.volume(id)?)?;
        checkpoint::check_room(id, s.port.checkpoints(id)?.len(), s.max)?;
        Ok(s.port.checkpoint(id, label)?)
    }

    /// Delete one of `id`'s checkpoints.
    pub fn delete_checkpoint(&self, id: &AgentId, cp: &CheckpointId) -> Result<(), FleetError> {
        let s = self.snaps()?;
        self.known(id)?;
        self.has_checkpoint(s, id, cp)?;
        Ok(s.port.delete(id, cp)?)
    }

    /// Put checkpoint `cp` back as `id`'s workspace. Paused only; the workspace it replaces is
    /// kept as a checkpoint first (so the cap must leave room), whose id is returned.
    pub fn restore(&self, id: &AgentId, cp: &CheckpointId) -> Result<CheckpointId, FleetError> {
        let s = self.snaps()?;
        checkpoint::check(id, Op::Restore, self.known(id)?, s.port.volume(id)?)?;
        let count = self.has_checkpoint(s, id, cp)?;
        checkpoint::check_room(id, count, s.max)?;
        Ok(s.port.restore(id, cp)?)
    }

    /// Create agent `to` on a writable snapshot of `from`'s workspace, or of its checkpoint `cp`,
    /// attaching `profiles` like a create. Nothing but files comes from `from`: not its egress
    /// profiles, not its jobs. A failed boot leaves `to` `Creating` on its forked workspace.
    pub fn fork(
        &self,
        from: &AgentId,
        cp: Option<&CheckpointId>,
        to: &AgentId,
        profiles: Option<&[ProfileName]>,
    ) -> Result<State, FleetError> {
        let s = self.snaps()?;
        checkpoint::check(from, Op::Fork, self.known(from)?, s.port.volume(from)?)?;
        if let Some(cp) = cp {
            self.has_checkpoint(s, from, cp)?;
        }
        checkpoint::check_fork_target(to, self.store.state(to)?, s.port.volume(to)?)?;
        self.admit(to)?;
        s.port.fork(from, cp, to)?;
        self.boot_new(to, profiles)
    }

    /// Turn `id`'s plain-directory workspace into a subvolume. Its VM must be stopped.
    pub fn migrate(&self, id: &AgentId) -> Result<(), FleetError> {
        let s = self.snaps()?;
        checkpoint::check(id, Op::Migrate, self.known(id)?, s.port.volume(id)?)?;
        Ok(s.port.migrate(id)?)
    }

    fn snaps(&self) -> Result<&Snapshotting, FleetError> {
        self.snapshots.as_ref().ok_or(FleetError::SnapshotsOff)
    }

    /// `id`'s state; unknown is an error.
    fn known(&self, id: &AgentId) -> Result<State, FleetError> {
        self.store
            .state(id)?
            .ok_or_else(|| FleetError::NotFound(id.clone()))
    }

    /// How many checkpoints `id` has, if `cp` is one of them.
    fn has_checkpoint(
        &self,
        s: &Snapshotting,
        id: &AgentId,
        cp: &CheckpointId,
    ) -> Result<usize, FleetError> {
        let all = s.port.checkpoints(id)?;
        match all.iter().any(|c| &c.id == cp) {
            true => Ok(all.len()),
            false => Err(FleetError::NoCheckpoint(id.clone(), cp.clone())),
        }
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
        if let Some(e) = &self.egress {
            e.sweep(&live)?;
        }
        Ok(changed)
    }

    /// A new credential for `id`'s boot; without egress, the image's settings.
    fn issue(&self, id: &AgentId, profiles: Option<&[ProfileName]>) -> Result<Boot, FleetError> {
        match &self.egress {
            Some(e) => Ok(e.issue(id, profiles)?),
            None => Ok(Boot::default()),
        }
    }

    fn revoke(&self, id: &AgentId) -> Result<(), PortError> {
        self.egress.as_ref().map_or(Ok(()), |e| e.revoke(id))
    }

    fn forget(&self, id: &AgentId) -> Result<(), PortError> {
        self.egress.as_ref().map_or(Ok(()), |e| e.forget(id))
    }

    /// A boot's outcome; a failed one also revokes the credential it was given.
    fn started(&self, id: &AgentId, booted: Result<(), PortError>) -> Result<(), PortError> {
        let Err(e) = booted else {
            return Ok(());
        };
        Err(match self.revoke(id) {
            Ok(()) => e,
            Err(r) => PortError {
                op: e.op,
                cause: format!("{}; revoking its credential also failed: {r}", e.cause),
            },
        })
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
    use ac_domain::Volume;

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
        /// What each create or resume was given.
        boots: RefCell<Vec<Boot>>,
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
        fn create(&self, _: &AgentId, boot: &Boot) -> Result<(), PortError> {
            self.boots.borrow_mut().push(boot.clone());
            self.call("create")
        }
        fn pause(&self, _: &AgentId) -> Result<(), PortError> {
            self.call("pause")
        }
        fn resume(&self, _: &AgentId, boot: &Boot) -> Result<(), PortError> {
            self.boots.borrow_mut().push(boot.clone());
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

    /// Records every call as text; `fail` names the operation that fails.
    #[derive(Default)]
    struct FakeEgress {
        calls: std::sync::Mutex<Vec<String>>,
        fail: Option<&'static str>,
    }

    impl FakeEgress {
        fn call(&self, op: &'static str, line: String) -> Result<(), PortError> {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(line);
            match self.fail {
                Some(f) if f == op => Err(PortError {
                    op,
                    cause: "disk full".into(),
                }),
                _ => Ok(()),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }
    }

    impl Egress for FakeEgress {
        fn issue(&self, id: &AgentId, p: Option<&[ProfileName]>) -> Result<Boot, PortError> {
            let p = p.map_or("-".to_owned(), ac_domain::egress::names);
            self.call("issue", format!("issue {id} {p}"))?;
            Ok(Boot::default().with_env("HTTPS_PROXY", format!("http://{id}:secret@proxy")))
        }
        fn revoke(&self, id: &AgentId) -> Result<(), PortError> {
            self.call("revoke", format!("revoke {id}"))
        }
        fn forget(&self, id: &AgentId) -> Result<(), PortError> {
            self.call("forget", format!("forget {id}"))
        }
        fn sweep(&self, live: &[AgentId]) -> Result<Vec<AgentId>, PortError> {
            let live: Vec<String> = live.iter().map(ToString::to_string).collect();
            self.call("sweep", format!("sweep {}", live.join(",")))
                .map(|()| Vec::new())
        }
    }

    fn with_egress<'a>(
        rt: &'a FakeRuntime,
        fail: Option<&'static str>,
    ) -> (Fleet<&'a FakeRuntime, FakeStore>, Arc<FakeEgress>) {
        let egress = Arc::new(FakeEgress {
            fail,
            ..Default::default()
        });
        let fleet = Fleet::new(rt, FakeStore::default()).with_egress(egress.clone());
        (fleet, egress)
    }

    #[test]
    fn every_boot_gets_a_new_credential_and_every_stop_revokes_it() {
        let rt = FakeRuntime::default();
        let (fleet, egress) = with_egress(&rt, None);
        let rust = [ProfileName::parse("rust").unwrap_or_else(|e| panic!("{e}"))];
        assert!(matches!(
            fleet.create_attached(&id(), Some(&rust)),
            Ok(State::Running)
        ));
        assert!(matches!(fleet.pause(&id()), Ok(State::Paused)));
        assert!(matches!(fleet.resume(&id()), Ok(State::Running)));
        assert!(matches!(fleet.destroy(&id(), false), Ok(State::Destroyed)));
        assert!(matches!(fleet.destroy(&id(), true), Ok(State::Destroyed)));
        assert_eq!(
            egress.calls(),
            [
                "issue a1 rust",
                "revoke a1",
                "issue a1 -",
                "revoke a1",
                "forget a1"
            ]
        );
        assert_eq!(
            *rt.calls.borrow(),
            ["create", "pause", "resume", "destroy", "purge"]
        );
        // What the runtime booted with came from the issue.
        let boots = rt.boots.borrow().clone();
        assert_eq!(boots.len(), 2);
        assert!(boots
            .iter()
            .all(|b| b.env()[0].1 == "http://a1:secret@proxy"));
        // A live purge revokes, then forgets.
        let _ = fleet.create(&agent("a2"));
        let _ = fleet.destroy(&agent("a2"), true);
        assert_eq!(
            egress.calls()[5..],
            ["issue a2 -", "revoke a2", "forget a2"]
        );
    }

    #[test]
    fn a_credential_that_cannot_be_stored_fails_the_boot_before_the_vm_starts() {
        let rt = FakeRuntime::default();
        let (fleet, egress) = with_egress(&rt, Some("issue"));
        assert!(matches!(fleet.create(&id()), Err(FleetError::Port(_))));
        assert_eq!(fleet.state(&id()).ok().flatten(), None);
        assert!(rt.calls.borrow().is_empty());
        assert_eq!(egress.calls(), ["issue a1 -"]);
        let _ = fleet.store.set_state(&id(), State::Paused);
        assert!(matches!(fleet.resume(&id()), Err(FleetError::Port(_))));
        assert_eq!(fleet.state(&id()).ok().flatten(), Some(State::Paused));
        assert!(rt.calls.borrow().is_empty());
    }

    #[test]
    fn a_failed_start_revokes_the_credential_it_was_given() {
        let rt = FakeRuntime {
            fail: true,
            ..Default::default()
        };
        let (fleet, egress) = with_egress(&rt, None);
        assert!(matches!(fleet.create(&id()), Err(FleetError::Port(_))));
        let _ = fleet.store.set_state(&agent("a2"), State::Paused);
        assert!(matches!(
            fleet.resume(&agent("a2")),
            Err(FleetError::Port(_))
        ));
        assert_eq!(
            egress.calls(),
            ["issue a1 -", "revoke a1", "issue a2 -", "revoke a2"]
        );
        // Both failing: the boot's error, with the revoke's after it.
        let (fleet, _) = with_egress(&rt, Some("revoke"));
        let err = fleet
            .create(&id())
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(
            err.contains("boom; revoking its credential also failed"),
            "{err}"
        );
    }

    #[test]
    fn a_revoke_that_fails_stops_the_pause_before_the_vm_stops() {
        let rt = FakeRuntime::default();
        let (fleet, egress) = with_egress(&rt, Some("revoke"));
        let _ = fleet.store.set_state(&id(), State::Running);
        assert!(matches!(fleet.pause(&id()), Err(FleetError::Port(_))));
        assert!(matches!(
            fleet.destroy(&id(), false),
            Err(FleetError::Port(_))
        ));
        assert!(rt.calls.borrow().is_empty());
        assert_eq!(fleet.state(&id()).ok().flatten(), Some(State::Running));
        assert_eq!(egress.calls(), ["revoke a1", "revoke a1"]);
    }

    #[test]
    fn illegal_commands_never_touch_credentials_and_reconcile_sweeps() {
        let rt = FakeRuntime {
            live: vec![agent("up")],
            ..Default::default()
        };
        let (fleet, egress) = with_egress(&rt, None);
        assert!(fleet.pause(&id()).is_err());
        assert!(fleet.resume(&id()).is_err());
        let _ = fleet.store.set_state(&id(), State::Running);
        assert!(matches!(fleet.resume(&id()), Err(FleetError::Illegal(_))));
        assert!(matches!(
            fleet.create(&id()),
            Err(FleetError::AlreadyExists(_))
        ));
        assert!(egress.calls().is_empty());
        assert_eq!(fleet.reconcile().ok(), Some(vec![id()]));
        assert_eq!(egress.calls(), ["sweep up"]);
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

    /// Each agent's volume, and its checkpoints.
    type Disk = (HashMap<String, Volume>, HashMap<String, Vec<CheckpointId>>);

    /// Workspaces as a map, and a log of calls.
    #[derive(Default)]
    struct FakeSnaps {
        disk: std::sync::Mutex<Disk>,
        calls: std::sync::Mutex<Vec<String>>,
    }

    impl FakeSnaps {
        fn with(volumes: &[(&str, Volume)]) -> Arc<Self> {
            let s = Self::default();
            if let Ok(mut d) = s.disk.lock() {
                for (id, v) in volumes {
                    d.0.insert((*id).to_owned(), *v);
                }
            }
            Arc::new(s)
        }
        fn log(&self, call: String) {
            if let Ok(mut c) = self.calls.lock() {
                c.push(call);
            }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().map(|c| c.clone()).unwrap_or_default()
        }
        fn add(&self, id: &AgentId) -> CheckpointId {
            let mut d = self.disk.lock().unwrap_or_else(|e| e.into_inner());
            let list = d.1.entry(id.to_string()).or_default();
            let cp = CheckpointId::next(1_793_192_712, list);
            list.push(cp.clone());
            cp
        }
    }

    impl Snapshots for FakeSnaps {
        fn volume(&self, id: &AgentId) -> Result<Volume, PortError> {
            let d = self.disk.lock().unwrap_or_else(|e| e.into_inner());
            Ok(d.0.get(id.as_str()).copied().unwrap_or(Volume::Missing))
        }
        fn checkpoints(&self, id: &AgentId) -> Result<Vec<Checkpoint>, PortError> {
            let d = self.disk.lock().unwrap_or_else(|e| e.into_inner());
            let ids = d.1.get(id.as_str()).cloned().unwrap_or_default();
            Ok(ids
                .into_iter()
                .map(|id| Checkpoint { id, label: None })
                .collect())
        }
        fn checkpoint(&self, id: &AgentId, _: Option<&Label>) -> Result<CheckpointId, PortError> {
            self.log(format!("checkpoint {id}"));
            Ok(self.add(id))
        }
        fn delete(&self, id: &AgentId, cp: &CheckpointId) -> Result<(), PortError> {
            self.log(format!("delete {id} {cp}"));
            let mut d = self.disk.lock().unwrap_or_else(|e| e.into_inner());
            d.1.entry(id.to_string()).or_default().retain(|c| c != cp);
            Ok(())
        }
        fn restore(&self, id: &AgentId, cp: &CheckpointId) -> Result<CheckpointId, PortError> {
            self.log(format!("restore {id} {cp}"));
            Ok(self.add(id))
        }
        fn fork(
            &self,
            from: &AgentId,
            cp: Option<&CheckpointId>,
            to: &AgentId,
        ) -> Result<(), PortError> {
            self.log(format!("fork {from} {cp:?} {to}"));
            let mut d = self.disk.lock().unwrap_or_else(|e| e.into_inner());
            d.0.insert(to.to_string(), Volume::Subvolume);
            Ok(())
        }
        fn migrate(&self, id: &AgentId) -> Result<(), PortError> {
            self.log(format!("migrate {id}"));
            let mut d = self.disk.lock().unwrap_or_else(|e| e.into_inner());
            d.0.insert(id.to_string(), Volume::Subvolume);
            Ok(())
        }
        fn purge(&self, id: &AgentId) -> Result<(), PortError> {
            self.log(format!("purge {id}"));
            Ok(())
        }
    }

    fn snap_fleet<'a>(
        rt: &'a FakeRuntime,
        snaps: &Arc<FakeSnaps>,
        agents: &[(&str, State)],
    ) -> Fleet<&'a FakeRuntime, FakeStore> {
        let store = FakeStore::default();
        for (id, state) in agents {
            let _ = store.set_state(&agent(id), *state);
        }
        Fleet::new(rt, store).with_snapshots(snaps.clone(), 2)
    }

    #[test]
    fn snapshots_off_is_its_own_error() {
        let rt = FakeRuntime::default();
        let fleet = Fleet::new(&rt, FakeStore::default());
        assert!(matches!(
            fleet.checkpoint(&id(), None),
            Err(FleetError::SnapshotsOff)
        ));
    }

    #[test]
    fn checkpoint_needs_a_live_agent_on_a_subvolume_and_room() {
        let rt = FakeRuntime::default();
        let snaps = FakeSnaps::with(&[("a1", Volume::Subvolume), ("plain", Volume::Directory)]);
        let fleet = snap_fleet(
            &rt,
            &snaps,
            &[
                ("a1", State::Running),
                ("plain", State::Paused),
                ("gone", State::Destroyed),
            ],
        );
        assert!(fleet.checkpoint(&id(), None).is_ok());
        assert!(fleet.checkpoint(&id(), None).is_ok());
        let full = fleet.checkpoint(&id(), None);
        assert!(matches!(
            full,
            Err(FleetError::Refused(Refusal::Full { max: 2, .. }))
        ));
        let plain = fleet.checkpoint(&agent("plain"), None);
        assert!(matches!(
            plain,
            Err(FleetError::Refused(Refusal::Volume { .. }))
        ));
        let gone = fleet.checkpoint(&agent("gone"), None);
        assert!(matches!(
            gone,
            Err(FleetError::Refused(Refusal::State { .. }))
        ));
        assert!(matches!(
            fleet.checkpoint(&agent("nobody"), None),
            Err(FleetError::NotFound(_))
        ));
        assert_eq!(snaps.calls(), ["checkpoint a1", "checkpoint a1"]);
        let (list, max) = fleet.checkpoints(&id()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((list.len(), max), (2, 2));
    }

    #[test]
    fn restore_needs_paused_a_known_checkpoint_and_room_for_the_one_it_keeps() {
        let rt = FakeRuntime::default();
        let snaps = FakeSnaps::with(&[("a1", Volume::Subvolume)]);
        let fleet = snap_fleet(&rt, &snaps, &[("a1", State::Running)]);
        let cp = fleet
            .checkpoint(&id(), None)
            .unwrap_or_else(|e| panic!("{e}"));
        let running = fleet.restore(&id(), &cp);
        assert!(matches!(
            running,
            Err(FleetError::Refused(Refusal::State { .. }))
        ));
        let _ = fleet.pause(&id());
        let unknown = CheckpointId::parse("20200101-000000").unwrap_or_else(|e| panic!("{e}"));
        assert!(matches!(
            fleet.restore(&id(), &unknown),
            Err(FleetError::NoCheckpoint(..))
        ));
        let pre = fleet.restore(&id(), &cp).unwrap_or_else(|e| panic!("{e}"));
        assert_ne!(pre, cp);
        // Two now, the cap: the next restore has no room for the checkpoint it keeps.
        assert!(matches!(
            fleet.restore(&id(), &cp),
            Err(FleetError::Refused(Refusal::Full { .. }))
        ));
        let _ = fleet.delete_checkpoint(&id(), &pre);
        assert!(fleet.restore(&id(), &cp).is_ok());
        assert!(matches!(
            fleet.delete_checkpoint(&id(), &unknown),
            Err(FleetError::NoCheckpoint(..))
        ));
    }

    #[test]
    fn fork_snapshots_then_boots_like_a_create() {
        let rt = FakeRuntime::default();
        let snaps = FakeSnaps::with(&[("a1", Volume::Subvolume), ("old", Volume::Directory)]);
        let fleet = snap_fleet(
            &rt,
            &snaps,
            &[("a1", State::Destroyed), ("old", State::Destroyed)],
        );
        let b1 = agent("b1");
        assert!(matches!(
            fleet.fork(&id(), None, &b1, None),
            Ok(State::Running)
        ));
        assert_eq!(fleet.state(&b1).ok().flatten(), Some(State::Running));
        assert_eq!(*rt.calls.borrow(), ["create"]);
        // b1 now exists; a destroyed agent that kept its workspace is taken too.
        for to in ["b1", "old"] {
            let r = fleet.fork(&id(), None, &agent(to), None);
            assert!(
                matches!(r, Err(FleetError::Refused(Refusal::Taken(_)))),
                "{to}"
            );
        }
        let unknown = CheckpointId::parse("20200101-000000").unwrap_or_else(|e| panic!("{e}"));
        let r = fleet.fork(&id(), Some(&unknown), &agent("c1"), None);
        assert!(matches!(r, Err(FleetError::NoCheckpoint(..))));
        assert_eq!(snaps.calls(), ["fork a1 None b1"]);
    }

    #[test]
    fn migrate_needs_a_stopped_vm_and_a_plain_directory() {
        let rt = FakeRuntime::default();
        let snaps = FakeSnaps::with(&[("a1", Volume::Directory), ("up", Volume::Directory)]);
        let fleet = snap_fleet(
            &rt,
            &snaps,
            &[("a1", State::Paused), ("up", State::Running)],
        );
        let up = fleet.migrate(&agent("up"));
        assert!(matches!(
            up,
            Err(FleetError::Refused(Refusal::State { .. }))
        ));
        assert!(fleet.migrate(&id()).is_ok());
        let again = fleet.migrate(&id());
        assert!(matches!(
            again,
            Err(FleetError::Refused(Refusal::Volume { .. }))
        ));
    }

    #[test]
    fn purge_deletes_checkpoints_but_destroy_keeps_them() {
        let rt = FakeRuntime::default();
        let snaps = FakeSnaps::with(&[("a1", Volume::Subvolume)]);
        let fleet = snap_fleet(&rt, &snaps, &[("a1", State::Running)]);
        let _ = fleet.destroy(&id(), false);
        assert!(snaps.calls().is_empty());
        let _ = fleet.destroy(&id(), true);
        assert_eq!(snaps.calls(), ["purge a1"]);
    }
}
