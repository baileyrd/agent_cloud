//! The task API (ADR-010): run a prompt in an agent as a job, with an id, live status and the
//! result, without exec-ing into the VM by hand.
//!
//! Routes (bearer token required), under `/api/v1/agents/{id}/`:
//! - `POST jobs` `{"prompt", "timeout_secs"?, "idempotency_key"?}`: 202 and the job (200 and the
//!   existing job for a known key);
//! - `GET jobs`: the agent's jobs, newest first;
//! - `GET jobs/{job}[?wait=N]`: the job; with `wait` (at most 30 s), once its state changes;
//! - `GET jobs/{job}/output[?part=stream]`: the final message, or the stream-json log, as text;
//! - `POST jobs/{job}/cancel`.
//!
//! The registry ([`JobStore`]) holds state, times, timeout and exit code. What the agent writes
//! (result, log) is untrusted: it is read in the VM through capped execs, kept capped under
//! `<data>/jobs/<agent>/<job>/`, served as `text/plain` with `nosniff`, and never parsed here. The
//! gateway never writes, chowns or reads a path below the workspace root (the agent controls it);
//! the prompt goes in with `O_CREAT|O_EXCL` at the root and the start exec moves it.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ac_ctr::CtrRuntime;
use ac_domain::job;
use ac_domain::{AgentId, Job, JobEvent, JobId, JobState, State};
use ac_ports::{JobStore, SessionStore};
use platform::process::Spawner;
use rusty_json::Value;

use crate::{error, escape, Busy, Reply};

/// Jobs waiting behind the running one, per agent.
pub const MAX_QUEUE: usize = 8;
/// Finished jobs kept per agent (registry records and host copies).
pub const KEEP: usize = 20;
/// Longest `?wait=`.
pub const MAX_WAIT_SECS: u64 = 30;
/// Long-polls at once; more are refused, so they cannot take the lifecycle API's connections.
pub const MAX_WAITERS: usize = 8;
/// What is kept of a job's final message.
pub const RESULT_CAP: usize = 256 * 1024;
/// What is kept of a job's stream log (its end).
pub const STREAM_CAP: usize = 1024 * 1024;
/// How often a running job is polled.
const POLL: Duration = Duration::from_secs(2);
/// Added to the job's timeout for the runner's own `timeout`, a backstop for a stopped gateway.
const BACKSTOP_SECS: u32 = 300;

/// Default `AC_JOB_ALLOWED_TOOLS`: read, search and edit files; `git` without push; `ls`, `cat`;
/// `python3` and `cargo` (both run arbitrary code as the agent user, inside the VM); `sleep`.
pub const DEFAULT_TOOLS: &str = "Read,Glob,Grep,Edit,Write,Bash(git status:*),Bash(git diff:*),\
    Bash(git log:*),Bash(git add:*),Bash(git commit:*),Bash(ls:*),Bash(cat:*),Bash(python3:*),\
    Bash(cargo build:*),Bash(cargo test:*),Bash(cargo check:*),Bash(cargo fmt:*),Bash(sleep:*)";
/// Permission modes a job may use without `AC_JOB_UNSAFE=1`.
const MODES: [&str; 5] = ["acceptEdits", "auto", "dontAsk", "manual", "plan"];

/// Start a job, as the agent user (argv: this, the runner script, the job id, the backstop
/// timeout, then `claude`'s permission flags). Refuses a job whose directory exists, so a job
/// never starts twice. Keeps the newest 5 job directories.
const START: &str = r#"set -eu
run=$1 id=$2
shift 2
cd /workspace
mkdir -p -m 700 .jobs
mkdir -m 700 ".jobs/$id"
mv ".job-$id.prompt" ".jobs/$id/prompt.txt"
ls -1t .jobs | grep -E '^[a-z0-9-]+$' | tail -n +6 | while read -r old; do rm -rf -- ".jobs/$old"; done
tmux has-session -t main 2>/dev/null || tmux new-session -d -s main -c /workspace
tmux new-window -d -t main: -n "job-$id" -c /workspace bash -c "$run" ac-job "$id" "$@""#;

