//! Sans-IO gateway logic: routing and error mapping over a [`Fleet`]. Tokens and scopes are
//! checked before a request gets here ([`access`], [`tokens`]); the HTTP transport lives in
//! `main.rs`, so every route is testable here.
//!
//! Routes: `GET /api/v1/agents` lists the agents the token is for, and their state. Under
//! `/api/v1/agents/{id}`: `GET` status, `POST` create, `POST .../pause`, `POST .../resume`, `DELETE` destroy (keeps the
//! workspace; `DELETE ...?purge=true` removes it, also for an agent already destroyed). Pause and destroy of
//! an agent with a queued or running job are refused (409) unless `?force=true`, which interrupts the job
//! first (ADR-010).
//!
//! Admission (ADR-012): a create or resume whose guest would not fit `/dev/shm` is refused with
//! 507 and `{"error","rule","needed_mib","available_mib","more_agents"}`; `GET /api/v1/capacity`
//! reports the figures, and with the task API also `running_jobs`, `queued_jobs` and
//! `max_running_jobs` (ADR-013).

pub mod access;
pub mod bind;
pub mod jobs;
pub mod tokens;
pub mod viewer;
pub mod watchdog;
pub mod web;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use ac_domain::{AgentId, Shortfall, State};
use ac_fleet::{Fleet, FleetError};
use ac_ports::{Runtime, SessionStore};
use tokens::Agents;

const PREFIX: &str = "/api/v1/agents/";
/// What admission control sees.
pub const CAPACITY: &str = "/api/v1/capacity";

/// A rendered response: status code and JSON body.
#[derive(Debug, PartialEq, Eq)]
pub struct Reply {
    /// HTTP status.
    pub status: u16,
    /// JSON document.
    pub body: String,
}

/// Agents the watchdog found wedged (by id). Shared with [`Api`], which reports them.
pub type Unhealthy = Arc<Mutex<HashSet<String>>>;

/// Jobs host-wide, for `GET /api/v1/capacity` (ADR-013).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobCounts {
    /// Running now.
    pub running: usize,
    /// Waiting to start.
    pub queued: usize,
    /// Most that may run at once.
    pub max_running: u32,
}

/// What the lifecycle API asks of an agent's jobs (ADR-010).
pub trait Busy: Send + Sync {
    /// Jobs host-wide; `None` without a task API or when the registry cannot be read.
    fn counts(&self) -> Option<JobCounts> {
        None
    }
    /// Whether the agent has a queued or running job.
    fn busy(&self, id: &AgentId) -> bool;
    /// The agent's VM is about to go away: kill its running job and mark it interrupted; cancel the
    /// queued ones.
    fn interrupt(&self, id: &AgentId);
    /// The agent's workspace was purged: forget its jobs.
    fn forget(&self, id: &AgentId);
}

/// No task API: nothing is ever busy.
struct NoJobs;

impl Busy for NoJobs {
    fn busy(&self, _: &AgentId) -> bool {
        false
    }
    fn interrupt(&self, _: &AgentId) {}
    fn forget(&self, _: &AgentId) {}
}

/// The lifecycle API.
pub struct Api<R, S> {
    fleet: Fleet<R, S>,
    unhealthy: Unhealthy,
    jobs: Arc<dyn Busy>,
}

impl<R: Runtime, S: SessionStore> Api<R, S> {
    /// Serve `fleet`.
    pub fn new(fleet: Fleet<R, S>) -> Self {
        Self {
            fleet,
            unhealthy: Unhealthy::default(),
            jobs: Arc::new(NoJobs),
        }
    }

    /// Ask `jobs` before pausing or destroying an agent.
    pub fn with_jobs(mut self, jobs: Arc<dyn Busy>) -> Self {
        self.jobs = jobs;
        self
    }

    /// An agent's lifecycle state, `None` if unknown.
    pub fn state(&self, id: &AgentId) -> Result<Option<State>, FleetError> {
        self.fleet.state(id)
    }

    /// Report agents in `set` as `"health":"unhealthy"`.
    pub fn with_unhealthy(mut self, set: Unhealthy) -> Self {
        self.unhealthy = set;
        self
    }

