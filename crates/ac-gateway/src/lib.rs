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
//! A create's VM boot runs outside the lifecycle lock (ADR-027): [`Api::begin_create`] reserves
//! the agent as `creating` under the lock, the caller boots it unlocked ([`Creation::boot`]), and
//! [`Api::finish_create`] commits it under the lock again. While it boots, every other change to
//! that agent (a second create, pause, destroy, egress, checkpoints, a fork to or from it) is
//! refused with 409; reads are served.
//!
//! Admission (ADR-012): a create or resume whose guest would not fit `/dev/shm` is refused with
//! 507 and `{"error","rule","needed_mib","available_mib","more_agents"}`; `GET /api/v1/capacity`
//! reports the figures, and with the task API also `running_jobs`, `queued_jobs` and
//! `max_running_jobs` (ADR-013).
//!
//! Egress profiles (ADR-016): `GET /api/v1/egress/profiles` lists the operator's profiles and their
//! host counts; `GET .../{id}/egress` an agent's profiles and how many hosts it may reach;
//! `PUT .../{id}/egress` with `{"profiles":["rust"]}` replaces them (the proxy applies it to the
//! next request). A create may carry `{"egress_profiles":[...]}`, which needs `egress:write` too.
//! The API attaches only profiles that exist; it never defines hosts.
//!
//! Images (card #1627): a create may carry `{"image":"<name>"}`, a name from the operator's
//! allowlist ([`Api::with_images`], `AC_IMAGES`); without one the agent boots on `AC_IMAGE`. The
//! agent's image is recorded, so every resume and fork boots from it. An unknown name is 400.
//!
//! MCP (ADR-017): `POST /api/v1/mcp` serves these routes, and the job and history ones, as tools
//! ([`mcp`]).
//!
//! Checkpoints and forks (ADR-018): `GET .../{id}/checkpoints` lists them and the cap,
//! `POST .../{id}/checkpoints` with `{"label"?}` makes one (201), `DELETE .../checkpoints/{cp}`
//! deletes one, `POST .../{id}/restore` with `{"checkpoint"}` puts one back on a paused agent and
//! keeps what it replaced as a checkpoint, and `POST .../{id}/fork` with
//! `{"id","checkpoint"?,"egress_profiles"?}` creates agent `id` on a copy of the workspace (201).
//! A fork needs a token for both agents, and `egress:write` to attach profiles. A refused one is
//! 409 and changes nothing; without snapshots (no `with_snapshots`), 404.

pub mod access;
pub mod admin;
pub mod agui;
pub mod backups;
pub mod bind;
pub mod egress;
pub mod jobs;
pub mod mcp;
pub mod notify;
pub mod tokens;
pub mod viewer;
pub mod watchdog;
pub mod web;

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};

use ac_domain::{AgentId, CheckpointId, ImageRef, Label, ProfileName, Shortfall, State};
use ac_fleet::{Fleet, FleetError, Reserved};
use ac_ports::{PortError, Runtime, SessionStore};
use egress::Profiles;
use rusty_json::Value;
use tokens::Agents;

const PREFIX: &str = "/api/v1/agents/";
/// What admission control sees.
pub const CAPACITY: &str = "/api/v1/capacity";
/// The operator's egress profiles.
pub const PROFILES: &str = "/api/v1/egress/profiles";

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
    /// The agent's workspace is about to change outside a job (a boot, a restore): take the end
    /// snapshots of its jobs that [`jobs::Jobs::startup`] ended without them, so their change
    /// notes hold only the jobs' own changes. One snapshot per job at most.
    fn settle(&self, _: &AgentId) {}
    /// The agent is about to be purged, which deletes its job snapshots: hold its jobs' change
    /// compares (ADR-019) until [`Busy::release`]. `false`, holding nothing, while one of them
    /// runs, since deleting a snapshot that `btrfs send` reads fails, or while a job's end
    /// snapshot is being taken (#31).
    fn hold(&self, _: &AgentId) -> bool {
        true
    }
    /// The purge is over, however it went: the agent's held compares may run (they find nothing
    /// after a purge that succeeded).
    fn release(&self, _: &AgentId) {}
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

/// Agents whose create is booting outside the lifecycle lock.
type Booting = Arc<Mutex<HashSet<AgentId>>>;

/// A create reserved by [`Api::begin_create`]: boot it with [`Self::boot`] without the lifecycle
/// lock, then pass the outcome to [`Api::finish_create`]. Until it is dropped, the agent counts as
/// booting, so other changes to it are refused; dropping it (also by a panic) releases that.
pub struct Creation<R> {
    reserved: Reserved<R>,
    booting: Booting,
}

impl<R: Runtime> Creation<R> {
    /// Provision and start the VM: the slow part, which needs no lock.
    pub fn boot(&self) -> Result<(), PortError> {
        self.reserved.boot()
    }
}

impl<R> Drop for Creation<R> {
    fn drop(&mut self) {
        let mut set = self.booting.lock().unwrap_or_else(|e| e.into_inner());
        set.remove(self.reserved.id());
    }
}

/// A paused agent held outside the lifecycle lock for a long change, a restore from backup
/// (ADR-031). Until it is dropped, other changes to the agent are refused as while a create boots.
pub struct Hold {
    id: AgentId,
    booting: Booting,
}

impl Hold {
    /// The agent held.
    pub fn id(&self) -> &AgentId {
        &self.id
    }

    /// A hold on `id` that guards nothing (tests).
    #[cfg(test)]
    pub(crate) fn detached(id: AgentId) -> Self {
        Self {
            id,
            booting: Booting::default(),
        }
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        let mut set = self.booting.lock().unwrap_or_else(|e| e.into_inner());
        set.remove(&self.id);
    }
}

/// The lifecycle API.
pub struct Api<R, S> {
    fleet: Fleet<R, S>,
    unhealthy: Unhealthy,
    jobs: Arc<dyn Busy>,
    egress: Option<Arc<dyn Profiles>>,
    booting: Booting,
    images: BTreeMap<String, ImageRef>,
}

impl<R: Runtime, S: SessionStore> Api<R, S> {
    /// Serve `fleet`.
    pub fn new(fleet: Fleet<R, S>) -> Self {
        Self {
            fleet,
            unhealthy: Unhealthy::default(),
            jobs: Arc::new(NoJobs),
            egress: None,
            booting: Booting::default(),
            images: BTreeMap::new(),
        }
    }

    /// Let a create name one of `images` (name to image); the fleet records it. Empty: a create
    /// naming any image is refused.
    pub fn with_images(mut self, images: BTreeMap<String, ImageRef>) -> Self {
        self.images = images;
        self
    }

