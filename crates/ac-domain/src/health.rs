//! Telling a slow agent from a wedged one (ADR-012). A wedged Kata shim (stdin EOF reached a live
//! exec, ADR-006) never answers again, however idle the host; a slow one answers within a generous
//! budget, or fails only while the host is busy. So the watchdog acts only on sustained failures on
//! a host that is not busy, and then walks a ladder: kill the gateway's own exec clients, then
//! stale foreign ones, then mark the agent unhealthy. Pure: the driver supplies time, probes,
//! pressure and the process list.

use std::fmt;
use std::time::Duration;

/// Exec-id prefix of the watchdog's probes.
pub const PROBE_EXEC_PREFIX: &str = "acprobe";
/// Exec-id prefix of the viewer bridge's long-lived execs.
pub const VIEW_EXEC_PREFIX: &str = "acview";
/// Exec-id prefix of the gateway's other one-shot execs (job start, status, kill, read; captures).
pub const ONCE_EXEC_PREFIX: &str = "acexec";
/// Every exec the gateway starts has an id `<prefix>-<agent>-<pid>-<n>` with one of these: the one
/// place that says which `ctr` exec clients are its own.
pub const OWN_EXEC_PREFIXES: [&str; 3] = [PROBE_EXEC_PREFIX, VIEW_EXEC_PREFIX, ONCE_EXEC_PREFIX];

/// Whether an exec id is one the gateway made.
pub fn is_own(exec_id: &str) -> bool {
    OWN_EXEC_PREFIXES.iter().any(|p| {
        exec_id
            .strip_prefix(p)
            .is_some_and(|rest| rest.starts_with('-'))
    })
}

/// When the watchdog may act. All configurable; the defaults are deliberately patient.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thresholds {
    /// How long one probe may take and still count as an answer.
    pub probe_budget: Duration,
    /// Consecutive failed probes before acting.
    pub failures: u32,
    /// ... spanning at least this long.
    pub window: Duration,
    /// Busy when the 1-minute load exceeds this many times the core count.
    pub load_per_core: f64,
    /// Busy when `/proc/pressure/io` `some avg10` exceeds this.
    pub io_some: f64,
    /// Busy when `/proc/pressure/cpu` `some avg10` exceeds this.
    pub cpu_some: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            probe_budget: Duration::from_secs(10),
            failures: 3,
            window: Duration::from_secs(90),
            load_per_core: 1.0,
            io_some: 20.0,
            cpu_some: 50.0,
        }
    }
}

/// How loaded the host is now.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pressure {
    /// 1-minute load average.
    pub load1: f64,
    /// Online CPUs.
    pub cores: u32,
    /// `/proc/pressure/cpu` `some avg10`.
    pub cpu_some: f64,
    /// `/proc/pressure/io` `some avg10`.
    pub io_some: f64,
}

impl Pressure {
    /// Why the host counts as busy, if it does.
    pub fn busy(&self, t: &Thresholds) -> Option<String> {
        let load_limit = t.load_per_core * f64::from(self.cores);
        if self.load1 > load_limit {
            return Some(format!("load {:.2} > {load_limit:.2}", self.load1));
        }
        if self.io_some > t.io_some {
            return Some(format!(
                "io PSI some {:.2} > {:.2}",
                self.io_some, t.io_some
            ));
        }
        if self.cpu_some > t.cpu_some {
            return Some(format!(
                "cpu PSI some {:.2} > {:.2}",
                self.cpu_some, t.cpu_some
            ));
        }
        None
    }
}

impl fmt::Display for Pressure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "load {:.2} on {} cores, cpu PSI some {:.2}, io PSI some {:.2}",
            self.load1, self.cores, self.cpu_some, self.io_some
        )
    }
}

/// What one probe showed, with how long it took.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// Answered within the budget.
    Answered(Duration),
    /// No answer within the budget.
    Failed(Duration),
}

/// What the watchdog should do after a probe.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// It answered. `recovered`: it had failed before or was marked unhealthy, now cleared.
    Healthy {
        /// Probe latency.
        latency: Duration,
        /// A failure streak or the unhealthy mark was cleared.
        recovered: bool,
    },
    /// It failed while the host is busy: slow, not wedged. Nothing is done and the streak restarts.
    Busy {
        /// Why the host counts as busy.
        why: String,
    },
    /// It failed on an idle host, but not yet for long enough. Nothing is done.
    Suspect {
        /// Consecutive failures so far.
        failures: u32,
        /// Since the first of them.
        span: Duration,
    },
    /// Sustained failures on an idle host: walk the ladder now.
    Escalate {
        /// Consecutive failures.
        failures: u32,
        /// Since the first of them.
        span: Duration,
        /// Their probe latencies, oldest first.
        latencies: Vec<Duration>,
    },
}

