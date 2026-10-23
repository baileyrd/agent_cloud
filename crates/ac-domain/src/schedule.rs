//! Which queued job starts next (ADR-013). A job starts only when its agent has no running job,
//! fewer than [`Limits::max_running`] jobs run host-wide, the host's CPU is not saturated, and the
//! last start was at least [`Limits::spacing_ms`] ago (so it shows in the pressure figures before
//! the next decision). Oldest eligible job first, across agents. Pure: the driver supplies the
//! active jobs, the pressure snapshot and the time.

use std::collections::HashSet;

use crate::health::Pressure;
use crate::{Job, JobId, JobState};

/// Host-wide limits on starting jobs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    /// Most jobs running at once, host-wide.
    pub max_running: u32,
    /// Start only while `/proc/pressure/cpu` `some avg10` is below this.
    pub cpu_some_max: f64,
    /// ... and the 1-minute load is below this many times the core count.
    pub load_per_core: f64,
    /// Least time between two starts, host-wide.
    pub spacing_ms: u64,
}

impl Limits {
    /// No limit at all: every agent starts its next job at once.
    pub const OPEN: Self = Self {
        max_running: u32::MAX,
        cpu_some_max: f64::INFINITY,
        load_per_core: f64::INFINITY,
        spacing_ms: 0,
    };

    /// The defaults for a host with `cores` CPUs: one job per core, CPU PSI below 40, load below
    /// the core count, 15 s between starts.
    pub fn for_cores(cores: u32) -> Self {
        Self {
            max_running: cores.max(1),
            cpu_some_max: 40.0,
            load_per_core: 1.0,
            spacing_ms: 15_000,
        }
    }

    /// Why the host is too busy to start a job, if it is.
    pub fn busy(&self, p: &Pressure) -> Option<String> {
        let load_limit = self.load_per_core * f64::from(p.cores);
        if p.load1 >= load_limit {
            return Some(format!("load {:.2} >= {load_limit:.2}", p.load1));
        }
        if p.cpu_some >= self.cpu_some_max {
            return Some(format!(
                "cpu PSI some {:.2} >= {:.2}",
                p.cpu_some, self.cpu_some_max
            ));
        }
        None
    }
}

/// Why a queued job has not started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    /// Its agent runs a job, or has an older one queued.
    AgentBusy,
    /// [`Limits::max_running`] jobs run.
    SlotsFull,
    /// The host's CPU is saturated.
    HostBusy,
    /// A job started less than [`Limits::spacing_ms`] ago.
    Spacing,
}

impl Wait {
    /// Wire name (`queued_reason`).
    pub fn name(self) -> &'static str {
        match self {
            Self::AgentBusy => "agent busy",
            Self::SlotsFull => "job slots full",
            Self::HostBusy => "host busy",
            Self::Spacing => "start spacing",
        }
    }
}

/// What to do now.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    /// The job to start, if any.
    pub start: Option<JobId>,
    /// Every other queued job and why it waits.
    pub waits: Vec<(JobId, Wait)>,
    /// Why the host counts as busy, when it does.
    pub busy: Option<String>,
}

