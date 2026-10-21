//! Ports: the traits adapters (containerd, the registry, ...) implement.
//! Synchronous by design; async arrives with the gateway, the one real I/O fan-out.

use std::fmt;
use std::sync::Arc;

use ac_domain::{AgentId, Job, JobId, State};

/// Adapter failure with context; adapters map their own errors into this.
#[derive(Debug)]
pub struct PortError {
    /// What was being attempted.
    pub op: &'static str,
    /// Underlying cause, rendered.
    pub cause: String,
}

impl fmt::Display for PortError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} failed: {}", self.op, self.cause)
    }
}

impl std::error::Error for PortError {}

/// Starts and stops per-agent microVMs (Kata via containerd).
pub trait Runtime {
    /// Provision the VM and workspace volume for a new agent.
    fn create(&self, id: &AgentId) -> Result<(), PortError>;
    /// Stop the VM, keeping its volume.
    fn pause(&self, id: &AgentId) -> Result<(), PortError>;
    /// Start a paused VM.
    fn resume(&self, id: &AgentId) -> Result<(), PortError>;
    /// Remove the VM, keeping its volume; `purge` removes the volume too. Must succeed when the
    /// VM is already gone, so a destroyed agent's volume can be purged later.
    fn destroy(&self, id: &AgentId, purge: bool) -> Result<(), PortError>;
    /// Agents whose VM is up: the runtime has a task for them that has not stopped.
    fn running(&self) -> Result<Vec<AgentId>, PortError>;
}

/// Durable record of agents and their lifecycle state.
pub trait SessionStore {
    /// Current state, or `None` if the agent is unknown.
    fn state(&self, id: &AgentId) -> Result<Option<State>, PortError>;
    /// Persist a new state.
    fn set_state(&self, id: &AgentId, state: State) -> Result<(), PortError>;
    /// Every known agent and its state, in no particular order.
    fn list(&self) -> Result<Vec<(AgentId, State)>, PortError>;
}

/// Durable record of jobs (ADR-010): states, times, exit codes. Agent-written output is not kept
/// here.
pub trait JobStore {
    /// Insert or replace a job.
    fn put_job(&self, job: &Job) -> Result<(), PortError>;
    /// Every job of `agent`, in no particular order.
    fn jobs(&self, agent: &AgentId) -> Result<Vec<Job>, PortError>;
    /// Every queued or running job, of every agent.
    fn active_jobs(&self) -> Result<Vec<Job>, PortError>;
    /// Forget a job; an unknown one is fine.
    fn remove_job(&self, agent: &AgentId, id: &JobId) -> Result<(), PortError>;
}

/// One store shared by the fleet and the job supervisor.
impl<T: SessionStore + ?Sized> SessionStore for Arc<T> {
    fn state(&self, id: &AgentId) -> Result<Option<State>, PortError> {
        (**self).state(id)
    }
    fn set_state(&self, id: &AgentId, state: State) -> Result<(), PortError> {
        (**self).set_state(id, state)
    }
    fn list(&self) -> Result<Vec<(AgentId, State)>, PortError> {
        (**self).list()
    }
}

impl<T: JobStore + ?Sized> JobStore for Arc<T> {
    fn put_job(&self, job: &Job) -> Result<(), PortError> {
        (**self).put_job(job)
    }
    fn jobs(&self, agent: &AgentId) -> Result<Vec<Job>, PortError> {
        (**self).jobs(agent)
    }
    fn active_jobs(&self) -> Result<Vec<Job>, PortError> {
        (**self).active_jobs()
    }
    fn remove_job(&self, agent: &AgentId, id: &JobId) -> Result<(), PortError> {
        (**self).remove_job(agent, id)
    }
}
