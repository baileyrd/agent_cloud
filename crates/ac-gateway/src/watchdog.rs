//! The wedge watchdog (ADR-012): probes every running agent, and acts only on sustained failures
//! on a host that is not busy ([`ac_domain::health`]). The ladder, each rung re-probing and stopping
//! as soon as the agent answers: (1) SIGKILL the gateway's own exec clients of the agent (by
//! exec-id prefix); (2) SIGKILL foreign clients that were already running when it stopped
//! answering, each with an evidence line; (3) mark it unhealthy, which is reported in its status
//! and cleared by the next answer, and never blocks a lifecycle call. Every step is logged to the
//! journal and to the agent's history.

use std::collections::HashMap;
use std::time::Duration;

use ac_domain::health::{self, Probe, Rung, Thresholds, Tracker, Verdict};
use ac_domain::AgentId;
use ac_ports::{Clock, HostLoad, Watch};

use crate::Unhealthy;

/// Where each line goes besides the journal: the agent's history.
pub type Notes = Box<dyn Fn(&AgentId, &str) + Send>;

/// Probes, pressure and time, injected; the trackers and the shared unhealthy set.
pub struct Watchdog<W, L, C> {
    watch: W,
    load: L,
    clock: C,
    thresholds: Thresholds,
    trackers: HashMap<String, Tracker>,
    unhealthy: Unhealthy,
    notes: Notes,
}

impl<W: Watch, L: HostLoad, C: Clock> Watchdog<W, L, C> {
    /// A watchdog marking agents in `unhealthy`, noting to `notes`.
    pub fn new(
        watch: W,
        load: L,
        clock: C,
        thresholds: Thresholds,
        unhealthy: Unhealthy,
        notes: Notes,
    ) -> Self {
        Self {
            watch,
            load,
            clock,
            thresholds,
            trackers: HashMap::new(),
            unhealthy,
            notes,
        }
    }

    /// One round over the agents that are running now. Agents no longer running are forgotten
    /// (and lose any unhealthy mark).
    pub fn check(&mut self, running: &[AgentId]) {
        self.trackers
            .retain(|id, _| running.iter().any(|r| r.as_str() == id));
        if let Ok(mut set) = self.unhealthy.lock() {
            set.retain(|id| running.iter().any(|r| r.as_str() == id));
        }
        for id in running {
            self.check_one(id);
        }
    }

    fn check_one(&mut self, id: &AgentId) {
        let probe = self.probe(id);
        let now = self.clock.now();
        let t = self.thresholds;
        let pressure = match self.load.pressure() {
            Ok(p) => p,
            Err(e) => {
                if let Probe::Failed(latency) = probe {
                    self.log(
                        id,
                        &format!(
                            "probe failed after {}; cannot read host pressure ({e}), not acting",
                            secs(latency)
                        ),
                    );
                }
                return;
            }
        };
        let tracker = self.trackers.entry(id.to_string()).or_default();
        let (verdict, marked) = (
            tracker.observe(now, probe, &pressure, &t),
            tracker.unhealthy(),
        );
        match verdict {
            Verdict::Healthy { latency, recovered } => {
                if recovered {
                    self.set_unhealthy(id, false);
                    self.log(id, &format!("answers again ({}); cleared", secs(latency)));
                } else if latency >= t.probe_budget / 4 {
                    self.log(
                        id,
                        &format!("answered slowly ({}); {pressure}", secs(latency)),
                    );
                }
            }
            Verdict::Busy { why } => self.log(
                id,
                &format!(
                    "probe failed after {}: slow, host busy ({why}; {pressure}), not acting",
                    secs(probe_time(probe))
                ),
            ),
            Verdict::Suspect { failures, span } => {
                let state = if marked {
                    "still marked unhealthy"
                } else {
                    "waiting for sustained evidence"
                };
                self.log(
                    id,
                    &format!(
                        "probe failed after {} ({failures} in a row over {}; {pressure}); {state}",
                        secs(probe_time(probe)),
                        secs(span)
                    ),
                );
            }
            Verdict::Escalate {
                failures,
                span,
                latencies,
            } => {
                let lat: Vec<String> = latencies.iter().map(|d| secs(*d)).collect();
                self.log(
                    id,
                    &format!(
                        "{failures} failed probes over {} (latencies {}); {pressure}; acting",
                        secs(span),
                        lat.join(", ")
                    ),
                );
                self.ladder(id, span);
            }
        }
    }