/// The job's window (bash): `claude` reads the prompt on stdin; its log is capped at 16 MiB (a
/// job that passes it is stopped); the terminal shows a digest. `exit_code` is written last.
const RUN: &str = r#"id=$1 secs=$2
shift 2
d=/workspace/.jobs/$id
show='fromjson? | if .type == "assistant" then (.message.content[]? | if .type == "text" then .text + "\n" elif .type == "tool_use" then "> " + .name + " " + (.input | tostring | .[0:160]) + "\n" else empty end) elif .type == "result" then "--- " + (.subtype // "result") + "\n" else empty end'
printf 'job %s started %s\n' "$id" "$(date -u +%FT%TZ)"
timeout -k 10 "$secs" claude -p --output-format stream-json --verbose "$@" < "$d/prompt.txt" \
    | head -c 16777216 | tee "$d/stream.jsonl" | jq -Rrj --unbuffered "$show"
rc=${PIPESTATUS[0]}
jq -Rrj 'fromjson? | select(.type == "result") | .result // empty' "$d/stream.jsonl" \
    | head -c 262144 > "$d/result.txt"
printf '%s\n' "$rc" > "$d/exit_code.tmp"
mv "$d/exit_code.tmp" "$d/exit_code"
printf '\njob %s ended: exit %s\n' "$id" "$rc""#;

/// Whether a job runs: its window's pane is alive (`running`), else its exit code (`exit N`),
/// else `gone`. Liveness first: the runner writes `exit_code` before it exits.
const STATUS: &str = r#"id=$1
dead=$(tmux display -p -t "main:=job-$id" '#{pane_dead}' 2>/dev/null) || dead=
if [ "$dead" = 0 ]; then echo running; exit 0; fi
f=/workspace/.jobs/$id/exit_code
if [ -f "$f" ]; then printf 'exit %s\n' "$(head -c 12 "$f" | tr -dc 0-9-)"; exit 0; fi
echo gone"#;

/// Kill a job's window: its process tree (by parent pid) and the pane's session, stopped first so
/// nothing forks meanwhile, then killed; the window closed. Prints `killed` once every one of them
/// is gone (zombies count as gone), `gone` if there was no window, else fails.
const KILL: &str = r#"t="main:=job-$1"
pid=$(tmux display -p -t "$t" '#{pane_pid}' 2>/dev/null) || pid=
[ -n "$pid" ] || { echo gone; exit 0; }
all=$pid new=$pid
while [ -n "$new" ]; do
    new=$(ps -o pid= --ppid "$(echo $new | tr ' ' ,)" | tr -s ' \n' '  ')
    all="$all $new"
done
all="$all $(ps -o pid= -s "$pid" | tr -s ' \n' '  ')"
kill -STOP $all 2>/dev/null
kill -KILL $all 2>/dev/null
tmux kill-window -t "$t" 2>/dev/null
i=0
while [ "$i" -lt 30 ]; do
    left=
    for p in $all; do
        s=$(ps -o stat= -p "$p" 2>/dev/null) || continue
        case $s in Z*) ;; *) left="$left $p" ;; esac
    done
    [ -z "$left" ] && { echo killed; exit 0; }
    i=$((i + 1))
    sleep 0.1
done
echo "alive:$left"
exit 1"#;

/// What a status probe found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// The job's window is alive.
    Running,
    /// It exited; `None` if its exit code was unreadable.
    Exited(Option<i32>),
    /// Its window is gone and it left no exit code.
    Gone,
}

/// Parse the status script's output.
pub fn parse_probe(out: &[u8]) -> Result<Probe, String> {
    let text = String::from_utf8_lossy(out);
    match text.trim() {
        "running" => Ok(Probe::Running),
        "gone" => Ok(Probe::Gone),
        line => match line.strip_prefix("exit") {
            Some(code) => Ok(Probe::Exited(code.trim().parse().ok())),
            None => Err(format!("unexpected status {line:?}")),
        },
    }
}

/// One of a job's agent-written files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    /// `result.txt`, the final message.
    Result,
    /// `stream.jsonl`, the stream-json log (its end, if long).
    Stream,
}

impl Part {
    fn file(self) -> &'static str {
        match self {
            Self::Result => "result.txt",
            Self::Stream => "stream.jsonl",
        }
    }
}