/// Decide over `active` (queued and running jobs; others are ignored) at `now_ms`. `pressure` is
/// `None` when it could not be read: the pressure gate is then skipped, the others still hold.
pub fn plan(active: &[Job], pressure: Option<&Pressure>, now_ms: u64, limits: &Limits) -> Plan {
    let running: Vec<&Job> = active
        .iter()
        .filter(|j| j.state == JobState::Running)
        .collect();
    let mut queued: Vec<&Job> = active
        .iter()
        .filter(|j| j.state == JobState::Queued)
        .collect();
    queued.sort_by(|a, b| (a.created_ms, a.id.as_str()).cmp(&(b.created_ms, b.id.as_str())));
    let busy = pressure.and_then(|p| limits.busy(p));
    let last = running.iter().filter_map(|j| j.started_ms).max();
    let gate = |n: usize, last: Option<u64>| -> Option<Wait> {
        if n >= limits.max_running as usize {
            Some(Wait::SlotsFull)
        } else if busy.is_some() {
            Some(Wait::HostBusy)
        } else if last.is_some_and(|l| now_ms < l.saturating_add(limits.spacing_ms)) {
            Some(Wait::Spacing)
        } else {
            None
        }
    };
    let mut agents: HashSet<&str> = running.iter().map(|j| j.agent.as_str()).collect();
    let mut plan = Plan::default();
    for j in queued {
        let wait = if !agents.insert(j.agent.as_str()) {
            Some(Wait::AgentBusy)
        } else if plan.start.is_some() {
            // What it will find once the chosen job has started.
            gate(running.len() + 1, Some(now_ms)).or(Some(Wait::Spacing))
        } else {
            gate(running.len(), last)
        };
        match wait {
            Some(w) => plan.waits.push((j.id.clone(), w)),
            None => plan.start = Some(j.id.clone()),
        }
    }
    plan.busy = busy;
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentId, JobEvent};

    const IDLE: Pressure = Pressure {
        load1: 0.4,
        cores: 4,
        cpu_some: 1.0,
        io_some: 0.0,
    };

    fn job(agent: &str, n: u8, created_ms: u64) -> Job {
        let agent = AgentId::parse(agent).unwrap_or_else(|e| panic!("{e}"));
        Job::new(agent, JobId::from_random([n; 8]), created_ms, 60, None)
    }

    fn running(agent: &str, n: u8, started_ms: u64) -> Job {
        let mut j = job(agent, n, started_ms);
        let _ = j.apply(JobEvent::Start, started_ms);
        j
    }

    fn id(n: u8) -> JobId {
        JobId::from_random([n; 8])
    }

    fn waits(p: &Plan) -> Vec<(JobId, &'static str)> {
        p.waits.iter().map(|(j, w)| (j.clone(), w.name())).collect()
    }

    #[test]
    fn oldest_eligible_job_first_across_agents() {
        let limits = Limits::for_cores(4);
        // a2's job is the oldest, a1's two come after it, a3's last.
        let active = [
            job("a1", 1, 200),
            job("a3", 3, 900),
            job("a1", 4, 300),
            job("a2", 2, 100),
        ];
        let p = plan(&active, Some(&IDLE), 10_000, &limits);
        assert_eq!(p.start, Some(id(2)));
        assert_eq!(
            waits(&p),
            [
                (id(1), "start spacing"),
                (id(4), "agent busy"),
                (id(3), "start spacing")
            ]
        );
        assert_eq!(p.busy, None);
    }

    #[test]
    fn an_agent_with_a_running_job_waits_and_others_go() {
        let active = [running("a1", 1, 0), job("a1", 2, 10), job("a2", 3, 20)];
        let p = plan(&active, Some(&IDLE), 60_000, &Limits::for_cores(4));
        assert_eq!(p.start, Some(id(3)));
        assert_eq!(waits(&p), [(id(2), "agent busy")]);
    }

    #[test]
    fn hard_cap_counts_every_running_job() {
        let limits = Limits {
            max_running: 2,
            ..Limits::for_cores(4)
        };
        let active = [running("a1", 1, 0), running("a2", 2, 0), job("a3", 3, 5)];
        let p = plan(&active, Some(&IDLE), 60_000, &limits);
        assert_eq!(p.start, None);
        assert_eq!(waits(&p), [(id(3), "job slots full")]);
        // One fewer running: it starts.
        let p = plan(&active[1..], Some(&IDLE), 60_000, &limits);
        assert_eq!(p.start, Some(id(3)));
        // The job that fills the last slot leaves the next one "job slots full".
        let active = [running("a1", 1, 0), job("a2", 2, 5), job("a3", 3, 6)];
        let p = plan(&active, Some(&IDLE), 60_000, &limits);
        assert_eq!(p.start, Some(id(2)));
        assert_eq!(waits(&p), [(id(3), "job slots full")]);
    }

    #[test]
    fn a_busy_host_starts_nothing_and_says_why() {
        let limits = Limits::for_cores(4);
        let active = [job("a1", 1, 0)];
        let cpu = Pressure {
            cpu_some: 40.0,
            ..IDLE
        };
        let p = plan(&active, Some(&cpu), 60_000, &limits);
        assert_eq!(p.start, None);
        assert_eq!(waits(&p), [(id(1), "host busy")]);
        assert_eq!(p.busy.as_deref(), Some("cpu PSI some 40.00 >= 40.00"));
        let load = Pressure { load1: 4.0, ..IDLE };
        let p = plan(&active, Some(&load), 60_000, &limits);
        assert_eq!(p.busy.as_deref(), Some("load 4.00 >= 4.00"));
        assert_eq!(p.start, None);
        // A threshold of 0 shuts the gate even on an idle host.
        let shut = Limits {
            cpu_some_max: 0.0,
            ..limits
        };
        let p = plan(
            &active,
            Some(&Pressure {
                cpu_some: 0.0,
                ..IDLE
            }),
            60_000,
            &shut,
        );
        assert_eq!(waits(&p), [(id(1), "host busy")]);
        // Unreadable pressure skips this gate only.
        assert_eq!(plan(&active, None, 60_000, &shut).start, Some(id(1)));
    }

    #[test]
    fn spacing_counts_from_the_last_start_host_wide() {
        let limits = Limits::for_cores(4);
        let active = [running("a1", 1, 50_000), job("a2", 2, 51_000)];
        let p = plan(&active, Some(&IDLE), 64_999, &limits);
        assert_eq!(p.start, None);
        assert_eq!(waits(&p), [(id(2), "start spacing")]);
        assert_eq!(
            plan(&active, Some(&IDLE), 65_000, &limits).start,
            Some(id(2))
        );
        // No spacing: everything eligible starts, one decision at a time.
        let open = Limits {
            spacing_ms: 0,
            ..limits
        };
        assert_eq!(plan(&active, Some(&IDLE), 50_001, &open).start, Some(id(2)));
    }

    #[test]
    fn gates_report_the_most_basic_reason_first() {
        // Full and busy and too soon: "job slots full".
        let limits = Limits {
            max_running: 1,
            ..Limits::for_cores(4)
        };
        let hot = Pressure {
            cpu_some: 99.0,
            ..IDLE
        };
        let active = [running("a1", 1, 59_000), job("a2", 2, 0)];
        assert_eq!(
            waits(&plan(&active, Some(&hot), 60_000, &limits)),
            [(id(2), "job slots full")]
        );
        // Busy and too soon: "host busy".
        let limits = Limits::for_cores(4);
        assert_eq!(
            waits(&plan(&active, Some(&hot), 60_000, &limits)),
            [(id(2), "host busy")]
        );
    }

    /// After a gateway restart the running jobs come back from the registry with their start
    /// times, so they fill slots and count for spacing as before.
    #[test]
    fn re_attached_running_jobs_count() {
        let limits = Limits {
            max_running: 2,
            ..Limits::for_cores(4)
        };
        let active = [
            running("a1", 1, 0),
            running("a2", 2, 30_000),
            job("a3", 3, 1),
        ];
        let p = plan(&active, Some(&IDLE), 31_000, &limits);
        assert_eq!(waits(&p), [(id(3), "job slots full")]);
    }

    #[test]
    fn finished_and_cancelled_jobs_are_ignored() {
        let mut cancelled = job("a1", 1, 0);
        let _ = cancelled.apply(JobEvent::Cancel, 5);
        let mut done = running("a2", 2, 0);
        let _ = done.apply(JobEvent::Exit(0), 5);
        let active = [cancelled, done, job("a3", 3, 9)];
        let p = plan(&active, Some(&IDLE), 6, &Limits::for_cores(1));
        assert_eq!(p.start, Some(id(3)));
        assert!(p.waits.is_empty());
    }

    #[test]
    fn open_limits_start_at_once() {
        let active = [running("a1", 1, 10), job("a2", 2, 0)];
        let p = plan(&active, None, 10, &Limits::OPEN);
        assert_eq!(p.start, Some(id(2)));
    }
}
