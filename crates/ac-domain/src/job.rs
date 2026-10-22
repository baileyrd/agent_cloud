//! Jobs (ADR-010): a prompt run in an agent, as a pure state machine plus input rules.

use std::fmt;

use crate::id::{check, IdError};
use crate::AgentId;

/// Largest prompt accepted, in bytes.
pub const MAX_PROMPT: usize = 64 * 1024;
/// Timeout when the caller gives none.
pub const DEFAULT_TIMEOUT_SECS: u32 = 30 * 60;
/// Longest timeout accepted.
pub const MAX_TIMEOUT_SECS: u32 = 4 * 60 * 60;
/// Longest idempotency key accepted.
pub const MAX_KEY: usize = 128;

/// A job identifier: the same rule as an [`AgentId`], so it is safe in paths, tmux names and argv.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct JobId(String);

impl JobId {
    /// Parse and validate external input.
    pub fn parse(s: &str) -> Result<Self, IdError> {
        check(s).map(|()| Self(s.to_owned()))
    }

    /// A new id from random bytes: 16 lowercase hex digits, always the same length.
    pub fn from_random(bytes: [u8; 8]) -> Self {
        Self(bytes.iter().map(|b| format!("{b:02x}")).collect())
    }

    /// The validated string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where a job is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    /// Waiting for the agent's running job to end.
    Queued,
    /// Started in the agent.
    Running,
    /// `claude` exited 0.
    Succeeded,
    /// `claude` exited non-zero, could not start, or its runner vanished.
    Failed,
    /// Cancelled by a caller, or dropped from the queue because the agent stopped.
    Cancelled,
    /// Killed at its deadline.
    TimedOut,
    /// Running when its agent's VM went away.
    Interrupted,
}

impl JobState {
    /// Every state.
    pub const ALL: [Self; 7] = [
        Self::Queued,
        Self::Running,
        Self::Succeeded,
        Self::Failed,
        Self::Cancelled,
        Self::TimedOut,
        Self::Interrupted,
    ];

    /// Wire name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
            Self::Interrupted => "interrupted",
        }
    }

    /// Whether nothing can happen to the job any more.
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Queued | Self::Running)
    }

    /// Apply `event`, returning the next state or why it is refused.
    pub fn apply(self, event: JobEvent) -> Result<Self, JobTransitionError> {
        use JobEvent as E;
        use JobState::*;
        match (self, event) {
            (Queued, E::Start) => Ok(Running),
            (Queued, E::Cancel | E::Interrupt) => Ok(Cancelled),
            (Running, E::Exit(0)) => Ok(Succeeded),
            (Running, E::Exit(_) | E::Fail) => Ok(Failed),
            (Running, E::Cancel) => Ok(Cancelled),
            (Running, E::Timeout) => Ok(TimedOut),
            (Running, E::Interrupt) => Ok(Interrupted),
            (from, event) => Err(JobTransitionError { from, event }),
        }
    }
}

/// Something that happens to a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobEvent {
    /// The supervisor starts it.
    Start,
    /// `claude` exited with this code.
    Exit(i32),
    /// It could not be started, or its runner ended without an exit code.
    Fail,
    /// A caller cancelled it (a running one has been killed).
    Cancel,
    /// Its deadline passed and it has been killed.
    Timeout,
    /// Its agent's VM went away (pause, destroy, reboot).
    Interrupt,
}

/// An event that is illegal in the job's current state.
#[derive(Debug, PartialEq, Eq)]
pub struct JobTransitionError {
    /// State the job was in.
    pub from: JobState,
    /// Event that was refused.
    pub event: JobEvent,
}

impl fmt::Display for JobTransitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "job is {}: cannot {:?}", self.from.name(), self.event)
    }
}

impl std::error::Error for JobTransitionError {}

