//! The terminal viewer (ADR-006): watch an agent's tmux session `main` live, take over its
//! input, and record what it shows.
//!
//! One tmux control client per viewed agent (the bridge, a [`ViewClient`]) feeds every viewer of
//! it; a second one (a [`Writer`]) exists only while a viewer holds the take-over lock. The view
//! guarantee rests on the gateway, not on tmux: non-holders get 403, the view client's whole
//! vocabulary is the closed [`ViewCommand`] enum, and the writer takes only [`Input`], whose
//! bytes are sent as hex. (tmux's own `read-only` flag is not used: while such a client is
//! attached, tmux refuses the agent's own tmux commands too.) Both wrap an [`Exec`], so their
//! stdin never reaches EOF while they run. Routes (bearer token required):
//! - `GET /api/v1/agents/{id}/term`: NDJSON stream ([`ac_viewer::screen`] messages);
//! - `POST /api/v1/agents/{id}/term/{viewer}/{keys|text|paste|resize|takeover|release}`;
//! - `GET /api/v1/agents/{id}/history?limit=N`: recorded output as plain text.

use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ac_ctr::{CtrRuntime, Exec};
use ac_domain::AgentId;
use ac_history::Log;
use ac_viewer::control::{Notice, Parser};
use ac_viewer::input::{self, Input, InputError};
use ac_viewer::record::{self, Record};
use ac_viewer::registry::{Full, Held, Registry};
use ac_viewer::screen::{self, Meta, Screen, DISPLAY_FORMAT};
use ac_viewer::sgr::Line;
use platform::process::Spawner;

use crate::{error, Reply};

/// Viewers of one agent at once.
pub const PER_AGENT: usize = 4;
/// Viewers in all: well under rusty_serve's 64 connections, leaving room for the lifecycle API.
pub const TOTAL: usize = 16;
/// Messages queued per viewer; a viewer that falls this far behind is dropped (it reconnects
/// and gets a full snapshot).
const QUEUE: usize = 256;
/// Keep-alive interval, under rusty_serve's 30 s idle timeout. A vanished browser is noticed
/// when a write fails, which takes one or two of these.
const HEARTBEAT: Duration = Duration::from_secs(5);
/// Longest control-mode line read; longer ones are dropped.
const MAX_LINE: usize = 1 << 20;
/// Take-over commands queued.
const INPUT_QUEUE: usize = 64;
/// History: segment size and total kept per agent.
const SEGMENT_BYTES: u64 = 1 << 20;
const KEEP_BYTES: u64 = 16 << 20;
/// Records returned by the history endpoint, by default and at most.
const HISTORY_DEFAULT: usize = 500;
const HISTORY_MAX: usize = 5000;

/// The view client never sizes the window; the take-over client gets no `%output`.
const VIEW_ATTACH: [&str; 7] = ["tmux", "-C", "attach", "-t", "main", "-f", "ignore-size"];
const WRITER_ATTACH: [&str; 7] = ["tmux", "-C", "attach", "-t", "main", "-f", "no-output"];
const QUIT: &[u8] = b"detach-client\n";

/// The view client's whole vocabulary, rendered by [`ViewCommand::line`] from constants. There is
/// no way to hand that client anything else, so no viewer-supplied value can reach it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewCommand {
    /// The client's own name (to detach it later).
    ClientName,
    /// Pane size, cursor and modes ([`DISPLAY_FORMAT`]).
    CursorInfo,
    /// The visible screen with colours.
    Snapshot,
}

impl ViewCommand {
    /// The command line.
    pub fn line(self) -> String {
        match self {
            Self::ClientName => "display -p '#{client_name}'\n".to_owned(),
            Self::CursorInfo => format!("display -p -t main '{DISPLAY_FORMAT}'\n"),
            Self::Snapshot => "capture-pane -p -e -t main\n".to_owned(),
        }
    }
}

/// The shared control client every viewer of an agent watches through.
pub struct ViewClient(Exec);

