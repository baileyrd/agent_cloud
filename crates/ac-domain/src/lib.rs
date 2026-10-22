//! Pure domain logic for agent-cloud. No I/O, no framework types.

pub mod allowlist;
pub mod id;
pub mod job;
pub mod lifecycle;

pub use allowlist::{Allowlist, AllowlistError};
pub use id::{AgentId, IdError};
pub use job::{Job, JobEvent, JobId, JobInputError, JobState, JobTransitionError};
pub use lifecycle::{Command, State, TransitionError};