/// One agent's failure streak and unhealthy mark.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Tracker {
    streak: Option<(Duration, Vec<Duration>)>,
    unhealthy: bool,
}

impl Tracker {
    /// Record probe `probe` taken at `now` (any monotonic clock) on a host under `pressure`.
    pub fn observe(
        &mut self,
        now: Duration,
        probe: Probe,
        pressure: &Pressure,
        t: &Thresholds,
    ) -> Verdict {
        let latency = match probe {
            Probe::Answered(latency) => {
                let recovered = self.streak.take().is_some() || self.unhealthy;
                self.unhealthy = false;
                return Verdict::Healthy { latency, recovered };
            }
            Probe::Failed(latency) => latency,
        };
        if let Some(why) = pressure.busy(t) {
            self.streak = None;
            return Verdict::Busy { why };
        }
        let (since, latencies) = self.streak.get_or_insert_with(|| (now, Vec::new()));
        latencies.push(latency);
        let failures = u32::try_from(latencies.len()).unwrap_or(u32::MAX);
        let span = now.saturating_sub(*since);
        if self.unhealthy || failures < t.failures || span < t.window {
            return Verdict::Suspect { failures, span };
        }
        Verdict::Escalate {
            failures,
            span,
            latencies: latencies.clone(),
        }
    }

    /// When the current failure streak started, if there is one.
    pub fn failing_since(&self) -> Option<Duration> {
        self.streak.as_ref().map(|(since, _)| *since)
    }

    /// The whole ladder failed: report it unhealthy until a probe answers. No further escalation
    /// until then.
    pub fn mark_unhealthy(&mut self) {
        self.unhealthy = true;
    }

    /// Whether it is marked unhealthy.
    pub fn unhealthy(&self) -> bool {
        self.unhealthy
    }
}

/// A host `ctr task exec` client of an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecClient {
    /// Process id.
    pub pid: u32,
    /// Its `--exec-id`.
    pub exec_id: String,
    /// How long it has run.
    pub age: Duration,
    /// Its command line, for the log.
    pub cmdline: String,
}

/// A rung of the ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rung {
    /// Kill the gateway's own clients (by exec-id prefix).
    Own,
    /// Kill foreign clients that were already running when the agent stopped answering.
    Stale,
}

