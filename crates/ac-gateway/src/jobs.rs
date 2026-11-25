//! The task API (ADR-010): run a prompt in an agent as a job, with an id, live status and the
//! result, without exec-ing into the VM by hand.
//!
//! Routes (bearer token required), under `/api/v1/agents/{id}/`:
//! - `POST jobs` `{"prompt", "timeout_secs"?, "idempotency_key"?, "credential"?}`: 202 and the job
//!   (200 and the existing job for a known key). A `credential` (ADR-028) is an opaque string the
//!   job's process finds in the file `$AC_JOB_CREDENTIAL_FILE`, on the guest's tmpfs, for as long
//!   as the job runs; it is never stored, logged or shown, and is redacted from collected output;
//! - `GET jobs`: the agent's jobs, newest first;
//! - `GET jobs/{job}[?wait=N]`: the job; with `wait` (at most 30 s), once its state changes. A
//!   queued job says why it waits in `queued_reason` (ADR-013; `null` until the supervisor's next
//!   tick, within a second);
//! - `GET jobs/{job}/output[?part=stream]`: the final message, or the stream-json log, as text;
//! - `POST jobs/{job}/cancel`;
//! - `GET jobs/{job}/witness`: an ended job's witness record and its SHA-256 (ADR-029).
//!
//! A queued job starts when its agent is free and the host-wide gate is open ([`schedule`]): fewer
//! than `max_running` jobs run, the CPU is not saturated, and the last start was long enough ago.
//!
//! The registry ([`JobStore`]) holds state, times, timeout and exit code. What the agent writes
//! (result, log) is untrusted: it is read in the VM through capped execs, kept capped under
//! `<data>/jobs/<agent>/<job>/`, served as `text/plain` with `nosniff`, and never parsed here. The
//! gateway never writes, chowns or reads a path below the workspace root (the agent controls it);
//! the prompt goes in with `O_CREAT|O_EXCL` at the root and the start exec moves it.
//!
//! What a started job changed (ADR-019) is noted in the agent's history when it ends: [`Changes`]
//! compares workspace snapshots from its start and end, which btrfs does without root opening a
//! path inside the workspace. Only the end snapshot is taken as the job ends. The compare costs as
//! much as the job made, so it never runs on the supervisor's thread, under the lifecycle lock or
//! before the gateway is ready: ended jobs queue for [`Jobs::diff_next`], which the gateway runs
//! on a thread of its own, one compare at a time.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ac_ctr::CtrRuntime;
use ac_domain::changes::Changed;
use ac_domain::health::Pressure;
use ac_domain::job::{self, Credential, ModelUrl};
use ac_domain::schedule::{self, Limits, Wait};
use ac_domain::witness::{self, EgressSource};
use ac_domain::{AgentId, Job, JobEvent, JobId, JobState, State};
use ac_ports::{HostLoad, JobStore, SessionStore};
use platform::process::Spawner;
use rusty_json::Value;
use rusty_sha2::{Hash, Sha256};

use crate::agui::{self, Frames};
use crate::{error, escape, Busy, JobCounts, Reply};

/// Jobs waiting behind the running one, per agent.
pub const MAX_QUEUE: usize = 8;
/// Finished jobs kept per agent (registry records and host copies).
pub const KEEP: usize = 20;
/// Where a job's compare result is kept in its host directory, for its witness record.
pub const CHANGES_FILE: &str = "changes";
/// Longest `?wait=`.
pub const MAX_WAIT_SECS: u64 = 30;
/// Long-polls at once; more are refused, so they cannot take the lifecycle API's connections.
pub const MAX_WAITERS: usize = 8;
/// What is kept of a job's final message.
pub const RESULT_CAP: usize = 256 * 1024;
/// What is kept of a job's stream log (its end).
pub const STREAM_CAP: usize = 1024 * 1024;
/// Most bytes of paths in a job's change note (ADR-019); the rest are counted.
pub const CHANGE_NOTE_CAP: usize = 2048;
/// Bytes of a job's log one event-stream read takes (#471).
pub const EVENTS_CHUNK: usize = 256 * 1024;
/// Reads an event stream makes per poll, so one busy log cannot hold a poll for long.
const EVENTS_READS: usize = 4;
/// Event streams open at once, across all agents: each polls its job's log with an exec.
pub const MAX_STREAMS: usize = 8;
/// Event streams open at once on one agent, so one agent's token cannot take every seat, and one
/// agent sees at most this many log execs per poll (the `PROBE` hazard).
pub const MAX_STREAMS_PER_AGENT: usize = 2;
/// Reads the last drain of a live-streamed job may take in all (16 MiB), [`EVENTS_READS`] per poll.
const FINAL_READS: usize = STREAM_CAP.div_ceil(EVENTS_CHUNK) * 16;
/// How often an event stream polls the registry and the log, by default.
pub const EVENTS_POLL: Duration = Duration::from_secs(2);
/// How often a running job is probed with an exec when it has not said it finished. Execs are
/// kept rare and unhurried: a `ctr` exec SIGKILLed at a short budget while the host was loaded
/// left a1's shim wedged (2026-10-21, a 2 s status poll every 2 s).
const PROBE: Duration = Duration::from_secs(30);
/// Least time between status execs prompted by the finished marker.
const MARKER_RETRY: Duration = Duration::from_secs(3);
/// Budget of a status exec.
const STATUS_LIMIT: Duration = Duration::from_secs(10);
/// Added to the job's timeout for the runner's own `timeout`, a backstop for a stopped gateway.
const BACKSTOP_SECS: u32 = 300;
/// In a job's host directory: the job was submitted with a credential (empty; never the secret).
const NEEDS_CREDENTIAL: &str = "needs-credential";
/// Empty marker: the host's copy of the log is all of it, not its last [`STREAM_CAP`], so its line
/// numbers match the live stream's (#471). Without it a copy may be capped.
const STREAM_WHOLE: &str = "stream.whole";
/// What stands in for a credential in collected output.
pub const REDACTED: &[u8] = b"[credential redacted]";

