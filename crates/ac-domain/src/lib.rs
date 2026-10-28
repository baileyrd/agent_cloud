//! Pure domain logic for agent-cloud. No I/O, no framework types.

pub mod allowlist;
pub mod capacity;
pub mod changes;
pub mod checkpoint;
pub mod egress;
pub mod health;
pub mod id;
pub mod job;
pub mod lifecycle;
pub mod schedule;
pub mod tune;

pub use allowlist::{Allowlist, AllowlistError};
pub use capacity::{GuestMemory, Shortfall};
pub use checkpoint::{Checkpoint, CheckpointId, Label, Refusal, Volume};
pub use egress::{Attachment, ProfileName};
pub use id::{AgentId, IdError};
pub use job::{Job, JobEvent, JobId, JobInputError, JobState, JobTransitionError};
pub use lifecycle::{Command, State, TransitionError};
