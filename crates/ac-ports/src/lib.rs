//! Ports: the traits adapters (containerd, Postgres, ...) implement.
//! Synchronous by design; async arrives with the gateway, the one real I/O fan-out.

use std::fmt;

use ac_domain::{AgentId, State};

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
    /// Remove the VM and its volume.
    fn destroy(&self, id: &AgentId) -> Result<(), PortError>;
}

/// Durable record of agents and their lifecycle state.
pub trait SessionStore {
    /// Current state, or `None` if the agent is unknown.
    fn state(&self, id: &AgentId) -> Result<Option<State>, PortError>;
    /// Persist a new state.
    fn set_state(&self, id: &AgentId, state: State) -> Result<(), PortError>;
}
