//! Sans-IO gateway logic: auth, routing and error mapping over a [`Fleet`].
//! The HTTP transport lives in `main.rs`, so every route is testable here.
//!
//! Routes (bearer token required): `GET /api/v1/agents` lists every agent and its state. Under
//! `/api/v1/agents/{id}`: `GET` status, `POST` create, `POST .../pause`, `POST .../resume`, `DELETE` destroy (keeps the
//! workspace; `DELETE ...?purge=true` removes it, also for an agent already destroyed). Pause and destroy of
//! an agent with a queued or running job are refused (409) unless `?force=true`, which interrupts the job
//! first (ADR-010).

pub mod bind;
pub mod jobs;
pub mod viewer;
pub mod web;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use ac_domain::{AgentId, State};
use ac_fleet::{Fleet, FleetError};
use ac_ports::{Runtime, SessionStore};

const PREFIX: &str = "/api/v1/agents/";

/// A rendered response: status code and JSON body.
#[derive(Debug, PartialEq, Eq)]
pub struct Reply {
    /// HTTP status.
    pub status: u16,
    /// JSON document.
    pub body: String,
}

/// The owner's bearer token. Checked without any other lock, so viewer requests never wait on
/// a slow lifecycle call.
#[derive(Clone)]
pub struct Auth(Arc<[u8]>);

impl Auth {
    /// `None` for an empty token.
    pub fn new(token: &str) -> Option<Self> {
        (!token.is_empty()).then(|| Self(token.as_bytes().into()))
    }

    /// Whether an `Authorization` header carries the token (`Bearer <token>`, exactly).
    pub fn check(&self, authorization: Option<&str>) -> bool {
        let presented = authorization
            .and_then(|h| h.strip_prefix("Bearer "))
            .unwrap_or("");
        constant_time_eq(presented.as_bytes(), &self.0)
    }
}

/// Agents the watchdog found wedged (by id). Shared with [`Api`], which reports them.
pub type Unhealthy = Arc<Mutex<HashSet<String>>>;

/// What the lifecycle API asks of an agent's jobs (ADR-010).
pub trait Busy: Send + Sync {
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

/// The authenticated lifecycle API.
pub struct Api<R, S> {
    fleet: Fleet<R, S>,
    auth: Auth,
    unhealthy: Unhealthy,
    jobs: Arc<dyn Busy>,
}

impl<R: Runtime, S: SessionStore> Api<R, S> {
    /// Build from a fleet and the owner's bearer token (must be non-empty).
    pub fn new(fleet: Fleet<R, S>, token: &str) -> Option<Self> {
        Some(Self {
            fleet,
            auth: Auth::new(token)?,
            unhealthy: Unhealthy::default(),
            jobs: Arc::new(NoJobs),
        })
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

    /// The token checker, for routes outside this API.
    pub fn auth(&self) -> Auth {
        self.auth.clone()
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

    /// Handle one request. Of the query string, only `purge` on `DELETE` and `force` on pause and
    /// `DELETE` are read.
    pub fn handle(&self, method: &str, target: &str, authorization: Option<&str>) -> Reply {
        if !self.auth.check(authorization) {
            return error(401, "unauthorized");
        }
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        if path == PREFIX.trim_end_matches('/') {
            return match method {
                "GET" => self.list(),
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

    fn status(&self, id: &AgentId) -> Reply {
        match self.fleet.state(id) {
            Ok(Some(state)) => self.agent(id, state, 200),
            Ok(None) => error(404, &FleetError::NotFound(id.clone()).to_string()),
            Err(e) => map_error(&e),
        }
    }

    fn list(&self) -> Reply {
        match self.fleet.list() {
            Ok(all) => {
                let items: Vec<String> = all.iter().map(|(id, s)| self.record(id, *s)).collect();
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
        FleetError::Port(_) => 500,
    };
    error(status, &e.to_string())
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

/// Compare without early exit on the first differing byte.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    a.len() == b.len() && diff == 0
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

    const AUTH: Option<&str> = Some("Bearer s3cret");

    fn api(fail: bool) -> Api<Rt, Store> {
        Api::new(Fleet::new(Rt { fail }, Store::default()), "s3cret")
            .unwrap_or_else(|| panic!("token"))
    }

    fn call(api: &Api<Rt, Store>, m: &str, t: &str) -> (u16, String) {
        let r = api.handle(m, t, AUTH);
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
        assert_eq!(a.handle("GET", "/api/v1/agents", None).status, 401);
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

    #[test]
    fn auth_is_required_and_exact() {
        let a = api(false);
        for h in [
            None,
            Some("s3cret"),
            Some("Bearer wrong"),
            Some("Bearer s3cre"),
            Some("bearer s3cret"),
            Some("Bearer "),
        ] {
            assert_eq!(a.handle("GET", "/api/v1/agents/a1", h).status, 401, "{h:?}");
        }
        assert!(Api::new(Fleet::new(Rt { fail: false }, Store::default()), "").is_none());
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

    #[test]
    fn runtime_failure_is_500_with_escaped_message() {
        let r = call(&api(true), "POST", "/api/v1/agents/a1");
        assert_eq!(r.0, 500);
        assert!(r.1.contains(r#"bad \"thing\""#), "{}", r.1);
    }
}