/// `bytes` with every occurrence of `secret` replaced by [`REDACTED`].
pub fn redact(bytes: &[u8], secret: &[u8]) -> Vec<u8> {
    if secret.is_empty() {
        return bytes.to_vec();
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(secret) {
            out.extend_from_slice(REDACTED);
            i += secret.len();
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

/// Default `AC_JOB_ALLOWED_TOOLS`: read, search and edit files; `git` without push; `ls`, `cat`;
/// `python3` and `cargo` (both run arbitrary code as the agent user, inside the VM); `sleep`.
pub const DEFAULT_TOOLS: &str = "Read,Glob,Grep,Edit,Write,Bash(git status:*),Bash(git diff:*),\
    Bash(git log:*),Bash(git add:*),Bash(git commit:*),Bash(ls:*),Bash(cat:*),Bash(python3:*),\
    Bash(cargo build:*),Bash(cargo test:*),Bash(cargo check:*),Bash(cargo fmt:*),Bash(sleep:*)";
/// Permission modes a job may use without `AC_JOB_UNSAFE=1`.
const MODES: [&str; 5] = ["acceptEdits", "auto", "dontAsk", "manual", "plan"];

/// Start a job, as the agent user (argv: this, the runner script, the job id, `1` if a credential
/// follows on stdin else `0`, the backstop timeout, then `claude`'s permission flags). Refuses a
/// job whose directory exists, so a job never starts twice. Keeps the newest 5 job directories.
/// A credential is read as one line from stdin (which never reaches EOF, see `exec_once`) and
/// written, last before the window opens, to `/dev/shm/ac-job-<id>/credential`, 0600 in a 0700
/// directory, on the guest's tmpfs: never in the workspace, so never in a snapshot, backup or
/// change note. A start that fails after that removes it again (and the gateway's kill does too,
/// for a start the exec timeout cut short).
const START: &str = r#"set -eu
run=$1 id=$2 cred=$3 model=$4
shift 4
c=/dev/shm/ac-job-$id
secret=
if [ "$cred" = 1 ]; then IFS= read -r secret; fi
trap 'rc=$?; [ "$rc" = 0 ] || rm -rf -- "$c"' EXIT
cd /workspace
mkdir -p -m 700 .jobs
mkdir -m 700 ".jobs/$id"
mv ".job-$id.prompt" ".jobs/$id/prompt.txt"
ls -1t .jobs | grep -E '^[a-z0-9-]+$' | tail -n +6 | while read -r old; do rm -rf -- ".jobs/$old"; done
rm -f -- .job-*.done
tmux has-session -t main 2>/dev/null || tmux new-session -d -s main -c /workspace
if [ "$cred" = 1 ]; then
    rm -rf -- "$c"
    (umask 077 && mkdir "$c" && printf '%s' "$secret" > "$c/credential")
fi
unset secret
tmux new-window -d -t main: -n "job-$id" -c /workspace bash -c "$run" ac-job "$id" "$cred" "$model" "$@""#;

/// The job's window (bash): `claude` reads the prompt on stdin; its log is capped at 16 MiB (a
/// job that passes it is stopped); the terminal shows a digest. With a credential, its log is
/// redacted line by line before it is stored or shown, so the raw secret never reaches the
/// workspace's `stream.jsonl` or `result.txt` (whole stream-json lines, so no split matches; its
/// characters need no JSON escaping and only `.` and `/` need escaping for `sed`). The job's credential file goes as
/// soon as `claude` exits, before the job reports that it ended (and on any exit of the window). `exit_code` is written last, then
/// the marker `.job-<id>.done` in the workspace root, which the gateway sees without an exec.
const RUN: &str = r#"id=$1 cred=$2 model=$3 secs=$4
shift 4
d=/workspace/.jobs/$id
c=/dev/shm/ac-job-$id
trap 'rm -rf -- "$c"' EXIT
redact() { cat; }
if [ "$cred" = 1 ]; then
    export AC_JOB_CREDENTIAL_FILE=$c/credential
    pat=$(sed 's/[./]/\\&/g' "$c/credential")
    redact() { sed -u "s/$pat/[credential redacted]/g"; }
    if [ "$model" != - ]; then
        export ANTHROPIC_BASE_URL="$model"
        ANTHROPIC_AUTH_TOKEN=$(cat "$c/credential")
        export ANTHROPIC_AUTH_TOKEN
    fi
fi
show='fromjson? | if .type == "assistant" then (.message.content[]? | if .type == "text" then .text + "\n" elif .type == "tool_use" then "> " + .name + " " + (.input | tostring | .[0:160]) + "\n" else empty end) elif .type == "result" then "--- " + (.subtype // "result") + "\n" else empty end'
printf 'job %s started %s\n' "$id" "$(date -u +%FT%TZ)"
timeout -k 10 "$secs" claude -p --output-format stream-json --verbose "$@" < "$d/prompt.txt" \
    | redact | head -c 16777216 | tee "$d/stream.jsonl" | jq -Rrj --unbuffered "$show"
rc=${PIPESTATUS[0]}
rm -rf -- "$c"
jq -Rrj 'fromjson? | select(.type == "result") | .result // empty' "$d/stream.jsonl" \
    | head -c 262144 > "$d/result.txt"
printf '%s\n' "$rc" > "$d/exit_code.tmp"
mv "$d/exit_code.tmp" "$d/exit_code"
: > "/workspace/.job-$id.done"
printf '\njob %s ended: exit %s\n' "$id" "$rc""#;

/// Whether a job runs: its window's pane is alive (`running`), else its exit code (`exit N`),
/// else `gone`. Liveness first: the runner writes `exit_code` before it exits. The window is found
/// by name in `list-windows`: `display -t` falls back to the current pane when the target is
/// missing, so it would report the agent's own shell.
const STATUS: &str = r#"id=$1
dead=$(tmux list-windows -t main -F '#{window_name} #{pane_dead}' 2>/dev/null | sed -n "s/^job-$id //p" | head -n 1)
if [ "$dead" = 0 ]; then echo running; exit 0; fi
f=/workspace/.jobs/$id/exit_code
if [ -f "$f" ]; then printf 'exit %s\n' "$(head -c 12 "$f" | tr -dc 0-9-)"; exit 0; fi
echo gone"#;

/// Kill a job's window (found by name, then addressed by its id, as in [`STATUS`]) and remove its
/// credential file (first, and again once the processes are gone): its process tree (by parent pid) and the pane's session, stopped first so nothing forks meanwhile, then
/// killed; the window closed. Prints `killed` once every one of them
/// is gone (zombies count as gone), `gone` if there was no window, else fails.
const KILL: &str = r#"rm -rf -- "/dev/shm/ac-job-$1"
w=$(tmux list-windows -t main -F '#{window_name} #{window_id} #{pane_pid}' 2>/dev/null | sed -n "s/^job-$1 //p" | head -n 1)
[ -n "$w" ] || { echo gone; exit 0; }
t=${w% *} pid=${w#* }
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
    [ -z "$left" ] && { rm -rf -- "/dev/shm/ac-job-$1"; echo killed; exit 0; }
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
    /// Put `prompt` in the agent's workspace, `credential` (if any) on the guest's tmpfs, and
    /// open the job's window; returns once it is open. With both a credential and `model`, the
    /// job reaches the model there with the credential as its token.
    #[allow(clippy::too_many_arguments)]
    fn start(
        &self,
        agent: &AgentId,
        job: &JobId,
        prompt: &[u8],
        credential: Option<&Credential>,
        model: Option<&ModelUrl>,
        timeout_secs: u32,
        claude: &[String],
    ) -> Result<(), String>;
    /// Whether the job's runner says it finished: its marker exists in the workspace root. Read on
    /// the host without opening anything; only a hint, confirmed by [`JobRunner::status`].
    fn finished(&self, agent: &AgentId, job: &JobId) -> bool;
    /// Whether the job still runs (an exec).
    fn status(&self, agent: &AgentId, job: &JobId) -> Result<Probe, String>;
    /// Kill the job's processes, close its window and remove its credential file; fails unless
    /// they are gone.
    fn kill(&self, agent: &AgentId, job: &JobId) -> Result<(), String>;
    /// Up to [`RESULT_CAP`] bytes of the result, or the last [`STREAM_CAP`] of the log.
    fn read(&self, agent: &AgentId, job: &JobId, part: Part) -> Result<Vec<u8>, String>;
    /// Up to [`EVENTS_CHUNK`] bytes of the log from byte `offset`; empty at its end. An error
    /// while the log does not exist yet.
    fn read_from(&self, agent: &AgentId, job: &JobId, offset: u64) -> Result<Vec<u8>, String>;
}

/// Which workspace paths a job changed (ADR-019): a snapshot as it starts, compared with one as
/// it ends. Implemented over btrfs for [`CtrRuntime`]; tests use a fake.
pub trait Changes: Send + Sync + 'static {
    /// Snapshot the agent's workspace as `job` starts; nothing, and `Ok`, where it cannot be.
    fn begin(&self, agent: &AgentId, job: &JobId) -> Result<(), String>;
    /// Snapshot it as `job` ends: one snapshot, whatever the job made. `false` if [`Changes::begin`]
    /// took none, so there is nothing to compare.
    fn end(&self, agent: &AgentId, job: &JobId) -> Result<bool, String>;
    /// What changed between the two, which are then deleted, also on failure; `None` if there
    /// was no start snapshot. Costs as much as the job made: never on the supervisor's thread or
    /// under a lock.
    fn diff(&self, agent: &AgentId, job: &JobId) -> Result<Option<Changed>, String>;
    /// Delete the job's snapshots without comparing them.
    fn discard(&self, agent: &AgentId, job: &JobId) -> Result<(), String>;
    /// Every job with snapshots on disk, of any agent: ended jobs whose compare a stopped gateway
    /// never ran, besides the running ones.
    fn leftovers(&self) -> Result<Vec<(AgentId, JobId)>, String>;
}

impl<S: Spawner + Send + Sync + 'static> Changes for CtrRuntime<S> {
    fn begin(&self, agent: &AgentId, job: &JobId) -> Result<(), String> {
        self.job_begin(agent, job).map_err(|e| e.to_string())
    }

    fn end(&self, agent: &AgentId, job: &JobId) -> Result<bool, String> {
        self.job_end(agent, job).map_err(|e| e.to_string())
    }

    fn diff(&self, agent: &AgentId, job: &JobId) -> Result<Option<Changed>, String> {
        self.job_changes(agent, job).map_err(|e| e.to_string())
    }

    fn discard(&self, agent: &AgentId, job: &JobId) -> Result<(), String> {
        self.job_discard(agent, job).map_err(|e| e.to_string())
    }

    fn leftovers(&self) -> Result<Vec<(AgentId, JobId)>, String> {
        self.job_leftovers().map_err(|e| e.to_string())
    }
}

/// The egress proxy's log for a time window (ADR-029), for witness records. Implemented over
/// the journal for [`CtrRuntime`]; tests use a fake.
pub trait EgressLog: Send + Sync + 'static {
    /// The proxy's log lines from `since_ms` to `until_ms`, possibly a little wider.
    fn between(&self, since_ms: u64, until_ms: u64) -> Result<String, String>;
}