/// A job as the registry keeps it. Agent-written output is not part of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// The agent it runs in.
    pub agent: AgentId,
    /// Its id.
    pub id: JobId,
    /// Its state.
    pub state: JobState,
    /// When it was accepted (ms since the epoch).
    pub created_ms: u64,
    /// When it was started.
    pub started_ms: Option<u64>,
    /// When it reached a terminal state.
    pub finished_ms: Option<u64>,
    /// How long it may run.
    pub timeout_secs: u32,
    /// `claude`'s exit code, if it exited.
    pub exit_code: Option<i32>,
    /// The caller's idempotency key.
    pub key: Option<String>,
    /// Why it failed or ended, when that is not obvious from the state.
    pub reason: Option<String>,
}

impl Job {
    /// A new queued job.
    pub fn new(
        agent: AgentId,
        id: JobId,
        created_ms: u64,
        timeout_secs: u32,
        key: Option<String>,
    ) -> Self {
        Self {
            agent,
            id,
            state: JobState::Queued,
            created_ms,
            started_ms: None,
            finished_ms: None,
            timeout_secs,
            exit_code: None,
            key,
            reason: None,
        }
    }

    /// Apply `event` at `now_ms`, recording start and finish times and the exit code.
    pub fn apply(&mut self, event: JobEvent, now_ms: u64) -> Result<(), JobTransitionError> {
        self.state = self.state.apply(event)?;
        if event == JobEvent::Start {
            self.started_ms = Some(now_ms);
        }
        if let JobEvent::Exit(code) = event {
            self.exit_code = Some(code);
        }
        if self.state.is_terminal() {
            self.finished_ms = Some(now_ms);
        }
        Ok(())
    }

    /// When a running job must be killed.
    pub fn deadline_ms(&self) -> Option<u64> {
        self.started_ms
            .map(|s| s + u64::from(self.timeout_secs) * 1000)
    }
}

/// A rejected job request.
#[derive(Debug, PartialEq, Eq)]
pub enum JobInputError {
    /// The prompt is empty.
    EmptyPrompt,
    /// The prompt is longer than [`MAX_PROMPT`] bytes.
    PromptTooLarge(usize),
    /// The timeout is not in 1..=[`MAX_TIMEOUT_SECS`].
    Timeout(u64),
    /// The idempotency key is empty, too long, or has characters outside `[A-Za-z0-9._:-]`.
    Key,
}

impl fmt::Display for JobInputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPrompt => f.write_str("prompt is empty"),
            Self::PromptTooLarge(n) => write!(f, "prompt is {n} bytes; at most {MAX_PROMPT}"),
            Self::Timeout(n) => write!(f, "timeout_secs {n} not in 1..={MAX_TIMEOUT_SECS}"),
            Self::Key => write!(
                f,
                "idempotency_key must be 1 to {MAX_KEY} characters of [A-Za-z0-9._:-]"
            ),
        }
    }
}

impl std::error::Error for JobInputError {}

/// Check a prompt's size.
pub fn check_prompt(prompt: &str) -> Result<(), JobInputError> {
    match prompt.len() {
        0 => Err(JobInputError::EmptyPrompt),
        n if n > MAX_PROMPT => Err(JobInputError::PromptTooLarge(n)),
        _ => Ok(()),
    }
}

/// The timeout to use: the default when absent.
pub fn timeout_secs(requested: Option<u64>) -> Result<u32, JobInputError> {
    match requested {
        None => Ok(DEFAULT_TIMEOUT_SECS),
        Some(n) if (1..=u64::from(MAX_TIMEOUT_SECS)).contains(&n) => {
            u32::try_from(n).map_err(|_| JobInputError::Timeout(n))
        }
        Some(n) => Err(JobInputError::Timeout(n)),
    }
}

