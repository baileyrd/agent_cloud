//! Workspace checkpoints and forks (ADR-018) as pure rules: checkpoint ids and labels, which
//! lifecycle states and workspace volumes allow each operation, and the per-agent cap.

use std::fmt;

use crate::id::{self, IdError};
use crate::{AgentId, State};

/// Longest label, in characters.
pub const MAX_LABEL: usize = 80;

/// A checkpoint's id: `YYYYMMDD-HHMMSS` in UTC, with `-N` added when that second is taken. Sorts
/// in creation order (within one second, `-2` sorts after the bare stamp); safe in paths.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CheckpointId(String);

impl CheckpointId {
    /// Parse and validate external input: the agent id rules ([`id::check`]).
    pub fn parse(s: &str) -> Result<Self, IdError> {
        id::check(s).map(|()| Self(s.to_owned()))
    }

    /// The id for a checkpoint made at `unix_secs`, unique among `taken`.
    pub fn next(unix_secs: u64, taken: &[CheckpointId]) -> Self {
        let base = stamp(unix_secs);
        let free = |s: &String| !taken.iter().any(|t| &t.0 == s);
        (1u32..)
            .map(|n| match n {
                1 => base.clone(),
                n => format!("{base}-{n}"),
            })
            .find(free)
            .map_or(Self(base), Self)
    }

    /// The validated string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CheckpointId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// `YYYYMMDD-HHMMSS` for `secs` (UTC); the bare number past the calendar's range.
fn stamp(secs: u64) -> String {
    let Some(t) = i64::try_from(secs)
        .ok()
        .and_then(|s| rusty_time::DateTime::from_unix_secs(s).ok())
    else {
        return secs.to_string();
    };
    let (d, h) = (t.date(), t.time());
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        d.year(),
        d.month(),
        d.day(),
        h.hour(),
        h.minute(),
        h.second()
    )
}

/// The Unix time an id made by [`CheckpointId::next`] stands for (its `YYYYMMDD-HHMMSS`, any
/// `-N` ignored); `None` for any other id.
pub fn stamp_secs(id: &CheckpointId) -> Option<u64> {
    let s = id.as_str();
    let (date, time) = (s.get(..8)?, s.get(9..15)?);
    if s.as_bytes().get(8) != Some(&b'-') || !matches!(s.as_bytes().get(15), None | Some(b'-')) {
        return None;
    }
    let num = |t: &str| {
        t.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| t.parse::<u32>().ok())?
    };
    let part = |t: &str, at: usize, len: usize| t.get(at..at + len).and_then(num);
    let d = rusty_time::Date::from_ymd(
        i32::try_from(part(date, 0, 4)?).ok()?,
        u8::try_from(part(date, 4, 2)?).ok()?,
        u8::try_from(part(date, 6, 2)?).ok()?,
    )
    .ok()?;
    let t = rusty_time::Time::from_hms_nano(
        u8::try_from(part(time, 0, 2)?).ok()?,
        u8::try_from(part(time, 2, 2)?).ok()?,
        u8::try_from(part(time, 4, 2)?).ok()?,
        0,
    )
    .ok()?;
    u64::try_from(rusty_time::DateTime::new(d, t, 0).timestamp()).ok()
}

/// A checkpoint's free-text label: 1 to [`MAX_LABEL`] characters, no control characters, not
/// blank.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label(String);

/// Why a label is rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelError;

impl fmt::Display for LabelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "a label is 1 to {MAX_LABEL} characters with no control characters"
        )
    }
}

impl std::error::Error for LabelError {}

impl Label {
    /// Parse and validate external input.
    pub fn parse(s: &str) -> Result<Self, LabelError> {
        let ok = !s.trim().is_empty()
            && s.chars().count() <= MAX_LABEL
            && !s.chars().any(char::is_control);
        ok.then(|| Self(s.to_owned())).ok_or(LabelError)
    }

    /// The validated text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A checkpoint as listed: its id and label, if it has one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    /// Its id.
    pub id: CheckpointId,
    /// Its label.
    pub label: Option<Label>,
}

/// What an agent's workspace is on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Volume {
    /// Nothing there (never created, or purged).
    Missing,
    /// A plain directory: works for every lifecycle operation, but cannot be snapshotted.
    Directory,
    /// A btrfs subvolume: can be snapshotted.
    Subvolume,
}

