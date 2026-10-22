//! The task API (ADR-010) over the real registry: the job supervisor and routes with a fake
//! runner behind the `JobRunner` seam, the HTTP surface over a loopback socket, and the `ctr`
//! runner itself against platform-mock's spawner.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ac_ctr::{CtrConfig, CtrRuntime};
use ac_domain::{AgentId, JobId, State};
use ac_fleet::Fleet;
use ac_gateway::jobs::{self, Answer, JobRunner, Jobs, Part, Probe, Route};
use ac_gateway::tokens::{self, Agents, Keys, Scopes, Token, TokenFile, Tokens};
use ac_gateway::viewer::Hub;
use ac_gateway::web::App;
use ac_gateway::{Api, Busy};
use ac_ports::{JobStore, PortError, Runtime, SessionStore};
use ac_store::EngineStore;
use platform::process::ExitStatus;
use platform_mock::MockSpawner;
use rusty_serve::Server;

const TOKEN: &str = "s3cret";

fn tmp() -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("ac-jobs-{}-{n}", std::process::id()))
}

fn agent(s: &str) -> AgentId {
    AgentId::parse(s).unwrap_or_else(|e| panic!("{e}"))
}

// ---- a fake runner ---------------------------------------------------------------------------

#[derive(Default)]
struct World {
    /// (job, prompt, timeout, flags) per start.
    starts: Vec<(String, Vec<u8>, u32, Vec<String>)>,
    /// What `status` reports per job; `Running` if absent.
    status: HashMap<String, Probe>,
    statuses: usize,
    kills: Vec<String>,
    kill_fails: bool,
    start_fails: bool,
}

#[derive(Clone, Default)]
struct Fake(Arc<Mutex<World>>);

impl Fake {
    fn w(&self) -> std::sync::MutexGuard<'_, World> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn set(&self, job: &str, p: Probe) {
        self.w().status.insert(job.to_owned(), p);
    }
}

impl JobRunner for Fake {
    fn start(
        &self,
        _: &AgentId,
        job: &JobId,
        prompt: &[u8],
        timeout: u32,
        claude: &[String],
    ) -> Result<(), String> {
        let mut w = self.w();
        if w.start_fails {
            return Err("no VM".into());
        }
        w.starts
            .push((job.to_string(), prompt.to_vec(), timeout, claude.to_vec()));
        Ok(())
    }
    /// The marker appears once the test says how the job ended.
    fn finished(&self, _: &AgentId, job: &JobId) -> bool {
        self.w()
            .status
            .get(job.as_str())
            .is_some_and(|p| *p != Probe::Running)
    }
    fn status(&self, _: &AgentId, job: &JobId) -> Result<Probe, String> {
        let mut w = self.w();
        w.statuses += 1;
        Ok(w.status
            .get(job.as_str())
            .copied()
            .unwrap_or(Probe::Running))
    }
    fn kill(&self, _: &AgentId, job: &JobId) -> Result<(), String> {
        let mut w = self.w();
        if w.kill_fails {
            return Err("alive: 42".into());
        }
        w.kills.push(job.to_string());
        w.status.insert(job.to_string(), Probe::Gone);
        Ok(())
    }
    fn read(&self, _: &AgentId, job: &JobId, part: Part) -> Result<Vec<u8>, String> {
        Ok(match part {
            Part::Result => format!("result of {job} <script>").into_bytes(),
            Part::Stream => b"{\"type\":\"result\"}\n".to_vec(),
        })
    }
}

// ---- a rig: real registry, fake runner, fake clock ---------------------------------------------

struct Rig {
    jobs: Arc<Jobs>,
    store: Arc<EngineStore>,
    fake: Fake,
    clock: Arc<AtomicU64>,
    notes: Arc<Mutex<Vec<String>>>,
    root: PathBuf,
}

const FLAGS: [&str; 4] = ["--permission-mode", "acceptEdits", "--allowedTools", "Read"];

fn jobs_over(store: &Arc<EngineStore>, runner: impl JobRunner, root: &Path) -> Jobs {
    Jobs::new(
        store.clone(),
        store.clone(),
        runner,
        root.join("jobs"),
        FLAGS.map(str::to_owned).to_vec(),
    )
}

fn rig() -> Rig {
    let root = tmp();
    let store = Arc::new(EngineStore::open(&root.join("data")).unwrap_or_else(|e| panic!("{e}")));
    for (id, s) in [
        ("a1", State::Running),
        ("a2", State::Running),
        ("p1", State::Paused),
    ] {
        let _ = store.set_state(&agent(id), s);
    }
    with_store(store, Fake::default(), root)
}

fn with_store(store: Arc<EngineStore>, fake: Fake, root: PathBuf) -> Rig {
    let clock = Arc::new(AtomicU64::new(1_000_000));
    let notes = Arc::new(Mutex::new(Vec::new()));
    let (c, n) = (Arc::clone(&clock), Arc::clone(&notes));
    let jobs = jobs_over(&store, fake.clone(), &root)
        .with_clock(move || c.load(Ordering::SeqCst))
        .with_notes(move |id, text| {
            let _ = n.lock().map(|mut v| v.push(format!("{id}: {text}")));
        });
    Rig {
        jobs: Arc::new(jobs),
        store,
        fake,
        clock,
        notes,
        root,
    }
}

impl Rig {
    fn submit(&self, id: &str, body: &str) -> (u16, String) {
        let state = self.store.state(&agent(id)).ok().flatten();
        let r = self.jobs.submit(&agent(id), state, body.as_bytes());
        (r.status, r.body)
    }