impl ViewClient {
    /// Wrap a spawned `tmux -C attach` exec.
    pub fn new(exec: Exec) -> Self {
        Self(exec)
    }

    /// Send one of the fixed commands.
    pub fn send(&mut self, cmd: ViewCommand) -> Result<(), String> {
        self.0.send(cmd.line().as_bytes())
    }

    /// Its control-mode output.
    pub fn take_stdout(&mut self) -> Option<Box<dyn platform::fs::File>> {
        self.0.take_stdout()
    }
}

/// The take-over client: exists only while a viewer holds the lock.
pub struct Writer(Exec);

impl Writer {
    /// Wrap a spawned `tmux -C attach` exec.
    pub fn new(exec: Exec) -> Self {
        Self(exec)
    }

    /// Send validated input: `send-keys -H` hex or `refresh-client -C` lines only.
    pub fn send(&mut self, input: &Input) -> Result<(), String> {
        input
            .lines()
            .iter()
            .try_for_each(|l| self.0.send(l.as_bytes()))
    }
}
/// Detach control clients a crashed gateway left, then make sure the session exists.
const PREPARE: &str = "tmux list-clients -F '#{client_name} #{client_flags}' 2>/dev/null \
    | awk '/control-mode/ { print $1 }' | xargs -r -n1 tmux detach-client -t; \
    tmux has-session -t main 2>/dev/null || tmux new-session -d -s main -c /workspace";

/// How the gateway reaches an agent's tmux. Implemented over `ctr` for [`CtrRuntime`]; tests
/// use a fake.
pub trait Terminal: Send + Sync + 'static {
    /// Detach control clients left by an earlier gateway; create session `main` if missing.
    fn prepare(&self, id: &AgentId) -> Result<(), String>;
    /// Attach the shared view client (with output). Called on the thread that will own it (an
    /// [`Exec`] is not `Send`).
    fn attach_view(&self, id: &AgentId) -> Result<ViewClient, String>;
    /// Attach a take-over client (no output), likewise.
    fn attach_writer(&self, id: &AgentId) -> Result<Writer, String>;
    /// Raw `capture-pane -e` lines of the scrollback above the screen.
    fn scrollback(&self, id: &AgentId) -> Result<Vec<String>, String>;
    /// Detach one tmux client by name.
    fn detach(&self, id: &AgentId, client: &str) -> Result<(), String>;
    /// Whether the agent answers a trivial exec within 2 s.
    fn probe(&self, id: &AgentId) -> bool;
    /// SIGKILL host exec clients of the agent: ours only, or all.
    fn kill_clients(&self, id: &AgentId, ours_only: bool) -> Result<(), String>;
}

impl<S: Spawner + Send + Sync + 'static> Terminal for CtrRuntime<S> {
    fn prepare(&self, id: &AgentId) -> Result<(), String> {
        let limit = Duration::from_secs(10);
        self.exec_once("prepare", id, &["sh", "-c", PREPARE], limit)
            .map(drop)
            .map_err(|e| e.to_string())
    }

    fn attach_view(&self, id: &AgentId) -> Result<ViewClient, String> {
        self.spawn_exec(id, &VIEW_ATTACH, true, QUIT)
            .map(ViewClient::new)
            .map_err(|e| e.to_string())
    }

    fn attach_writer(&self, id: &AgentId) -> Result<Writer, String> {
        self.spawn_exec(id, &WRITER_ATTACH, false, QUIT)
            .map(Writer::new)
            .map_err(|e| e.to_string())
    }

    fn scrollback(&self, id: &AgentId) -> Result<Vec<String>, String> {
        let args = [
            "tmux",
            "capture-pane",
            "-p",
            "-e",
            "-S",
            "-1000",
            "-E",
            "-1",
            "-t",
            "main",
        ];
        let out = self
            .exec_once("scrollback", id, &args, Duration::from_secs(10))
            .map_err(|e| e.to_string())?;
        Ok(String::from_utf8_lossy(&out)
            .lines()
            .map(str::to_owned)
            .collect())
    }

    fn detach(&self, id: &AgentId, client: &str) -> Result<(), String> {
        let args = ["tmux", "detach-client", "-t", client];
        self.exec_once("detach", id, &args, Duration::from_secs(10))
            .map(drop)
            .map_err(|e| e.to_string())
    }

    fn probe(&self, id: &AgentId) -> bool {
        CtrRuntime::probe(self, id)
    }

    fn kill_clients(&self, id: &AgentId, ours_only: bool) -> Result<(), String> {
        self.kill_exec_clients(id, ours_only)
            .map_err(|e| e.to_string())
    }
}