/// A workspace operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Snapshot the workspace read-only.
    Checkpoint,
    /// Replace the workspace with a checkpoint.
    Restore,
    /// Copy the workspace (or one of its checkpoints) to a new agent: this is the source's side.
    Fork,
    /// Turn a plain-directory workspace into a subvolume.
    Migrate,
}

impl Op {
    fn name(self) -> &'static str {
        match self {
            Self::Checkpoint => "checkpoint",
            Self::Restore => "restore",
            Self::Fork => "fork",
            Self::Migrate => "migrate",
        }
    }

    /// The lifecycle states it is allowed in. Restore and migrate swap the workspace directory,
    /// which a running VM's bind mount would keep holding, so they need the VM stopped.
    fn states(self) -> &'static [State] {
        match self {
            Self::Checkpoint => &[State::Running, State::Paused],
            Self::Restore => &[State::Paused],
            Self::Fork => &[State::Running, State::Paused, State::Destroyed],
            Self::Migrate => &[State::Paused, State::Destroyed],
        }
    }
}

/// Why a workspace operation is refused. Nothing was changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The agent's lifecycle state does not allow it.
    State {
        /// The agent.
        agent: AgentId,
        /// What was asked.
        op: Op,
        /// The state it is in.
        state: State,
    },
    /// The agent's workspace is not what the operation needs.
    Volume {
        /// The agent.
        agent: AgentId,
        /// What was asked.
        op: Op,
        /// What its workspace is.
        volume: Volume,
    },
    /// A fork's new agent already has a workspace, or a running or paused VM.
    Taken(AgentId),
    /// The agent has as many checkpoints as it may keep.
    Full {
        /// The agent.
        agent: AgentId,
        /// The cap.
        max: usize,
    },
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::State { agent, op, state } => {
                let want: Vec<String> = op
                    .states()
                    .iter()
                    .map(|s| format!("{s:?}").to_lowercase())
                    .collect();
                let state = format!("{state:?}").to_lowercase();
                write!(
                    f,
                    "cannot {} agent {agent}: it is {state}; needs {}",
                    op.name(),
                    want.join(" or ")
                )
            }
            Self::Volume {
                agent,
                op,
                volume: Volume::Directory,
            } if *op != Op::Migrate => write!(
                f,
                "cannot {} agent {agent}: its workspace is a plain directory; pause it and run \
                 `ac-ctl workspace migrate {agent}`",
                op.name()
            ),
            Self::Volume { agent, op, volume } => write!(
                f,
                "cannot {} agent {agent}: its workspace is {}",
                op.name(),
                match volume {
                    Volume::Missing => "gone (purged)",
                    Volume::Directory => "a plain directory",
                    Volume::Subvolume => "already a subvolume",
                }
            ),
            Self::Taken(agent) => write!(
                f,
                "agent {agent} already has a workspace or a VM; destroy --purge it first"
            ),
            Self::Full { agent, max } => write!(
                f,
                "agent {agent} has {max} checkpoints, the most it may keep; delete one first"
            ),
        }
    }
}

impl std::error::Error for Refusal {}

/// Whether `op` may run on `agent`, in `state`, with its workspace `volume`.
pub fn check(agent: &AgentId, op: Op, state: State, volume: Volume) -> Result<(), Refusal> {
    if !op.states().contains(&state) {
        return Err(Refusal::State {
            agent: agent.clone(),
            op,
            state,
        });
    }
    let want = match op {
        Op::Migrate => Volume::Directory,
        _ => Volume::Subvolume,
    };
    if volume != want {
        return Err(Refusal::Volume {
            agent: agent.clone(),
            op,
            volume,
        });
    }
    Ok(())
}

/// Whether `agent` may become a fork: it has no running or paused VM and no workspace.
pub fn check_fork_target(
    agent: &AgentId,
    state: Option<State>,
    volume: Volume,
) -> Result<(), Refusal> {
    let live = matches!(state, Some(State::Running | State::Paused));
    if live || volume != Volume::Missing {
        return Err(Refusal::Taken(agent.clone()));
    }
    Ok(())
}