    /// Rungs 1 and 2, re-probing after each; rung 3 if neither helped.
    fn ladder(&mut self, id: &AgentId, failing_for: Duration) {
        for (n, rung) in [(1, Rung::Own), (2, Rung::Stale)] {
            let clients = match self.watch.exec_clients(id) {
                Ok(c) => c,
                Err(e) => {
                    self.log(id, &format!("rung {n}: cannot list exec clients ({e})"));
                    Vec::new()
                }
            };
            let (kill, spare) = health::targets(rung, &clients, failing_for);
            for (c, why) in &spare {
                self.log(
                    id,
                    &format!(
                        "rung {n}: leave pid {} (exec-id {}, age {}): {why}",
                        c.pid,
                        c.exec_id,
                        secs(c.age)
                    ),
                );
            }
            for c in &kill {
                let why = match rung {
                    Rung::Own => "the gateway's own (exec-id prefix)".to_owned(),
                    Rung::Stale => format!(
                        "not the gateway's; running {} before the agent stopped answering",
                        secs(c.age.saturating_sub(failing_for))
                    ),
                };
                self.log(
                    id,
                    &format!(
                        "rung {n}: SIGKILL pid {} (exec-id {}, age {}): {why}; {}",
                        c.pid,
                        c.exec_id,
                        secs(c.age),
                        c.cmdline
                    ),
                );
            }
            let pids: Vec<u32> = kill.iter().map(|c| c.pid).collect();
            if let Err(e) = self.watch.kill(&pids) {
                self.log(id, &format!("rung {n}: kill: {e}"));
            }
            let probe = self.probe(id);
            if let Probe::Answered(latency) = probe {
                if let (Some(tracker), Ok(p)) =
                    (self.trackers.get_mut(id.as_str()), self.load.pressure())
                {
                    let _ = tracker.observe(self.clock.now(), probe, &p, &self.thresholds);
                }
                self.set_unhealthy(id, false);
                self.log(
                    id,
                    &format!("answers again after rung {n} ({})", secs(latency)),
                );
                return;
            }
            self.log(
                id,
                &format!("rung {n}: still no answer ({})", secs(probe_time(probe))),
            );
        }
        if let Some(tracker) = self.trackers.get_mut(id.as_str()) {
            tracker.mark_unhealthy();
        }
        self.set_unhealthy(id, true);
        self.log(
            id,
            "rung 3: marked unhealthy; VM left running; lifecycle calls still allowed; cleared by \
             the next answer",
        );
    }

    fn probe(&self, id: &AgentId) -> Probe {
        let start = self.clock.now();
        let ok = self.watch.probe(id, self.thresholds.probe_budget);
        let took = self.clock.now().saturating_sub(start);
        if ok {
            Probe::Answered(took)
        } else {
            Probe::Failed(took)
        }
    }

    fn set_unhealthy(&self, id: &AgentId, on: bool) {
        if let Ok(mut set) = self.unhealthy.lock() {
            if on {
                set.insert(id.to_string());
            } else {
                set.remove(id.as_str());
            }
        }
    }

    fn log(&self, id: &AgentId, line: &str) {
        let line = format!("watchdog: {id}: {line}");
        eprintln!("ac-gateway: {line}");
        (self.notes)(id, &line);
    }
}

fn probe_time(p: Probe) -> Duration {
    match p {
        Probe::Answered(d) | Probe::Failed(d) => d,
    }
}