/// How the gateway reaches a job in its agent. Implemented over `ctr` for [`CtrRuntime`]; tests
/// use a fake.
pub trait JobRunner: Send + Sync + 'static {
    /// Put `prompt` in the agent's workspace and open the job's window; returns once it is open.
    fn start(
        &self,
        agent: &AgentId,
        job: &JobId,
        prompt: &[u8],
        timeout_secs: u32,
        claude: &[String],
    ) -> Result<(), String>;
    /// Whether the job still runs (a 2 s exec).
    fn status(&self, agent: &AgentId, job: &JobId) -> Result<Probe, String>;
    /// Kill the job's processes and close its window; fails unless they are gone.
    fn kill(&self, agent: &AgentId, job: &JobId) -> Result<(), String>;
    /// Up to [`RESULT_CAP`] bytes of the result, or the last [`STREAM_CAP`] of the log.
    fn read(&self, agent: &AgentId, job: &JobId, part: Part) -> Result<Vec<u8>, String>;
}

impl<S: Spawner + Send + Sync + 'static> JobRunner for CtrRuntime<S> {
    fn start(
        &self,
        agent: &AgentId,
        job: &JobId,
        prompt: &[u8],
        timeout_secs: u32,
        claude: &[String],
    ) -> Result<(), String> {
        let cfg = self.config();
        // The workspace root is the bind mount itself, which the agent cannot replace; with
        // O_CREAT|O_EXCL an existing name (a symlink included) fails instead of being followed.
        let path = cfg
            .workspace_root
            .join(agent.as_str())
            .join(format!(".job-{job}.prompt"));
        let written = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .and_then(|mut f| {
                if let Some((uid, gid)) = cfg.workspace_owner {
                    std::os::unix::fs::fchown(&f, Some(uid), Some(gid))?;
                }
                f.write_all(prompt)
            });
        if let Err(e) = written {
            // Unlink never follows a symlink, so this removes at most the name itself.
            let _ = std::fs::remove_file(&path);
            return Err(format!("{}: {e}", path.display()));
        }
        let secs = (timeout_secs + BACKSTOP_SECS).to_string();
        let mut args = vec!["sh", "-c", START, "ac-job-start", RUN, job.as_str(), &secs];
        args.extend(claude.iter().map(String::as_str));
        self.exec_once("job start", agent, &args, Duration::from_secs(30))
            .map(drop)
            .map_err(|e| {
                let _ = std::fs::remove_file(&path);
                e.to_string()
            })
    }

    fn status(&self, agent: &AgentId, job: &JobId) -> Result<Probe, String> {
        let args = ["sh", "-c", STATUS, "ac-job-status", job.as_str()];
        let out = self
            .exec_once("job status", agent, &args, Duration::from_secs(2))
            .map_err(|e| e.to_string())?;
        parse_probe(&out)
    }

    fn kill(&self, agent: &AgentId, job: &JobId) -> Result<(), String> {
        let args = ["sh", "-c", KILL, "ac-job-kill", job.as_str()];
        let out = self
            .exec_once("job kill", agent, &args, Duration::from_secs(15))
            .map_err(|e| e.to_string())?;
        match String::from_utf8_lossy(&out).trim() {
            "killed" | "gone" => Ok(()),
            other => Err(format!("kill: {other}")),
        }
    }

    fn read(&self, agent: &AgentId, job: &JobId, part: Part) -> Result<Vec<u8>, String> {
        let (tool, cap) = match part {
            Part::Result => ("head", RESULT_CAP),
            Part::Stream => ("tail", STREAM_CAP),
        };
        let cap = cap.to_string();
        let path = format!("/workspace/.jobs/{job}/{}", part.file());
        let args = [tool, "-c", &cap, "--", &path];
        self.exec_once("job read", agent, &args, Duration::from_secs(10))
            .map_err(|e| e.to_string())
    }
}

/// `claude`'s permission flags from `AC_JOB_PERMISSION_MODE`, `AC_JOB_ALLOWED_TOOLS` and
/// `AC_JOB_UNSAFE` (ADR-010). `bypassPermissions` needs `unsafe_ok`, which on its own gives
/// `--dangerously-skip-permissions`.
pub fn claude_flags(
    mode: Option<&str>,
    tools: Option<&str>,
    unsafe_ok: bool,
) -> Result<Vec<String>, String> {
    if unsafe_ok {
        return Ok(vec!["--dangerously-skip-permissions".to_owned()]);
    }
    let mode = mode.unwrap_or("acceptEdits");
    if !MODES.contains(&mode) {
        return Err(format!(
            "AC_JOB_PERMISSION_MODE {mode:?}: use one of {} (bypassPermissions needs AC_JOB_UNSAFE=1)",
            MODES.join(", ")
        ));
    }
    let mut flags = vec!["--permission-mode".to_owned(), mode.to_owned()];
    let tools: Vec<&str> = tools
        .unwrap_or(DEFAULT_TOOLS)
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    if let Some(bad) = tools
        .iter()
        .find(|t| t.starts_with('-') || t.chars().any(char::is_control))
    {
        return Err(format!("AC_JOB_ALLOWED_TOOLS: bad entry {bad:?}"));
    }
    if !tools.is_empty() {
        flags.push("--allowedTools".to_owned());
        flags.extend(tools.into_iter().map(str::to_owned));
    }
    Ok(flags)
}