/// What a viewer route answers.
pub enum Answer {
    /// A JSON reply.
    Json(Reply),
    /// The live stream.
    Stream(Stream),
    /// Plain text (the history).
    Text(String),
}

/// A viewer route.
#[derive(Debug, PartialEq, Eq)]
enum Route<'a> {
    Stream(AgentId),
    Action(AgentId, &'a str, &'a str),
    History(AgentId),
}

/// Parse a viewer route: `None` if `path` is not one (the lifecycle API handles it).
fn route<'a>(method: &str, path: &'a str) -> Option<Result<Route<'a>, Reply>> {
    let rest = path.strip_prefix("/api/v1/agents/")?;
    let parts: Vec<&str> = rest.split('/').collect();
    let (id, tail) = parts.split_first()?;
    let want = match tail {
        ["term"] | ["history"] => "GET",
        ["term", _, _] => "POST",
        _ => return None,
    };
    let id = match AgentId::parse(id) {
        Ok(id) => id,
        Err(e) => return Some(Err(error(400, &e.to_string()))),
    };
    if method != want {
        return Some(Err(error(405, "method not allowed")));
    }
    Some(Ok(match tail {
        ["term"] => Route::Stream(id),
        ["history"] => Route::History(id),
        [_, viewer, action] => Route::Action(id, viewer, action),
        _ => return None,
    }))
}

/// Every viewer and bridge. Cheap to clone.
#[derive(Clone)]
pub struct Hub {
    inner: Arc<Inner>,
}

struct Inner {
    term: Box<dyn Terminal>,
    state: Mutex<State>,
    recorder: Recorder,
}

struct State {
    registry: Registry,
    views: HashMap<String, View>,
    next_gen: u64,
}

/// A viewed agent: lives exactly as long as its bridge.
struct View {
    gen: u64,
    viewers: HashMap<String, SyncSender<Vec<u8>>>,
    screen: Option<Screen>,
    client: Option<String>,
    owner: Option<SyncSender<Input>>,
}

impl Hub {
    /// Viewers reach agents through `term`; recordings go under `history_dir/<id>` (mode 0700).
    pub fn new(term: impl Terminal, history_dir: PathBuf) -> Self {
        Self::with_caps(term, history_dir, PER_AGENT, TOTAL)
    }