fn secs(d: Duration) -> String {
    format!("{:.1}s", d.as_secs_f64())
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};

    use ac_domain::health::{ExecClient, Pressure};
    use ac_ports::PortError;

    use super::*;

    const S: fn(u64) -> Duration = Duration::from_secs;

    /// Shared fake state: the clock, the probe script (latency, answered), the clients, the kills.
    #[derive(Default)]
    struct World {
        now: Cell<Duration>,
        probes: RefCell<Vec<(u64, bool)>>,
        clients: RefCell<Vec<ExecClient>>,
        killed: RefCell<Vec<Vec<u32>>>,
        pressure: Cell<Option<Pressure>>,
    }

    #[derive(Clone)]
    struct Fake(Rc<World>);

    impl Watch for Fake {
        fn probe(&self, _: &AgentId, budget: Duration) -> bool {
            let (secs, ok) = self.0.probes.borrow_mut().remove(0);
            // A probe never takes longer than its budget: `timeout` stops it.
            let took = S(secs).min(budget);
            self.0.now.set(self.0.now.get() + took);
            ok && S(secs) <= budget
        }
        fn exec_clients(&self, _: &AgentId) -> Result<Vec<ExecClient>, PortError> {
            Ok(self.0.clients.borrow().clone())
        }
        fn kill(&self, pids: &[u32]) -> Result<(), PortError> {
            self.0.killed.borrow_mut().push(pids.to_vec());
            self.0
                .clients
                .borrow_mut()
                .retain(|c| !pids.contains(&c.pid));
            Ok(())
        }
    }

    impl HostLoad for Fake {
        fn pressure(&self) -> Result<Pressure, PortError> {
            self.0.pressure.get().ok_or_else(|| PortError {
                op: "pressure",
                cause: "none".into(),
            })
        }
    }

    impl Clock for Fake {
        fn now(&self) -> Duration {
            self.0.now.get()
        }
    }

    const IDLE: Pressure = Pressure {
        load1: 0.2,
        cores: 4,
        cpu_some: 0.5,
        io_some: 1.0,
    };
    const BUSY: Pressure = Pressure {
        load1: 9.5,
        cores: 4,
        cpu_some: 60.0,
        io_some: 45.0,
    };

    fn client(pid: u32, exec_id: &str, age: u64) -> ExecClient {
        ExecClient {
            pid,
            exec_id: exec_id.into(),
            age: S(age),
            cmdline: format!("ctr -n agents task exec --exec-id {exec_id} a1 true"),
        }
    }

    /// The evidence lines the watchdog noted (its history log).
    type Log = Arc<Mutex<Vec<String>>>;

    fn dog(world: &Rc<World>, t: Thresholds) -> (Watchdog<Fake, Fake, Fake>, Unhealthy, Log) {
        let unhealthy = Unhealthy::default();
        let log = Log::default();
        let sink = Arc::clone(&log);
        let notes: Notes = Box::new(move |_, line| {
            if let Ok(mut l) = sink.lock() {
                l.push(line.to_owned());
            }
        });
        let fake = Fake(Rc::clone(world));
        let d = Watchdog::new(
            fake.clone(),
            fake.clone(),
            fake,
            t,
            Arc::clone(&unhealthy),
            notes,
        );
        (d, unhealthy, log)
    }

    fn lines(log: &Log) -> Vec<String> {
        log.lock().map(|l| l.clone()).unwrap_or_default()
    }

    fn a1() -> AgentId {
        AgentId::parse("a1").unwrap_or_else(|e| panic!("{e}"))
    }

    /// Run `rounds` probe rounds `every` apart.
    fn rounds(d: &mut Watchdog<Fake, Fake, Fake>, world: &World, n: usize, every: u64) {
        for _ in 0..n {
            d.check(&[a1()]);
            world.now.set(world.now.get() + S(every));
        }
    }

    fn world(probes: &[(u64, bool)], pressure: Pressure) -> Rc<World> {
        let w = World::default();
        *w.probes.borrow_mut() = probes.to_vec();
        *w.clients.borrow_mut() = vec![
            client(10, "acprobe-a1-1-5", 8),
            client(11, "acview-a1-1-0", 5000),
            client(20, "bench-99-1", 900),
            client(21, "late-1", 5),
        ];
        w.pressure.set(Some(pressure));
        Rc::new(w)
    }

    #[test]
    fn a_slow_but_answering_agent_is_never_touched() {
        let w = world(&[(9, true); 6], IDLE);
        let (mut d, unhealthy, log) = dog(&w, Thresholds::default());
        rounds(&mut d, &w, 6, 30);
        assert!(w.killed.borrow().is_empty());
        assert!(unhealthy.lock().is_ok_and(|s| s.is_empty()));
        assert!(
            lines(&log).iter().all(|l| l.contains("answered slowly")),
            "{:?}",
            lines(&log)
        );
    }

    #[test]
    fn failures_on_a_busy_host_never_kill_anything() {
        let w = world(&[(10, false); 8], BUSY);
        let (mut d, unhealthy, log) = dog(&w, Thresholds::default());
        rounds(&mut d, &w, 8, 30);
        assert!(w.killed.borrow().is_empty());
        assert!(unhealthy.lock().is_ok_and(|s| s.is_empty()));
        assert!(lines(&log)
            .iter()
            .all(|l| l.contains("slow, host busy") && l.contains("not acting")));
    }

    #[test]
    fn unreadable_pressure_never_acts() {
        let w = world(&[(10, false); 6], IDLE);
        w.pressure.set(None);
        let (mut d, _, _) = dog(&w, Thresholds::default());
        rounds(&mut d, &w, 6, 30);
        assert!(w.killed.borrow().is_empty());
    }

    #[test]
    fn the_ladder_runs_in_order_and_stops_when_the_agent_answers() {
        // Four failures over 90 s escalate; rung 1's re-probe fails; rung 2's answers.
        let w = world(
            &[
                (10, false),
                (10, false),
                (10, false),
                (10, false),
                (10, false),
                (1, true),
            ],
            IDLE,
        );
        let (mut d, unhealthy, log) = dog(&w, Thresholds::default());
        rounds(&mut d, &w, 4, 30);
        // Rung 1: only the gateway's own; rung 2: only the foreign client older than the failures.
        assert_eq!(*w.killed.borrow(), [vec![10, 11], vec![20]]);
        assert!(unhealthy.lock().is_ok_and(|s| s.is_empty()));
        let log = lines(&log);
        let at = |needle: &str| log.iter().position(|l| l.contains(needle));
        let order = [
            at("4 failed probes over"),
            at("rung 1: leave pid 20"),
            at("rung 1: SIGKILL pid 10"),
            at("rung 1: still no answer"),
            at("rung 2: leave pid 21 (exec-id late-1, age 5.0s): started after"),
            at("rung 2: SIGKILL pid 20 (exec-id bench-99-1"),
            at("answers again after rung 2"),
        ];
        assert!(order.iter().all(Option::is_some), "{log:#?}");
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{log:#?}");
        assert!(at("rung 3").is_none());
    }

    #[test]
    fn rung_one_alone_can_recover_and_never_touches_a_foreign_client() {
        let w = world(
            &[
                (10, false),
                (10, false),
                (10, false),
                (10, false),
                (1, true),
            ],
            IDLE,
        );
        let (mut d, _, _) = dog(&w, Thresholds::default());
        rounds(&mut d, &w, 4, 30);
        assert_eq!(*w.killed.borrow(), [vec![10, 11]]);
        assert!(w.clients.borrow().iter().any(|c| c.pid == 20));
    }

    #[test]
    fn the_whole_ladder_failing_marks_unhealthy_and_an_answer_clears_it() {
        let mut probes = vec![(10, false); 7];
        probes.push((1, true));
        let w = world(&probes, IDLE);
        let (mut d, unhealthy, log) = dog(&w, Thresholds::default());
        rounds(&mut d, &w, 4, 30);
        assert!(unhealthy.lock().is_ok_and(|s| s.contains("a1")));
        assert!(lines(&log)
            .iter()
            .any(|l| l.contains("rung 3: marked unhealthy")));
        // Still failing: no second ladder while marked.
        rounds(&mut d, &w, 1, 30);
        assert_eq!(w.killed.borrow().len(), 2);
        // The next answer clears it.
        rounds(&mut d, &w, 1, 30);
        assert!(unhealthy.lock().is_ok_and(|s| s.is_empty()));
        assert!(lines(&log)
            .last()
            .is_some_and(|l| l.contains("answers again")));
    }

    #[test]
    fn thresholds_are_configurable() {
        // One failure is enough, with no window, and a host at load 9.5 on 4 cores is not busy.
        let t = Thresholds {
            failures: 1,
            window: S(0),
            load_per_core: 4.0,
            io_some: 90.0,
            cpu_some: 90.0,
            ..Thresholds::default()
        };
        let w = world(&[(10, false), (1, true)], BUSY);
        let (mut d, _, _) = dog(&w, t);
        rounds(&mut d, &w, 1, 30);
        assert_eq!(*w.killed.borrow(), [vec![10, 11]]);
        // A short budget turns a 9 s answer into a failure.
        let tight = Thresholds {
            probe_budget: S(5),
            ..Thresholds::default()
        };
        let w = world(&[(9, true)], IDLE);
        let (mut d, _, log) = dog(&w, tight);
        rounds(&mut d, &w, 1, 30);
        assert!(
            lines(&log)
                .iter()
                .any(|l| l.contains("probe failed after 5.0s")),
            "{:?}",
            lines(&log)
        );
    }

    #[test]
    fn an_agent_no_longer_running_is_forgotten() {
        let w = world(&[(10, false); 7], IDLE);
        let (mut d, unhealthy, _) = dog(&w, Thresholds::default());
        rounds(&mut d, &w, 4, 30);
        assert!(unhealthy.lock().is_ok_and(|s| s.contains("a1")));
        d.check(&[]);
        assert!(unhealthy.lock().is_ok_and(|s| s.is_empty()));
    }
}