impl<S: Spawner + Send + Sync + 'static> EgressLog for CtrRuntime<S> {
    fn between(&self, since_ms: u64, until_ms: u64) -> Result<String, String> {
        self.egress_journal(since_ms, until_ms)
            .map_err(|e| e.to_string())
    }
}

/// The history note for a job whose listing was over the cap: what it changed is not listed, but
/// that it changed a great deal is said.
pub fn too_many_note(id: &JobId) -> String {
    format!("job {id} changed too many paths to list")
}

/// The history note for the paths job `id` changed, leaving out the runner's own files (the
/// `.jobs` directory and the root's `.job-*` names, see `START`). Paths are listed, space-separated,
/// until [`CHANGE_NOTE_CAP`] bytes; the rest are counted. Claude Code's own files go last (see
/// [`tool_churn`]), so they cannot push the job's real changes past the cap.
pub fn change_note(id: &JobId, paths: &[String]) -> String {
    let ours = |p: &&String| {
        p.as_str() == ".jobs"
            || p.starts_with(".jobs/")
            || (p.starts_with(".job-") && !p.contains('/'))
    };
    let mut paths: Vec<&String> = paths.iter().filter(|p| !ours(p)).collect();
    paths.sort_by_key(|p| tool_churn(p));
    let mut note = match paths.len() {
        0 => return format!("job {id} changed no paths"),
        1 => format!("job {id} changed 1 path:"),
        n => format!("job {id} changed {n} paths:"),
    };
    let budget = note.len() + CHANGE_NOTE_CAP;
    let shown = paths
        .iter()
        .take_while(|p| {
            let fits = note.len() + 1 + p.len() <= budget;
            if fits {
                note.push(' ');
                note.push_str(p);
            }
            fits
        })
        .count();
    if shown < paths.len() {
        note.push_str(&format!(" (and {} more)", paths.len() - shown));
    }
    note
}

/// Whether `path` is under the root's `.claude`, where Claude Code keeps its sessions and settings:
/// a job can write hundreds of these (#1624, 150 paths seen on 2026-11-24).
fn tool_churn(path: &str) -> bool {
    path == ".claude" || path.starts_with(".claude/")
}