/// What a job route answers.
pub enum Answer {
    /// A JSON reply.
    Json(Reply),
    /// Agent-written bytes: serve as `text/plain` with `nosniff`, never interpret.
    Output(Vec<u8>),
}

/// A job route.
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    /// `POST .../jobs`.
    Submit(AgentId),
    /// `GET .../jobs`.
    List(AgentId),
    /// `GET .../jobs/{job}`.
    Get(AgentId, JobId),
    /// `GET .../jobs/{job}/output`.
    Output(AgentId, JobId),
    /// `POST .../jobs/{job}/cancel`.
    Cancel(AgentId, JobId),
}

/// Parse a job route: `None` if `path` is not one.
pub fn route(method: &str, path: &str) -> Option<Result<Route, Reply>> {
    let rest = path.strip_prefix("/api/v1/agents/")?;
    let parts: Vec<&str> = rest.split('/').collect();
    let (id, tail) = parts.split_first()?;
    if tail.first() != Some(&"jobs") {
        return None;
    }
    let bad = |e: ac_domain::IdError| Some(Err(error(400, &e.to_string())));
    let id = match AgentId::parse(id) {
        Ok(id) => id,
        Err(e) => return bad(e),
    };
    let job = match tail.get(1).map(|j| JobId::parse(j)) {
        Some(Err(e)) => return bad(e),
        Some(Ok(j)) => Some(j),
        None => None,
    };
    let route = match (method, &tail[1..], job) {
        ("POST", [], None) => Route::Submit(id),
        ("GET", [], None) => Route::List(id),
        ("GET", [_], Some(j)) => Route::Get(id, j),
        ("GET", [_, "output"], Some(j)) => Route::Output(id, j),
        ("POST", [_, "cancel"], Some(j)) => Route::Cancel(id, j),
        (_, [] | [_] | [_, "output" | "cancel"], _) => {
            return Some(Err(error(405, "method not allowed")))
        }
        _ => return Some(Err(error(404, "not found"))),
    };
    Some(Ok(route))
}

/// A job request body, validated.
#[derive(Debug, PartialEq, Eq)]
pub struct Request {
    /// The prompt, at most [`job::MAX_PROMPT`] bytes.
    pub prompt: String,
    /// Seconds the job may run.
    pub timeout_secs: u32,
    /// The caller's idempotency key.
    pub key: Option<String>,
}

/// Parse `{"prompt", "timeout_secs"?, "idempotency_key"?}`; unknown members are refused.
pub fn parse_request(body: &[u8]) -> Result<Request, Reply> {
    let bad = |m: &str| error(400, m);
    let text = std::str::from_utf8(body).map_err(|_| bad("body is not UTF-8"))?;
    let value = Value::parse(text).map_err(|e| bad(&format!("bad JSON: {e}")))?;
    let Some(obj) = value.as_object() else {
        return Err(bad("body must be a JSON object"));
    };
    if let Some(k) = obj
        .keys()
        .find(|k| !matches!(k.as_str(), "prompt" | "timeout_secs" | "idempotency_key"))
    {
        return Err(bad(&format!("unknown member {k:?}")));
    }
    let prompt = match value.get("prompt") {
        Some(Value::String(p)) => p.clone(),
        _ => return Err(bad("prompt must be a string")),
    };
    job::check_prompt(&prompt).map_err(|e| {
        let status = if prompt.is_empty() { 400 } else { 413 };
        error(status, &e.to_string())
    })?;
    let timeout = match value.get("timeout_secs") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .ok_or_else(|| bad("timeout_secs must be a positive integer"))?,
        ),
    };
    let timeout_secs = job::timeout_secs(timeout).map_err(|e| bad(&e.to_string()))?;
    let key = match value.get("idempotency_key") {
        None | Some(Value::Null) => None,
        Some(Value::String(k)) => {
            job::check_key(k).map_err(|e| bad(&e.to_string()))?;
            Some(k.clone())
        }
        Some(_) => return Err(bad("idempotency_key must be a string")),
    };
    Ok(Request {
        prompt,
        timeout_secs,
        key,
    })
}