/// Which `clients` a rung kills, and which it leaves (with why), for an agent failing for
/// `failing_for`. [`Rung::Own`] never touches a foreign client; [`Rung::Stale`] never touches one
/// that started after the failures began (it cannot be the cause).
pub fn targets(
    rung: Rung,
    clients: &[ExecClient],
    failing_for: Duration,
) -> (Vec<&ExecClient>, Vec<(&ExecClient, &'static str)>) {
    let mut kill = Vec::new();
    let mut spare = Vec::new();
    for c in clients {
        let own = is_own(&c.exec_id);
        match rung {
            Rung::Own if own => kill.push(c),
            Rung::Own => spare.push((c, "not the gateway's (exec-id prefix)")),
            Rung::Stale if own => spare.push((c, "the gateway's own, handled at rung 1")),
            Rung::Stale if c.age >= failing_for => kill.push(c),
            Rung::Stale => spare.push((c, "started after the agent stopped answering")),
        }
    }
    (kill, spare)
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: fn(u64) -> Duration = Duration::from_secs;

    fn idle() -> Pressure {
        Pressure {
            load1: 0.3,
            cores: 4,
            cpu_some: 1.0,
            io_some: 2.0,
        }
    }

    #[test]
    fn own_exec_ids() {
        assert!(is_own("acprobe-a1-123-0"));
        assert!(is_own("acview-a1-123-7"));
        assert!(is_own("acexec-a1-1-1"));
        assert!(!is_own("acviewer-a1-1"));
        assert!(!is_own("bench-55-12"));
        assert!(!is_own("acprobe"));
    }

    #[test]
    fn busy_host() {
        let t = Thresholds::default();
        assert_eq!(idle().busy(&t), None);
        let load = Pressure {
            load1: 4.5,
            ..idle()
        };
        assert_eq!(load.busy(&t).as_deref(), Some("load 4.50 > 4.00"));
        let io = Pressure {
            io_some: 30.0,
            ..idle()
        };
        assert!(io.busy(&t).is_some_and(|w| w.starts_with("io PSI some")));
        let cpu = Pressure {
            cpu_some: 60.0,
            ..idle()
        };
        assert!(cpu.busy(&t).is_some_and(|w| w.starts_with("cpu PSI some")));
        // Configurable.
        let lax = Thresholds {
            load_per_core: 2.0,
            io_some: 50.0,
            cpu_some: 90.0,
            ..t
        };
        assert_eq!(load.busy(&lax), None);
        assert_eq!(io.busy(&lax), None);
    }

    #[test]
    fn escalates_only_after_k_failures_over_the_window() {
        let t = Thresholds::default();
        let mut h = Tracker::default();
        let fail = Probe::Failed(S(10));
        assert_eq!(
            h.observe(S(0), fail, &idle(), &t),
            Verdict::Suspect {
                failures: 1,
                span: S(0)
            }
        );
        assert!(matches!(
            h.observe(S(30), fail, &idle(), &t),
            Verdict::Suspect { failures: 2, .. }
        ));
        // Three failures, but only 60 s apart.
        assert!(matches!(
            h.observe(S(60), fail, &idle(), &t),
            Verdict::Suspect { failures: 3, .. }
        ));
        assert!(matches!(
            h.observe(S(90), fail, &idle(), &t),
            Verdict::Escalate { failures: 4, span, .. } if span == S(90)
        ));
        assert_eq!(h.failing_since(), Some(S(0)));
    }

    #[test]
    fn a_busy_host_restarts_the_streak_and_an_answer_clears_it() {
        let t = Thresholds::default();
        let mut h = Tracker::default();
        let fail = Probe::Failed(S(10));
        let _ = h.observe(S(0), fail, &idle(), &t);
        let busy = Pressure {
            load1: 9.0,
            ..idle()
        };
        assert!(matches!(
            h.observe(S(30), fail, &busy, &t),
            Verdict::Busy { .. }
        ));
        assert_eq!(h.failing_since(), None);
        let _ = h.observe(S(60), fail, &idle(), &t);
        assert_eq!(
            h.observe(S(70), Probe::Answered(S(9)), &idle(), &t),
            Verdict::Healthy {
                latency: S(9),
                recovered: true
            }
        );
        assert_eq!(
            h.observe(S(80), Probe::Answered(S(1)), &idle(), &t),
            Verdict::Healthy {
                latency: S(1),
                recovered: false
            }
        );
    }

    #[test]
    fn unhealthy_holds_without_re_escalating_until_an_answer() {
        let t = Thresholds {
            failures: 1,
            window: S(0),
            ..Thresholds::default()
        };
        let mut h = Tracker::default();
        let fail = Probe::Failed(S(10));
        assert!(matches!(
            h.observe(S(0), fail, &idle(), &t),
            Verdict::Escalate { .. }
        ));
        h.mark_unhealthy();
        assert!(matches!(
            h.observe(S(30), fail, &idle(), &t),
            Verdict::Suspect { .. }
        ));
        assert!(h.unhealthy());
        let v = h.observe(S(60), Probe::Answered(S(1)), &idle(), &t);
        assert!(matches!(
            v,
            Verdict::Healthy {
                recovered: true,
                ..
            }
        ));
        assert!(!h.unhealthy());
    }

    fn client(pid: u32, exec_id: &str, age: u64) -> ExecClient {
        ExecClient {
            pid,
            exec_id: exec_id.into(),
            age: S(age),
            cmdline: format!("ctr -n agents task exec --exec-id {exec_id} a1 sh"),
        }
    }

    #[test]
    fn rungs_pick_their_targets() {
        let clients = [
            client(1, "acprobe-a1-9-3", 12),
            client(2, "acview-a1-9-1", 3000),
            client(3, "bench-77-1", 600),
            client(4, "user-shell", 20),
        ];
        let (kill, spare) = targets(Rung::Own, &clients, S(120));
        assert_eq!(kill.iter().map(|c| c.pid).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(spare.iter().map(|(c, _)| c.pid).collect::<Vec<_>>(), [3, 4]);
        let (kill, spare) = targets(Rung::Stale, &clients, S(120));
        assert_eq!(kill.iter().map(|c| c.pid).collect::<Vec<_>>(), [3]);
        assert_eq!(
            spare
                .iter()
                .map(|(c, why)| (c.pid, *why))
                .collect::<Vec<_>>(),
            [
                (1, "the gateway's own, handled at rung 1"),
                (2, "the gateway's own, handled at rung 1"),
                (4, "started after the agent stopped answering")
            ]
        );
    }
}