impl<S: Spawner + Send + Sync + 'static> JobRunner for CtrRuntime<S> {
    fn start(
        &self,
        agent: &AgentId,
        job: &JobId,
        prompt: &[u8],
        credential: Option<&Credential>,
        model: Option<&ModelUrl>,
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
        let (flag, input) = match credential {
            Some(c) => ("1", format!("{}\n", c.expose())),
            None => ("0", String::new()),
        };
        let model = match (credential, model) {
            (Some(_), Some(m)) => m.as_str(),
            _ => "-",
        };
        let mut args = vec![
            "sh",
            "-c",
            START,
            "ac-job-start",
            RUN,
            job.as_str(),
            flag,
            model,
            &secs,
        ];
        args.extend(claude.iter().map(String::as_str));
        self.exec_once_with_input(
            "job start",
            agent,
            &args,
            input.as_bytes(),
            Duration::from_secs(30),
        )
        .map(drop)
        .map_err(|e| {
            let _ = std::fs::remove_file(&path);
            e.to_string()
        })
    }

    fn finished(&self, agent: &AgentId, job: &JobId) -> bool {
        // lstat of a name directly in the workspace root (the bind mount itself): follows no
        // symlink and opens nothing, so the agent can only make it say yes or no.
        let marker = self
            .config()
            .workspace_root
            .join(agent.as_str())
            .join(format!(".job-{job}.done"));
        std::fs::symlink_metadata(marker).is_ok()
    }

    fn status(&self, agent: &AgentId, job: &JobId) -> Result<Probe, String> {
        let args = ["sh", "-c", STATUS, "ac-job-status", job.as_str()];
        let out = self
            .exec_once("job status", agent, &args, STATUS_LIMIT)
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

    fn read_from(&self, agent: &AgentId, job: &JobId, offset: u64) -> Result<Vec<u8>, String> {
        // GNU dd: `iflag=skip_bytes,count_bytes` makes skip and count bytes, not blocks. The agent
        // images are Fedora (coreutils); a busybox base would need another reader.
        let input = format!("if=/workspace/.jobs/{job}/{}", Part::Stream.file());
        let skip = format!("skip={offset}");
        let count = format!("count={EVENTS_CHUNK}");
        let args = [
            "dd",
            &input,
            "iflag=skip_bytes,count_bytes",
            &skip,
            &count,
            "status=none",
        ];
        self.exec_once("job events", agent, &args, Duration::from_secs(10))
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
    /// `GET .../jobs/{job}/witness`.
    Witness(AgentId, JobId),
    /// `GET .../jobs/{job}/events`: the job as an AG-UI run over SSE (#471).
    Events(AgentId, JobId),
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
        ("GET", [_, "witness"], Some(j)) => Route::Witness(id, j),
        ("GET", [_, "events"], Some(j)) => Route::Events(id, j),
        (_, [] | [_] | [_, "output" | "cancel" | "witness" | "events"], _) => {
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
    /// The job's credential (ADR-028).
    pub credential: Option<Credential>,
}

/// Parse `{"prompt", "timeout_secs"?, "idempotency_key"?, "credential"?}`; unknown members are
/// refused.
pub fn parse_request(body: &[u8]) -> Result<Request, Reply> {
    let bad = |m: &str| error(400, m);
    let text = std::str::from_utf8(body).map_err(|_| bad("body is not UTF-8"))?;
    let value = Value::parse(text).map_err(|e| bad(&format!("bad JSON: {e}")))?;
    let Some(obj) = value.as_object() else {
        return Err(bad("body must be a JSON object"));
    };
    if let Some(k) = obj.keys().find(|k| {
        !matches!(
            k.as_str(),
            "prompt" | "timeout_secs" | "idempotency_key" | "credential"
        )
    }) {
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
    let credential = match value.get("credential") {
        None | Some(Value::Null) => None,
        Some(Value::String(c)) => Some(Credential::parse(c).map_err(|e| bad(&e.to_string()))?),
        Some(_) => return Err(bad("credential must be a string")),
    };
    Ok(Request {
        prompt,
        timeout_secs,
        key,
        credential,
    })
}

/// A job as JSON; `wait` is why it is still queued, if known.
pub fn render(j: &Job, wait: Option<Wait>) -> String {
    let opt = |v: Option<String>| v.unwrap_or_else(|| "null".to_owned());
    let text = |v: &Option<String>| opt(v.as_deref().map(|s| format!("\"{}\"", escape(s))));
    let wait = wait.filter(|_| j.state == JobState::Queued);
    format!(
        r#"{{"job_id":"{}","agent":"{}","state":"{}","created_ms":{},"started_ms":{},"finished_ms":{},"timeout_secs":{},"exit_code":{},"idempotency_key":{},"reason":{},"queued_reason":{}}}"#,
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
        opt(wait.map(|w| format!("\"{}\"", w.name()))),
    )
}

type Notes = Box<dyn Fn(&AgentId, &str) + Send + Sync>;
type Clock = Box<dyn Fn() -> u64 + Send + Sync>;

/// Every job of every agent: the routes, the supervisor and the lifecycle hooks.
pub struct Jobs {
    store: Arc<dyn JobStore + Send + Sync>,
    agents: Arc<dyn SessionStore + Send + Sync>,
    runner: Box<dyn JobRunner>,
    changes: Option<Box<dyn Changes>>,
    egress: Option<Box<dyn EgressLog>>,
    model: Option<ModelUrl>,
    dir: PathBuf,
    claude: Vec<String>,
    notes: Notes,
    clock: Clock,
    limits: Limits,
    load: Option<Box<dyn HostLoad + Send + Sync>>,
    /// Which jobs this run ended, until the first tick has swept leftovers.
    restart: Mutex<Restart>,
    /// Jobs [`Jobs::startup`] ended without their end snapshot, taken by the first tick or by
    /// [`Busy::settle`] before their agent boots, whichever comes first.
    unsealed: Mutex<Vec<(AgentId, JobId)>>,
    /// Why each queued job waited at the last tick (by job id).
    waits: Mutex<HashMap<String, Wait>>,
    /// The last pressure read failed (logged once per failure streak).
    blind: AtomicBool,
    /// Serialises read-modify-write of job records; holds when each running job was last polled.
    book: Mutex<HashMap<String, u64>>,
    changed: Condvar,
    waiters: AtomicUsize,
    /// Ended jobs whose changes are still to be compared ([`Jobs::diff_next`]).
    diffs: Mutex<Diffs>,
    queued: Condvar,
    /// Credentials of jobs not yet ended (by job id), in memory only (ADR-028).
    secrets: Mutex<HashMap<String, Credential>>,
    /// Event streams open now, per agent ([`MAX_STREAMS`], [`MAX_STREAMS_PER_AGENT`]).
    streams: Mutex<HashMap<AgentId, usize>>,
    /// How often each polls.
    events_poll: Duration,
}

/// Telling an earlier run's leftover snapshots from this run's jobs, by what this run did rather
/// than by the clock (which can go back across a restart).
#[derive(Default)]
struct Restart {
    /// [`Jobs::queue_leftovers`] has run.
    swept: bool,
    /// A sweep failed and is retried each tick; logged once.
    failed: bool,
    /// Started jobs this run ended before that.
    ended: HashSet<(AgentId, JobId)>,
}

/// When a started job's end snapshot is taken as it ends.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Seal {
    /// At once, then its compare is queued.
    Now,
    /// Later ([`Jobs::startup`]): no host command yet.
    Later,
}

/// The compares to run, and which agents' are off limits to them.
#[derive(Default)]
struct Diffs {
    /// Ended jobs in turn order: oldest first, except that the agent whose compare was taken last
    /// goes behind every other agent's at the next take ([`Diffs::take`]).
    queue: VecDeque<(AgentId, JobId)>,
    /// The agent whose job is being compared now.
    listing: Option<AgentId>,
    /// The agent whose compare was taken last: its others go behind every other agent's at the
    /// next take, so a job queued meanwhile gets the next turn.
    last: Option<AgentId>,
    /// Agents being purged ([`Busy::hold`]): their compares wait, since the purge deletes the
    /// snapshots they read.
    held: HashSet<AgentId>,
}

impl Diffs {
    /// Where the oldest compare of an agent not held is.
    fn next(&self) -> Option<usize> {
        self.queue.iter().position(|(a, _)| !self.held.contains(a))
    }

    /// Take the next compare, after moving the last-served agent's behind every other agent's,
    /// so agents take turns: each turn compares the oldest queued job of each agent with compares
    /// queued, a job queued during a compare included, and an agent with many large compares
    /// queued delays another's by one of them per turn.
    fn take(&mut self) -> Option<(AgentId, JobId)> {
        if let Some(last) = self.last.take() {
            let (theirs, others): (VecDeque<_>, VecDeque<_>) =
                self.queue.drain(..).partition(|(a, _)| *a == last);
            self.queue = others;
            self.queue.extend(theirs);
        }
        let next = self.next().and_then(|i| self.queue.remove(i))?;
        self.last = Some(next.0.clone());
        Some(next)
    }
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
            changes: None,
            egress: None,
            model: None,
            dir,
            claude,
            notes: Box::new(|_, _| {}),
            clock: Box::new(now_ms),
            limits: Limits::OPEN,
            load: None,
            restart: Mutex::new(Restart::default()),
            unsealed: Mutex::new(Vec::new()),
            waits: Mutex::new(HashMap::new()),
            blind: AtomicBool::new(false),
            book: Mutex::new(HashMap::new()),
            changed: Condvar::new(),
            waiters: AtomicUsize::new(0),
            diffs: Mutex::new(Diffs::default()),
            queued: Condvar::new(),
            secrets: Mutex::new(HashMap::new()),
            streams: Mutex::new(HashMap::new()),
            events_poll: EVENTS_POLL,
        }
    }

    /// Poll event streams every `poll` instead of [`EVENTS_POLL`].
    pub fn with_events_poll(mut self, poll: Duration) -> Self {
        self.events_poll = poll;
        self
    }

    /// `GET .../jobs/{job}/events[?after=N]`: the job as an AG-UI run (#471), live while it runs.
    /// `after` skips the log's first N lines: each event from the log carries its line number as
    /// its SSE id, so a client that reconnects passes the last id it saw. An ended job whose host
    /// copy was capped has no such numbering: its replay carries no ids, and with `after` it ends at
    /// once with `RUN_ERROR` code `replay_unavailable`. 429 past [`MAX_STREAMS`] in all or
    /// [`MAX_STREAMS_PER_AGENT`] on this agent.
    pub fn events(
        jobs: &Arc<Self>,
        agent: &AgentId,
        id: &JobId,
        query: &str,
    ) -> Result<EventStream, Reply> {
        let skip = match param(query, "after").map(str::parse::<u64>) {
            None => 0,
            Some(Ok(n)) => n,
            Some(Err(_)) => return Err(error(400, "after must be a line number")),
        };
        jobs.find(agent, id)?;
        jobs.take_seat(agent)?;
        Ok(EventStream {
            jobs: Arc::clone(jobs),
            agent: agent.clone(),
            id: id.clone(),
            secret: jobs.secrets().get(id.as_str()).cloned(),
            frames: Frames::new(),
            ready: VecDeque::new(),
            offset: 0,
            partial: Vec::new(),
            overlong: false,
            lines: 0,
            skip,
            ids: true,
            last: None,
            live: false,
            final_reads: None,
            done: false,
            first: true,
        })
    }

    /// Take an event-stream seat on `agent`; 429 when all, or all of this agent's, are taken.
    fn take_seat(&self, agent: &AgentId) -> Result<(), Reply> {
        let mut seats = self.streams.lock().unwrap_or_else(|e| e.into_inner());
        if seats.values().sum::<usize>() >= MAX_STREAMS {
            return Err(error(429, "too many event streams"));
        }
        let mine = seats.entry(agent.clone()).or_insert(0);
        if *mine >= MAX_STREAMS_PER_AGENT {
            return Err(error(429, &format!("too many event streams on {agent}")));
        }
        *mine += 1;
        Ok(())
    }

    fn free_seat(&self, agent: &AgentId) {
        let mut seats = self.streams.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = seats.get_mut(agent) {
            *n -= 1;
            if *n == 0 {
                seats.remove(agent);
            }
        }
    }

    /// Event streams open now: in all, and on `agent`.
    pub fn streams_open(&self, agent: &AgentId) -> (usize, usize) {
        let seats = self.streams.lock().unwrap_or_else(|e| e.into_inner());
        (seats.values().sum(), seats.get(agent).copied().unwrap_or(0))
    }

    /// Jobs that carry a credential reach the model at `url`, with the credential as their token
    /// (ADR-028 addendum); jobs without one use the agent's own login, as before.
    pub fn with_model_url(mut self, url: ModelUrl) -> Self {
        self.model = Some(url);
        self
    }

    /// Read each witness record's egress decisions from `log` (ADR-029). Without this, records
    /// say `egress unavailable`.
    pub fn with_egress_log(mut self, log: impl EgressLog) -> Self {
        self.egress = Some(Box::new(log));
        self
    }

    /// Record state changes with `notes` (the agent's history log).
    pub fn with_notes(mut self, notes: impl Fn(&AgentId, &str) + Send + Sync + 'static) -> Self {
        self.notes = Box::new(notes);
        self
    }

    /// Note in the agent's history which paths each started job changed, found by `changes`
    /// (ADR-019) on whichever thread calls [`Jobs::diff_next`]. Without this, no change notes.
    pub fn with_changes(mut self, changes: impl Changes) -> Self {
        self.changes = Some(Box::new(changes));
        self
    }

    /// Read the time (ms since the epoch) from `clock`.
    pub fn with_clock(mut self, clock: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        self.clock = Box::new(clock);
        self
    }

    /// Start queued jobs only within `limits`, reading host pressure from `load` (ADR-013).
    /// Without this, every agent starts its next job at once.
    pub fn with_gate(
        mut self,
        limits: Limits,
        load: impl HostLoad + Send + Sync + 'static,
    ) -> Self {
        self.limits = limits;
        self.load = Some(Box::new(load));
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
            return ok(200, self.render(j));
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
            .save_prompt(&j, req.prompt.as_bytes(), req.credential.is_some())
            .and_then(|()| self.store.put_job(&j).map_err(|e| e.to_string()));
        if let Err(e) = saved {
            return error(500, &e);
        }
        if let Some(c) = req.credential {
            self.secrets().insert(j.id.to_string(), c);
        }
        (self.notes)(agent, &format!("job {} queued", j.id));
        self.changed.notify_all();
        ok(202, self.render(&j))
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
            Route::Witness(id, job) => self.witness(&id, &job),
            Route::Events(id, job) => {
                error(500, &format!("events of {id}/{job} need Jobs::events"))
            }
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
                let items: Vec<String> = jobs.iter().map(|j| self.render(j)).collect();
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
            return ok(200, self.render(&first));
        }
        if self.waiters.fetch_add(1, Ordering::SeqCst) >= MAX_WAITERS {
            self.waiters.fetch_sub(1, Ordering::SeqCst);
            return error(429, "too many waiting requests");
        }
        let end = Instant::now() + Duration::from_secs(wait);
        let mut book = self.lock();
        let reply = loop {
            match self.find(agent, id) {
                Ok(j) if j.state != first.state => break ok(200, self.render(&j)),
                Ok(j) => {
                    let left = end.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        break ok(200, self.render(&j));
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
                Ok(bytes) => Answer::Output(self.redact(&j, bytes)),
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
            Ok(j) => ok(200, self.render(&j)),
            Err(reply) => reply,
        }
    }

    /// An ended job's witness record (ADR-029) and its SHA-256, from the registry, the changes
    /// kept when its compare ran and the proxy's log for its agent while it ran. Nothing the
    /// agent wrote is read. 409 until the job ends; `changes pending` until its compare has run.
    fn witness(&self, agent: &AgentId, id: &JobId) -> Reply {
        let j = match self.find(agent, id) {
            Ok(j) => j,
            Err(reply) => return reply,
        };
        if !j.state.is_terminal() {
            return error(
                409,
                &format!(
                    "job {id} is {}; its witness comes when it ends",
                    j.state.name()
                ),
            );
        }
        let changes = match self.kept_changes(&j) {
            Ok(changes) => changes,
            Err(e) => return error(500, &format!("job {id} changes: {e}")),
        };
        let record = witness::render(&j, &changes, &self.egress_of(&j));
        let digest: String = Sha256::digest(record.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        ok(
            200,
            format!(
                r#"{{"record":{},"sha256":"{digest}"}}"#,
                Value::from(record).to_json_string()
            ),
        )
    }

    /// What the job's compare found, as kept in its host directory by [`Jobs::diff_next`].
    fn kept_changes(&self, j: &Job) -> Result<witness::Changes, String> {
        if j.started_ms.is_none() || self.changes.is_none() {
            return Ok(witness::Changes::None);
        }
        match std::fs::read_to_string(self.job_dir(j).join(CHANGES_FILE)) {
            Ok(text) => witness::parse_changes(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(witness::Changes::Pending),
            Err(e) => Err(e.to_string()),
        }
    }

    /// The proxy's decisions for the job's agent while it ran; none for a job that never started.
    fn egress_of(&self, j: &Job) -> EgressSource {
        let Some(log) = &self.egress else {
            return EgressSource::Unavailable;
        };
        let Some(started) = j.started_ms else {
            return EgressSource::Journal(Vec::new());
        };
        let finished = j.finished_ms.unwrap_or(started).max(started);
        match log.between(started, finished) {
            Ok(text) => EgressSource::Journal(witness::decisions(&text, &j.agent)),
            Err(e) => {
                eprintln!("ac-gateway: jobs: {}/{}: egress log: {e}", j.agent, j.id);
                EgressSource::Unavailable
            }
        }
    }

    /// End the jobs of agents that are not running, as [`Jobs::reconcile`] does but in the
    /// registry only, with no host command, so it fits before `READY=1` however many jobs a
    /// reboot interrupted. A caller then never sees such a job `running`, and a resume cannot
    /// race it. The end snapshots are taken by the first tick or, for an agent booted before
    /// that, by [`Busy::settle`] first.
    pub fn startup(&self) {
        self.end_stopped(Seal::Later);
    }

    /// Take the end snapshots [`Jobs::startup`] left, one at a time, each under the lock
    /// [`Busy::settle`] takes, so an agent never boots while its job's is being taken.
    fn seal_unsealed(&self) {
        loop {
            let mut unsealed = self.unsealed.lock().unwrap_or_else(|e| e.into_inner());
            let Some((agent, id)) = unsealed.pop() else {
                return;
            };
            self.seal(&agent, &id);
        }
    }

    /// One supervisor step: end jobs of agents that stopped, poll or time out the running jobs,
    /// then start what the gate allows, oldest first ([`schedule::plan`]). Decisions use the
    /// snapshot taken at the start of the tick, so a job that just ended frees its agent and its
    /// slot at the next tick. Called every second by the gateway. The first tick also queues the
    /// compares an earlier run left.
    pub fn tick(&self) {
        self.seal_unsealed();
        self.reconcile();
        self.queue_leftovers();
        let mut active = match self.store.active_jobs() {
            Ok(all) => all,
            Err(e) => {
                eprintln!("ac-gateway: jobs: {e}");
                return;
            }
        };
        for j in active.iter().filter(|j| j.state == JobState::Running) {
            self.supervise(j);
        }
        let queued = active.iter().any(|j| j.state == JobState::Queued);
        let pressure = if queued { self.pressure() } else { None };
        let plan = loop {
            let now = (self.clock)();
            let plan = schedule::plan(&active, pressure.as_ref(), now, &self.limits);
            let Some(j) = plan
                .start
                .as_ref()
                .and_then(|id| active.iter_mut().find(|j| j.id == *id))
            else {
                break plan;
            };
            self.start(j);
            // Counted as running from now on in this tick's decisions, however its start went.
            let _ = j.apply(JobEvent::Start, now);
        };
        self.remember(plan, &active);
    }

    /// Host pressure for the gate; `None` (gate skipped) without a source or when unreadable.
    fn pressure(&self) -> Option<Pressure> {
        match self.load.as_ref()?.pressure() {
            Ok(p) => {
                if self.blind.swap(false, Ordering::SeqCst) {
                    eprintln!("ac-gateway: jobs: host pressure readable again");
                }
                Some(p)
            }
            Err(e) => {
                if !self.blind.swap(true, Ordering::SeqCst) {
                    eprintln!("ac-gateway: jobs: {e}; starting jobs without the pressure gate");
                }
                None
            }
        }
    }

    /// Keep why each queued job waits, for `queued_reason`, and log each change of reason.
    fn remember(&self, plan: schedule::Plan, active: &[Job]) {
        let mut waits = self.waits.lock().unwrap_or_else(|e| e.into_inner());
        let next: HashMap<String, Wait> = plan
            .waits
            .into_iter()
            .map(|(id, w)| (id.to_string(), w))
            .collect();
        for (id, w) in &next {
            if waits.get(id) != Some(w) {
                let why = match (w, &plan.busy) {
                    (Wait::HostBusy, Some(b)) => format!(" ({b})"),
                    _ => String::new(),
                };
                let agent = active
                    .iter()
                    .find(|j| j.id.as_str() == id)
                    .map(|j| &j.agent);
                let agent = agent.map_or_else(String::new, |a| format!("{a}/"));
                eprintln!("ac-gateway: jobs: {agent}{id}: waits: {}{why}", w.name());
            }
        }
        *waits = next;
    }

    fn render(&self, j: &Job) -> String {
        let waits = self.waits.lock().unwrap_or_else(|e| e.into_inner());
        render(j, waits.get(j.id.as_str()).copied())
    }

    /// End the jobs of agents that are not running: running ones become `interrupted`, queued
    /// ones `cancelled` (ADR-010). Run every tick; [`Jobs::startup`] does the same before READY.
    pub fn reconcile(&self) {
        self.end_stopped(Seal::Now);
    }

    fn end_stopped(&self, seal: Seal) {
        let Ok(active) = self.store.active_jobs() else {
            return;
        };
        for j in active {
            if !matches!(self.agents.state(&j.agent), Ok(Some(State::Running))) {
                let _ = self.step(
                    &j.agent,
                    &j.id,
                    JobEvent::Interrupt,
                    Some("agent is not running"),
                    seal,
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
        // An exec every PROBE (to notice a runner that vanished), or sooner once the runner says it
        // finished; a marker that lies costs at most one exec per MARKER_RETRY.
        {
            let mut book = self.lock();
            let last = book.entry(j.id.to_string()).or_insert(0);
            let since = Duration::from_millis(now.saturating_sub(*last));
            if since < PROBE && (since < MARKER_RETRY || !self.runner.finished(&j.agent, &j.id)) {
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
            Ok(Probe::Gone) => {
                // The window went without the runner's own cleanup: remove the credential file.
                if let Err(e) = self.runner.kill(&j.agent, &j.id) {
                    eprintln!("ac-gateway: jobs: {}/{}: cleanup: {e}", j.agent, j.id);
                }
                (JobEvent::Fail, Some("runner ended without an exit code"))
            }
        };
        self.collect(j);
        let _ = self.transition(&j.agent, &j.id, event, why);
    }

    fn start(&self, j: &Job) {
        // The snapshot comes first, so a job never runs ahead of it; a refused start drops it.
        if let Some(Err(e)) = self.changes.as_ref().map(|c| c.begin(&j.agent, &j.id)) {
            eprintln!(
                "ac-gateway: jobs: {}/{}: start snapshot: {e}",
                j.agent, j.id
            );
            // No compare will run, so the witness must not wait for one: a job that fills the
            // disk cannot make its changes look merely pending.
            self.keep_changes(&j.agent, &j.id, &witness::Changes::Failed);
        }
        if self
            .transition(&j.agent, &j.id, JobEvent::Start, None)
            .is_err()
        {
            if let Some(Err(e)) = self.changes.as_ref().map(|c| c.discard(&j.agent, &j.id)) {
                eprintln!("ac-gateway: jobs: {}/{}: drop snapshot: {e}", j.agent, j.id);
            }
            return;
        }
        let credential = match self.credential(j) {
            Ok(credential) => credential,
            Err(e) => {
                let why = format!("start failed: {e}");
                let _ = self.transition(&j.agent, &j.id, JobEvent::Fail, Some(&why));
                return;
            }
        };
        let started = std::fs::read(self.job_dir(j).join("prompt.txt"))
            .map_err(|e| format!("prompt: {e}"))
            .and_then(|prompt| {
                self.runner.start(
                    &j.agent,
                    &j.id,
                    &prompt,
                    credential.as_ref(),
                    self.model.as_ref(),
                    j.timeout_secs,
                    &self.claude,
                )
            });
        if let Err(e) = started {
            // A start cut short (a failed step, the exec timeout) may have left the credential
            // file in the guest's tmpfs with no window whose exit would remove it.
            if credential.is_some() {
                if let Err(k) = self.runner.kill(&j.agent, &j.id) {
                    eprintln!(
                        "ac-gateway: jobs: {}/{}: cleanup after a failed start: {k}",
                        j.agent, j.id
                    );
                }
            }
            let why = format!("start failed: {e}");
            let _ = self.transition(&j.agent, &j.id, JobEvent::Fail, Some(&why));
        }
    }

    /// Copy the capped result and log to the host, so they outlive the VM.
    fn collect(&self, j: &Job) {
        for part in [Part::Result, Part::Stream] {
            let copied = self.runner.read(&j.agent, &j.id, part).and_then(|bytes| {
                let whole = part == Part::Stream && bytes.len() < STREAM_CAP;
                write_private(&self.job_dir(j).join(part.file()), &self.redact(j, bytes))?;
                match whole {
                    true => write_private(&self.job_dir(j).join(STREAM_WHOLE), b""),
                    false => Ok(()),
                }
            });
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
    /// the agent's history and wake long-polls. A started job that ended then gets its end
    /// snapshot, outside the lock, and is queued for its change note.
    fn transition(
        &self,
        agent: &AgentId,
        id: &JobId,
        event: JobEvent,
        reason: Option<&str>,
    ) -> Result<Job, Reply> {
        self.step(agent, id, event, reason, Seal::Now)
    }

    /// [`Jobs::transition`], taking a started job's end snapshot as `seal` says.
    fn step(
        &self,
        agent: &AgentId,
        id: &JobId,
        event: JobEvent,
        reason: Option<&str>,
        seal: Seal,
    ) -> Result<Job, Reply> {
        let mut book = self.lock();
        let mut j = self.find(agent, id)?;
        let was = j.state;
        j.apply(event, (self.clock)())
            .map_err(|e| error(409, &e.to_string()))?;
        let ended = was == JobState::Running && j.state.is_terminal();
        if ended {
            // Before the record says so, so the sweep sees it running or sees it here.
            let mut restart = self.restart();
            if !restart.swept {
                restart.ended.insert((agent.clone(), id.clone()));
            }
        }
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
            self.secrets().remove(id.as_str());
            self.prune(agent);
        }
        drop(book);
        self.changed.notify_all();
        match (ended, seal) {
            (false, _) => {}
            (true, Seal::Now) => self.seal(agent, id),
            (true, Seal::Later) => self
                .unsealed
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((agent.clone(), id.clone())),
        }
        Ok(j)
    }

    /// Take the job's end snapshot and queue it for its change note, if a start snapshot was
    /// taken. A failed end snapshot is queued too, so the compare deletes the start one.
    fn seal(&self, agent: &AgentId, id: &JobId) {
        let Some(changes) = &self.changes else {
            return;
        };
        match changes.end(agent, id) {
            Ok(false) => return,
            Ok(true) => {}
            Err(e) => eprintln!("ac-gateway: jobs: {agent}/{id}: end snapshot: {e}"),
        }
        self.queue().queue.push_back((agent.clone(), id.clone()));
        self.queued.notify_all();
    }

    fn restart(&self) -> MutexGuard<'_, Restart> {
        self.restart.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queue the compare of every ended job whose snapshots are still on disk: a gateway stopped
    /// with compares queued leaves them, and nothing else would delete them before a purge. Jobs
    /// still queued or running keep theirs, and so do jobs that ended in this run: their own
    /// transition seals and queues them, and a caller's cancel may still be taking the end
    /// snapshot. Run by the first tick, after its reconcile, and by each later one until it
    /// succeeds. Which jobs this run ended is recorded, not read from the clock.
    fn queue_leftovers(&self) {
        if self.restart().swept {
            return;
        }
        let Some(changes) = &self.changes else {
            self.restart().swept = true;
            return;
        };
        let found = match changes.leftovers() {
            Ok(found) => found,
            Err(e) => {
                let mut restart = self.restart();
                if !restart.failed {
                    eprintln!("ac-gateway: jobs: leftover job snapshots: {e}; retrying each tick");
                    restart.failed = true;
                }
                return;
            }
        };
        // Held throughout, so a job that ends meanwhile is either still running in the registry
        // here or already in `ended` (its transition records it first).
        let mut restart = self.restart();
        restart.swept = true;
        let live = |agent: &AgentId, id: &JobId| {
            self.store
                .jobs(agent)
                .is_ok_and(|jobs| jobs.iter().any(|j| j.id == *id && !j.state.is_terminal()))
        };
        let ended: Vec<_> = found
            .into_iter()
            .filter(|(a, j)| !restart.ended.contains(&(a.clone(), j.clone())) && !live(a, j))
            .collect();
        restart.ended = HashSet::new();
        drop(restart);
        if ended.is_empty() {
            return;
        }
        eprintln!(
            "ac-gateway: jobs: comparing {} ended job(s) left from an earlier run",
            ended.len()
        );
        self.queue().queue.extend(ended);
        self.queued.notify_all();
    }

    /// Compare the next ended job's snapshots, taking turns per agent, and note in its agent's
    /// history which paths it changed, waiting up to `wait` for one to be queued. Whether there
    /// was one. The gateway calls this in a loop on a thread of its own, so one job's large
    /// compare delays only the notes queued behind it, and agents take turns in that queue.
    /// Compares of an agent being purged wait until [`Busy::release`].
    pub fn diff_next(&self, wait: Duration) -> bool {
        let next = {
            let mut diffs = self.queue();
            // Cleared here too, in case the last compare panicked.
            diffs.listing = None;
            let (mut diffs, _) = self
                .queued
                .wait_timeout_while(diffs, wait, |d| d.next().is_none())
                .unwrap_or_else(|e| e.into_inner());
            let next = diffs.take();
            diffs.listing = next.as_ref().map(|(agent, _)| agent.clone());
            next
        };
        let (Some((agent, id)), Some(changes)) = (next, &self.changes) else {
            return false;
        };
        let kept = match changes.diff(&agent, &id) {
            Ok(Some(Changed::Paths(paths))) => {
                (self.notes)(&agent, &change_note(&id, &paths));
                witness::Changes::Listed(paths)
            }
            Ok(Some(Changed::TooMany)) => {
                (self.notes)(&agent, &too_many_note(&id));
                witness::Changes::TooMany
            }
            Ok(None) => witness::Changes::None,
            // Noted too, so a job cannot hide what it did by making its listing fail (a send
            // timed out on millions of files); the cause, host paths and all, only in the log.
            Err(e) => {
                eprintln!("ac-gateway: jobs: {agent}/{id}: changes: {e}");
                (self.notes)(&agent, &format!("job {id} changes could not be listed"));
                witness::Changes::Failed
            }
        };
        self.keep_changes(&agent, &id, &kept);
        self.queue().listing = None;
        true
    }

    fn queue(&self) -> MutexGuard<'_, Diffs> {
        self.diffs.lock().unwrap_or_else(|e| e.into_inner())
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

    /// Keep what a job's compare found for its witness record, beside its prompt and output.
    /// A job already pruned has no directory, and nothing is kept.
    fn keep_changes(&self, agent: &AgentId, id: &JobId, changes: &witness::Changes) {
        let dir = self.dir.join(agent.as_str()).join(id.as_str());
        if !dir.is_dir() {
            return;
        }
        let path = dir.join(CHANGES_FILE);
        // A compare that found no start snapshot says nothing new about a start that failed.
        if *changes == witness::Changes::None && path.exists() {
            return;
        }
        let text = witness::render_changes(changes);
        if let Err(e) = write_private(&path, text.as_bytes()) {
            eprintln!("ac-gateway: jobs: {agent}/{id}: keeping changes: {e}");
        }
    }

    fn job_dir(&self, j: &Job) -> PathBuf {
        self.dir.join(j.agent.as_str()).join(j.id.as_str())
    }

    /// The prompt, and with `credential` an empty [`NEEDS_CREDENTIAL`] marker (never the
    /// credential itself), so a job whose credential a restart lost is failed, not started bare.
    fn save_prompt(&self, j: &Job, prompt: &[u8], credential: bool) -> Result<(), String> {
        let dir = self.job_dir(j);
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .map_err(|e| format!("{}: {e}", dir.display()))?;
        if credential {
            write_private(&dir.join(NEEDS_CREDENTIAL), b"")?;
        }
        write_private(&dir.join("prompt.txt"), prompt)
    }

    fn secrets(&self) -> MutexGuard<'_, HashMap<String, Credential>> {
        self.secrets.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The job's credential, `Ok(None)` if it has none, or why it cannot start: it had one, and
    /// the gateway restarted since it was submitted.
    fn credential(&self, j: &Job) -> Result<Option<Credential>, String> {
        if let Some(c) = self.secrets().get(j.id.as_str()) {
            return Ok(Some(c.clone()));
        }
        match self.job_dir(j).join(NEEDS_CREDENTIAL).exists() {
            true => {
                Err("its credential was lost when the gateway restarted; submit it again".into())
            }
            false => Ok(None),
        }
    }

    /// `bytes` with every copy of the job's credential replaced, when it is still known.
    fn redact(&self, j: &Job, bytes: Vec<u8>) -> Vec<u8> {
        match self.secrets().get(j.id.as_str()) {
            Some(c) => redact(&bytes, c.expose().as_bytes()),
            None => bytes,
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, u64>> {
        self.book.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// How a read of the log from the VM stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadEnd {
    /// A short read: the log's end as it stands.
    Eof,
    /// A read failed (no log yet, or the agent stopped).
    Failed,
    /// An ended job's host copy is missing or unreadable.
    NoCopy,
    /// An ended job's host copy holds only the log's tail (it was capped).
    Capped,
    /// The read budget ran out with more to read.
    Limit,
}

impl ReadEnd {
    /// Why the log is incomplete, for [`agui::log_incomplete`]; `None` at its end.
    fn incomplete(self) -> Option<&'static str> {
        match self {
            Self::Eof => None,
            Self::Failed => Some("read_failed"),
            Self::NoCopy => Some("no_copy"),
            Self::Capped => Some("capped"),
            Self::Limit => Some("read_limit"),
        }
    }
}

/// One job's AG-UI run as SSE text, one frame (or keep-alive) per item (#471). It starts with
/// `RUN_STARTED` and the job's state, sends each new state, and turns each complete log line into
/// events ([`agui::line`]) as it polls, then ends with `RUN_FINISHED` or `RUN_ERROR` once the
/// registry says the job ended. While the job runs the log is read from the VM from where the last
/// read stopped, redacted with the job's credential (captured at open: the registry drops it when
/// the job ends, before the last read). A job that had already ended is replayed from the host's
/// copy, which is redacted; line numbers stay those of the live stream only when the copy is the
/// whole log ([`STREAM_WHOLE`]).
pub struct EventStream {
    jobs: Arc<Jobs>,
    agent: AgentId,
    id: JobId,
    secret: Option<Credential>,
    frames: Frames,
    ready: VecDeque<String>,
    /// Log bytes read from the VM so far.
    offset: u64,
    /// The log's unfinished last line.
    partial: Vec<u8>,
    /// The line being read is past [`agui::MAX_LINE`]: dropped up to its newline.
    overlong: bool,
    /// Complete log lines seen: the SSE id of the events they make.
    lines: u64,
    /// Lines the client already has (`after`).
    skip: u64,
    /// Events carry their line number as SSE id: false when replaying a capped copy.
    ids: bool,
    /// The job as last sent.
    last: Option<String>,
    /// The log was read from the VM (the job ran while streamed).
    live: bool,
    /// Once a live-streamed job has ended: the reads its last drain has left ([`FINAL_READS`]).
    /// The drain takes [`EVENTS_READS`] per poll, so frames, keep-alives and the token check
    /// carry on between them.
    final_reads: Option<usize>,
    done: bool,
    first: bool,
}

impl Iterator for EventStream {
    type Item = Vec<u8>;

    fn next(&mut self) -> Option<Vec<u8>> {
        loop {
            if let Some(frame) = self.ready.pop_front() {
                return Some(frame.into_bytes());
            }
            if self.done {
                return None;
            }
            // A draining stream reads on at once: the log no longer grows.
            if !self.first && self.final_reads.is_none() {
                std::thread::sleep(self.jobs.events_poll);
            }
            self.poll();
            if self.ready.is_empty() && !self.done {
                return Some(agui::KEEP_ALIVE.as_bytes().to_vec());
            }
        }
    }
}

impl Drop for EventStream {
    fn drop(&mut self) {
        self.jobs.free_seat(&self.agent);
    }
}

impl EventStream {
    fn poll(&mut self) {
        let j = match self.jobs.find(&self.agent, &self.id) {
            Ok(j) => j,
            Err(_) => return self.fail("job no longer recorded", "gone"),
        };
        if self.first {
            self.first = false;
            self.send(agui::started(&j), None);
        }
        let rendered = render(&j, None);
        if self.last.as_deref() != Some(&rendered) {
            self.send(agui::snapshot(&rendered), None);
            self.last = Some(rendered.clone());
        }
        match j.state {
            JobState::Queued => {}
            JobState::Running => {
                self.live = true;
                self.read_live(EVENTS_READS);
            }
            _ if self.live => self.drain(&j, &rendered),
            _ => {
                if let Some(end) = self.replay_copy(&j) {
                    self.finish(&j, &rendered, end);
                }
            }
        }
    }

    /// One step of a live-streamed job's last drain: the rest of its log, [`EVENTS_READS`] at a
    /// time, then the end. A budget spent exactly at the log's end is checked with one more read,
    /// so it is not reported as incomplete.
    fn drain(&mut self, j: &Job, rendered: &str) {
        let left = *self.final_reads.get_or_insert(FINAL_READS);
        let reads = left.min(EVENTS_READS);
        let mut end = self.read_live(reads);
        self.final_reads = Some(left - reads);
        if end == ReadEnd::Limit && left > reads {
            return;
        }
        if end == ReadEnd::Limit {
            end = match self
                .jobs
                .runner
                .read_from(&self.agent, &self.id, self.offset)
            {
                Ok(rest) if rest.is_empty() => ReadEnd::Eof,
                _ => ReadEnd::Limit,
            };
        }
        self.finish(j, rendered, end);
    }

    /// The run's end: the last partial line, `log_incomplete` if the log was not read to its end,
    /// and `RUN_FINISHED` or `RUN_ERROR` with the job's own outcome.
    fn finish(&mut self, j: &Job, rendered: &str, end: ReadEnd) {
        self.feed(b"\n");
        if let Some(why) = end.incomplete() {
            self.send(agui::log_incomplete(why), None);
        }
        if let Some(last) = agui::ended(j, rendered) {
            self.send(last, None);
        }
        self.done = true;
    }

    /// Feed the host's copy of an ended job's log, and say whether it was all there; `None` once
    /// the stream has failed. A capped copy starts mid-log, so its line numbers are not the live
    /// stream's: it is sent without ids, and a cursor into it is refused. A job that started but
    /// ended without one (cancelled, timed out, or the copy failed) is reported as `no_copy`, not
    /// as an empty log.
    fn replay_copy(&mut self, j: &Job) -> Option<ReadEnd> {
        let dir = self.jobs.job_dir(j);
        let Ok(copy) = std::fs::read(dir.join(Part::Stream.file())) else {
            // A job that never started has no log to miss.
            return Some(match j.started_ms {
                Some(_) => ReadEnd::NoCopy,
                None => ReadEnd::Eof,
            });
        };
        if dir.join(STREAM_WHOLE).exists() {
            self.feed(&copy);
            return Some(ReadEnd::Eof);
        }
        if self.skip > 0 {
            self.fail(
                "the job's log was capped when it ended; replay from this cursor is unavailable",
                "replay_unavailable",
            );
            return None;
        }
        // Only the tail: sent without ids (nothing to resume from), then marked incomplete.
        self.ids = false;
        self.feed(&copy);
        Some(ReadEnd::Capped)
    }

    /// Read up to `reads` chunks of the log from the VM, and say how the reading stopped.
    fn read_live(&mut self, reads: usize) -> ReadEnd {
        for _ in 0..reads {
            let Ok(bytes) = self
                .jobs
                .runner
                .read_from(&self.agent, &self.id, self.offset)
            else {
                return ReadEnd::Failed;
            };
            self.offset += bytes.len() as u64;
            self.feed(&bytes);
            if self.done {
                return ReadEnd::Failed;
            }
            if bytes.len() < EVENTS_CHUNK {
                return ReadEnd::Eof;
            }
        }
        ReadEnd::Limit
    }

    /// Append log bytes and send the events of every line they complete.
    fn feed(&mut self, bytes: &[u8]) {
        for chunk in bytes.split_inclusive(|b| *b == b'\n') {
            let complete = chunk.ends_with(b"\n");
            if !self.overlong {
                self.partial.extend_from_slice(chunk);
                self.overlong = self.partial.len() > agui::MAX_LINE + 1;
            }
            if !complete {
                continue;
            }
            let line = std::mem::take(&mut self.partial);
            let skipped = std::mem::replace(&mut self.overlong, false);
            if line == b"\n" {
                continue;
            }
            self.lines += 1;
            if skipped || self.lines <= self.skip {
                continue;
            }
            let line = match &self.secret {
                Some(c) => redact(&line, c.expose().as_bytes()),
                None => line,
            };
            let events = agui::line(String::from_utf8_lossy(&line).trim_end());
            let last = events.len().saturating_sub(1);
            for (i, kind) in events.into_iter().enumerate() {
                self.send(kind, (self.ids && i == last).then_some(self.lines));
            }
        }
    }

    fn send(&mut self, kind: agui::EventKind, id: Option<u64>) {
        if self.done {
            return;
        }
        match self.frames.frame(kind, id) {
            Ok(frame) => self.ready.push_back(frame),
            Err(rule) => self.fail(&rule, "sequence"),
        }
    }

    fn fail(&mut self, message: &str, code: &str) {
        let frame = self.frames.error(message, code);
        self.ready.push_back(frame);
        self.done = true;
    }
}

impl Busy for Jobs {
    fn counts(&self) -> Option<JobCounts> {
        let active = self.store.active_jobs().ok()?;
        let running = active
            .iter()
            .filter(|j| j.state == JobState::Running)
            .count();
        Some(JobCounts {
            running,
            queued: active.len() - running,
            max_running: self.limits.max_running,
        })
    }

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

    fn settle(&self, id: &AgentId) {
        let mut unsealed = self.unsealed.lock().unwrap_or_else(|e| e.into_inner());
        let (ours, rest): (Vec<_>, Vec<_>) = unsealed.drain(..).partition(|(a, _)| a == id);
        *unsealed = rest;
        for (agent, job) in ours {
            self.seal(&agent, &job);
        }
    }

    fn hold(&self, id: &AgentId) -> bool {
        let mut diffs = self.queue();
        if diffs.listing.as_ref() == Some(id) {
            return false;
        }
        diffs.held.insert(id.clone());
        true
    }

    fn release(&self, id: &AgentId) {
        self.queue().held.remove(id);
        self.queued.notify_all();
    }

    fn forget(&self, id: &AgentId) {
        self.queue().queue.retain(|(agent, _)| agent != id);
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
