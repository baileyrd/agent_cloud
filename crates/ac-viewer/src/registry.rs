//! Who is watching which agent, within caps, and the take-over lock: at most one viewer per
//! agent may type. Everyone else is view-only. A second take-over is refused unless forced;
//! releasing or disconnecting returns the agent to view-only. Every change is an [`Event`].

use std::collections::HashMap;
use std::fmt;

/// Why a viewer could not join.
#[derive(Debug, PartialEq, Eq)]
pub enum Full {
    /// This agent has its maximum number of viewers.
    Agent,
    /// The gateway has its maximum number of viewers.
    Total,
}

/// A take-over or release, for the audit log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// `viewer` now holds the lock; `displaced` held it before a forced take-over.
    TakeOver {
        /// The new holder.
        viewer: String,
        /// Taken from another holder.
        forced: bool,
        /// The previous holder, if forced.
        displaced: Option<String>,
    },
    /// `viewer` no longer holds the lock.
    Release {
        /// The former holder.
        viewer: String,
        /// Why.
        reason: Reason,
    },
}

/// Why a holder lost the lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// It asked to.
    Released,
    /// Its stream ended.
    Disconnected,
}

impl fmt::Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TakeOver {
                viewer,
                forced: false,
                ..
            } => write!(f, "takeover viewer={viewer}"),
            Self::TakeOver {
                viewer, displaced, ..
            } => write!(
                f,
                "takeover viewer={viewer} forced=true displaced={}",
                displaced.as_deref().unwrap_or("-")
            ),
            Self::Release {
                viewer,
                reason: Reason::Released,
            } => write!(f, "release viewer={viewer}"),
            Self::Release {
                viewer,
                reason: Reason::Disconnected,
            } => {
                write!(f, "release viewer={viewer} reason=disconnected")
            }
        }
    }
}

/// Refused: someone else holds the lock.
#[derive(Debug, PartialEq, Eq)]
pub struct Held(pub String);

#[derive(Default)]
struct Agent {
    viewers: Vec<String>,
    owner: Option<String>,
}

/// Viewers of every agent, with caps.
pub struct Registry {
    per_agent: usize,
    total: usize,
    count: usize,
    agents: HashMap<String, Agent>,
}

impl Registry {
    /// At most `per_agent` viewers of one agent and `total` in all.
    pub fn new(per_agent: usize, total: usize) -> Self {
        Self {
            per_agent,
            total,
            count: 0,
            agents: HashMap::new(),
        }
    }

    /// Add a view-only viewer.
    pub fn join(&mut self, agent: &str, viewer: &str) -> Result<(), Full> {
        if self.viewers(agent) >= self.per_agent {
            return Err(Full::Agent);
        }
        if self.count >= self.total {
            return Err(Full::Total);
        }
        let a = self.agents.entry(agent.to_owned()).or_default();
        a.viewers.push(viewer.to_owned());
        self.count += 1;
        Ok(())
    }

    /// Remove a viewer; if it held the lock, that is released.
    pub fn leave(&mut self, agent: &str, viewer: &str) -> Option<Event> {
        let a = self.agents.get_mut(agent)?;
        let at = a.viewers.iter().position(|v| v == viewer)?;
        a.viewers.remove(at);
        self.count -= 1;
        let event = (a.owner.as_deref() == Some(viewer)).then(|| {
            a.owner = None;
            Event::Release {
                viewer: viewer.to_owned(),
                reason: Reason::Disconnected,
            }
        });
        if a.viewers.is_empty() {
            self.agents.remove(agent);
        }
        event
    }

    /// Give `viewer` the lock. Refused while another viewer holds it, unless `force`.
    /// `Ok(None)`: it already held it.
    pub fn take_over(
        &mut self,
        agent: &str,
        viewer: &str,
        force: bool,
    ) -> Result<Option<Event>, Held> {
        let Some(a) = self
            .agents
            .get_mut(agent)
            .filter(|a| a.viewers.iter().any(|v| v == viewer))
        else {
            return Err(Held(String::new()));
        };
        match a.owner.clone() {
            Some(o) if o == viewer => Ok(None),
            Some(o) if !force => Err(Held(o)),
            previous => {
                a.owner = Some(viewer.to_owned());
                Ok(Some(Event::TakeOver {
                    viewer: viewer.to_owned(),
                    forced: previous.is_some(),
                    displaced: previous,
                }))
            }
        }
    }