/// Check an idempotency key.
pub fn check_key(key: &str) -> Result<(), JobInputError> {
    let ok = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-');
    if key.is_empty() || key.len() > MAX_KEY || !key.chars().all(ok) {
        return Err(JobInputError::Key);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{JobEvent::*, JobState::*, *};

    const EVENTS: [JobEvent; 7] = [Start, Exit(0), Exit(3), Fail, Cancel, Timeout, Interrupt];

    fn job() -> Job {
        let agent = AgentId::parse("a1").unwrap_or_else(|e| panic!("{e}"));
        Job::new(agent, JobId::from_random([0xab; 8]), 10, 60, None)
    }

    #[test]
    fn every_transition() {
        let legal = [
            (Queued, Start, Running),
            (Queued, Cancel, Cancelled),
            (Queued, Interrupt, Cancelled),
            (Running, Exit(0), Succeeded),
            (Running, Exit(3), Failed),
            (Running, Fail, Failed),
            (Running, Cancel, Cancelled),
            (Running, Timeout, TimedOut),
            (Running, Interrupt, Interrupted),
        ];
        for from in JobState::ALL {
            for event in EVENTS {
                let want = legal
                    .iter()
                    .find(|(f, e, _)| *f == from && *e == event)
                    .map(|(_, _, to)| *to);
                match want {
                    Some(to) => assert_eq!(from.apply(event), Ok(to), "{from:?} {event:?}"),
                    None => assert_eq!(
                        from.apply(event),
                        Err(JobTransitionError { from, event }),
                        "{from:?} {event:?}"
                    ),
                }
            }
        }
    }

    #[test]
    fn terminal_states_are_final() {
        for s in JobState::ALL.into_iter().filter(|s| s.is_terminal()) {
            assert!(EVENTS.iter().all(|e| s.apply(*e).is_err()), "{s:?}");
        }
        assert!(!Queued.is_terminal() && !Running.is_terminal());
    }

    #[test]
    fn job_records_times_and_exit_code() {
        let mut j = job();
        assert_eq!(j.deadline_ms(), None);
        assert!(j.apply(Exit(0), 15).is_err());
        assert_eq!(j.state, Queued);
        assert!(j.apply(Start, 20).is_ok());
        assert_eq!((j.started_ms, j.deadline_ms()), (Some(20), Some(60_020)));
        assert!(j.apply(Exit(2), 30).is_ok());
        assert_eq!(
            (j.state, j.exit_code, j.finished_ms),
            (Failed, Some(2), Some(30))
        );
        assert!(j.apply(Cancel, 40).is_err());
        assert_eq!(j.finished_ms, Some(30));
    }

    #[test]
    fn queued_job_dropped_by_interrupt_is_cancelled_without_start() {
        let mut j = job();
        assert!(j.apply(Interrupt, 5).is_ok());
        assert_eq!(
            (j.state, j.started_ms, j.finished_ms),
            (Cancelled, None, Some(5))
        );
    }

    #[test]
    fn names_are_distinct_snake_case() {
        let names: Vec<_> = JobState::ALL.iter().map(|s| s.name()).collect();
        assert_eq!(
            names,
            [
                "queued",
                "running",
                "succeeded",
                "failed",
                "cancelled",
                "timed_out",
                "interrupted"
            ]
        );
    }

    #[test]
    fn ids() {
        assert_eq!(
            JobId::from_random([0, 1, 2, 0xff, 4, 5, 6, 7]).as_str(),
            "000102ff04050607"
        );
        assert!(JobId::parse("0a1b").is_ok());
        for bad in ["", "A1", "../x", "a;b", "a b", "-a", &"a".repeat(33)] {
            assert!(JobId::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn input_rules() {
        assert_eq!(check_prompt(""), Err(JobInputError::EmptyPrompt));
        assert_eq!(check_prompt(&"x".repeat(MAX_PROMPT)), Ok(()));
        assert_eq!(
            check_prompt(&"x".repeat(MAX_PROMPT + 1)),
            Err(JobInputError::PromptTooLarge(MAX_PROMPT + 1))
        );
        assert_eq!(timeout_secs(None), Ok(DEFAULT_TIMEOUT_SECS));
        assert_eq!(timeout_secs(Some(1)), Ok(1));
        assert_eq!(timeout_secs(Some(14_400)), Ok(14_400));
        for bad in [0, 14_401, u64::MAX] {
            assert_eq!(timeout_secs(Some(bad)), Err(JobInputError::Timeout(bad)));
        }
        assert_eq!(check_key("ci:run-42_a.b"), Ok(()));
        for bad in ["", "a b", "a/b", "ü", &"k".repeat(MAX_KEY + 1)] {
            assert_eq!(check_key(bad), Err(JobInputError::Key), "{bad:?}");
        }
    }
}