    /// Serve the egress routes from `profiles` (the fleet issues the credentials).
    pub fn with_egress(mut self, profiles: Arc<dyn Profiles>) -> Self {
        self.egress = Some(profiles);
        self
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

    /// Handle one request without a body from a token that may not attach profiles.
    pub fn handle(&self, method: &str, target: &str, agents: &Agents) -> Reply {
        self.request(method, target, b"", agents, false)
    }

    /// Handle one request from a token for `agents` (the list shows only those; the caller has
    /// checked the rest); `may_attach` is whether it has `egress:write`. Of the query string,
    /// only `purge` on `DELETE` and `force` on pause and `DELETE` are read.
    pub fn request(
        &self,
        method: &str,
        target: &str,
        body: &[u8],
        agents: &Agents,
        may_attach: bool,
    ) -> Reply {
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        if path == PROFILES {
            return match method {
                "GET" => self.profiles(),
                _ => error(405, "method not allowed"),
            };
        }
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
        if method != "GET" && self.is_booting(&id) {
            return booting(&id);
        }
        match (method, action) {
            ("GET", None) => self.status(&id),
            ("POST", None) => self.create(&id, body, may_attach),
            ("GET", Some("egress")) => self.egress(&id),
            ("PUT", Some("egress")) => self.attach(&id, body),
            ("POST", Some("pause")) => match flag(query, "force") {
                Some(force) => self.vacate(&id, force, false, || self.fleet.pause(&id)),
                None => error(400, "force must be true or false"),
            },
            ("POST", Some("resume")) => {
                self.jobs.settle(&id);
                self.done(&id, self.fleet.resume(&id), 200)
            }
            ("DELETE", None) => match (flag(query, "purge"), flag(query, "force")) {
                (Some(purge), Some(force)) => {
                    let reply = self.vacate(&id, force, purge, || self.fleet.destroy(&id, purge));
                    if purge && reply.status == 200 {
                        self.jobs.forget(&id);
                    }
                    reply
                }
                (None, _) => error(400, "purge must be true or false"),
                (_, None) => error(400, "force must be true or false"),
            },
            ("GET", Some("checkpoints")) => self.checkpoints(&id),
            ("POST", Some("checkpoints")) => self.checkpoint(&id, body),
            ("POST", Some("restore")) => self.restore(&id, body),
            ("POST", Some("fork")) => self.fork(&id, body, agents, may_attach),
            ("DELETE", Some(a)) if one_checkpoint(a).is_some() => match one_checkpoint(a) {
                Some(Ok(cp)) => match self.fleet.delete_checkpoint(&id, &cp) {
                    Ok(()) => Reply {
                        status: 200,
                        body: format!(r#"{{"id":"{id}","deleted":"{cp}"}}"#),
                    },
                    Err(e) => map_error(&e),
                },
                _ => error(400, "bad checkpoint id"),
            },
            (
                _,
                None | Some("pause" | "resume" | "egress" | "checkpoints" | "restore" | "fork"),
            ) => error(405, "method not allowed"),
            (_, Some(a)) if one_checkpoint(a).is_some() => error(405, "method not allowed"),
            _ => error(404, "not found"),
        }
    }

    /// `{"id","max","checkpoints":[{"id","label"?}, ...]}`, oldest first.
    fn checkpoints(&self, id: &AgentId) -> Reply {
        match self.fleet.checkpoints(id) {
            Ok((all, max)) => {
                let items: Vec<String> = all
                    .iter()
                    .map(|c| checkpoint_json(&c.id, c.label.as_ref()))
                    .collect();
                Reply {
                    status: 200,
                    body: format!(
                        r#"{{"id":"{id}","max":{max},"checkpoints":[{}]}}"#,
                        items.join(",")
                    ),
                }
            }
            Err(e) => map_error(&e),
        }
    }

    fn checkpoint(&self, id: &AgentId, body: &[u8]) -> Reply {
        let label = match only_text(body, "label") {
            Ok(None) => None,
            Ok(Some(l)) => match Label::parse(&l) {
                Ok(l) => Some(l),
                Err(e) => return error(400, &e.to_string()),
            },
            Err(reply) => return reply,
        };
        match self.fleet.checkpoint(id, label.as_ref()) {
            Ok(cp) => Reply {
                status: 201,
                body: checkpoint_json(&cp, label.as_ref()),
            },
            Err(e) => map_error(&e),
        }
    }

    /// Refused while the agent has a job (a paused agent normally has none).
    fn restore(&self, id: &AgentId, body: &[u8]) -> Reply {
        let cp = match only_text(body, "checkpoint") {
            Ok(Some(cp)) => match CheckpointId::parse(&cp) {
                Ok(cp) => cp,
                Err(e) => return error(400, &format!("checkpoint: {e}")),
            },
            Ok(None) => return error(400, "body must be {\"checkpoint\":\"<id>\"}"),
            Err(reply) => return reply,
        };
        if self.jobs.busy(id) {
            return error(409, &format!("agent {id} has a queued or running job"));
        }
        self.jobs.settle(id);
        match self.fleet.restore(id, &cp) {
            Ok(kept) => Reply {
                status: 200,
                body: format!(r#"{{"id":"{id}","restored":"{cp}","kept":"{kept}"}}"#),
            },
            Err(e) => map_error(&e),
        }
    }

    /// The new agent must be one the token is for: 404 otherwise, as for any other agent.
    fn fork(&self, from: &AgentId, body: &[u8], agents: &Agents, may_attach: bool) -> Reply {
        let f = match Fork::parse(body) {
            Ok(f) => f,
            Err(reply) => return reply,
        };
        if !agents.contains(f.to.as_str()) {
            return error(404, "not found");
        }
        if self.is_booting(&f.to) {
            return booting(&f.to);
        }
        if let Some(p) = &f.profiles {
            if !p.is_empty() && !may_attach {
                return error(403, "attaching egress profiles needs scope egress:write");
            }
            if let Err(reply) = self.usable(p) {
                return reply;
            }
        }
        let made = self
            .fleet
            .fork(from, f.checkpoint.as_ref(), &f.to, f.profiles.as_deref());
        self.done(&f.to, made, 201)
    }

    /// Create in one go, boot included: [`Self::begin_create`], [`Creation::boot`],
    /// [`Self::finish_create`].
    fn create(&self, id: &AgentId, body: &[u8], may_attach: bool) -> Reply {
        match self.begin_create(id, body, may_attach) {
            Ok(creation) => {
                let booted = creation.boot();
                self.finish_create(creation, booted)
            }
            Err(reply) => reply,
        }
    }

    /// The part of a create that needs the lifecycle lock: check the body (its `egress_profiles`
    /// need `egress:write`), admit the guest, issue its credential and record it `creating`. The
    /// reply on refusal is the create's. The caller boots the result unlocked.
    pub fn begin_create(
        &self,
        id: &AgentId,
        body: &[u8],
        may_attach: bool,
    ) -> Result<Creation<R>, Reply> {
        let Create { profiles, image } = Create::parse(body)?;
        let image = image.map(|name| self.allowed_image(&name)).transpose()?;
        if let Some(p) = &profiles {
            if !p.is_empty() && !may_attach {
                return Err(error(
                    403,
                    "attaching egress profiles needs scope egress:write",
                ));
            }
            self.usable(p)?;
        }
        let mut set = self.booting.lock().unwrap_or_else(|e| e.into_inner());
        if set.contains(id) {
            return Err(booting(id));
        }
        self.jobs.settle(id);
        let reserved = self
            .fleet
            .reserve(id, profiles.as_deref(), image)
            .map_err(|e| map_error(&e))?;
        set.insert(id.clone());
        Ok(Creation {
            reserved,
            booting: Arc::clone(&self.booting),
        })
    }

    /// The image allowlisted as `name`; 400 naming the allowed ones otherwise.
    fn allowed_image(&self, name: &str) -> Result<&ImageRef, Reply> {
        self.images.get(name).ok_or_else(|| {
            let names: Vec<&str> = self.images.keys().map(String::as_str).collect();
            let allowed = match names.is_empty() {
                true => "none (AC_IMAGES is empty)".to_owned(),
                false => names.join(", "),
            };
            let name: String = name.chars().take(40).collect();
            error(
                400,
                &format!("image {name:?} is not allowed; allowed: {allowed}"),
            )
        })
    }

    /// Commit a create begun by [`Self::begin_create`] with its boot's outcome: 201 and the
    /// record, or the error (the agent stays `creating`, as for any failed create).
    pub fn finish_create(&self, creation: Creation<R>, booted: Result<(), PortError>) -> Reply {
        let committed = self.fleet.commit(&creation.reserved, booted);
        self.done(creation.reserved.id(), committed, 201)
    }

    /// Hold `id` for a restore from backup: it must be paused, with no job and room for the
    /// checkpoint that keeps the workspace it replaces. The caller restores unlocked, then drops
    /// the hold.
    pub fn hold_for_restore(&self, id: &AgentId) -> Result<Hold, Reply> {
        let mut set = self.booting.lock().unwrap_or_else(|e| e.into_inner());
        if set.contains(id) {
            return Err(booting(id));
        }
        match self.fleet.state(id).map_err(|e| map_error(&e))? {
            Some(State::Paused) => {}
            Some(state) => {
                let state = ac_domain::state::state_name(state);
                return Err(error(
                    409,
                    &format!("agent {id} is {state}: pause it first"),
                ));
            }
            None => return Err(error(404, &format!("no agent {id}"))),
        }
        if self.jobs.busy(id) {
            return Err(error(
                409,
                &format!("agent {id} has a queued or running job"),
            ));
        }
        let (kept, max) = self.fleet.checkpoints(id).map_err(|e| map_error(&e))?;
        ac_domain::checkpoint::check_room(id, kept.len(), max)
            .map_err(|e| error(409, &e.to_string()))?;
        self.jobs.settle(id);
        set.insert(id.clone());
        Ok(Hold {
            id: id.clone(),
            booting: Arc::clone(&self.booting),
        })
    }

    /// Whether `id`'s create is booting outside the lock.
    fn is_booting(&self, id: &AgentId) -> bool {
        let set = self.booting.lock().unwrap_or_else(|e| e.into_inner());
        set.contains(id)
    }

    fn profiles(&self) -> Reply {
        let Some(egress) = &self.egress else {
            return no_egress();
        };
        match egress.list() {
            Ok(all) => {
                let items: Vec<String> = all
                    .iter()
                    .map(|(name, hosts)| match hosts {
                        Ok(l) if l.exclusive() => format!(
                            r#"{{"name":"{name}","hosts":{},"exclusive":true}}"#,
                            l.len()
                        ),
                        Ok(l) => format!(r#"{{"name":"{name}","hosts":{}}}"#, l.len()),
                        Err(e) => format!(r#"{{"name":"{name}","error":"{}"}}"#, escape(e)),
                    })
                    .collect();
                Reply {
                    status: 200,
                    body: format!(r#"{{"profiles":[{}]}}"#, items.join(",")),
                }
            }
            Err(e) => error(500, &e),
        }
    }

    /// `{"id","profiles","effective_hosts"}`, plus `"missing"` for attached profiles with no
    /// usable file. An exclusive or missing profile leaves the base list out of the count, as the
    /// proxy leaves it out of the agent's list.
    fn egress(&self, id: &AgentId) -> Reply {
        let Some(egress) = &self.egress else {
            return no_egress();
        };
        match self.fleet.state(id) {
            Ok(Some(_)) => {}
            Ok(None) => return error(404, &FleetError::NotFound(id.clone()).to_string()),
            Err(e) => return map_error(&e),
        }
        let figures = egress.attached(id).and_then(|attached| {
            let listed = egress.list()?;
            let mut hosts = 0;
            let (mut missing, mut exclusive) = (Vec::new(), false);
            for p in &attached {
                match listed.iter().find(|(n, _)| n == p) {
                    Some((_, Ok(l))) => {
                        hosts += l.len();
                        exclusive |= l.exclusive();
                    }
                    _ => missing.push(format!(r#""{p}""#)),
                }
            }
            // As the proxy does: an exclusive or missing profile drops the base list.
            if !exclusive && missing.is_empty() {
                hosts += egress.base_hosts()?;
            }
            Ok((attached, hosts, missing))
        });
        let (attached, hosts, missing) = match figures {
            Ok(f) => f,
            Err(e) => return error(500, &e),
        };
        let names: Vec<String> = attached.iter().map(|p| format!(r#""{p}""#)).collect();
        let missing = match missing.is_empty() {
            true => String::new(),
            false => format!(r#","missing":[{}]"#, missing.join(",")),
        };
        Reply {
            status: 200,
            body: format!(
                r#"{{"id":"{id}","profiles":[{}],"effective_hosts":{hosts}{missing}}}"#,
                names.join(",")
            ),
        }
    }

    /// Replace the agent's profiles with the body's `profiles`; the proxy applies it to the next
    /// request, with no restart.
    fn attach(&self, id: &AgentId, body: &[u8]) -> Reply {
        let Some(egress) = &self.egress else {
            return no_egress();
        };
        let profiles = match profiles_in(body, "profiles") {
            Ok(Some(p)) => p,
            Ok(None) => return error(400, "body must be {\"profiles\":[...]}"),
            Err(reply) => return reply,
        };
        match self.fleet.state(id) {
            Ok(Some(State::Destroyed)) => return error(409, &format!("agent {id} is destroyed")),
            Ok(Some(_)) => {}
            Ok(None) => return error(404, &FleetError::NotFound(id.clone()).to_string()),
            Err(e) => return map_error(&e),
        }
        if let Err(reply) = self.usable(&profiles) {
            return reply;
        }
        match egress.attach(id, &profiles) {
            Ok(()) => self.egress(id),
            Err(e) => error(500, &e),
        }
    }

    /// 400 unless every one of `profiles` has a file the proxy would accept.
    fn usable(&self, profiles: &[ProfileName]) -> Result<(), Reply> {
        let Some(egress) = &self.egress else {
            return Err(no_egress());
        };
        let listed = egress.list().map_err(|e| error(500, &e))?;
        for p in profiles {
            match listed.iter().find(|(n, _)| n == p) {
                None => return Err(error(400, &format!("unknown egress profile {p}"))),
                Some((_, Err(e))) => {
                    return Err(error(
                        400,
                        &format!("egress profile {p} cannot be used: {e}"),
                    ))
                }
                Some((_, Ok(_))) => {}
            }
        }
        Ok(())
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
        purge: bool,
        act: impl FnOnce() -> Result<State, FleetError>,
    ) -> Reply {
        if !purge {
            return self.stop(id, force, act);
        }
        if !self.jobs.hold(id) {
            return error(
                409,
                &format!(
                    "agent {id}: what its last job changed is still being listed; retry the purge \
                     in a minute"
                ),
            );
        }
        let reply = self.stop(id, force, act);
        self.jobs.release(id);
        reply
    }

    /// Interrupt the agent's jobs (with `force`; else 409 if it has any), then `act`.
    fn stop(
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

/// 409 for a change to an agent whose create is still booting.
fn booting(id: &AgentId) -> Reply {
    error(409, &format!("agent {id} is being created or restored"))
}

/// A create through the lifecycle lock `api`, held only to reserve and to commit, so creates of
/// different agents boot at the same time (ADR-027).
pub fn create<R: Runtime, S: SessionStore>(
    api: &Mutex<Api<R, S>>,
    id: &AgentId,
    body: &[u8],
    may_attach: bool,
) -> Reply {
    let begun = {
        let api = api.lock().unwrap_or_else(|e| e.into_inner());
        api.begin_create(id, body, may_attach)
    };
    let creation = match begun {
        Ok(c) => c,
        Err(reply) => return reply,
    };
    // A panicking boot is a failed boot: it still reaches `finish_create`, which revokes the
    // proxy credential, so the two never differ.
    let booted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| creation.boot()))
        .unwrap_or_else(|_| {
            Err(PortError {
                op: "create",
                cause: "the boot panicked".into(),
            })
        });
    let api = api.lock().unwrap_or_else(|e| e.into_inner());
    api.finish_create(creation, booted)
}

/// The agent a `POST /api/v1/agents/{id}` creates, so the transport can boot it outside the
/// lifecycle lock; `None` for any other request, or an id that does not parse (the locked path
/// answers 400).
pub fn create_target(method: &str, target: &str) -> Option<AgentId> {
    let path = target.split_once('?').map_or(target, |(p, _)| p);
    let id = path.strip_prefix(PREFIX)?;
    match method == "POST" && !id.contains('/') {
        true => AgentId::parse(id).ok(),
        false => None,
    }
}

/// 404 when the gateway runs without agent networking, so without the proxy.
fn no_egress() -> Reply {
    error(404, "egress profiles are off (AC_NETWORK is not 1)")
}

/// A create's body: `{"egress_profiles"?,"image"?}`; empty is fine.
#[derive(Debug)]
pub struct Create {
    /// Profiles to attach before boot.
    pub profiles: Option<Vec<ProfileName>>,
    /// The allowlisted image name to boot from; `None`: the default image.
    pub image: Option<String>,
}

impl Create {
    /// Parse a create's body; the image name is checked against the allowlist later.
    pub fn parse(body: &[u8]) -> Result<Self, Reply> {
        let value = members(body, &["egress_profiles", "image"])?;
        let profiles = match &value {
            Some(v) => profile_list(v, "egress_profiles")?,
            None => None,
        };
        let image = text(value.as_ref(), "image")?.map(str::to_owned);
        Ok(Self { profiles, image })
    }
}

/// A fork's body: `{"id","checkpoint"?,"egress_profiles"?}`.
#[derive(Debug)]
pub struct Fork {
    /// The new agent.
    pub to: AgentId,
    /// The checkpoint to fork from; `None`: the workspace as it is now.
    pub checkpoint: Option<CheckpointId>,
    /// Profiles to attach to the new agent.
    pub profiles: Option<Vec<ProfileName>>,
}

impl Fork {
    /// Parse and validate a fork's body.
    pub fn parse(body: &[u8]) -> Result<Self, Reply> {
        let value = members(body, &["id", "checkpoint", "egress_profiles"])?;
        let to = match text(value.as_ref(), "id")? {
            Some(id) => AgentId::parse(id).map_err(|e| error(400, &format!("id: {e}")))?,
            None => {
                return Err(error(
                    400,
                    "body must name the new agent: {\"id\":\"<id>\"}",
                ))
            }
        };
        let checkpoint = text(value.as_ref(), "checkpoint")?
            .map(CheckpointId::parse)
            .transpose()
            .map_err(|e| error(400, &format!("checkpoint: {e}")))?;
        let profiles = match &value {
            Some(v) => profile_list(v, "egress_profiles")?,
            None => None,
        };
        Ok(Self {
            to,
            checkpoint,
            profiles,
        })
    }
}

/// `checkpoints/<cp>` as a checkpoint id (`Err` if malformed); `None` for any other action.
fn one_checkpoint(action: &str) -> Option<Result<CheckpointId, ac_domain::IdError>> {
    let cp = action.strip_prefix("checkpoints/")?;
    (!cp.contains('/')).then(|| CheckpointId::parse(cp))
}

fn checkpoint_json(cp: &CheckpointId, label: Option<&Label>) -> String {
    match label {
        Some(l) => format!(r#"{{"id":"{cp}","label":"{}"}}"#, escape(l.as_str())),
        None => format!(r#"{{"id":"{cp}"}}"#),
    }
}

/// The body as a JSON object with no member outside `allowed`; `None` for an empty body.
pub(crate) fn members(body: &[u8], allowed: &[&str]) -> Result<Option<Value>, Reply> {
    let bad = |m: &str| error(400, m);
    if body.trim_ascii().is_empty() {
        return Ok(None);
    }
    let text = std::str::from_utf8(body).map_err(|_| bad("body is not UTF-8"))?;
    let value = Value::parse(text).map_err(|e| bad(&format!("bad JSON: {e}")))?;
    let Some(obj) = value.as_object() else {
        return Err(bad("body must be a JSON object"));
    };
    if let Some(k) = obj.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(bad(&format!(
            "unknown member {:?}",
            k.chars().take(40).collect::<String>()
        )));
    }
    Ok(Some(value))
}

/// The string member `key`, if present.
fn text<'a>(value: Option<&'a Value>, key: &str) -> Result<Option<&'a str>, Reply> {
    match value.and_then(|v| v.get(key)) {
        None => Ok(None),
        Some(v) => v
            .as_str()
            .map(Some)
            .ok_or_else(|| error(400, &format!("{key} must be a string"))),
    }
}

/// The string member `key`, the body's only one; `None` for an empty body or one without it.
fn only_text(body: &[u8], key: &str) -> Result<Option<String>, Reply> {
    let value = members(body, &[key])?;
    Ok(text(value.as_ref(), key)?.map(str::to_owned))
}

/// `{"<key>": ["name", ...]}`, the body's only member, as validated profile names without
/// duplicates. `None` for an empty body or one without `key`.
pub fn profiles_in(body: &[u8], key: &str) -> Result<Option<Vec<ProfileName>>, Reply> {
    match members(body, &[key])? {
        Some(value) => profile_list(&value, key),
        None => Ok(None),
    }
}

/// Member `key` of `value` as validated profile names without duplicates; `None` if absent.
fn profile_list(value: &Value, key: &str) -> Result<Option<Vec<ProfileName>>, Reply> {
    let bad = |m: &str| error(400, m);
    let Some(list) = value.get(key) else {
        return Ok(None);
    };
    let Some(items) = list.as_array() else {
        return Err(bad(&format!("{key} must be an array of profile names")));
    };
    let mut names = Vec::new();
    for item in items {
        let Some(name) = item.as_str() else {
            return Err(bad(&format!("{key} must be an array of profile names")));
        };
        let name = ProfileName::parse(name).map_err(|e| bad(&e.to_string()))?;
        if names.contains(&name) {
            return Err(bad(&format!("egress profile {name} listed twice")));
        }
        names.push(name);
    }
    Ok(Some(names))
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
        FleetError::NotFound(_) | FleetError::NoCheckpoint(..) | FleetError::SnapshotsOff => 404,
        FleetError::ImagesOff => 400,
        FleetError::AlreadyExists(_) | FleetError::Illegal(_) | FleetError::Refused(_) => 409,
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

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
        fn create(&self, _: &AgentId, _: &ac_ports::Boot) -> Result<(), PortError> {
            self.go()
        }
        fn pause(&self, _: &AgentId) -> Result<(), PortError> {
            self.go()
        }
        fn resume(&self, _: &AgentId, _: &ac_ports::Boot) -> Result<(), PortError> {
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
        let (a, _) = egress_api();
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
            for v in &words {
                paths.push(format!("/api/v1/{w}/{v}"));
            }
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
            let life = a.request(m, p, b"", &Agents::All, true);
            let life = life.status != 405
                && life.body != error(404, "not found").body
                && !(life.status == 400 && empty);
            let own = (m == "GET" && p == web::WHOAMI) || (m == "POST" && p == mcp::PATH);
            viewer || job || life || own || admin::route(m, p).is_some()
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

    /// Profiles rust (2 hosts), node (refused by the proxy) and the base list (3 hosts); the
    /// attachments in memory. The fleet's Egress and the API's Profiles, as `EgressStore` is.
    #[derive(Default)]
    struct FakeEgress(Mutex<HashMap<String, Vec<ProfileName>>>);

    impl FakeEgress {
        fn of(&self, id: &str) -> Vec<String> {
            let m = self.0.lock().unwrap_or_else(|e| e.into_inner());
            m.get(id)
                .map_or_else(Vec::new, |v| v.iter().map(ToString::to_string).collect())
        }
    }

    impl ac_ports::Egress for FakeEgress {
        fn issue(
            &self,
            id: &AgentId,
            p: Option<&[ProfileName]>,
        ) -> Result<ac_ports::Boot, PortError> {
            let mut m = self.0.lock().unwrap_or_else(|e| e.into_inner());
            let line = m.entry(id.to_string()).or_default();
            if let Some(p) = p {
                *line = p.to_vec();
            }
            Ok(ac_ports::Boot::default())
        }
        fn revoke(&self, _: &AgentId) -> Result<(), PortError> {
            Ok(())
        }
        fn forget(&self, id: &AgentId) -> Result<(), PortError> {
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(id.as_str());
            Ok(())
        }
        fn sweep(&self, _: &[AgentId]) -> Result<Vec<AgentId>, PortError> {
            Ok(Vec::new())
        }
    }

    impl Profiles for FakeEgress {
        fn list(&self) -> Result<egress::Listed, String> {
            let p = |s| ProfileName::parse(s).map_err(|e| e.to_string());
            Ok(vec![
                (p("node")?, Err("mode 644".into())),
                (
                    p("rust")?,
                    ac_domain::Allowlist::new(["a.test", "b.test"]).map_err(|e| e.to_string()),
                ),
                (
                    p("wall")?,
                    ac_domain::Allowlist::parse_file("!exclusive\nendpoint mcp.test:8443\n")
                        .map_err(|e| e.to_string()),
                ),
            ])
        }
        fn base_hosts(&self) -> Result<usize, String> {
            Ok(3)
        }
        fn attached(&self, id: &AgentId) -> Result<Vec<ProfileName>, String> {
            let m = self.0.lock().unwrap_or_else(|e| e.into_inner());
            Ok(m.get(id.as_str()).cloned().unwrap_or_default())
        }
        fn attach(&self, id: &AgentId, p: &[ProfileName]) -> Result<(), String> {
            let mut m = self.0.lock().unwrap_or_else(|e| e.into_inner());
            m.insert(id.to_string(), p.to_vec());
            Ok(())
        }
    }

    fn egress_api() -> (Api<Rt, Store>, Arc<FakeEgress>) {
        let e = Arc::new(FakeEgress::default());
        let fleet = Fleet::new(Rt { fail: false }, Store::default()).with_egress(e.clone());
        (Api::new(fleet).with_egress(e.clone()), e)
    }

    fn send(a: &Api<Rt, Store>, m: &str, t: &str, body: &str, may_attach: bool) -> (u16, String) {
        let r = a.request(m, t, body.as_bytes(), &Agents::All, may_attach);
        (r.status, r.body)
    }

    #[test]
    fn egress_profiles_are_listed_with_host_counts() {
        let (a, _) = egress_api();
        assert_eq!(
            call(&a, "GET", PROFILES),
            (
                200,
                r#"{"profiles":[{"name":"node","error":"mode 644"},{"name":"rust","hosts":2},{"name":"wall","hosts":1,"exclusive":true}]}"#
                    .into()
            )
        );
        assert_eq!(call(&a, "POST", PROFILES).0, 405);
        assert_eq!(
            call(&api(false), "GET", PROFILES).0,
            404,
            "no egress without AC_NETWORK"
        );
    }

    #[test]
    fn attach_and_detach_an_agents_profiles() {
        let (a, e) = egress_api();
        let path = "/api/v1/agents/a1/egress";
        assert_eq!(call(&a, "GET", path).0, 404, "unknown agent");
        assert_eq!(
            send(&a, "PUT", path, r#"{"profiles":["rust"]}"#, true).0,
            404
        );
        let _ = call(&a, "POST", "/api/v1/agents/a1");
        assert_eq!(
            call(&a, "GET", path),
            (
                200,
                r#"{"id":"a1","profiles":[],"effective_hosts":3}"#.into()
            )
        );
        assert_eq!(
            send(&a, "PUT", path, r#"{"profiles":["rust"]}"#, true),
            (
                200,
                r#"{"id":"a1","profiles":["rust"],"effective_hosts":5}"#.into()
            )
        );
        assert_eq!(e.of("a1"), ["rust"]);
        // An exclusive profile drops the base list from the count.
        assert_eq!(
            send(&a, "PUT", path, r#"{"profiles":["wall","rust"]}"#, true).1,
            r#"{"id":"a1","profiles":["wall","rust"],"effective_hosts":3}"#
        );
        assert_eq!(
            send(&a, "PUT", path, r#"{"profiles":[]}"#, true).1,
            r#"{"id":"a1","profiles":[],"effective_hosts":3}"#
        );
        assert!(e.of("a1").is_empty());
        // A profile whose file went away is reported, and adds nothing.
        let gone = ProfileName::parse("gone").unwrap_or_else(|e| panic!("{e}"));
        let _ = e.attach(
            &AgentId::parse("a1").unwrap_or_else(|e| panic!("{e}")),
            &[gone],
        );
        assert_eq!(
            call(&a, "GET", path).1,
            r#"{"id":"a1","profiles":["gone"],"effective_hosts":0,"missing":["gone"]}"#
        );
        let _ = call(&a, "DELETE", "/api/v1/agents/a1");
        assert_eq!(send(&a, "PUT", path, r#"{"profiles":[]}"#, true).0, 409);
        assert_eq!(call(&a, "POST", path).0, 405);
    }

    #[test]
    fn bad_attach_bodies_are_400_and_change_nothing() {
        let (a, e) = egress_api();
        let _ = call(&a, "POST", "/api/v1/agents/a1");
        let path = "/api/v1/agents/a1/egress";
        for (body, want) in [
            ("", "body must be"),
            ("{}", "body must be"),
            ("[]", "JSON object"),
            ("{\"profiles\":\"rust\"}", "array of profile names"),
            ("{\"profiles\":[1]}", "array of profile names"),
            ("{\"profiles\":[\"Rust\"]}", "profile name"),
            ("{\"profiles\":[\"-\"]}", "profile name"),
            ("{\"profiles\":[\"rust\",\"rust\"]}", "listed twice"),
            ("{\"profiles\":[\"go\"]}", "unknown egress profile go"),
            (
                "{\"profiles\":[\"node\"]}",
                "egress profile node cannot be used: mode 644",
            ),
            (
                "{\"profiles\":[],\"hosts\":[\"evil.example\"]}",
                "unknown member \\\"hosts\\\"",
            ),
            ("{\"profiles\":[", "bad JSON"),
            ("\u{ff}", "bad JSON"),
        ] {
            let (status, reply) = send(&a, "PUT", path, body, true);
            assert_eq!(status, 400, "{body:?}: {reply}");
            assert!(reply.contains(want), "{body:?}: {reply}");
        }
        assert!(e.of("a1").is_empty());
    }

    #[test]
    fn a_desktop_agent_created_over_the_api_resumes_on_its_desktop_image() {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("ac-gw-images-{}-{n}", std::process::id()));
        let file = Arc::new(ac_store::images::ImageFile::new(ac_store::images::path(
            &dir,
        )));
        let desktop = ImageRef::parse("agent-desktop:latest").unwrap_or_else(|e| panic!("{e}"));
        let allow = BTreeMap::from([("desktop".to_owned(), desktop.clone())]);
        let a = Api::new(Fleet::new(Rt { fail: false }, Store::default()).with_images(file))
            .with_images(allow);
        let (status, reply) = send(&a, "POST", "/api/v1/agents/d0", r#"{"image":"gpu"}"#, false);
        assert_eq!(status, 400, "{reply}");
        assert!(reply.contains("allowed: desktop"), "{reply}");
        assert_eq!(
            call(&a, "GET", "/api/v1/agents/d0").0,
            404,
            "nothing created"
        );
        let body = r#"{"image":"desktop"}"#;
        assert_eq!(send(&a, "POST", "/api/v1/agents/d1", body, false).0, 201);
        assert_eq!(call(&a, "POST", "/api/v1/agents/d1/pause").0, 200);
        assert_eq!(call(&a, "POST", "/api/v1/agents/d1/resume").0, 200);
        let image = |id: &str| {
            a.fleet
                .image(&AgentId::parse(id).unwrap_or_else(|e| panic!("{e}")))
        };
        assert_eq!(image("d1").ok().flatten(), Some(desktop));
        assert_eq!(call(&a, "POST", "/api/v1/agents/a1").0, 201);
        assert_eq!(image("a1").ok().flatten(), None, "no image named: AC_IMAGE");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn naming_an_image_without_an_allowlist_is_refused() {
        let (status, reply) = send(
            &api(false),
            "POST",
            "/api/v1/agents/d1",
            r#"{"image":"desktop"}"#,
            false,
        );
        assert_eq!(status, 400, "{reply}");
        assert!(reply.contains("AC_IMAGES is empty"), "{reply}");
    }

    #[test]
    fn create_can_attach_but_only_with_egress_write() {
        let (a, e) = egress_api();
        let body = r#"{"egress_profiles":["rust"]}"#;
        let (status, reply) = send(&a, "POST", "/api/v1/agents/a1", body, false);
        assert_eq!(
            (status, reply.contains("needs scope egress:write")),
            (403, true),
            "{reply}"
        );
        assert_eq!(
            call(&a, "GET", "/api/v1/agents/a1").0,
            404,
            "nothing created"
        );
        assert_eq!(send(&a, "POST", "/api/v1/agents/a1", body, true).0, 201);
        assert_eq!(e.of("a1"), ["rust"]);
        // No profiles needs nothing more; an unknown one is refused before anything is created.
        assert_eq!(
            send(
                &a,
                "POST",
                "/api/v1/agents/a2",
                r#"{"egress_profiles":[]}"#,
                false
            )
            .0,
            201
        );
        let (status, _) = send(
            &a,
            "POST",
            "/api/v1/agents/a3",
            r#"{"egress_profiles":["go"]}"#,
            true,
        );
        assert_eq!(status, 400);
        assert_eq!(call(&a, "GET", "/api/v1/agents/a3").0, 404);
        assert_eq!(
            send(&a, "POST", "/api/v1/agents/a4", r#"{"x":1}"#, true).0,
            400
        );
        // Without egress, a create naming profiles is refused, and one without works as before.
        assert_eq!(
            send(&api(false), "POST", "/api/v1/agents/a1", body, true).0,
            404
        );
        assert_eq!(
            send(&api(false), "POST", "/api/v1/agents/a1", "", false).0,
            201
        );
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
        fn create(&self, id: &AgentId, _: &ac_ports::Boot) -> Result<(), PortError> {
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
        fn resume(&self, _: &AgentId, _: &ac_ports::Boot) -> Result<(), PortError> {
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

    /// Each create waits inside the runtime until `want` creates are inside at once, so a create
    /// path that boots under the lifecycle lock times out instead of passing.
    struct Gate {
        inside: AtomicUsize,
        want: usize,
        fail: bool,
    }
    impl Gate {
        fn new(want: usize, fail: bool) -> Self {
            Self {
                inside: AtomicUsize::new(0),
                want,
                fail,
            }
        }
    }
    impl Runtime for Gate {
        fn create(&self, _: &AgentId, _: &ac_ports::Boot) -> Result<(), PortError> {
            self.inside.fetch_add(1, Ordering::SeqCst);
            let start = Instant::now();
            while self.inside.load(Ordering::SeqCst) < self.want {
                if start.elapsed() > Duration::from_secs(5) {
                    return Err(PortError {
                        op: "create",
                        cause: "creates did not overlap".into(),
                    });
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            match self.fail {
                true => Err(PortError {
                    op: "create",
                    cause: "boot failed".into(),
                }),
                false => Ok(()),
            }
        }
        fn pause(&self, _: &AgentId) -> Result<(), PortError> {
            Ok(())
        }
        fn resume(&self, _: &AgentId, _: &ac_ports::Boot) -> Result<(), PortError> {
            Ok(())
        }
        fn destroy(&self, _: &AgentId, _: bool) -> Result<(), PortError> {
            Ok(())
        }
        fn running(&self) -> Result<Vec<AgentId>, PortError> {
            Ok(Vec::new())
        }
    }

    type Locked = Arc<Mutex<Api<Gate, Store>>>;

    fn gated(gate: Gate, admission: bool) -> Locked {
        let mut fleet = Fleet::new(gate, Store::default());
        if admission {
            fleet = fleet.with_admission(Box::new(Shm9000), 512);
        }
        Arc::new(Mutex::new(Api::new(fleet)))
    }

    fn create_in(a: &Locked, id: &str) -> std::thread::JoinHandle<u16> {
        let a = Arc::clone(a);
        let id = AgentId::parse(id).unwrap_or_else(|e| panic!("{e}"));
        std::thread::spawn(move || create(&a, &id, b"", false).status)
    }

    fn locked(a: &Locked, m: &str, t: &str) -> (u16, String) {
        let api = a.lock().unwrap_or_else(|e| e.into_inner());
        let r = api.handle(m, t, &Agents::All);
        (r.status, r.body)
    }

    fn statuses(threads: Vec<std::thread::JoinHandle<u16>>) -> Vec<u16> {
        let mut got: Vec<u16> = threads.into_iter().map(|t| t.join().unwrap_or(0)).collect();
        got.sort_unstable();
        got
    }

    /// Four creates boot at the same time: each waits in the runtime for all four.
    #[test]
    fn concurrent_creates_boot_outside_the_lock() {
        let a = gated(Gate::new(4, false), false);
        let threads: Vec<_> = ["c1", "c2", "c3", "c4"]
            .iter()
            .map(|id| create_in(&a, id))
            .collect();
        assert_eq!(statuses(threads), [201, 201, 201, 201]);
        for id in ["c1", "c2", "c3", "c4"] {
            let (_, body) = locked(&a, "GET", &format!("/api/v1/agents/{id}"));
            assert!(body.contains(r#""state":"running""#), "{body}");
        }
    }

    /// Room for two guests: of three creates booting at once, the reservations admit exactly two.
    #[test]
    fn unlocked_creates_cannot_over_admit() {
        let a = gated(Gate::new(2, false), true);
        let threads: Vec<_> = ["o1", "o2", "o3"]
            .iter()
            .map(|id| create_in(&a, id))
            .collect();
        assert_eq!(statuses(threads), [201, 201, 507]);
    }

    /// While an agent boots, every change to it is 409 and reads are served; afterwards it is
    /// an ordinary running agent.
    #[test]
    fn a_booting_agent_refuses_changes() {
        let a = gated(Gate::new(2, false), false);
        let first = create_in(&a, "b1");
        let start = Instant::now();
        let b1 = AgentId::parse("b1").unwrap_or_else(|e| panic!("{e}"));
        while !a.lock().is_ok_and(|api| api.is_booting(&b1)) {
            assert!(start.elapsed() < Duration::from_secs(5), "b1 never booted");
            std::thread::sleep(Duration::from_millis(1));
        }
        for (m, t) in [
            ("POST", "/api/v1/agents/b1"),
            ("POST", "/api/v1/agents/b1/pause?force=false"),
            ("DELETE", "/api/v1/agents/b1?purge=true&force=true"),
            ("PUT", "/api/v1/agents/b1/egress"),
            ("POST", "/api/v1/agents/b1/checkpoints"),
            ("POST", "/api/v1/agents/b1/resume"),
            ("POST", "/api/v1/agents/b1/restore"),
            (
                "DELETE",
                "/api/v1/agents/b1/checkpoints/20261125T000000Z-0001",
            ),
            ("POST", "/api/v1/agents/b1/fork"),
        ] {
            let (status, body) = locked(&a, m, t);
            assert_eq!(status, 409, "{m} {t}: {body}");
            assert!(body.contains("is being created"), "{body}");
        }
        // A fork to the booting agent, from another one.
        let (status, body) = {
            let api = a.lock().unwrap_or_else(|e| e.into_inner());
            let r = api.request(
                "POST",
                "/api/v1/agents/zz/fork",
                br#"{"id":"b1"}"#,
                &Agents::All,
                false,
            );
            (r.status, r.body)
        };
        assert_eq!(status, 409, "fork to b1: {body}");
        assert!(body.contains("is being created"), "{body}");
        let (status, body) = locked(&a, "GET", "/api/v1/agents/b1");
        assert_eq!(status, 200);
        assert!(body.contains(r#""state":"creating""#), "{body}");
        let second = create_in(&a, "b2");
        assert_eq!(statuses(vec![first, second]), [201, 201]);
        assert_eq!(
            locked(&a, "POST", "/api/v1/agents/b1/pause?force=false").0,
            200
        );
    }

    /// A failed boot answers 500, leaves the agent `creating` as before, and frees the id: a
    /// retry or a destroy is served, not refused as booting.
    #[test]
    fn a_failed_boot_releases_the_agent() {
        let a = gated(Gate::new(1, true), true);
        assert_eq!(statuses(vec![create_in(&a, "f1")]), [500]);
        let (_, body) = locked(&a, "GET", "/api/v1/agents/f1");
        assert!(body.contains(r#""state":"creating""#), "{body}");
        assert_eq!(statuses(vec![create_in(&a, "f1")]), [500]);
        let (status, body) = locked(&a, "DELETE", "/api/v1/agents/f1?purge=true&force=false");
        assert_eq!(status, 200, "{body}");
    }

    #[test]
    fn only_a_bare_post_is_a_create_target() {
        let a1 = AgentId::parse("a1").ok();
        assert_eq!(create_target("POST", "/api/v1/agents/a1"), a1);
        assert_eq!(create_target("POST", "/api/v1/agents/a1?x=1"), a1);
        assert_eq!(create_target("GET", "/api/v1/agents/a1"), None);
        assert_eq!(create_target("POST", "/api/v1/agents/a1/pause"), None);
        assert_eq!(create_target("POST", "/api/v1/agents/BAD id"), None);
        assert_eq!(create_target("POST", "/api/v1/agents"), None);
    }

    #[test]
    fn runtime_failure_is_500_with_escaped_message() {
        let r = call(&api(true), "POST", "/api/v1/agents/a1");
        assert_eq!(r.0, 500);
        assert!(r.1.contains(r#"bad \"thing\""#), "{}", r.1);
    }

    /// Every workspace a subvolume; checkpoints in memory, numbered from a fixed time.
    #[derive(Default)]
    struct Snaps(Mutex<HashMap<String, Vec<ac_domain::Checkpoint>>>);

    impl ac_ports::Snapshots for Snaps {
        fn volume(&self, id: &AgentId) -> Result<ac_domain::Volume, PortError> {
            Ok(match id.as_str() {
                "free" | "other" => ac_domain::Volume::Missing,
                _ => ac_domain::Volume::Subvolume,
            })
        }
        fn checkpoints(&self, id: &AgentId) -> Result<Vec<ac_domain::Checkpoint>, PortError> {
            let m = self.0.lock().unwrap_or_else(|e| e.into_inner());
            Ok(m.get(id.as_str()).cloned().unwrap_or_default())
        }
        fn checkpoint(
            &self,
            id: &AgentId,
            label: Option<&Label>,
        ) -> Result<CheckpointId, PortError> {
            let mut m = self.0.lock().unwrap_or_else(|e| e.into_inner());
            let list = m.entry(id.to_string()).or_default();
            let taken: Vec<CheckpointId> = list.iter().map(|c| c.id.clone()).collect();
            let cp = CheckpointId::next(1_793_192_712, &taken);
            list.push(ac_domain::Checkpoint {
                id: cp.clone(),
                label: label.cloned(),
            });
            Ok(cp)
        }
        fn delete(&self, id: &AgentId, cp: &CheckpointId) -> Result<(), PortError> {
            let mut m = self.0.lock().unwrap_or_else(|e| e.into_inner());
            m.entry(id.to_string()).or_default().retain(|c| &c.id != cp);
            Ok(())
        }
        fn restore(&self, id: &AgentId, _: &CheckpointId) -> Result<CheckpointId, PortError> {
            self.checkpoint(id, None)
        }
        fn fork(
            &self,
            _: &AgentId,
            _: Option<&CheckpointId>,
            _: &AgentId,
        ) -> Result<(), PortError> {
            Ok(())
        }
        fn migrate(&self, _: &AgentId) -> Result<(), PortError> {
            Ok(())
        }
        fn purge(&self, _: &AgentId) -> Result<(), PortError> {
            Ok(())
        }
    }

    fn snap_api() -> Api<Rt, Store> {
        Api::new(
            Fleet::new(Rt { fail: false }, Store::default())
                .with_snapshots(Arc::new(Snaps::default()), 2),
        )
    }

    const A1: &str = "/api/v1/agents/a1";

    #[test]
    fn checkpoints_off_is_404_with_its_reason() {
        let a = api(false);
        let _ = call(&a, "POST", A1);
        let (status, body) = call(&a, "GET", &format!("{A1}/checkpoints"));
        assert_eq!(
            (status, body.as_str()),
            (404, r#"{"error":"checkpoints are off"}"#)
        );
    }

    #[test]
    fn checkpoint_list_and_delete() {
        let a = snap_api();
        let _ = call(&a, "POST", A1);
        let cps = format!("{A1}/checkpoints");
        let (status, body) = send(&a, "POST", &cps, r#"{"label":"before \"x\""}"#, false);
        assert_eq!(status, 201);
        assert_eq!(body, r#"{"id":"20261028-130512","label":"before \"x\""}"#);
        assert_eq!(send(&a, "POST", &cps, "", false).0, 201);
        assert_eq!(
            call(&a, "GET", &cps).1,
            r#"{"id":"a1","max":2,"checkpoints":[{"id":"20261028-130512","label":"before \"x\""},{"id":"20261028-130512-2"}]}"#
        );
        let (status, body) = send(&a, "POST", &cps, "", false);
        assert_eq!(status, 409, "{body}");
        assert!(body.contains("delete one first"), "{body}");
        for bad in [r#"{"label":""}"#, r#"{"label":3}"#, r#"{"name":"x"}"#, "[]"] {
            assert_eq!(send(&a, "POST", &cps, bad, false).0, 400, "{bad}");
        }
        assert_eq!(call(&a, "DELETE", &format!("{cps}/20261028-130512")).0, 200);
        assert_eq!(call(&a, "DELETE", &format!("{cps}/20261028-130512")).0, 404);
        assert_eq!(call(&a, "DELETE", &format!("{cps}/BAD")).0, 400);
        assert_eq!(call(&a, "POST", &format!("{cps}/20261028-130512-2")).0, 405);
        assert_eq!(call(&a, "DELETE", &format!("{cps}/x/y")).0, 404);
        assert_eq!(call(&a, "GET", "/api/v1/agents/zz/checkpoints").0, 404);
    }

    #[test]
    fn restore_needs_a_paused_agent() {
        let a = snap_api();
        let _ = call(&a, "POST", A1);
        let _ = send(&a, "POST", &format!("{A1}/checkpoints"), "", false);
        let restore = format!("{A1}/restore");
        let body = r#"{"checkpoint":"20261028-130512"}"#;
        assert_eq!(send(&a, "POST", &restore, body, false).0, 409);
        let _ = call(&a, "POST", &format!("{A1}/pause"));
        assert_eq!(
            send(&a, "POST", &restore, body, false),
            (
                200,
                r#"{"id":"a1","restored":"20261028-130512","kept":"20261028-130512-2"}"#.into()
            )
        );
        let unknown = r#"{"checkpoint":"20200101-000000"}"#;
        assert_eq!(send(&a, "POST", &restore, unknown, false).0, 404);
        assert_eq!(send(&a, "POST", &restore, "", false).0, 400);
        assert_eq!(
            send(&a, "POST", &restore, r#"{"checkpoint":"../x"}"#, false).0,
            400
        );
    }

    /// ADR-031: a restore from backup holds a paused agent with room for one more checkpoint;
    /// while held, changes to it are refused and reads served; dropping the hold ends that.
    #[test]
    fn a_restore_hold_needs_a_paused_agent_and_blocks_changes() {
        let a = snap_api();
        let id = AgentId::parse("a1").unwrap_or_else(|e| panic!("{e}"));
        let status = |r: Result<Hold, Reply>| r.err().map(|e| e.status);
        assert_eq!(status(a.hold_for_restore(&id)), Some(404), "unknown");
        let _ = call(&a, "POST", A1);
        assert_eq!(status(a.hold_for_restore(&id)), Some(409), "running");
        let _ = call(&a, "POST", &format!("{A1}/pause"));
        let hold = a.hold_for_restore(&id).unwrap_or_else(|r| panic!("{r:?}"));
        assert_eq!(status(a.hold_for_restore(&id)), Some(409), "held already");
        let (code, body) = call(&a, "POST", &format!("{A1}/resume"));
        assert_eq!(code, 409);
        assert!(body.contains("being created or restored"), "{body}");
        assert_eq!(call(&a, "GET", A1).0, 200, "reads are served");
        drop(hold);
        let cps = format!("{A1}/checkpoints");
        let _ = send(&a, "POST", &cps, "", false);
        let _ = send(&a, "POST", &cps, "", false);
        let (code, body) = a
            .hold_for_restore(&id)
            .err()
            .map(|r| (r.status, r.body))
            .unwrap_or_default();
        assert_eq!(code, 409, "no room for the kept workspace");
        assert!(body.contains("delete one first"), "{body}");
        let _ = call(&a, "DELETE", &format!("{cps}/20261028-130512"));
        assert!(a.hold_for_restore(&id).is_ok());
        assert_eq!(call(&a, "POST", &format!("{A1}/resume")).0, 200, "released");
    }

    #[test]
    fn fork_needs_a_token_for_the_new_agent_and_egress_write_for_profiles() {
        let a = snap_api();
        let _ = call(&a, "POST", A1);
        let fork = format!("{A1}/fork");
        let only_a1 = Agents::Only(vec![AgentId::parse("a1").unwrap_or_else(|e| panic!("{e}"))]);
        let r = a.request("POST", &fork, br#"{"id":"free"}"#, &only_a1, false);
        assert_eq!(
            (r.status, r.body.as_str()),
            (404, r#"{"error":"not found"}"#)
        );
        let with_profiles = br#"{"id":"free","egress_profiles":["rust"]}"#;
        assert_eq!(
            a.request("POST", &fork, with_profiles, &Agents::All, false)
                .status,
            403
        );
        assert_eq!(
            send(&a, "POST", &fork, r#"{"id":"free"}"#, false),
            (201, r#"{"id":"free","state":"running"}"#.into())
        );
        // "a1" has a workspace already, and is running.
        let (status, body) = send(&a, "POST", &fork, r#"{"id":"a1"}"#, false);
        assert_eq!(status, 409);
        assert!(body.contains("already has a workspace"), "{body}");
        let from_unknown = r#"{"id":"other","checkpoint":"20200101-000000"}"#;
        assert_eq!(send(&a, "POST", &fork, from_unknown, false).0, 404);
        for bad in ["", r#"{"id":"BAD"}"#, r#"{"id":"x","extra":1}"#] {
            assert_eq!(send(&a, "POST", &fork, bad, false).0, 400, "{bad}");
        }
    }
}