    /// Give the lock up. `None` if `viewer` did not hold it.
    pub fn release(&mut self, agent: &str, viewer: &str) -> Option<Event> {
        let a = self.agents.get_mut(agent)?;
        (a.owner.as_deref() == Some(viewer)).then(|| {
            a.owner = None;
            Event::Release {
                viewer: viewer.to_owned(),
                reason: Reason::Released,
            }
        })
    }

    /// Whether `viewer` may type into `agent`.
    pub fn may_write(&self, agent: &str, viewer: &str) -> bool {
        self.owner(agent) == Some(viewer)
    }

    /// The lock holder.
    pub fn owner(&self, agent: &str) -> Option<&str> {
        self.agents.get(agent)?.owner.as_deref()
    }

    /// Whether `viewer` is watching `agent`.
    pub fn is_viewer(&self, agent: &str, viewer: &str) -> bool {
        self.agents
            .get(agent)
            .is_some_and(|a| a.viewers.iter().any(|v| v == viewer))
    }

    /// Viewers of `agent`.
    pub fn viewers(&self, agent: &str) -> usize {
        self.agents.get(agent).map_or(0, |a| a.viewers.len())
    }

    /// Viewers in all.
    pub fn watching(&self) -> usize {
        self.count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_per_agent_and_total() {
        let mut r = Registry::new(2, 3);
        assert_eq!(r.join("a1", "v1"), Ok(()));
        assert_eq!(r.join("a1", "v2"), Ok(()));
        assert_eq!(r.join("a1", "v3"), Err(Full::Agent));
        assert_eq!(r.join("a2", "v4"), Ok(()));
        assert_eq!(r.join("a3", "v5"), Err(Full::Total));
        assert_eq!(
            r.join("a1", "v6"),
            Err(Full::Agent),
            "per-agent is reported first"
        );
        let _ = r.leave("a1", "v1");
        assert_eq!(r.join("a3", "v5"), Ok(()));
        assert_eq!(r.watching(), 3);
    }

    #[test]
    fn view_only_by_default_and_one_writer() {
        let mut r = Registry::new(4, 16);
        let _ = (r.join("a1", "v1"), r.join("a1", "v2"));
        assert!(!r.may_write("a1", "v1"));
        assert_eq!(
            r.take_over("a1", "v1", false),
            Ok(Some(Event::TakeOver {
                viewer: "v1".into(),
                forced: false,
                displaced: None
            }))
        );
        assert!(r.may_write("a1", "v1") && !r.may_write("a1", "v2"));
        assert_eq!(r.take_over("a1", "v1", false), Ok(None));
        assert_eq!(r.take_over("a1", "v2", false), Err(Held("v1".into())));
        assert!(r.may_write("a1", "v1"));
    }

    #[test]
    fn force_displaces_and_release_returns_to_view_only() {
        let mut r = Registry::new(4, 16);
        let _ = (r.join("a1", "v1"), r.join("a1", "v2"));
        let _ = r.take_over("a1", "v1", false);
        let e = r.take_over("a1", "v2", true);
        assert_eq!(
            e,
            Ok(Some(Event::TakeOver {
                viewer: "v2".into(),
                forced: true,
                displaced: Some("v1".into())
            }))
        );
        assert_eq!(
            r.release("a1", "v1"),
            None,
            "a displaced viewer cannot release"
        );
        assert_eq!(
            r.release("a1", "v2").map(|e| e.to_string()).as_deref(),
            Some("release viewer=v2")
        );
        assert_eq!(r.owner("a1"), None);
    }

    #[test]
    fn disconnect_releases() {
        let mut r = Registry::new(4, 16);
        let _ = (r.join("a1", "v1"), r.join("a1", "v2"));
        let _ = r.take_over("a1", "v1", false);
        assert_eq!(r.leave("a1", "v2"), None);
        assert_eq!(
            r.leave("a1", "v1").map(|e| e.to_string()).as_deref(),
            Some("release viewer=v1 reason=disconnected")
        );
        assert_eq!((r.owner("a1"), r.watching()), (None, 0));
        assert_eq!(r.leave("a1", "v1"), None);
    }

    #[test]
    fn strangers_cannot_take_over() {
        let mut r = Registry::new(4, 16);
        let _ = r.join("a1", "v1");
        assert!(r.take_over("a1", "zz", true).is_err());
        assert!(r.take_over("a2", "v1", true).is_err());
        assert_eq!(r.owner("a1"), None);
    }

    #[test]
    fn event_text() {
        let e = Event::TakeOver {
            viewer: "v2".into(),
            forced: true,
            displaced: Some("v1".into()),
        };
        assert_eq!(e.to_string(), "takeover viewer=v2 forced=true displaced=v1");
    }
}
