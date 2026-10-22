//! Agent lifecycle as a pure state machine.

use std::fmt;

/// Where an agent is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// microVM and volume being provisioned.
    Creating,
    /// Running and reachable through the gateway.
    Running,
    /// VM stopped; workspace volume kept.
    Paused,
    /// Torn down. Terminal.
    Destroyed,
}

/// A requested change of state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Provisioning finished.
    Ready,
    /// Stop the VM, keep the volume.
    Pause,
    /// Start a paused VM.
    Resume,
    /// Tear everything down.
    Destroy,
}

/// A command that is illegal in the current state.
#[derive(Debug, PartialEq, Eq)]
pub struct TransitionError {
    /// State the agent was in.
    pub from: State,
    /// Command that was refused.
    pub command: Command,
}

impl fmt::Display for TransitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cannot {:?} an agent that is {:?}",
            self.command, self.from
        )
    }
}

impl std::error::Error for TransitionError {}

impl State {
    /// Apply `command`, returning the next state or why it is refused.
    pub fn apply(self, command: Command) -> Result<State, TransitionError> {
        use Command::*;
        use State::*;
        match (self, command) {
            (Creating, Ready) | (Paused, Resume) => Ok(Running),
            (Running, Pause) => Ok(Paused),
            (Creating | Running | Paused, Destroy) => Ok(Destroyed),
            (from, command) => Err(TransitionError { from, command }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Command::*, State::*, *};

    #[test]
    fn happy_path() {
        let s = Creating
            .apply(Ready)
            .and_then(|s| s.apply(Pause))
            .and_then(|s| s.apply(Resume));
        assert_eq!(s, Ok(Running));
        assert_eq!(Running.apply(Destroy), Ok(Destroyed));
    }

    #[test]
    fn destroyed_is_terminal() {
        for c in [Ready, Pause, Resume, Destroy] {
            assert!(Destroyed.apply(c).is_err(), "{c:?}");
        }
    }

    #[test]
    fn refuses_illegal() {
        assert_eq!(
            Creating.apply(Pause),
            Err(TransitionError {
                from: Creating,
                command: Pause
            })
        );
        assert!(Running.apply(Resume).is_err());
        assert!(Paused.apply(Ready).is_err());
    }
}