    fn ok(&self, id: &str, prompt: &str) -> String {
        let (status, body) = self.submit(id, &format!(r#"{{"prompt":"{prompt}"}}"#));
        assert_eq!(status, 202, "{body}");
        field(&body, "job_id")
    }

    fn json(&self, route: Route, query: &str) -> (u16, String) {
        match self.jobs.handle(route, query) {
            Answer::Json(r) => (r.status, r.body),
            Answer::Output(b) => (200, String::from_utf8_lossy(&b).into_owned()),
        }
    }

    fn get(&self, id: &str, job: &str) -> String {
        self.json(Route::Get(agent(id), job_id(job)), "").1
    }

    fn state(&self, id: &str, job: &str) -> String {
        field(&self.get(id, job), "state")
    }

    fn advance(&self, ms: u64) {
        self.clock.fetch_add(ms, Ordering::SeqCst);
    }

    /// Tick past the least time between marker-prompted probes.
    fn tick(&self) {
        self.advance(5_000);
        self.jobs.tick();
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn job_id(s: &str) -> JobId {
    JobId::parse(s).unwrap_or_else(|e| panic!("{s}: {e}"))
}

/// A string or bare field of our own flat JSON.
fn field(body: &str, name: &str) -> String {
    let rest = body
        .split_once(&format!("\"{name}\":"))
        .map(|(_, r)| r)
        .unwrap_or_else(|| panic!("no {name} in {body}"));
    match rest.strip_prefix('"') {
        Some(s) => s.split('"').next().unwrap_or_default().to_owned(),
        None => rest.split([',', '}']).next().unwrap_or_default().to_owned(),
    }
}

// ---- the supervisor and routes ---------------------------------------------------------------

#[test]
fn submit_validates_input_and_agent_state() {
    let r = rig();
    let big = "x".repeat(64 * 1024 + 1);
    for (body, want) in [
        ("", 400),
        ("not json", 400),
        ("[1]", 400),
        (r#"{"prompt":"hi""#, 400),
        (r#"{"prompt":7}"#, 400),
        (r#"{"prompt":""}"#, 400),
        (r#"{"prompt":"hi","extra":1}"#, 400),
        (r#"{"prompt":"hi","timeout_secs":0}"#, 400),
        (r#"{"prompt":"hi","timeout_secs":-5}"#, 400),
        (r#"{"prompt":"hi","timeout_secs":1.5}"#, 400),
        (r#"{"prompt":"hi","timeout_secs":"60"}"#, 400),
        (r#"{"prompt":"hi","timeout_secs":14401}"#, 400),
        (r#"{"prompt":"hi","idempotency_key":"a b"}"#, 400),
        (r#"{"prompt":"hi","idempotency_key":5}"#, 400),
        (&format!(r#"{{"prompt":"{big}"}}"#), 413),
    ] {
        assert_eq!(r.submit("a1", body).0, want, "{body:.60}");
    }
    assert_eq!(r.submit("p1", r#"{"prompt":"hi"}"#).0, 409);
    assert!(r
        .submit("p1", r#"{"prompt":"hi"}"#)
        .1
        .contains("resume it first"));
    assert_eq!(r.submit("zz", r#"{"prompt":"hi"}"#).0, 404);
    let (status, body) = r.submit("a1", r#"{"prompt":"hi","timeout_secs":14400}"#);
    assert_eq!((status, field(&body, "state")), (202, "queued".into()));
    assert_eq!(field(&body, "timeout_secs"), "14400");
    assert_eq!(field(&body, "started_ms"), "null");
    assert!(r.fake.w().starts.is_empty(), "nothing starts before a tick");
}

#[test]
fn one_job_at_a_time_through_to_success_with_output_kept() {
    let r = rig();
    let first = r.ok("a1", "one");
    let second = r.ok("a1", "two");
    let other = r.ok("a2", "elsewhere");
    r.jobs.tick();
    let started: Vec<String> = r.fake.w().starts.iter().map(|s| s.0.clone()).collect();
    assert_eq!(started.len(), 2, "one per agent: {started:?}");
    assert!(started.contains(&first) && started.contains(&other));
    assert_eq!(r.state("a1", &first), "running");
    assert_eq!(r.state("a1", &second), "queued");
    let (prompt, timeout, flags) = {
        let w = r.fake.w();
        let s = w.starts.iter().find(|s| s.0 == first).cloned();
        s.map(|s| (s.1, s.2, s.3)).unwrap_or_default()
    };
    assert_eq!((prompt.as_slice(), timeout), (&b"one"[..], 1800));
    assert_eq!(flags, FLAGS);
    // Running: the result is not there yet; the stream is read live.
    let out = |job: &str, q: &str| r.json(Route::Output(agent("a1"), job_id(job)), q);
    assert_eq!(out(&first, "").0, 409);
    assert_eq!(out(&first, "part=stream").1, "{\"type\":\"result\"}\n");
    assert_eq!(out(&first, "part=x").0, 400);
    r.tick();
    assert_eq!(r.state("a1", &second), "queued", "still one at a time");
    r.fake.set(&first, Probe::Exited(Some(0)));
    r.tick();
    let done = r.get("a1", &first);
    assert_eq!(field(&done, "state"), "succeeded");
    assert_eq!(field(&done, "exit_code"), "0");
    assert_ne!(field(&done, "finished_ms"), "null");
    assert_eq!(r.state("a1", &second), "queued", "started on the next tick");
    r.jobs.tick();
    assert_eq!(r.state("a1", &second), "running");
    // Kept on the host: readable however the VM is now.
    r.fake.w().start_fails = true;
    assert_eq!(out(&first, "").1, format!("result of {first} <script>"));
    assert_eq!(out(&second, "").0, 409);
    let list = r.json(Route::List(agent("a1")), "").1;
    assert!(
        list.find(&second) < list.find(&first),
        "newest first: {list}"
    );
    assert_eq!(r.json(Route::List(agent("zz")), "").0, 404);
}

#[test]
fn history_notes_carry_ids_and_states_only() {
    let r = rig();
    let id = r.ok("a1", "SECRET-PROMPT");
    r.jobs.tick();
    r.fake.set(&id, Probe::Exited(Some(3)));
    r.tick();
    let notes = r.notes.lock().map(|v| v.clone()).unwrap_or_default();
    assert_eq!(
        notes,
        [
            format!("a1: job {id} queued"),
            format!("a1: job {id} started"),
            format!("a1: job {id} failed (exit 3)"),
        ]
    );
}

#[test]
fn timeout_kills_and_marks_timed_out() {
    let r = rig();
    let id = r.ok("a1", "sleep");
    let (_, body) = r.submit("a1", r#"{"prompt":"x","timeout_secs":20}"#);
    let short = field(&body, "job_id");
    r.jobs.tick();
    r.fake.set(&id, Probe::Exited(Some(0)));
    r.tick();
    r.jobs.tick();
    assert_eq!(r.state("a1", &short), "running");
    r.advance(19_000);
    r.jobs.tick();
    assert_eq!(r.state("a1", &short), "running");
    r.advance(2_000);
    r.jobs.tick();
    let j = r.get("a1", &short);
    assert_eq!(field(&j, "state"), "timed_out");
    assert_eq!(field(&j, "reason"), "killed after 20 s");
    assert_eq!(r.fake.w().kills, std::slice::from_ref(&short));
    // A kill that fails leaves the job running, to be retried.
    let id3 = r.ok("a1", "z");
    r.jobs.tick();
    r.fake.w().kill_fails = true;
    r.advance(1_800_001);
    r.jobs.tick();
    assert_eq!(r.state("a1", &id3), "running");
    r.fake.w().kill_fails = false;
    r.jobs.tick();
    assert_eq!(r.state("a1", &id3), "timed_out");
}

#[test]
fn cancel_queued_running_and_finished() {
    let r = rig();
    let running = r.ok("a1", "a");
    let queued = r.ok("a1", "b");
    r.jobs.tick();
    let cancel = |job: &str| r.json(Route::Cancel(agent("a1"), job_id(job)), "");
    let (status, body) = cancel(&queued);
    assert_eq!((status, field(&body, "state")), (200, "cancelled".into()));
    assert!(
        r.fake.w().kills.is_empty(),
        "a queued job has nothing to kill"
    );
    r.fake.w().kill_fails = true;
    assert_eq!(cancel(&running).0, 500);
    assert_eq!(r.state("a1", &running), "running");
    r.fake.w().kill_fails = false;
    assert_eq!(field(&cancel(&running).1, "state"), "cancelled");
    assert_eq!(r.fake.w().kills, std::slice::from_ref(&running));
    assert_eq!(cancel(&running).0, 409);
    assert_eq!(cancel("0000000000000000").0, 404);
}

#[test]
fn vanished_runner_bad_exit_code_and_failed_start_fail_the_job() {
    let r = rig();
    let gone = r.ok("a1", "a");
    r.jobs.tick();
    r.fake.set(&gone, Probe::Gone);
    r.tick();
    let j = r.get("a1", &gone);
    assert_eq!(field(&j, "state"), "failed");
    assert_eq!(field(&j, "reason"), "runner ended without an exit code");
    let garbled = r.ok("a1", "b");
    r.jobs.tick();
    r.fake.set(&garbled, Probe::Exited(None));
    r.tick();
    assert_eq!(
        field(&r.get("a1", &garbled), "reason"),
        "unreadable exit code"
    );
    r.fake.w().start_fails = true;
    let unstarted = r.ok("a1", "c");
    r.jobs.tick();
    let j = r.get("a1", &unstarted);
    assert_eq!(field(&j, "state"), "failed");
    assert_eq!(field(&j, "reason"), "start failed: no VM");
}

#[test]
fn queue_is_bounded_per_agent() {
    let r = rig();
    let _running = r.ok("a1", "first");
    r.jobs.tick();
    for i in 0..jobs::MAX_QUEUE {
        r.ok("a1", &format!("q{i}"));
    }
    let (status, body) = r.submit("a1", r#"{"prompt":"one too many"}"#);
    assert_eq!(status, 429, "{body}");
    r.ok("a2", "other agents are unaffected");
}

#[test]
fn idempotency_key_returns_the_existing_job() {
    let r = rig();
    let body = r#"{"prompt":"p","idempotency_key":"ci:42"}"#;
    let (s1, b1) = r.submit("a1", body);
    let (s2, b2) = r.submit("a1", r#"{"prompt":"different","idempotency_key":"ci:42"}"#);
    assert_eq!((s1, s2), (202, 200));
    assert_eq!(field(&b1, "job_id"), field(&b2, "job_id"));
    assert_eq!(field(&b2, "idempotency_key"), "ci:42");
    assert_eq!(
        r.json(Route::List(agent("a1")), "")
            .1
            .matches("job_id")
            .count(),
        1
    );
    let (s3, b3) = r.submit("a2", body);
    assert_eq!(s3, 202, "keys are per agent");
    assert_ne!(field(&b3, "job_id"), field(&b1, "job_id"));
    let (s4, _) = r.submit("a1", r#"{"prompt":"p","idempotency_key":"ci:43"}"#);
    assert_eq!(s4, 202);
}

#[test]
fn stopped_agent_interrupts_running_and_cancels_queued() {
    let r = rig();
    let running = r.ok("a1", "a");
    let queued = r.ok("a1", "b");
    let other = r.ok("a2", "c");
    r.jobs.tick();
    assert!(r.jobs.busy(&agent("a1")));
    r.jobs.interrupt(&agent("a1"));
    assert_eq!(
        r.fake.w().kills,
        std::slice::from_ref(&running),
        "killed before its VM goes"
    );
    assert_eq!(r.state("a1", &running), "interrupted");
    assert_eq!(r.state("a1", &queued), "cancelled");
    assert!(!r.jobs.busy(&agent("a1")));
    // Startup reconcile: a2's VM is gone (host reboot).
    let _ = r.store.set_state(&agent("a2"), State::Paused);
    r.jobs.reconcile();
    let j = r.get("a2", &other);
    assert_eq!(field(&j, "state"), "interrupted");
    assert_eq!(field(&j, "reason"), "agent is not running");
    assert_eq!(
        r.fake.w().kills.len(),
        1,
        "no kill for an agent without a VM"
    );
}

#[test]
fn restart_reattaches_without_starting_again() {
    let r = rig();
    let id = r.ok("a1", "long");
    let queued = r.ok("a1", "next");
    r.jobs.tick();
    let mut r = r;
    let root = std::mem::take(&mut r.root);
    let fake = r.fake.clone();
    // The gateway goes away (its registry lock with it); the registry is reopened from disk.
    drop(r);
    let store = Arc::new(EngineStore::open(&root.join("data")).unwrap_or_else(|e| panic!("{e}")));
    let r = with_store(store, fake, root);
    r.jobs.reconcile();
    r.tick();
    assert_eq!(r.state("a1", &id), "running");
    assert_eq!(r.state("a1", &queued), "queued");
    assert_eq!(r.fake.w().starts.len(), 1, "never started again");
    assert!(r.fake.w().statuses >= 1, "polled instead");
    r.fake.set(&id, Probe::Exited(Some(0)));
    r.tick();
    assert_eq!(r.state("a1", &id), "succeeded");
    r.jobs.tick();
    assert_eq!(r.fake.w().starts.len(), 2);
}

#[test]
fn retention_keeps_the_newest_finished_jobs() {
    let r = rig();
    let mut ids = Vec::new();
    for i in 0..jobs::KEEP + 3 {
        let id = r.ok("a1", &format!("j{i}"));
        r.advance(10);
        r.jobs.tick();
        r.fake.set(&id, Probe::Exited(Some(0)));
        r.tick();
        assert_eq!(r.state("a1", &id), "succeeded");
        ids.push(id);
    }
    let kept = r.store.jobs(&agent("a1")).map(|v| v.len()).unwrap_or(0);
    assert_eq!(kept, jobs::KEEP);
    for old in &ids[..3] {
        assert_eq!(r.json(Route::Get(agent("a1"), job_id(old)), "").0, 404);
        assert!(!r.root.join("jobs/a1").join(old).exists());
    }
    assert!(r
        .root
        .join("jobs/a1")
        .join(&ids[3])
        .join("result.txt")
        .is_file());
}

#[test]
fn long_poll_returns_on_change_and_is_bounded() {
    let r = rig();
    let id = r.ok("a1", "a");
    assert_eq!(
        r.json(Route::Get(agent("a1"), job_id(&id)), "wait=31").0,
        400
    );
    assert_eq!(
        r.json(Route::Get(agent("a1"), job_id(&id)), "wait=x").0,
        400
    );
    let jobs = Arc::clone(&r.jobs);
    let t = Instant::now();
    let waiter = {
        let id = id.clone();
        std::thread::spawn(move || {
            match jobs.handle(Route::Get(agent("a1"), job_id(&id)), "wait=10") {
                Answer::Json(reply) => reply.body,
                Answer::Output(_) => String::new(),
            }
        })
    };
    std::thread::sleep(Duration::from_millis(200));
    r.jobs.tick();
    let body = waiter.join().unwrap_or_default();
    assert_eq!(field(&body, "state"), "running");
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    // Unchanged: returns after the wait with the same state.
    let t = Instant::now();
    let (_, body) = r.json(Route::Get(agent("a1"), job_id(&id)), "wait=1");
    assert_eq!(field(&body, "state"), "running");
    assert!(t.elapsed() >= Duration::from_secs(1));
    // Too many waiters at once.
    let handles: Vec<_> = (0..jobs::MAX_WAITERS)
        .map(|_| {
            let (jobs, id) = (Arc::clone(&r.jobs), id.clone());
            std::thread::spawn(move || {
                let _ = jobs.handle(Route::Get(agent("a1"), job_id(&id)), "wait=3");
            })
        })
        .collect();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        r.json(Route::Get(agent("a1"), job_id(&id)), "wait=3").0,
        429
    );
    for h in handles {
        let _ = h.join();
    }
}

// ---- over HTTP -------------------------------------------------------------------------------

struct Rt;
impl Runtime for Rt {
    fn create(&self, _: &AgentId) -> Result<(), PortError> {
        Ok(())
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

struct NoTerm;
impl ac_gateway::viewer::Terminal for NoTerm {
    fn prepare(&self, _: &AgentId) -> Result<(), String> {
        Err("none".into())
    }
    fn attach_view(&self, _: &AgentId) -> Result<ac_gateway::viewer::ViewClient, String> {
        Err("none".into())
    }
    fn attach_writer(&self, _: &AgentId) -> Result<ac_gateway::viewer::Writer, String> {
        Err("none".into())
    }
    fn scrollback(&self, _: &AgentId) -> Result<Vec<String>, String> {
        Err("none".into())
    }
    fn capture(&self, _: &AgentId, _: ac_viewer::windows::Target) -> Result<Vec<String>, String> {
        Err("none".into())
    }
    fn detach(&self, _: &AgentId, _: &str) -> Result<(), String> {
        Ok(())
    }
    fn probe(&self, _: &AgentId) -> bool {
        true
    }
    fn kill_clients(&self, _: &AgentId, _: bool) -> Result<(), String> {
        Ok(())
    }
}

fn legacy() -> Keys {
    let mut t = Tokens::default();
    let _ = t.add(Token::admin(tokens::LEGACY, TOKEN));
    Keys::new(t)
}

fn serve(r: &Rig) -> SocketAddr {
    serve_with(r, legacy())
}

fn serve_with(r: &Rig, keys: Keys) -> SocketAddr {
    let fleet = Fleet::new(Rt, Arc::clone(&r.store));
    let api = Api::new(fleet).with_jobs(r.jobs.clone());
    let hub = Hub::new(NoTerm, r.root.join("history"));
    let app = App::new(api, hub, keys).with_jobs(Arc::clone(&r.jobs));
    let server = Server::bind_shared("127.0.0.1:0".parse().unwrap_or_else(|e| panic!("{e}")), app)
        .unwrap_or_else(|e| panic!("bind: {e}"));
    let addr = server.local_addr().unwrap_or_else(|e| panic!("{e}"));
    std::thread::spawn(move || server.run());
    addr
}

/// (status, headers, body).
fn request(addr: SocketAddr, method: &str, path: &str, body: &str) -> (u16, String, String) {
    request_as(addr, TOKEN, method, path, body)
}

fn request_as(
    addr: SocketAddr,
    token: &str,
    method: &str,
    path: &str,
    body: &str,
) -> (u16, String, String) {
    let mut s = TcpStream::connect(addr).unwrap_or_else(|e| panic!("{e}"));
    let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\n\
         Connection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut raw = String::new();
    let _ = s.read_to_string(&mut raw);
    let status = raw.get(9..12).and_then(|c| c.parse().ok()).unwrap_or(0);
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or_default();
    (status, head.to_lowercase(), body.to_owned())
}

#[test]
fn routes_over_http() {
    let r = rig();
    let addr = serve(&r);
    let base = "/api/v1/agents/a1/jobs";
    let (status, _, body) = request(addr, "POST", base, r#"{"prompt":"hi"}"#);
    assert_eq!(status, 202, "{body}");
    let id = field(&body, "job_id");
    for (method, path, want) in [
        ("GET", base.to_owned(), 200),
        ("GET", format!("{base}/{id}"), 200),
        ("GET", format!("{base}/{id}?wait=0"), 200),
        ("GET", format!("{base}/0000000000000000"), 404),
        ("GET", format!("{base}/..%2F..%2Fetc"), 400),
        ("GET", format!("{base}/A1"), 400),
        ("GET", format!("{base}/x;rm"), 400),
        ("GET", format!("{base}/{}", "a".repeat(33)), 400),
        ("GET", "/api/v1/agents/..%2Fa/jobs".to_owned(), 400),
        ("GET", "/api/v1/agents/BAD/jobs".to_owned(), 400),
        ("PUT", base.to_owned(), 405),
        ("DELETE", format!("{base}/{id}"), 405),
        ("GET", format!("{base}/{id}/cancel"), 405),
        ("POST", format!("{base}/{id}/output"), 405),
        ("GET", format!("{base}/{id}/explode"), 404),
        ("GET", "/api/v1/agents/zz/jobs".to_owned(), 404),
        ("POST", "/api/v1/agents/p1/jobs".to_owned(), 400),
    ] {
        let (status, _, body) = request(addr, method, &path, "");
        assert_eq!(status, want, "{method} {path}: {body}");
    }
    assert_eq!(
        request(addr, "POST", "/api/v1/agents/p1/jobs", r#"{"prompt":"x"}"#).0,
        409
    );
    // Without the token nothing is answered.
    let mut s = TcpStream::connect(addr).unwrap_or_else(|e| panic!("{e}"));
    let _ = write!(
        s,
        "GET {base} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    );
    let mut raw = String::new();
    let _ = s.read_to_string(&mut raw);
    assert!(raw.starts_with("HTTP/1.1 401"), "{raw}");
    // A busy agent is not paused or destroyed unless forced, which interrupts its job.
    r.jobs.tick();
    assert_eq!(request(addr, "POST", "/api/v1/agents/a1/pause", "").0, 409);
    assert_eq!(request(addr, "DELETE", "/api/v1/agents/a1", "").0, 409);
    assert_eq!(
        request(addr, "POST", "/api/v1/agents/a1/pause?force=maybe", "").0,
        400
    );
    let (status, _, body) = request(addr, "POST", "/api/v1/agents/a1/pause?force=true", "");
    assert_eq!(
        (status, body.as_str()),
        (200, r#"{"id":"a1","state":"paused"}"#)
    );
    assert_eq!(r.state("a1", &id), "interrupted");
    assert_eq!(request(addr, "POST", "/api/v1/agents/a1/resume", "").0, 200);
    // Output is plain text that a browser will not sniff or render.
    let (status, head, body) = request(addr, "GET", &format!("{base}/{id}/output?part=stream"), "");
    assert_eq!(status, 200);
    assert_eq!(body, "", "an interrupted job was never collected");
    for h in [
        "content-type: text/plain; charset=utf-8",
        "x-content-type-options: nosniff",
        "content-security-policy: default-src 'none'; sandbox",
        "cache-control: no-store",
    ] {
        assert!(head.contains(h), "{h} missing: {head}");
    }
}

/// Tokens made as `ac-gateway token create` makes them, plus the legacy one.
struct Scoped {
    file: TokenFile,
    keys: Keys,
    jobs_a1: String,
    viewer: String,
    read_a1: String,
}

fn scoped(root: &Path) -> Scoped {
    use std::os::unix::fs::MetadataExt;
    let _ = std::fs::create_dir_all(root);
    let me = std::fs::metadata("/proc/self").map_or(0, |m| m.uid());
    let file = TokenFile::new(root.join("tokens"), me);
    let make = |name: &str, scopes: &str, agents: &str| {
        let scopes = Scopes::parse(scopes).unwrap_or_else(|e| panic!("{e}"));
        let agents = Agents::parse(agents).unwrap_or_else(|e| panic!("{e}"));
        file.create(name, scopes, agents, 1)
            .unwrap_or_else(|e| panic!("{e}"))
    };
    let jobs_a1 = make("jobs-a1", "jobs:read,jobs:write", "a1");
    let viewer = make("watch", "viewer:read", "*");
    let read_a1 = make("read-a1", "agents:read", "a1");
    let keys = Keys::new(Tokens::default());
    reload(&file, &keys);
    Scoped {
        file,
        keys,
        jobs_a1,
        viewer,
        read_a1,
    }
}

/// What the gateway's watcher does when the file changes.
fn reload(file: &TokenFile, keys: &Keys) {
    let mut t = file.list().unwrap_or_else(|e| panic!("{e}"));
    let _ = t.add(Token::admin(tokens::LEGACY, TOKEN));
    keys.set(t);
}

#[test]
fn scoped_tokens_over_http() {
    let r = rig();
    let t = scoped(&r.root);
    let addr = serve_with(&r, t.keys.clone());
    let status =
        |token: &str, method: &str, path: &str| request_as(addr, token, method, path, "").0;
    let base = "/api/v1/agents/a1";
    let term = format!("{base}/term/0011223344556677");

    // A jobs token for a1: its jobs, nothing else.
    let (code, _, body) = request_as(
        addr,
        &t.jobs_a1,
        "POST",
        &format!("{base}/jobs"),
        r#"{"prompt":"hi"}"#,
    );
    assert_eq!(code, 202, "{body}");
    let job = field(&body, "job_id");
    assert_eq!(
        status(&t.jobs_a1, "GET", &format!("{base}/jobs/{job}")),
        200
    );
    assert_eq!(status(&t.jobs_a1, "GET", &format!("{base}/jobs")), 200);
    for (method, path) in [
        ("POST", format!("{base}/pause")),
        ("POST", format!("{base}/pause?force=true")),
        ("POST", format!("{base}/resume")),
        ("POST", base.to_owned()),
        ("DELETE", base.to_owned()),
        ("DELETE", format!("{base}?purge=true&force=true")),
        ("GET", base.to_owned()),
        ("GET", "/api/v1/agents".to_owned()),
        ("GET", format!("{base}/term")),
        ("GET", format!("{base}/history")),
        ("POST", format!("{term}/takeover")),
        ("POST", format!("{term}/takeover?force=true")),
        ("POST", format!("{term}/keys")),
    ] {
        assert_eq!(
            status(&t.jobs_a1, method, &path),
            403,
            "jobs token: {method} {path}"
        );
    }
    // Other agents do not exist for it, whether they exist or not. (A wrong method is still 405:
    // that depends on the path's shape alone.)
    for (method, path) in [
        ("GET", "/api/v1/agents/a2/jobs"),
        ("POST", "/api/v1/agents/a2/jobs"),
        ("POST", "/api/v1/agents/a2/pause"),
        ("GET", "/api/v1/agents/a2"),
        ("DELETE", "/api/v1/agents/a2"),
        ("GET", "/api/v1/agents/zz/jobs"),
        ("GET", "/api/v1/agents/..%2Fa1/jobs"),
        ("GET", "/api/v1/agents/a2/term"),
    ] {
        assert_eq!(status(&t.jobs_a1, method, path), 404, "{method} {path}");
    }
    let (code, _, body) = request_as(addr, &t.jobs_a1, "GET", "/api/v1/whoami", "");
    assert_eq!(
        (code, body.as_str()),
        (
            200,
            r#"{"name":"jobs-a1","scopes":["jobs:read","jobs:write"],"agents":["a1"]}"#
        )
    );
    assert!(!body.contains(&t.jobs_a1));

    // A viewer:read token watches and reads history, never types or takes over.
    assert_eq!(status(&t.viewer, "GET", &format!("{base}/history")), 200);
    for action in [
        "takeover",
        "takeover?force=true",
        "release",
        "keys",
        "text",
        "paste",
        "resize?cols=80&rows=24",
    ] {
        assert_eq!(
            status(&t.viewer, "POST", &format!("{term}/{action}")),
            403,
            "{action}"
        );
    }
    assert_eq!(status(&t.viewer, "POST", &format!("{base}/jobs")), 403);
    let (_, _, body) = request_as(addr, &t.viewer, "GET", "/api/v1/whoami", "");
    assert_eq!(
        body,
        r#"{"name":"watch","scopes":["viewer:read"],"agents":"*"}"#
    );

    // The list shows a restricted token only its agents.
    let (code, _, body) = request_as(addr, &t.read_a1, "GET", "/api/v1/agents", "");
    assert_eq!(
        (code, body.as_str()),
        (200, r#"{"agents":[{"id":"a1","state":"running"}]}"#)
    );
    assert_eq!(status(&t.read_a1, "GET", "/api/v1/agents/a2"), 404);

    // The legacy token still does everything; audit lines name the token, never the secret.
    assert_eq!(status(TOKEN, "POST", "/api/v1/agents/a2/pause"), 200);
    let (_, _, history) = request(addr, "GET", &format!("{base}/history"), "");
    assert!(
        history.contains(&format!(
            "audit token=jobs-a1 action=job-submit job={job} status=202"
        )),
        "{history}"
    );
    let (_, _, history) = request(addr, "GET", "/api/v1/agents/a2/history", "");
    assert!(
        history.contains("audit token=legacy action=pause status=200"),
        "{history}"
    );
    assert!(!history.contains(TOKEN) && !history.contains(&t.jobs_a1));
    let refused = request_as(addr, &t.jobs_a1, "GET", &format!("{base}/history"), "");
    assert_eq!(
        refused.0, 403,
        "denied calls are not audited in the agent's history"
    );

    // Revoked and reloaded: the token stops working; the others do not.
    assert!(t.file.revoke("jobs-a1").is_ok());
    reload(&t.file, &t.keys);
    assert_eq!(status(&t.jobs_a1, "GET", &format!("{base}/jobs")), 401);
    assert_eq!(status(&t.viewer, "GET", &format!("{base}/history")), 200);
}

#[test]
fn failed_auth_is_limited_without_locking_out_valid_tokens() {
    let r = rig();
    let addr = serve(&r);
    let codes: Vec<u16> = (0..25)
        .map(|i| request_as(addr, &format!("guess{i}"), "GET", "/api/v1/agents", "").0)
        .collect();
    assert_eq!(codes[..19], [401; 19]);
    assert!(codes[19..].iter().all(|c| *c == 429), "{codes:?}");
    assert_eq!(request(addr, "GET", "/api/v1/agents", "").0, 200);
    for hostile in [
        "Bearer",
        "Basic czNjcmV0",
        "Bearer s3cret\u{0}",
        "Bearer s3cret s3cret",
    ] {
        let mut s = TcpStream::connect(addr).unwrap_or_else(|e| panic!("{e}"));
        let _ = write!(
            s,
            "GET /api/v1/agents HTTP/1.1\r\nHost: {addr}\r\nAuthorization: {hostile}\r\nConnection: close\r\n\r\n"
        );
        let mut raw = String::new();
        let _ = s.read_to_string(&mut raw);
        let code: u16 = raw.get(9..12).and_then(|c| c.parse().ok()).unwrap_or(0);
        assert!(code == 429 || code == 400, "{hostile:?}: {raw}");
    }
}

// ---- the ctr runner, against a mock spawner ---------------------------------------------------

fn ctr(
    root: &Path,
    stdout: &str,
) -> (
    CtrRuntime<MockSpawner>,
    Arc<Mutex<Vec<platform_mock::process::SpawnRecord>>>,
) {
    let spawner = MockSpawner::new().script_with_output("timeout", ExitStatus::Code(0), stdout);
    let log = Arc::clone(&spawner.spawned);
    let cfg = CtrConfig {
        ctr_bin: "ctr".into(),
        namespace: "agents".into(),
        image: "agent:latest".into(),
        workspace_root: root.join("workspaces"),
        workspace_owner: None,
        network: None,
        vc_root: root.join("vc"),
        task_root: root.join("task"),
        command_timeout: None,
        trace: false,
    };
    let _ = std::fs::create_dir_all(root.join("workspaces/a1"));
    (CtrRuntime::new(spawner, cfg), log)
}

fn argvs(log: &Mutex<Vec<platform_mock::process::SpawnRecord>>) -> Vec<Vec<String>> {
    let log = log.lock().unwrap_or_else(|e| e.into_inner());
    log.iter()
        .map(|r| {
            std::iter::once(&r.program)
                .chain(&r.argv)
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        })
        .collect()
}

/// JSON string literal for `s`.
fn json_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Prompts built from every hostile fragment, in many orders and repeats (xorshift, fixed seed).
fn hostile_prompts() -> Vec<String> {
    const BITS: [&str; 16] = [
        "\n",
        "\r\n",
        "'",
        "\"",
        "$(touch /tmp/pwned)",
        "`id`",
        "\0",
        ";",
        "; rm -rf / #",
        "\\",
        "${HOME}",
        "|| true",
        "&&",
        "$1",
        "--dangerously-skip-permissions",
        "é漢",
    ];
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut out: Vec<String> = BITS.iter().map(|b| format!("P{b}Q")).collect();
    out.push(BITS.concat());
    for n in 0..200 {
        let mut p = format!("MARK{n}:");
        for _ in 0..12 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            p.push_str(BITS[(x % BITS.len() as u64) as usize]);
        }
        out.push(p);
    }
    out
}

#[test]
fn ctr_start_is_argv_only_and_the_prompt_arrives_byte_for_byte() {
    let root = tmp();
    let (rt, log) = ctr(&root, "");
    let flags: Vec<String> = FLAGS.map(str::to_owned).to_vec();
    for (n, prompt) in hostile_prompts().iter().enumerate() {
        let job = JobId::from_random([n as u8, 1, 2, 3, 4, 5, 6, 7]);
        let r = rt.start(&agent("a1"), &job, prompt.as_bytes(), 60, &flags);
        assert!(r.is_ok(), "{r:?}");
        let file = root.join(format!("workspaces/a1/.job-{job}.prompt"));
        assert_eq!(
            std::fs::read(&file).ok().as_deref(),
            Some(prompt.as_bytes())
        );
        let argv = argvs(&log).pop().unwrap_or_default();
        // timeout -s TERM -k 3 30 ctr -n agents task exec --exec-id <x> a1 sh -c <START>
        // ac-job-start <RUN> <job> <secs> <flags...>: constants, the job id and the operator's
        // flags only.
        assert_eq!(argv[..6], ["timeout", "-s", "TERM", "-k", "3", "30"]);
        assert_eq!(argv[6..11], ["ctr", "-n", "agents", "task", "exec"]);
        assert_eq!(argv[13..16], ["a1", "sh", "-c"]);
        assert_eq!(argv[17], "ac-job-start");
        assert_eq!(argv[19..21], [job.to_string(), "360".to_owned()]);
        assert_eq!(argv[21..], flags[..]);
        let held = log.lock().map(|l| {
            l.iter()
                .all(|r| matches!(r.stdin, platform_mock::process::StdioKind::Pipe))
        });
        assert_eq!(held.ok(), Some(true), "every exec's stdin is held open");
        for a in &argv {
            assert!(!a.contains(prompt.as_str()), "prompt in argv");
            assert!(
                !a.contains("MARK") && !a.contains("pwned"),
                "prompt fragment in {a:?}"
            );
        }
    }
    // An existing name, a symlink included, is never followed or overwritten.
    let job = JobId::from_random([9; 8]);
    let victim = root.join("victim");
    let _ = std::fs::write(&victim, "keep");
    let link = root.join(format!("workspaces/a1/.job-{job}.prompt"));
    let _ = std::os::unix::fs::symlink(&victim, &link);
    assert!(rt.start(&agent("a1"), &job, b"evil", 60, &flags).is_err());
    assert_eq!(
        std::fs::read_to_string(&victim).ok().as_deref(),
        Some("keep")
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn hostile_prompt_through_the_api_reaches_the_file_intact() {
    let root = tmp();
    // Every exec "prints" this: start ignores it, and it lets cancel kill the job.
    let (rt, log) = ctr(&root, "killed\n");
    let store = Arc::new(EngineStore::open(&root.join("data")).unwrap_or_else(|e| panic!("{e}")));
    let _ = store.set_state(&agent("a1"), State::Running);
    let jobs = jobs_over(&store, rt, &root);
    for prompt in hostile_prompts().iter().take(40) {
        let body = format!(r#"{{"prompt":{}}}"#, json_string(prompt));
        let reply = jobs.submit(&agent("a1"), Some(State::Running), body.as_bytes());
        assert_eq!(reply.status, 202, "{}", reply.body);
        let id = field(&reply.body, "job_id");
        jobs.tick();
        let file = root.join(format!("workspaces/a1/.job-{id}.prompt"));
        assert_eq!(
            std::fs::read(&file).ok().as_deref(),
            Some(prompt.as_bytes())
        );
        assert!(argvs(&log).iter().flatten().all(|a| !a.contains("MARK")));
        // End it so the next one starts.
        let _ = jobs.handle(Route::Cancel(agent("a1"), job_id(&id)), "");
    }
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn ctr_status_kill_and_read_parse_strictly() {
    let root = tmp();
    let job = JobId::from_random([1; 8]);
    let a1 = agent("a1");
    for (out, want) in [
        ("running\n", Ok(Probe::Running)),
        ("exit 0\n", Ok(Probe::Exited(Some(0)))),
        ("exit 137\n", Ok(Probe::Exited(Some(137)))),
        ("exit \n", Ok(Probe::Exited(None))),
        ("gone\n", Ok(Probe::Gone)),
        ("pwned\n", Err(())),
    ] {
        let (rt, log) = ctr(&root, out);
        assert_eq!(rt.status(&a1, &job).map_err(drop), want, "{out:?}");
        let argv = argvs(&log).pop().unwrap_or_default();
        assert_eq!(
            argv[..6],
            ["timeout", "-s", "TERM", "-k", "3", "10"],
            "a 10 s probe"
        );
        assert_eq!(argv.last(), Some(&job.to_string()));
    }
    for (out, ok) in [("killed\n", true), ("gone\n", true), ("alive: 12\n", false)] {
        let (rt, _) = ctr(&root, out);
        assert_eq!(rt.kill(&a1, &job).is_ok(), ok, "{out:?}");
    }
    let (rt, log) = ctr(&root, "x");
    let _ = rt.read(&a1, &job, Part::Result);
    let _ = rt.read(&a1, &job, Part::Stream);
    let all = argvs(&log);
    let tails: Vec<&[String]> = all.iter().map(|a| &a[14..]).collect();
    assert_eq!(
        tails[0],
        [
            "head",
            "-c",
            "262144",
            "--",
            &format!("/workspace/.jobs/{job}/result.txt")
        ]
    );
    assert_eq!(
        tails[1],
        [
            "tail",
            "-c",
            "1048576",
            "--",
            &format!("/workspace/.jobs/{job}/stream.jsonl")
        ]
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn permission_flags() {
    let tools: Vec<String> = jobs::DEFAULT_TOOLS.split(',').map(str::to_owned).collect();
    let flags = jobs::claude_flags(None, None, false).unwrap_or_default();
    assert_eq!(
        flags[..3],
        ["--permission-mode", "acceptEdits", "--allowedTools"]
    );
    assert_eq!(flags[3..], tools[..]);
    assert!(flags.iter().all(|f| f != "--dangerously-skip-permissions"));
    assert_eq!(
        jobs::claude_flags(Some("plan"), Some(" Read , Bash(git log:*) ,"), false).ok(),
        Some(
            [
                "--permission-mode",
                "plan",
                "--allowedTools",
                "Read",
                "Bash(git log:*)"
            ]
            .map(str::to_owned)
            .to_vec()
        )
    );
    assert!(jobs::claude_flags(Some("bypassPermissions"), None, false).is_err());
    assert!(jobs::claude_flags(Some("yolo"), None, false).is_err());
    assert!(jobs::claude_flags(None, Some("Read,--dangerously-skip-permissions"), false).is_err());
    assert!(jobs::claude_flags(None, Some("Read\nWrite"), false).is_err());
    assert_eq!(
        jobs::claude_flags(Some("plan"), None, true).ok(),
        Some(vec!["--dangerously-skip-permissions".to_owned()])
    );
}

#[test]
fn running_job_is_probed_rarely_until_it_says_it_finished() {
    let r = rig();
    let id = r.ok("a1", "long");
    r.jobs.tick();
    r.jobs.tick();
    let first = r.fake.w().statuses;
    assert_eq!(first, 1, "probed once on the tick after it started");
    for _ in 0..29 {
        r.advance(1_000);
        r.jobs.tick();
    }
    assert_eq!(r.fake.w().statuses, first, "no exec for 29 s");
    r.advance(1_000);
    r.jobs.tick();
    assert_eq!(r.fake.w().statuses, first + 1, "then one per 30 s");
    // The marker brings the next probe forward, after at least 3 s.
    r.fake.set(&id, Probe::Exited(Some(0)));
    r.advance(1_000);
    r.jobs.tick();
    assert_eq!(r.state("a1", &id), "running");
    r.advance(2_000);
    r.jobs.tick();
    assert_eq!(r.state("a1", &id), "succeeded");
}
