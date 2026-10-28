//! Ports: the traits adapters (containerd, the registry, ...) implement.
//! Synchronous by design; async arrives with the gateway, the one real I/O fan-out.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use ac_domain::health::{ExecClient, Pressure};
use ac_domain::{AgentId, Checkpoint, CheckpointId, Job, JobId, Label, ProfileName, State, Volume};

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

/// What one boot of an agent's VM gets besides its image: the environment that carries its proxy
/// credential (ADR-016), overriding the image's credential-less proxy variables. `Debug` shows
/// the names only, never a value.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Boot {
    env: Vec<(String, String)>,
}

impl Boot {
    /// Add `name=value` to the environment.
    pub fn with_env(mut self, name: &str, value: String) -> Self {
        self.env.push((name.to_owned(), value));
        self
    }

    /// The environment, in order.
    pub fn env(&self) -> &[(String, String)] {
        &self.env
    }
}

impl fmt::Debug for Boot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.env.iter().map(|(k, _)| k.as_str()).collect();
        write!(f, "Boot {{ env: {names:?} }}")
    }
}

/// Starts and stops per-agent microVMs (Kata via containerd).
pub trait Runtime {
    /// Provision the VM and workspace volume for a new agent and boot it with `boot`.
    fn create(&self, id: &AgentId, boot: &Boot) -> Result<(), PortError>;
    /// Stop the VM, keeping its volume.
    fn pause(&self, id: &AgentId) -> Result<(), PortError>;
    /// Start a paused VM with `boot`.
    fn resume(&self, id: &AgentId, boot: &Boot) -> Result<(), PortError>;
    /// Remove the VM, keeping its volume; `purge` removes the volume too. Must succeed when the
    /// VM is already gone, so a destroyed agent's volume can be purged later.
    fn destroy(&self, id: &AgentId, purge: bool) -> Result<(), PortError>;
    /// Agents whose VM is up: the runtime has a task for them that has not stopped.
    fn running(&self) -> Result<Vec<AgentId>, PortError>;
}

/// Agents' workspace volumes as btrfs subvolumes: checkpoints, restore, forks (ADR-018). Every
/// method acts on whole volumes and never opens a path inside a workspace, which the agent
/// controls. The caller checks lifecycle state and the cap ([`ac_domain::checkpoint`]) first.
pub trait Snapshots {
    /// What `id`'s workspace is on disk.
    fn volume(&self, id: &AgentId) -> Result<Volume, PortError>;
    /// `id`'s checkpoints, oldest first.
    fn checkpoints(&self, id: &AgentId) -> Result<Vec<Checkpoint>, PortError>;
    /// Snapshot `id`'s workspace read-only; returns the new checkpoint's id.
    fn checkpoint(&self, id: &AgentId, label: Option<&Label>) -> Result<CheckpointId, PortError>;
    /// Delete one checkpoint.
    fn delete(&self, id: &AgentId, cp: &CheckpointId) -> Result<(), PortError>;
    /// Replace `id`'s workspace (its VM stopped) with a writable snapshot of `cp`, after keeping
    /// the current one as a checkpoint, whose id is returned.
    fn restore(&self, id: &AgentId, cp: &CheckpointId) -> Result<CheckpointId, PortError>;
    /// Make `to`'s workspace a writable snapshot of `from`'s, or of its checkpoint `cp`.
    fn fork(
        &self,
        from: &AgentId,
        cp: Option<&CheckpointId>,
        to: &AgentId,
    ) -> Result<(), PortError>;
    /// Turn `id`'s plain-directory workspace (its VM stopped) into a subvolume with the same
    /// contents.
    fn migrate(&self, id: &AgentId) -> Result<(), PortError>;
    /// Delete every checkpoint of `id`; nothing there is fine.
    fn purge(&self, id: &AgentId) -> Result<(), PortError>;
}

/// The agents' proxy credentials and profile attachments (ADR-016), kept in the agents file the
/// egress proxy reads. Every boot gets a new credential; only its digest is stored.
pub trait Egress {
    /// A new credential for `id`'s next boot, its digest stored (and `profiles` attached in the
    /// same write, when given) before the VM starts. Returns what to boot with.
    fn issue(&self, id: &AgentId, profiles: Option<&[ProfileName]>) -> Result<Boot, PortError>;
    /// `id`'s VM is going or gone: its credential stops working. Its profiles stay.
    fn revoke(&self, id: &AgentId) -> Result<(), PortError>;
    /// `id`'s workspace was purged: forget it, profiles too.
    fn forget(&self, id: &AgentId) -> Result<(), PortError>;
    /// Only the agents in `live` have a VM: revoke every other credential. Returns those it
    /// revoked.
    fn sweep(&self, live: &[AgentId]) -> Result<Vec<AgentId>, PortError>;
}

/// `/dev/shm`, where every guest's RAM lives (Kata's `memory-backend-file`), in MiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shm {
    /// Size of the filesystem.
    pub total_mib: u64,
    /// Free now.
    pub free_mib: u64,
}

/// What admission control reads from the host.
pub trait Capacity {
    /// `/dev/shm` now.
    fn shm(&self) -> Result<Shm, PortError>;
    /// The memory each guest is configured with, in MiB.
    fn guest_mib(&self) -> Result<u64, PortError>;
}

/// What the watchdog asks of the runtime (ADR-012).
pub trait Watch {
    /// Whether agent `id` answers a trivial exec within `budget`.
    fn probe(&self, id: &AgentId, budget: Duration) -> bool;
    /// The host `ctr task exec` clients of agent `id`.
    fn exec_clients(&self, id: &AgentId) -> Result<Vec<ExecClient>, PortError>;
    /// SIGKILL these processes.
    fn kill(&self, pids: &[u32]) -> Result<(), PortError>;
}

/// How loaded the host is.
pub trait HostLoad {
    /// Load average and pressure now.
    fn pressure(&self) -> Result<Pressure, PortError>;
}

/// A monotonic clock.
pub trait Clock {
    /// Time since some fixed start.
    fn now(&self) -> Duration;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_debug_shows_names_not_values() {
        let boot =
            Boot::default().with_env("HTTPS_PROXY", "http://a1:s3cr3t@10.89.0.1:3128".into());
        assert_eq!(format!("{boot:?}"), r#"Boot { env: ["HTTPS_PROXY"] }"#);
        assert_eq!(boot.env()[0].1, "http://a1:s3cr3t@10.89.0.1:3128");
    }
}