/// A job as JSON.
pub fn render(j: &Job) -> String {
    let opt = |v: Option<String>| v.unwrap_or_else(|| "null".to_owned());
    let text = |v: &Option<String>| opt(v.as_deref().map(|s| format!("\"{}\"", escape(s))));
    format!(
        r#"{{"job_id":"{}","agent":"{}","state":"{}","created_ms":{},"started_ms":{},"finished_ms":{},"timeout_secs":{},"exit_code":{},"idempotency_key":{},"reason":{}}}"#,
        j.id,
        j.agent,
        j.state.name(),
        j.created_ms,
        opt(j.started_ms.map(|v| v.to_string())),
        opt(j.finished_ms.map(|v| v.to_string())),
        j.timeout_secs,
        opt(j.exit_code.map(|v| v.to_string())),
        text(&j.key),
        text(&j.reason),
    )
}

type Notes = Box<dyn Fn(&AgentId, &str) + Send + Sync>;
type Clock = Box<dyn Fn() -> u64 + Send + Sync>;

/// Every job of every agent: the routes, the supervisor and the lifecycle hooks.
pub struct Jobs {
    store: Arc<dyn JobStore + Send + Sync>,
    agents: Arc<dyn SessionStore + Send + Sync>,
    runner: Box<dyn JobRunner>,
    dir: PathBuf,
    claude: Vec<String>,
    notes: Notes,
    clock: Clock,
    /// Serialises read-modify-write of job records; holds when each running job was last polled.
    book: Mutex<HashMap<String, u64>>,
    changed: Condvar,
    waiters: AtomicUsize,
}

impl Jobs {
    /// Jobs recorded in `store`, for agents whose state is in `agents`, run through `runner` with
    /// `claude` permission flags. Prompts and collected output go under `dir` (mode 0700).
    pub fn new(
        store: Arc<dyn JobStore + Send + Sync>,
        agents: Arc<dyn SessionStore + Send + Sync>,
        runner: impl JobRunner,
        dir: PathBuf,
        claude: Vec<String>,
    ) -> Self {
        Self {
            store,
            agents,
            runner: Box::new(runner),
            dir,
            claude,
            notes: Box::new(|_, _| {}),
            clock: Box::new(now_ms),
            book: Mutex::new(HashMap::new()),
            changed: Condvar::new(),
            waiters: AtomicUsize::new(0),
        }
    }

    /// Record state changes with `notes` (the agent's history log).
    pub fn with_notes(mut self, notes: impl Fn(&AgentId, &str) + Send + Sync + 'static) -> Self {
        self.notes = Box::new(notes);
        self
    }

    /// Read the time (ms since the epoch) from `clock`.
    pub fn with_clock(mut self, clock: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        self.clock = Box::new(clock);
        self
    }

    /// Accept a job for `agent`, whose lifecycle state is `state`. The caller holds the lifecycle
    /// lock, so the agent cannot be paused between the check and the queueing.
    pub fn submit(&self, agent: &AgentId, state: Option<State>, body: &[u8]) -> Reply {
        let req = match parse_request(body) {
            Ok(r) => r,
            Err(reply) => return reply,
        };
        match state {
            None => return error(404, &format!("agent {agent} not found")),
            Some(State::Running) => {}
            Some(s) => {
                let s = format!("{s:?}").to_lowercase();
                return error(409, &format!("agent {agent} is {s}; resume it first"));
            }
        }
        let _book = self.lock();
        let jobs = match self.store.jobs(agent) {
            Ok(j) => j,
            Err(e) => return error(500, &e.to_string()),
        };
        if let Some(j) = jobs.iter().find(|j| req.key.is_some() && j.key == req.key) {
            return ok(200, render(j));
        }
        if jobs.iter().filter(|j| j.state == JobState::Queued).count() >= MAX_QUEUE {
            return error(429, &format!("{MAX_QUEUE} jobs already queued for {agent}"));
        }
        // Strictly after every job of this agent, so jobs submitted in the same millisecond still
        // run in order.
        let after = jobs.iter().map(|j| j.created_ms + 1).max().unwrap_or(0);
        let j = Job::new(
            agent.clone(),
            JobId::from_random(random()),
            (self.clock)().max(after),
            req.timeout_secs,
            req.key,
        );
        let saved = self
            .save_prompt(&j, req.prompt.as_bytes())
            .and_then(|()| self.store.put_job(&j).map_err(|e| e.to_string()));
        if let Err(e) = saved {
            return error(500, &e);
        }
        (self.notes)(agent, &format!("job {} queued", j.id));
        self.changed.notify_all();
        ok(202, render(&j))
    }