    /// Agents recorded running, for the watchdog.
    pub fn running(&self) -> Vec<AgentId> {
        self.fleet
            .list()
            .map(|all| {
                all.into_iter()
                    .filter(|(_, s)| *s == State::Running)
                    .map(|(id, _)| id)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Handle one request from a token for `agents` (the list shows only those; the caller has
    /// checked the rest). Of the query string, only `purge` on `DELETE` and `force` on pause and
    /// `DELETE` are read.
    pub fn handle(&self, method: &str, target: &str, agents: &Agents) -> Reply {
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        if path == CAPACITY {
            return match method {
                "GET" => self.capacity(),
                _ => error(405, "method not allowed"),
            };
        }
        if path == PREFIX.trim_end_matches('/') {
            return match method {
                "GET" => self.list(agents),
                _ => error(405, "method not allowed"),
            };
        }
        let Some(rest) = path.strip_prefix(PREFIX) else {
            return error(404, "not found");
        };
        let (id, action) = match rest.split_once('/') {
            Some((id, action)) => (id, Some(action)),
            None => (rest, None),
        };
        let id = match AgentId::parse(id) {
            Ok(id) => id,
            Err(e) => return error(400, &e.to_string()),
        };
        match (method, action) {
            ("GET", None) => self.status(&id),
            ("POST", None) => self.done(&id, self.fleet.create(&id), 201),
            ("POST", Some("pause")) => match flag(query, "force") {
                Some(force) => self.vacate(&id, force, || self.fleet.pause(&id)),
                None => error(400, "force must be true or false"),
            },
            ("POST", Some("resume")) => self.done(&id, self.fleet.resume(&id), 200),
            ("DELETE", None) => match (flag(query, "purge"), flag(query, "force")) {
                (Some(purge), Some(force)) => {
                    let reply = self.vacate(&id, force, || self.fleet.destroy(&id, purge));
                    if purge && reply.status == 200 {
                        self.jobs.forget(&id);
                    }
                    reply
                }
                (None, _) => error(400, "purge must be true or false"),
                (_, None) => error(400, "force must be true or false"),
            },
            (_, None | Some("pause" | "resume")) => error(405, "method not allowed"),
            _ => error(404, "not found"),
        }
    }

    fn capacity(&self) -> Reply {
        let jobs = self.jobs.counts().map_or_else(String::new, |c| {
            format!(
                r#","running_jobs":{},"queued_jobs":{},"max_running_jobs":{}"#,
                c.running, c.queued, c.max_running
            )
        });
        match self.fleet.capacity() {
            Ok(Some(r)) => Reply {
                status: 200,
                body: format!(
                    r#"{{"shm_total_mib":{},"shm_free_mib":{},"guest_mib":{},"running_agents":{},"committed_mib":{},"margin_mib":{},"max_more_agents":{}{jobs}}}"#,
                    r.shm_total_mib,
                    r.shm_free_mib,
                    r.guest_mib,
                    r.running_agents,
                    r.committed_mib,
                    r.margin_mib,
                    r.max_more_agents
                ),
            },
            Ok(None) => error(404, "admission control is off"),
            Err(e) => map_error(&e),
        }
    }

    fn status(&self, id: &AgentId) -> Reply {
        match self.fleet.state(id) {
            Ok(Some(state)) => self.agent(id, state, 200),
            Ok(None) => error(404, &FleetError::NotFound(id.clone()).to_string()),
            Err(e) => map_error(&e),
        }
    }

    fn list(&self, agents: &Agents) -> Reply {
        match self.fleet.list() {
            Ok(all) => {
                let items: Vec<String> = all
                    .iter()
                    .filter(|(id, _)| agents.contains(id.as_str()))
                    .map(|(id, s)| self.record(id, *s))
                    .collect();
                Reply {
                    status: 200,
                    body: format!(r#"{{"agents":[{}]}}"#, items.join(",")),
                }
            }
            Err(e) => map_error(&e),
        }
    }

    /// Pause or destroy: refused while the agent has a job, unless `force`, which interrupts it
    /// first, so a job never outlives its VM untracked.
    fn vacate(
        &self,
        id: &AgentId,
        force: bool,
        act: impl FnOnce() -> Result<State, FleetError>,
    ) -> Reply {
        if self.jobs.busy(id) {
            if !force {
                return error(
                    409,
                    &format!("agent {id} has a queued or running job; ?force=true interrupts it"),
                );
            }
            self.jobs.interrupt(id);
        }
        self.done(id, act(), 200)
    }

    fn done(&self, id: &AgentId, result: Result<State, FleetError>, ok: u16) -> Reply {
        match result {
            Ok(state) => self.agent(id, state, ok),
            Err(e) => map_error(&e),
        }
    }
}

/// A boolean query parameter (`purge`, `force`): absent or `false` is false. `None` if malformed.
fn flag(query: &str, name: &str) -> Option<bool> {
    let Some(p) = query.split('&').find(|p| p.split('=').next() == Some(name)) else {
        return Some(false);
    };
    match p.strip_prefix(name) {
        Some("=false") => Some(false),
        Some("=true") => Some(true),
        _ => None,
    }
}

impl<R, S> Api<R, S> {
    fn agent(&self, id: &AgentId, state: State, status: u16) -> Reply {
        Reply {
            status,
            body: self.record(id, state),
        }
    }

    fn record(&self, id: &AgentId, state: State) -> String {
        let state = format!("{state:?}").to_lowercase();
        let sick = self
            .unhealthy
            .lock()
            .is_ok_and(|set| set.contains(id.as_str()));
        let health = if sick { r#","health":"unhealthy""# } else { "" };
        format!(r#"{{"id":"{id}","state":"{state}"{health}}}"#)
    }
}

fn map_error(e: &FleetError) -> Reply {
    let status = match e {
        FleetError::NotFound(_) => 404,
        FleetError::AlreadyExists(_) | FleetError::Illegal(_) => 409,
        FleetError::NoCapacity(s) => return no_capacity(s),
        FleetError::Port(_) => 500,
    };
    error(status, &e.to_string())
}

/// 507: the guest does not fit `/dev/shm`. The body carries the figures besides the message.
fn no_capacity(s: &Shortfall) -> Reply {
    let rule = match s.rule {
        ac_domain::capacity::Rule::Total => "total",
        ac_domain::capacity::Rule::Free => "free",
    };
    Reply {
        status: 507,
        body: format!(
            r#"{{"error":"{}","rule":"{rule}","needed_mib":{},"available_mib":{},"more_agents":{}}}"#,
            escape(&s.to_string()),
            s.needed_mib,
            s.available_mib,
            s.more
        ),
    }
}

/// A JSON error reply.
pub fn error(status: u16, message: &str) -> Reply {
    Reply {
        status,
        body: format!(r#"{{"error":"{}"}}"#, escape(message)),
    }
}

pub(crate) fn escape(s: &str) -> String {
    s.chars()
        .flat_map(|c| match c {
            '"' => vec!['\\', '"'],
            '\\' => vec!['\\', '\\'],
            c if c.is_control() => vec![' '],
            c => vec![c],
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use ac_ports::PortError;

    use super::*;

    #[derive(Default)]
    struct Store(RefCell<HashMap<String, State>>);
    impl SessionStore for Store {
        fn state(&self, id: &AgentId) -> Result<Option<State>, PortError> {
            Ok(self.0.borrow().get(id.as_str()).copied())
        }
        fn set_state(&self, id: &AgentId, s: State) -> Result<(), PortError> {
            self.0.borrow_mut().insert(id.to_string(), s);
            Ok(())
        }
        fn list(&self) -> Result<Vec<(AgentId, State)>, PortError> {
            let all = self.0.borrow();
            Ok(all
                .iter()
                .map(|(k, s)| (AgentId::parse(k).unwrap_or_else(|e| panic!("{e}")), *s))
                .collect())
        }
    }

    struct Rt {
        fail: bool,
    }
    impl Rt {
        fn go(&self) -> Result<(), PortError> {
            if self.fail {
                return Err(PortError {
                    op: "ctr",
                    cause: "bad \"thing\"".into(),
                });
            }
            Ok(())
        }
    }
    impl Runtime for Rt {
        fn create(&self, _: &AgentId) -> Result<(), PortError> {
            self.go()
        }
        fn pause(&self, _: &AgentId) -> Result<(), PortError> {
            self.go()
        }
        fn resume(&self, _: &AgentId) -> Result<(), PortError> {
            self.go()
        }
        fn destroy(&self, _: &AgentId, _: bool) -> Result<(), PortError> {
            self.go()
        }
        fn running(&self) -> Result<Vec<AgentId>, PortError> {
            self.go().map(|()| Vec::new())
        }
    }

    fn api(fail: bool) -> Api<Rt, Store> {
        Api::new(Fleet::new(Rt { fail }, Store::default()))
    }

    fn call(api: &Api<Rt, Store>, m: &str, t: &str) -> (u16, String) {
        let r = api.handle(m, t, &Agents::All);
        (r.status, r.body)
    }

    #[test]
    fn full_lifecycle_over_routes() {
        let a = api(false);
        assert_eq!(
            call(&a, "POST", "/api/v1/agents/a1"),
            (201, r#"{"id":"a1","state":"running"}"#.into())
        );
        assert_eq!(call(&a, "POST", "/api/v1/agents/a1/pause").0, 200);
        assert_eq!(
            call(&a, "GET", "/api/v1/agents/a1?x=1"),
            (200, r#"{"id":"a1","state":"paused"}"#.into())
        );
        assert_eq!(call(&a, "POST", "/api/v1/agents/a1/resume").0, 200);
        assert_eq!(
            call(&a, "DELETE", "/api/v1/agents/a1").1,
            r#"{"id":"a1","state":"destroyed"}"#
        );
    }

    #[test]
    fn list_route() {
        let a = api(false);
        assert_eq!(
            call(&a, "GET", "/api/v1/agents"),
            (200, r#"{"agents":[]}"#.into())
        );
        let _ = call(&a, "POST", "/api/v1/agents/b2");
        let _ = call(&a, "POST", "/api/v1/agents/a1");
        let _ = call(&a, "POST", "/api/v1/agents/a1/pause");
        assert_eq!(
            call(&a, "GET", "/api/v1/agents?x=1").1,
            r#"{"agents":[{"id":"a1","state":"paused"},{"id":"b2","state":"running"}]}"#
        );
        assert_eq!(call(&a, "POST", "/api/v1/agents").0, 405);
        let only_b2 = Agents::parse("b2,zz").unwrap_or(Agents::All);
        assert_eq!(
            a.handle("GET", "/api/v1/agents", &only_b2).body,
            r#"{"agents":[{"id":"b2","state":"running"}]}"#
        );
    }

    #[test]
    fn delete_purge_parameter() {
        let a = api(false);
        let _ = call(&a, "POST", "/api/v1/agents/a1");
        for bad in ["purge=yes", "purge=", "purge", "x=1&purge=1"] {
            let r = call(&a, "DELETE", &format!("/api/v1/agents/a1?{bad}"));
            assert_eq!(r.0, 400, "{bad}");
        }
        assert_eq!(call(&a, "DELETE", "/api/v1/agents/a1?purge=false").0, 200);
        // Already destroyed: a plain destroy is refused, a purge still goes through.
        assert_eq!(call(&a, "DELETE", "/api/v1/agents/a1").0, 409);
        assert_eq!(
            call(&a, "DELETE", "/api/v1/agents/a1?x=1&purge=true"),
            (200, r#"{"id":"a1","state":"destroyed"}"#.into())
        );
        assert_eq!(call(&a, "DELETE", "/api/v1/agents/zz?purge=true").0, 404);
    }

    /// Every route a handler accepts has an entry in the scope table, and every entry is a route
    /// some handler accepts, over every path built from the table's words, a few ids and
    /// strangers. (Accepted: answered with anything but "not found" or 405; 400 for a bad id
    /// counts, since the shape is a route, unless the bad id is an empty segment, which no
    /// placeholder matches.) A route added without an entry is unreachable anyway: the table is
    /// checked first.
    #[test]
    fn every_route_has_exactly_one_scope_entry() {
        let a = api(false);
        let mut words: Vec<&str> = access::ROUTES
            .iter()
            .flat_map(|r| r.path.split('/'))
            .filter(|w| !w.is_empty() && !w.starts_with('{') && *w != "api" && *w != "v1")
            .collect();
        words.extend(["a1", "0123456789abcdef", "explode", ""]);
        words.sort_unstable();
        words.dedup();
        let mut paths = vec!["/api/v1/agents".to_owned()];
        for w in &words {
            paths.push(format!("/api/v1/{w}"));
        }
        // Every tail of up to 3 words (the deepest routes), and each 3-word tail plus one more.
        let mut tails = vec![String::new()];
        for _ in 0..3 {
            let longer: Vec<String> = tails
                .iter()
                .flat_map(|t| words.iter().map(move |w| format!("{t}/{w}")))
                .collect();
            for t in &longer {
                paths.push(format!("/api/v1/agents/a1{t}"));
            }
            tails = longer;
        }
        for t in &tails {
            paths.push(format!("/api/v1/agents/a1{t}/cancel"));
        }
        let handled = |m: &str, p: &str| -> bool {
            let empty = p.split('/').skip(1).any(str::is_empty);
            let ours = |r: Option<Result<(), Reply>>| match r {
                Some(Ok(())) => true,
                Some(Err(e)) => e.status == 400 && !empty,
                None => false,
            };
            let viewer = ours(viewer::route(m, p).map(|r| r.map(drop)));
            let job = ours(jobs::route(m, p).map(|r| r.map(drop)));
            let life = a.handle(m, p, &Agents::All);
            let life = life.status != 405
                && life.body != error(404, "not found").body
                && !(life.status == 400 && empty);
            viewer || job || life || (m == "GET" && p == web::WHOAMI)
        };
        let mut checked = 0;
        for m in ["GET", "POST", "DELETE", "PUT"] {
            for p in &paths {
                assert_eq!(handled(m, p), access::find(m, p).is_ok(), "{m} {p}");
                checked += 1;
            }
        }
        assert!(checked > 50_000, "{checked}");
    }

    #[test]
    fn unhealthy_agents_are_reported() {
        let set = Unhealthy::default();
        let a = api(false).with_unhealthy(Arc::clone(&set));
        let _ = call(&a, "POST", "/api/v1/agents/a1");
        let _ = set.lock().map(|mut s| s.insert("a1".into()));
        assert_eq!(
            call(&a, "GET", "/api/v1/agents/a1").1,
            r#"{"id":"a1","state":"running","health":"unhealthy"}"#
        );
        assert_eq!(a.running().len(), 1);
    }

    #[test]
    fn status_mapping() {
        let a = api(false);
        assert_eq!(call(&a, "GET", "/api/v1/agents/zz").0, 404);
        assert_eq!(call(&a, "POST", "/api/v1/agents/a1/pause").0, 404);
        assert_eq!(call(&a, "POST", "/api/v1/agents/a1").0, 201);
        assert_eq!(call(&a, "POST", "/api/v1/agents/a1").0, 409);
        assert_eq!(call(&a, "POST", "/api/v1/agents/a1/resume").0, 409);
    }

    #[test]
    fn bad_input_and_routes() {
        let a = api(false);
        assert_eq!(call(&a, "GET", "/api/v1/agents/..%2Fx").0, 400);
        assert_eq!(call(&a, "GET", "/api/v1/agents/BAD").0, 400);
        assert_eq!(call(&a, "PUT", "/api/v1/agents/a1").0, 405);
        assert_eq!(call(&a, "POST", "/api/v1/agents/a1/explode").0, 404);
        assert_eq!(call(&a, "GET", "/other").0, 404);
    }

    /// `/dev/shm` of 9000 MiB, all free; guests of 4096 MiB.
    struct Shm9000;
    impl ac_ports::Capacity for Shm9000 {
        fn shm(&self) -> Result<ac_ports::Shm, PortError> {
            Ok(ac_ports::Shm {
                total_mib: 9000,
                free_mib: 9000,
            })
        }
        fn guest_mib(&self) -> Result<u64, PortError> {
            Ok(4096)
        }
    }

    #[test]
    fn third_agent_is_refused_with_507_and_a_paused_one_does_not_count() {
        let fleet =
            Fleet::new(Rt { fail: false }, Store::default()).with_admission(Box::new(Shm9000), 512);
        let a = Api::new(fleet);
        assert_eq!(
            call(&a, "GET", CAPACITY).1,
            r#"{"shm_total_mib":9000,"shm_free_mib":9000,"guest_mib":4096,"running_agents":0,"committed_mib":0,"margin_mib":512,"max_more_agents":2}"#
        );
        assert_eq!(call(&a, "POST", "/api/v1/agents/adm-1").0, 201);
        assert_eq!(call(&a, "POST", "/api/v1/agents/adm-2").0, 201);
        let (status, body) = call(&a, "POST", "/api/v1/agents/adm-3");
        assert_eq!(status, 507);
        assert!(
            body.ends_with(
                r#","rule":"total","needed_mib":12800,"available_mib":9000,"more_agents":0}"#
            ),
            "{body}"
        );
        assert!(
            body.contains("needs 12800 MiB, /dev/shm size is 9000 MiB; 0 more"),
            "{body}"
        );
        assert_eq!(call(&a, "GET", "/api/v1/agents/adm-3").0, 404);
        assert!(call(&a, "GET", CAPACITY)
            .1
            .contains(r#""running_agents":2,"committed_mib":8192"#));
        assert_eq!(call(&a, "POST", "/api/v1/agents/adm-1/pause").0, 200);
        assert_eq!(call(&a, "POST", "/api/v1/agents/adm-3").0, 201);
        assert_eq!(call(&a, "POST", "/api/v1/agents/adm-1/resume").0, 507);
        assert_eq!(call(&a, "POST", CAPACITY).0, 405);
        assert_eq!(call(&api(false), "GET", CAPACITY).0, 404);
    }

    /// Two jobs running and three queued, of at most four.
    struct Counted;
    impl Busy for Counted {
        fn counts(&self) -> Option<JobCounts> {
            Some(JobCounts {
                running: 2,
                queued: 3,
                max_running: 4,
            })
        }
        fn busy(&self, _: &AgentId) -> bool {
            false
        }
        fn interrupt(&self, _: &AgentId) {}
        fn forget(&self, _: &AgentId) {}
    }

    #[test]
    fn capacity_reports_job_slots_with_the_task_api() {
        let fleet =
            Fleet::new(Rt { fail: false }, Store::default()).with_admission(Box::new(Shm9000), 512);
        let a = Api::new(fleet).with_jobs(Arc::new(Counted));
        assert_eq!(
            call(&a, "GET", CAPACITY).1,
            r#"{"shm_total_mib":9000,"shm_free_mib":9000,"guest_mib":4096,"running_agents":0,"committed_mib":0,"margin_mib":512,"max_more_agents":2,"running_jobs":2,"queued_jobs":3,"max_running_jobs":4}"#
        );
    }

    /// A runtime whose create takes a while, so two creates overlap unless serialized.
    struct Slow(Mutex<Vec<String>>);
    impl Runtime for Slow {
        fn create(&self, id: &AgentId) -> Result<(), PortError> {
            std::thread::sleep(std::time::Duration::from_millis(50));
            self.0
                .lock()
                .map(|mut v| v.push(id.to_string()))
                .map_err(|_| PortError {
                    op: "create",
                    cause: "poisoned".into(),
                })
        }
        fn pause(&self, _: &AgentId) -> Result<(), PortError> {
            Ok(())
        }
        fn resume(&self, _: &AgentId) -> Result<(), PortError> {
            Ok(())
        }
        fn destroy(&self, _: &AgentId, _: bool) -> Result<(), PortError> {
            Ok(())
        }
        fn running(&self) -> Result<Vec<AgentId>, PortError> {
            Ok(Vec::new())
        }
    }

    /// The gateway serves every lifecycle call under one lock (`App`'s `Mutex<Api>`), so of two
    /// creates racing for the last slot exactly one is admitted.
    #[test]
    fn two_concurrent_creates_cannot_both_pass() {
        let fleet = Fleet::new(Slow(Mutex::new(Vec::new())), Store::default())
            .with_admission(Box::new(Shm9000), 512);
        let a = Arc::new(Mutex::new(Api::new(fleet)));
        let _ = a
            .lock()
            .map(|a| a.handle("POST", "/api/v1/agents/adm-1", &Agents::All));
        let racers: Vec<_> = ["adm-2", "adm-3"]
            .into_iter()
            .map(|id| {
                let a = Arc::clone(&a);
                std::thread::spawn(move || {
                    a.lock()
                        .map(|a| {
                            a.handle("POST", &format!("/api/v1/agents/{id}"), &Agents::All)
                                .status
                        })
                        .unwrap_or(0)
                })
            })
            .collect();
        let mut got: Vec<u16> = racers.into_iter().map(|t| t.join().unwrap_or(0)).collect();
        got.sort_unstable();
        assert_eq!(got, [201, 507]);
    }

    #[test]
    fn runtime_failure_is_500_with_escaped_message() {
        let r = call(&api(true), "POST", "/api/v1/agents/a1");
        assert_eq!(r.0, 500);
        assert!(r.1.contains(r#"bad \"thing\""#), "{}", r.1);
    }
}