/// Whether `agent`, holding `count` checkpoints, may make one more under the cap `max`.
pub fn check_room(agent: &AgentId, count: usize, max: usize) -> Result<(), Refusal> {
    if count >= max {
        return Err(Refusal::Full {
            agent: agent.clone(),
            max,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a1() -> AgentId {
        AgentId::parse("a1").unwrap_or_else(|e| panic!("{e}"))
    }

    fn cp(s: &str) -> CheckpointId {
        CheckpointId::parse(s).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn ids_are_utc_stamps_made_unique() {
        // 2026-10-28 13:05:12 UTC.
        let t = 1_793_192_712;
        assert_eq!(CheckpointId::next(t, &[]).as_str(), "20261028-130512");
        let taken = [cp("20261028-130512"), cp("20261028-130512-2")];
        assert_eq!(CheckpointId::next(t, &taken).as_str(), "20261028-130512-3");
        assert!(cp("20261028-130512") < cp("20261028-130512-2"));
        assert!(cp("20261028-130512-9") < cp("20261028-130513"));
    }

    #[test]
    fn stamps_read_back_as_the_time_they_were_made_at() {
        let t = 1_793_192_712;
        assert_eq!(stamp_secs(&CheckpointId::next(t, &[])), Some(t));
        assert_eq!(stamp_secs(&cp("20261028-130512-3")), Some(t));
        for other in [
            "a1",
            "20261028-130512x",
            "20261328-130512",
            "2026102-8130512",
            "20261028-1305",
        ] {
            assert_eq!(stamp_secs(&cp(other)), None, "{other}");
        }
    }

    #[test]
    fn ids_reject_what_agent_ids_reject() {
        for bad in ["", "../x", "a.b", "A", "-a"] {
            assert!(CheckpointId::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn labels() {
        assert!(Label::parse("before the refactor").is_ok());
        assert!(Label::parse(&"x".repeat(MAX_LABEL)).is_ok());
        for bad in ["", "   ", "a\nb", "tab\there", &"x".repeat(MAX_LABEL + 1)] {
            assert_eq!(Label::parse(bad), Err(LabelError), "{bad:?}");
        }
    }

    #[test]
    fn states_each_op_allows() {
        use State::*;
        let ok = |op, s| check(&a1(), op, s, Volume::Subvolume).is_ok();
        assert!(ok(Op::Checkpoint, Running) && ok(Op::Checkpoint, Paused));
        assert!(!ok(Op::Checkpoint, Destroyed) && !ok(Op::Checkpoint, Creating));
        assert!(ok(Op::Restore, Paused));
        assert!(!ok(Op::Restore, Running) && !ok(Op::Restore, Destroyed));
        assert!(ok(Op::Fork, Running) && ok(Op::Fork, Paused) && ok(Op::Fork, Destroyed));
        assert!(!ok(Op::Fork, Creating));
        let migrate = |s| check(&a1(), Op::Migrate, s, Volume::Directory).is_ok();
        assert!(migrate(Paused) && migrate(Destroyed) && !migrate(Running));
    }

    #[test]
    fn volumes_each_op_needs() {
        let r = check(&a1(), Op::Checkpoint, State::Running, Volume::Directory);
        let msg = r.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(msg.contains("ac-ctl workspace migrate a1"), "{msg}");
        let r = check(&a1(), Op::Fork, State::Destroyed, Volume::Missing);
        assert!(matches!(r, Err(Refusal::Volume { .. })));
        let r = check(&a1(), Op::Migrate, State::Paused, Volume::Subvolume);
        let msg = r.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(msg.contains("already a subvolume"), "{msg}");
    }

    #[test]
    fn state_is_checked_before_volume() {
        let r = check(&a1(), Op::Restore, State::Running, Volume::Directory);
        let msg = r.err().map(|e| e.to_string()).unwrap_or_default();
        assert_eq!(msg, "cannot restore agent a1: it is running; needs paused");
    }

    #[test]
    fn fork_target_must_be_free() {
        let free = |s, v| check_fork_target(&a1(), s, v).is_ok();
        assert!(free(None, Volume::Missing));
        assert!(free(Some(State::Destroyed), Volume::Missing));
        assert!(!free(Some(State::Destroyed), Volume::Directory));
        assert!(!free(Some(State::Paused), Volume::Missing));
        assert!(!free(None, Volume::Subvolume));
    }

    #[test]
    fn cap() {
        assert!(check_room(&a1(), 7, 8).is_ok());
        assert_eq!(
            check_room(&a1(), 8, 8),
            Err(Refusal::Full {
                agent: a1(),
                max: 8
            })
        );
    }
}