    /// Like [`Hub::new`] with other viewer caps.
    pub fn with_caps(
        term: impl Terminal,
        history_dir: PathBuf,
        per_agent: usize,
        total: usize,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                term: Box::new(term),
                state: Mutex::new(State {
                    registry: Registry::new(per_agent, total),
                    views: HashMap::new(),
                    next_gen: 0,
                }),
                recorder: Recorder {
                    dir: history_dir,
                    logs: Mutex::new(HashMap::new()),
                },
            }),
        }
    }

    /// The terminal, for the watchdog and startup cleanup.
    pub fn terminal(&self) -> &dyn Terminal {
        self.inner.term.as_ref()
    }

    /// Record an event (a job's state change) in the agent's history log.
    pub fn note(&self, id: &AgentId, text: &str) {
        self.inner.recorder.event(id, text);
    }

    /// Answer a viewer route (`None`: not one). The caller has checked the token and Origin.
    pub fn handle(&self, method: &str, target: &str, body: &[u8]) -> Option<Answer> {
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let route = match route(method, path)? {
            Ok(r) => r,
            Err(reply) => return Some(Answer::Json(reply)),
        };
        Some(match route {
            Route::Stream(id) => self.open(&id),
            Route::History(id) => self.history(&id, query),
            Route::Action(id, viewer, action) => {
                Answer::Json(self.action(&id, viewer, action, query, body))
            }
        })
    }

    fn open(&self, id: &AgentId) -> Answer {
        let viewer = new_viewer_id();
        // Fetched before taking the lock: it is an exec. Fails harmlessly before the session exists.
        let scroll = self
            .inner
            .term
            .scrollback(id)
            .map(|raw| screen::lines(&raw))
            .unwrap_or_default();
        let (tx, rx) = mpsc::sync_channel(QUEUE);
        let start = {
            let mut st = self.inner.lock();
            match st.registry.join(id.as_str(), &viewer) {
                Err(Full::Agent) => {
                    return Answer::Json(error(429, "too many viewers of this agent"))
                }
                Err(Full::Total) => return Answer::Json(error(503, "too many viewers")),
                Ok(()) => {}
            }
            st.next_gen += 1;
            let gen = st.next_gen;
            let owner = st.registry.owner(id.as_str()).map(str::to_owned);
            let view = st.views.entry(id.to_string()).or_insert_with(|| View {
                gen,
                viewers: HashMap::new(),
                screen: None,
                client: None,
                owner: None,
            });
            let _ = tx.try_send(screen::hello(&viewer).into_bytes());
            let _ = tx.try_send(screen::mode(owner.as_deref()).into_bytes());
            // Sent under the lock, so no delta for a newer screen can overtake it.
            if let Some(s) = &view.screen {
                let _ = tx.try_send(screen::full(s, &scroll).into_bytes());
            }
            view.viewers.insert(viewer.clone(), tx);
            (view.gen == gen).then_some(gen)
        };
        if let Some(gen) = start {
            let inner = Arc::clone(&self.inner);
            let id = id.clone();
            std::thread::spawn(move || bridge(inner, id, gen));
        }
        Answer::Stream(Stream {
            rx,
            hub: self.clone(),
            id: id.clone(),
            viewer,
        })
    }

    fn leave(&self, id: &AgentId, viewer: &str) {
        let detach = {
            let mut st = self.inner.lock();
            let event = st.registry.leave(id.as_str(), viewer);
            if let Some(e) = &event {
                self.inner.recorder.event(id, &e.to_string());
            }
            let Some(view) = st.views.get_mut(id.as_str()) else {
                return;
            };
            view.viewers.remove(viewer);
            if event.is_some() {
                view.owner = None;
                broadcast(view, &screen::mode(None));
            }
            if !view.viewers.is_empty() {
                return;
            }
            st.views.remove(id.as_str()).and_then(|v| v.client)
        };
        // Wakes the bridge's blocked read; it sees it is no longer wanted and closes its client.
        if let Some(client) = detach {
            let _ = self.inner.term.detach(id, &client);
        }
    }

    fn action(&self, id: &AgentId, viewer: &str, action: &str, query: &str, body: &[u8]) -> Reply {
        let mut st = self.inner.lock();
        if !st.registry.is_viewer(id.as_str(), viewer) {
            return error(404, "no such viewer");
        }
        match action {
            "takeover" => {
                let force = query.split('&').any(|p| p == "force=true");
                match st.registry.take_over(id.as_str(), viewer, force) {
                    Err(Held(o)) => {
                        error(409, &format!("viewer {o} has control; force to take over"))
                    }
                    Ok(event) => {
                        if let Some(e) = event {
                            self.inner.recorder.event(id, &e.to_string());
                        }
                        if let Some(view) = st.views.get_mut(id.as_str()) {
                            if view.owner.is_none() {
                                let (tx, rx) = mpsc::sync_channel(INPUT_QUEUE);
                                view.owner = Some(tx);
                                let inner = Arc::clone(&self.inner);
                                let id = id.clone();
                                std::thread::spawn(move || writer(inner, id, rx));
                            }
                            broadcast(view, &screen::mode(Some(viewer)));
                        }
                        ok(&format!(r#"{{"owner":"{viewer}"}}"#))
                    }
                }
            }
            "release" => match st.registry.release(id.as_str(), viewer) {
                None => error(409, "you do not have control"),
                Some(e) => {
                    self.inner.recorder.event(id, &e.to_string());
                    if let Some(view) = st.views.get_mut(id.as_str()) {
                        view.owner = None;
                        broadcast(view, &screen::mode(None));
                    }
                    ok(r#"{"owner":null}"#)
                }
            },
            "keys" | "text" | "paste" | "resize" => {
                // View-only input is discarded here, before any command is built.
                if !st.registry.may_write(id.as_str(), viewer) {
                    return error(403, "view-only: input discarded");
                }
                let Some(view) = st.views.get(id.as_str()) else {
                    return error(409, "not attached");
                };
                let meta = view.screen.as_ref().map(|s| s.meta);
                let input = match action {
                    "keys" => input::keys(
                        &String::from_utf8_lossy(body),
                        meta.is_some_and(|m| m.app_cursor),
                    ),
                    "text" => input::text(body),
                    "paste" => input::paste(body, meta.is_some_and(|m| m.bracketed_paste)),
                    _ => size(query).and_then(|(c, r)| input::resize(c, r)),
                };
                let input = match input {
                    Ok(i) => i,
                    Err(InputError::TooLarge) => return error(413, "input too large"),
                    Err(e) => return error(400, &e.to_string()),
                };
                match view.owner.as_ref().map(|tx| tx.try_send(input)) {
                    Some(Ok(())) => ok(r#"{"sent":true}"#),
                    Some(Err(TrySendError::Full(_))) => error(503, "input queue full"),
                    _ => error(503, "take-over client is gone"),
                }
            }
            _ => error(404, "not found"),
        }
    }

    fn history(&self, id: &AgentId, query: &str) -> Answer {
        let limit = query
            .split('&')
            .find_map(|p| p.strip_prefix("limit="))
            .and_then(|v| v.parse().ok())
            .unwrap_or(HISTORY_DEFAULT)
            .min(HISTORY_MAX);
        match self.inner.recorder.tail(id, limit) {
            Ok(records) => Answer::Text(record::render(&records)),
            Err(e) => Answer::Json(error(500, &e)),
        }
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether bridge `gen` is still wanted.
    fn alive(&self, id: &AgentId, gen: u64) -> bool {
        self.lock()
            .views
            .get(id.as_str())
            .is_some_and(|v| v.gen == gen)
    }

    fn set_client(&self, id: &AgentId, gen: u64, name: Option<String>) {
        if let Some(view) = self
            .lock()
            .views
            .get_mut(id.as_str())
            .filter(|v| v.gen == gen)
        {
            view.client = name;
        }
    }

    /// A new snapshot: a full one (with scrollback) the first time, else only what changed.
    fn publish(&self, id: &AgentId, gen: u64, next: Screen, scroll: Option<Vec<Line>>) {
        let mut st = self.lock();
        let Some(view) = st.views.get_mut(id.as_str()).filter(|v| v.gen == gen) else {
            return;
        };
        let msg = match &view.screen {
            Some(prev) => screen::delta(prev, &next),
            None => Some(screen::full(&next, &scroll.unwrap_or_default())),
        };
        view.screen = Some(next);
        if let Some(m) = msg {
            broadcast(view, &m);
        }
    }

    /// The bridge stopped: tell its viewers and drop them (their streams end; pages reconnect).
    fn ended(&self, id: &AgentId, gen: u64, reason: &str) {
        let mut st = self.lock();
        if st.views.get(id.as_str()).is_some_and(|v| v.gen == gen) {
            if let Some(view) = st.views.remove(id.as_str()) {
                for tx in view.viewers.values() {
                    let _ = tx.try_send(screen::bye(reason).into_bytes());
                }
            }
        }
    }
}

/// Send to every viewer; one whose queue is full is dropped rather than waited for.
fn broadcast(view: &mut View, msg: &str) {
    view.viewers
        .retain(|_, tx| tx.try_send(msg.as_bytes().to_vec()).is_ok());
}

/// A viewer's stream: messages, a keep-alive when idle, and on drop (the browser went away,
/// or the viewer was dropped) its seat is given up.
pub struct Stream {
    rx: Receiver<Vec<u8>>,
    hub: Hub,
    id: AgentId,
    viewer: String,
}

impl Iterator for Stream {
    type Item = Vec<u8>;

    fn next(&mut self) -> Option<Vec<u8>> {
        match self.rx.recv_timeout(HEARTBEAT) {
            Ok(msg) => Some(msg),
            Err(RecvTimeoutError::Timeout) => Some(screen::ping().into_bytes()),
            Err(RecvTimeoutError::Disconnected) => None,
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.hub.leave(&self.id, &self.viewer);
    }
}

/// What the bridge is waiting for, in send order.
enum Want {
    Name,
    Meta,
    Capture,
}

fn bridge(inner: Arc<Inner>, id: AgentId, gen: u64) {
    let reason = match run_bridge(&inner, &id, gen) {
        Ok(()) => "session ended".to_owned(),
        Err(e) => e,
    };
    inner.ended(&id, gen, &reason);
}

/// Read the read-only client's lines until it exits or is no longer wanted. Output is recorded
/// and triggers a snapshot; while one is in flight, further output only marks it dirty, which
/// batches bursts without a timer. The client is closed on every way out.
fn run_bridge(inner: &Inner, id: &AgentId, gen: u64) -> Result<(), String> {
    inner.term.prepare(id)?;
    let scroll = inner.term.scrollback(id).map(|r| screen::lines(&r)).ok();
    let mut client = inner.term.attach_view(id)?;
    let stdout = client.take_stdout().ok_or("control client has no stdout")?;
    let mut lines = Lines::new(stdout);
    let mut parser = Parser::default();
    let mut wants = VecDeque::from([Want::Name]);
    client.send(ViewCommand::ClientName)?;
    snapshot(&mut client, &mut wants)?;
    let (mut busy, mut dirty, mut meta, mut scroll) = (true, false, None, scroll);
    while let Some(line) = lines.next_line() {
        if !inner.alive(id, gen) {
            break;
        }
        match parser.line(&line) {
            Some(Notice::Output(bytes)) => {
                inner.recorder.output(id, &bytes);
                if busy {
                    dirty = true;
                } else {
                    snapshot(&mut client, &mut wants)?;
                    busy = true;
                }
            }
            Some(Notice::Reply { ok, lines: reply }) => match wants.pop_front() {
                Some(Want::Name) => inner.set_client(id, gen, reply.into_iter().next()),
                Some(Want::Meta) => {
                    meta = reply.first().filter(|_| ok).and_then(|l| Meta::parse(l))
                }
                Some(Want::Capture) => {
                    if let (true, Some(m)) = (ok, meta.take()) {
                        inner.publish(id, gen, Screen::from_capture(m, &reply), scroll.take());
                    }
                    busy = dirty;
                    if dirty {
                        dirty = false;
                        snapshot(&mut client, &mut wants)?;
                    }
                }
                None => {}
            },
            Some(Notice::Exit) => break,
            None => {}
        }
    }
    Ok(())
}

fn snapshot(client: &mut ViewClient, wants: &mut VecDeque<Want>) -> Result<(), String> {
    client.send(ViewCommand::CursorInfo)?;
    client.send(ViewCommand::Snapshot)?;
    wants.extend([Want::Meta, Want::Capture]);
    Ok(())
}

/// The take-over client: forwards the lock holder's commands until the lock is released (the
/// sender is dropped), then closes.
fn writer(inner: Arc<Inner>, id: AgentId, rx: Receiver<Input>) {
    let mut writer = match inner.term.attach_writer(&id) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("ac-gateway: viewer: {id}: take-over client: {e}");
            return;
        }
    };
    while let Ok(input) = rx.recv() {
        if writer.send(&input).is_err() {
            break;
        }
    }
}

/// Lines from a control client's stdout; a line longer than [`MAX_LINE`] is dropped whole.
struct Lines {
    file: Box<dyn platform::fs::File>,
    buf: Vec<u8>,
    overlong: bool,
}

impl Lines {
    fn new(file: Box<dyn platform::fs::File>) -> Self {
        Self {
            file,
            buf: Vec::new(),
            overlong: false,
        }
    }

    fn next_line(&mut self) -> Option<Vec<u8>> {
        let mut chunk = [0u8; 8192];
        loop {
            if let Some(end) = self.buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=end).collect();
                if std::mem::take(&mut self.overlong) || line.len() > MAX_LINE {
                    continue;
                }
                return Some(line);
            }
            if self.buf.len() > MAX_LINE {
                self.buf.clear();
                self.overlong = true;
            }
            match self.file.read(&mut chunk) {
                Ok(0) | Err(_) => return None,
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
            }
        }
    }
}

/// Per-agent recordings, one [`Log`] each, all writes and reads through this lock.
struct Recorder {
    dir: PathBuf,
    logs: Mutex<HashMap<String, Log>>,
}

impl Recorder {
    fn output(&self, id: &AgentId, bytes: &[u8]) {
        self.append(
            id,
            &Record::Output {
                ms: now_ms(),
                bytes: bytes.to_vec(),
            },
            false,
        );
    }

    fn event(&self, id: &AgentId, text: &str) {
        self.append(
            id,
            &Record::Event {
                ms: now_ms(),
                text: text.to_owned(),
            },
            true,
        );
    }

    fn append(&self, id: &AgentId, record: &Record, sync: bool) {
        let mut logs = self.logs.lock().unwrap_or_else(|e| e.into_inner());
        let result = self.open(&mut logs, id).and_then(|log| {
            let seq = log.append(&record.encode()).map_err(|e| e.to_string())?;
            if seq % 64 == 0 {
                log.prune(KEEP_BYTES).map_err(|e| e.to_string())?;
            }
            if sync {
                log.sync().map_err(|e| e.to_string())?;
            }
            Ok(())
        });
        if let Err(e) = result {
            eprintln!("ac-gateway: history {id}: {e}");
        }
    }

    fn tail(&self, id: &AgentId, n: usize) -> Result<Vec<Record>, String> {
        let mut logs = self.logs.lock().unwrap_or_else(|e| e.into_inner());
        if !logs.contains_key(id.as_str()) && !self.dir.join(id.as_str()).is_dir() {
            return Ok(Vec::new());
        }
        let log = self.open(&mut logs, id)?;
        let entries = log.tail(n).map_err(|e| e.to_string())?;
        Ok(entries
            .iter()
            .filter_map(|e| Record::decode(&e.payload))
            .collect())
    }

    fn open<'a>(
        &self,
        logs: &'a mut HashMap<String, Log>,
        id: &AgentId,
    ) -> Result<&'a mut Log, String> {
        if !logs.contains_key(id.as_str()) {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&self.dir)
                .map_err(|e| format!("{}: {e}", self.dir.display()))?;
            let log =
                Log::open(self.dir.join(id.as_str()), SEGMENT_BYTES).map_err(|e| e.to_string())?;
            logs.insert(id.to_string(), log);
        }
        logs.get_mut(id.as_str())
            .ok_or_else(|| "log vanished".to_owned())
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// 16 hex digits from the kernel's random source (a counter if it is unreadable: unique, if
/// guessable; every request needs the token anyway).
fn new_viewer_id() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let mut b = [0u8; 8];
    let random = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b));
    if random.is_err() {
        b = (now_ms() ^ N.fetch_add(1, Ordering::Relaxed) << 48).to_le_bytes();
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn size(query: &str) -> Result<(u16, u16), InputError> {
    let get = |k: &str| {
        query
            .split('&')
            .find_map(|p| p.strip_prefix(k)?.strip_prefix('='))
            .and_then(|v| v.parse().ok())
    };
    match (get("cols"), get("rows")) {
        (Some(c), Some(r)) => Ok((c, r)),
        _ => Err(InputError::BadSize),
    }
}

fn ok(body: &str) -> Reply {
    Reply {
        status: 200,
        body: body.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> AgentId {
        AgentId::parse("a1").unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn routes() {
        assert_eq!(
            route("GET", "/api/v1/agents/a1/term").map(Result::ok),
            Some(Some(Route::Stream(id())))
        );
        assert_eq!(
            route("GET", "/api/v1/agents/a1/history").map(Result::ok),
            Some(Some(Route::History(id())))
        );
        assert_eq!(
            route("POST", "/api/v1/agents/a1/term/v1/keys").map(Result::ok),
            Some(Some(Route::Action(id(), "v1", "keys")))
        );
        assert_eq!(
            route("POST", "/api/v1/agents/a1/term").map(|r| r.err().map(|e| e.status)),
            Some(Some(405))
        );
        assert_eq!(
            route("GET", "/api/v1/agents/a1/term/v1/keys").map(|r| r.err().map(|e| e.status)),
            Some(Some(405))
        );
        assert_eq!(
            route("GET", "/api/v1/agents/..%2F/term").map(|r| r.err().map(|e| e.status)),
            Some(Some(400))
        );
        for other in [
            "/api/v1/agents/a1",
            "/api/v1/agents/a1/pause",
            "/api/v1/agents",
            "/ui/a1",
            "/api/v1/agents/a1/term/v1",
        ] {
            assert!(route("GET", other).is_none(), "{other}");
        }
    }

    struct Bytes(VecDeque<u8>, usize);
    impl platform::fs::File for Bytes {
        fn read(&mut self, buf: &mut [u8]) -> platform::error::Result<usize> {
            let n = buf.len().min(self.0.len()).min(self.1);
            for b in buf.iter_mut().take(n) {
                *b = self.0.pop_front().unwrap_or_default();
            }
            Ok(n)
        }
        fn write(&mut self, b: &[u8]) -> platform::error::Result<usize> {
            Ok(b.len())
        }
        fn flush(&mut self) -> platform::error::Result<()> {
            Ok(())
        }
        fn sync_all(&mut self) -> platform::error::Result<()> {
            Ok(())
        }
        fn try_clone(&self) -> platform::error::Result<Box<dyn platform::fs::File>> {
            Ok(Box::new(Bytes(self.0.clone(), self.1)))
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    #[test]
    fn lines_split_across_reads_and_overlong_lines_are_dropped() {
        let mut data = b"one\ntw".to_vec();
        data.extend_from_slice(b"o\n");
        data.extend(std::iter::repeat_n(b'x', MAX_LINE + 10));
        data.extend_from_slice(b"\nthree\nunterminated");
        let mut lines = Lines::new(Box::new(Bytes(data.into(), 3000)));
        let got: Vec<Vec<u8>> = std::iter::from_fn(|| lines.next_line()).collect();
        assert_eq!(
            got,
            [b"one\n".to_vec(), b"two\n".to_vec(), b"three\n".to_vec()]
        );
    }

    #[test]
    fn size_query() {
        assert_eq!(size("cols=120&rows=40"), Ok((120, 40)));
        assert_eq!(size("rows=40"), Err(InputError::BadSize));
        assert_eq!(size("cols=x&rows=40"), Err(InputError::BadSize));
    }

    #[test]
    fn viewer_ids_are_hex_and_differ() {
        let (a, b) = (new_viewer_id(), new_viewer_id());
        assert!(a.len() == 16 && a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