    /// Answer a job route other than [`Route::Submit`].
    pub fn handle(&self, route: Route, query: &str) -> Answer {
        let reply = match route {
            Route::Submit(id) => error(500, &format!("submit for {id} needs its state")),
            Route::List(id) => self.list(&id),
            Route::Get(id, job) => match param(query, "wait").map(str::parse::<u64>) {
                None => self.get(&id, &job, 0),
                Some(Ok(n)) if n <= MAX_WAIT_SECS => self.get(&id, &job, n),
                Some(_) => error(400, &format!("wait must be 0..={MAX_WAIT_SECS}")),
            },
            Route::Output(id, job) => match param(query, "part") {
                None | Some("result") => return self.output(&id, &job, Part::Result),
                Some("stream") => return self.output(&id, &job, Part::Stream),
                Some(_) => error(400, "part must be result or stream"),
            },
            Route::Cancel(id, job) => self.cancel(&id, &job),
        };
        Answer::Json(reply)
    }

    fn list(&self, agent: &AgentId) -> Reply {
        if let Some(reply) = self.unknown(agent) {
            return reply;
        }
        match self.store.jobs(agent) {
            Ok(mut jobs) => {
                jobs.sort_by_key(|j| std::cmp::Reverse(j.created_ms));
                let items: Vec<String> = jobs.iter().map(render).collect();
                ok(200, format!(r#"{{"jobs":[{}]}}"#, items.join(",")))
            }
            Err(e) => error(500, &e.to_string()),
        }
    }

    fn get(&self, agent: &AgentId, id: &JobId, wait: u64) -> Reply {
        let first = match self.find(agent, id) {
            Ok(j) => j,
            Err(reply) => return reply,
        };
        if wait == 0 || first.state.is_terminal() {
            return ok(200, render(&first));
        }
        if self.waiters.fetch_add(1, Ordering::SeqCst) >= MAX_WAITERS {
            self.waiters.fetch_sub(1, Ordering::SeqCst);
            return error(429, "too many waiting requests");
        }
        let end = Instant::now() + Duration::from_secs(wait);
        let mut book = self.lock();
        let reply = loop {
            match self.find(agent, id) {
                Ok(j) if j.state != first.state => break ok(200, render(&j)),
                Ok(j) => {
                    let left = end.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        break ok(200, render(&j));
                    }
                    book = match self.changed.wait_timeout(book, left) {
                        Ok((b, _)) => b,
                        Err(e) => e.into_inner().0,
                    };
                }
                Err(reply) => break reply,
            }
        };
        drop(book);
        self.waiters.fetch_sub(1, Ordering::SeqCst);
        reply
    }

    fn output(&self, agent: &AgentId, id: &JobId, part: Part) -> Answer {
        let j = match self.find(agent, id) {
            Ok(j) => j,
            Err(reply) => return Answer::Json(reply),
        };
        match (j.state, part) {
            (JobState::Queued, _) => Answer::Output(Vec::new()),
            (JobState::Running, Part::Result) => {
                Answer::Json(error(409, "job is running; its result comes when it ends"))
            }
            (JobState::Running, Part::Stream) => match self.runner.read(agent, id, part) {
                Ok(bytes) => Answer::Output(bytes),
                Err(_) => Answer::Output(Vec::new()),
            },
            _ => Answer::Output(
                std::fs::read(self.job_dir(&j).join(part.file())).unwrap_or_default(),
            ),
        }
    }

    fn cancel(&self, agent: &AgentId, id: &JobId) -> Reply {
        let j = match self.find(agent, id) {
            Ok(j) => j,
            Err(reply) => return reply,
        };
        if j.state.is_terminal() {
            return error(409, &format!("job {id} is {}", j.state.name()));
        }
        if j.state == JobState::Running {
            if let Err(e) = self.runner.kill(agent, id) {
                return error(500, &format!("could not kill job {id}: {e}"));
            }
        }
        match self.transition(agent, id, JobEvent::Cancel, Some("cancelled by a caller")) {
            Ok(j) => ok(200, render(&j)),
            Err(reply) => reply,
        }
    }

    /// One supervisor step: end jobs of agents that stopped, then per agent poll or time out the
    /// running job, or start the oldest queued one. Called every second by the gateway.
    pub fn tick(&self) {
        self.reconcile();
        let mut by_agent: HashMap<String, Vec<Job>> = HashMap::new();
        match self.store.active_jobs() {
            Ok(all) => all.into_iter().for_each(|j| {
                by_agent.entry(j.agent.to_string()).or_default().push(j);
            }),
            Err(e) => eprintln!("ac-gateway: jobs: {e}"),
        }
        for mut jobs in by_agent.into_values() {
            jobs.sort_by_key(|j| j.created_ms);
            if let Some(running) = jobs.iter().find(|j| j.state == JobState::Running) {
                self.supervise(running);
            } else if let Some(next) = jobs.first() {
                self.start(next);
            }
        }
    }

    /// End the jobs of agents that are not running: running ones become `interrupted`, queued
    /// ones `cancelled` (ADR-010). Run at startup, after the fleet's reconcile, and every tick.
    pub fn reconcile(&self) {
        let Ok(active) = self.store.active_jobs() else {
            return;
        };
        for j in active {
            if !matches!(self.agents.state(&j.agent), Ok(Some(State::Running))) {
                let _ = self.transition(
                    &j.agent,
                    &j.id,
                    JobEvent::Interrupt,
                    Some("agent is not running"),
                );
            }
        }
    }

    fn supervise(&self, j: &Job) {
        let now = (self.clock)();
        if j.deadline_ms().is_some_and(|d| now >= d) {
            match self.runner.kill(&j.agent, &j.id) {
                Ok(()) => {
                    let why = format!("killed after {} s", j.timeout_secs);
                    let _ = self.transition(&j.agent, &j.id, JobEvent::Timeout, Some(&why));
                }
                Err(e) => eprintln!("ac-gateway: jobs: {}/{}: timeout kill: {e}", j.agent, j.id),
            }
            return;
        }
        {
            let mut book = self.lock();
            let last = book.entry(j.id.to_string()).or_insert(0);
            if now.saturating_sub(*last) < POLL.as_millis() as u64 {
                return;
            }
            *last = now;
        }
        let (event, why) = match self.runner.status(&j.agent, &j.id) {
            Ok(Probe::Running) => return,
            Err(e) => {
                eprintln!("ac-gateway: jobs: {}/{}: status: {e}", j.agent, j.id);
                return;
            }
            Ok(Probe::Exited(Some(code))) => (JobEvent::Exit(code), None),
            Ok(Probe::Exited(None)) => (JobEvent::Fail, Some("unreadable exit code")),
            Ok(Probe::Gone) => (JobEvent::Fail, Some("runner ended without an exit code")),
        };
        self.collect(j);
        let _ = self.transition(&j.agent, &j.id, event, why);
    }

    fn start(&self, j: &Job) {
        if self
            .transition(&j.agent, &j.id, JobEvent::Start, None)
            .is_err()
        {
            return;
        }
        let started = std::fs::read(self.job_dir(j).join("prompt.txt"))
            .map_err(|e| format!("prompt: {e}"))
            .and_then(|prompt| {
                self.runner
                    .start(&j.agent, &j.id, &prompt, j.timeout_secs, &self.claude)
            });
        if let Err(e) = started {
            let why = format!("start failed: {e}");
            let _ = self.transition(&j.agent, &j.id, JobEvent::Fail, Some(&why));
        }
    }

    /// Copy the capped result and log to the host, so they outlive the VM.
    fn collect(&self, j: &Job) {
        for part in [Part::Result, Part::Stream] {
            let copied = self
                .runner
                .read(&j.agent, &j.id, part)
                .and_then(|bytes| write_private(&self.job_dir(j).join(part.file()), &bytes));
            if let Err(e) = copied {
                eprintln!(
                    "ac-gateway: jobs: {}/{}: {}: {e}",
                    j.agent,
                    j.id,
                    part.file()
                );
            }
        }
    }

    /// Apply `event` to a job under the lock, persist it, prune old jobs if it ended, note it in
    /// the agent's history and wake long-polls.
    fn transition(
        &self,
        agent: &AgentId,
        id: &JobId,
        event: JobEvent,
        reason: Option<&str>,
    ) -> Result<Job, Reply> {
        let mut book = self.lock();
        let mut j = self.find(agent, id)?;
        j.apply(event, (self.clock)())
            .map_err(|e| error(409, &e.to_string()))?;
        if let Some(r) = reason {
            j.reason = Some(r.to_owned());
        }
        self.store
            .put_job(&j)
            .map_err(|e| error(500, &e.to_string()))?;
        let mut note = format!(
            "job {id} {}",
            match j.state {
                JobState::Running => "started",
                s => s.name(),
            }
        );
        if let Some(code) = j.exit_code {
            note.push_str(&format!(" (exit {code})"));
        }
        (self.notes)(agent, &note);
        if j.state.is_terminal() {
            book.remove(id.as_str());
            self.prune(agent);
        }
        drop(book);
        self.changed.notify_all();
        Ok(j)
    }

    /// Keep the newest [`KEEP`] finished jobs of `agent`. Called with the lock held.
    fn prune(&self, agent: &AgentId) {
        let Ok(mut done) = self.store.jobs(agent) else {
            return;
        };
        done.retain(|j| j.state.is_terminal());
        done.sort_by_key(|j| std::cmp::Reverse(j.created_ms));
        for old in done.iter().skip(KEEP) {
            let _ = self.store.remove_job(agent, &old.id);
            let _ = std::fs::remove_dir_all(self.job_dir(old));
        }
    }

    fn find(&self, agent: &AgentId, id: &JobId) -> Result<Job, Reply> {
        self.store
            .jobs(agent)
            .map_err(|e| error(500, &e.to_string()))?
            .into_iter()
            .find(|j| j.id == *id)
            .ok_or_else(|| error(404, &format!("job {id} not found for agent {agent}")))
    }

    fn unknown(&self, agent: &AgentId) -> Option<Reply> {
        match self.agents.state(agent) {
            Ok(Some(_)) => None,
            Ok(None) => Some(error(404, &format!("agent {agent} not found"))),
            Err(e) => Some(error(500, &e.to_string())),
        }
    }

    fn job_dir(&self, j: &Job) -> PathBuf {
        self.dir.join(j.agent.as_str()).join(j.id.as_str())
    }

    fn save_prompt(&self, j: &Job, prompt: &[u8]) -> Result<(), String> {
        let dir = self.job_dir(j);
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .map_err(|e| format!("{}: {e}", dir.display()))?;
        write_private(&dir.join("prompt.txt"), prompt)
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, u64>> {
        self.book.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Busy for Jobs {
    fn busy(&self, id: &AgentId) -> bool {
        self.store
            .jobs(id)
            .is_ok_and(|jobs| jobs.iter().any(|j| !j.state.is_terminal()))
    }

    fn interrupt(&self, id: &AgentId) {
        let Ok(jobs) = self.store.jobs(id) else {
            return;
        };
        for j in jobs.iter().filter(|j| !j.state.is_terminal()) {
            if j.state == JobState::Running {
                if let Err(e) = self.runner.kill(id, &j.id) {
                    eprintln!("ac-gateway: jobs: {id}/{}: interrupt kill: {e}", j.id);
                }
            }
            let _ = self.transition(
                id,
                &j.id,
                JobEvent::Interrupt,
                Some("agent paused or destroyed"),
            );
        }
    }

    fn forget(&self, id: &AgentId) {
        let _book = self.lock();
        if let Ok(jobs) = self.store.jobs(id) {
            for j in jobs {
                let _ = self.store.remove_job(id, &j.id);
            }
        }
        let _ = std::fs::remove_dir_all(self.dir.join(id.as_str()));
    }
}

/// The value of `name` in a query string.
fn param<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    query
        .split('&')
        .find_map(|p| p.strip_prefix(name)?.strip_prefix('='))
}

fn ok(status: u16, body: String) -> Reply {
    Reply { status, body }
}

/// Create or replace `path` with mode 0600.
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut f| f.write_all(bytes))
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn random() -> [u8; 8] {
    let mut b = [0u8; 8];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .is_err()
    {
        static N: AtomicUsize = AtomicUsize::new(0);
        b = (now_ms() ^ (N.fetch_add(1, Ordering::Relaxed) as u64) << 48).to_le_bytes();
    }
    b
}
