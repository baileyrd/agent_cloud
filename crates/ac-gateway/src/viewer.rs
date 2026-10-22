//! The terminal viewer (ADR-006): watch an agent's tmux session `main` live, take over its
//! input, and record what it shows.
//!
//! One tmux control client per viewed agent (the bridge, a [`ViewClient`]) feeds every viewer of
//! it; a second one (a [`Writer`]) exists only while a viewer holds the take-over lock. The view
//! guarantee rests on the gateway, not on tmux: non-holders get 403, the view client's whole
//! vocabulary is the closed [`ViewCommand`] enum, and the writer takes only [`Input`], whose
//! bytes are sent as hex. (tmux's own `read-only` flag is not used: while such a client is
//! attached, tmux refuses the agent's own tmux commands too.) Both wrap an [`Exec`], so their
//! stdin never reaches EOF while they run. A viewer id works only with the token that opened it.
//!
//! Each viewer watches a [`Target`]: the window the agent has current (the default), or a window it
//! picked from the list the bridge keeps (`list-windows`, refreshed on tmux's window notices). A
//! pick never runs `select-window`, which would move the agent too: the bridge snapshots every
//! watched window by id (`capture-pane -t @<n>`), and a taker's input goes to the active pane of
//! the window it watches (`send-keys -t %<n>`, the pane id from that list). The only viewer text
//! that reaches a command is a window id the server parsed into a number and found in the list.
//! When a watched window closes, its viewers fall back to the agent's window.
//!
//! Routes (scopes in [`crate::access`]):
//! - `GET /api/v1/agents/{id}/term`: NDJSON stream ([`ac_viewer::screen`] and
//!   [`ac_viewer::windows`] messages);
//! - `POST /api/v1/agents/{id}/term/{viewer}/{keys|text|paste|resize|takeover|release}`;
//! - `POST /api/v1/agents/{id}/term/{viewer}/window`, body `@<n>` or `active`: watch that window;
//! - `GET /api/v1/agents/{id}/history?limit=N`: recorded output as plain text.

use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
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
use ac_viewer::windows::{self, PaneId, Target, Window, WindowId, LIST_FORMAT};
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
/// Window picks being captured at once (each is an exec); more get 429.
const MAX_PICKS: usize = 4;

/// The view client never sizes the window; the take-over client gets no `%output`.
const VIEW_ATTACH: [&str; 7] = ["tmux", "-C", "attach", "-t", "main", "-f", "ignore-size"];
const WRITER_ATTACH: [&str; 7] = ["tmux", "-C", "attach", "-t", "main", "-f", "no-output"];
const QUIT: &[u8] = b"detach-client\n";

/// The view client's whole vocabulary, rendered by [`ViewCommand::line`] from constants and, for a
/// [`Target`], `main` or a window id rendered from its number. There is no way to hand that client
/// anything else, so no viewer-supplied text can reach it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewCommand {
    /// The client's own name (to detach it later).
    ClientName,
    /// Pane size, cursor and modes ([`DISPLAY_FORMAT`]) of the target's active pane.
    CursorInfo(Target),
    /// The target's visible screen with colours.
    Snapshot(Target),
    /// The windows of `main` ([`LIST_FORMAT`]).
    ListWindows,
}

impl ViewCommand {
    /// The command line.
    pub fn line(self) -> String {
        match self {
            Self::ClientName => "display -p '#{client_name}'\n".to_owned(),
            Self::CursorInfo(t) => format!("display -p -t {t} '{DISPLAY_FORMAT}'\n"),
            Self::Snapshot(t) => format!("capture-pane -p -e -t {t}\n"),
            Self::ListWindows => format!("list-windows -t main -F '{LIST_FORMAT}'\n"),
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

    /// Send validated input to `pane` (`None`: `main`'s active pane): `send-keys -H` hex or
    /// `refresh-client -C` lines only.
    pub fn send(&mut self, input: &Input, pane: Option<PaneId>) -> Result<(), String> {
        input
            .lines(pane)
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
    /// What `target` shows, in one exec: a [`DISPLAY_FORMAT`] line, then raw `capture-pane -e`
    /// lines of up to 1000 lines of scrollback and the screen (see [`split_capture`]). Fails if
    /// the window is gone.
    fn capture(&self, id: &AgentId, target: Target) -> Result<Vec<String>, String>;
    /// Detach one tmux client by name.
    fn detach(&self, id: &AgentId, client: &str) -> Result<(), String>;
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

    fn capture(&self, id: &AgentId, target: Target) -> Result<Vec<String>, String> {
        let t = target.to_string();
        let args = [
            "tmux",
            "display",
            "-p",
            "-t",
            &t,
            DISPLAY_FORMAT,
            ";",
            "capture-pane",
            "-p",
            "-e",
            "-S",
            "-1000",
            "-t",
            &t,
        ];
        let out = self
            .exec_once("capture", id, &args, Duration::from_secs(10))
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
pub(crate) enum Route<'a> {
    Stream(AgentId),
    Action(AgentId, &'a str, Action),
    History(AgentId),
}

/// What a viewer asks for under `term/{viewer}/`: a closed set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    Takeover,
    Release,
    Keys,
    Text,
    Paste,
    Resize,
    Window,
}

impl Action {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "takeover" => Self::Takeover,
            "release" => Self::Release,
            "keys" => Self::Keys,
            "text" => Self::Text,
            "paste" => Self::Paste,
            "resize" => Self::Resize,
            "window" => Self::Window,
            _ => return None,
        })
    }
}

/// Parse a viewer route: `None` if `path` is not one (the lifecycle API handles it).
pub(crate) fn route<'a>(method: &str, path: &'a str) -> Option<Result<Route<'a>, Reply>> {
    let rest = path.strip_prefix("/api/v1/agents/")?;
    let parts: Vec<&str> = rest.split('/').collect();
    let (id, tail) = parts.split_first()?;
    let want = match tail {
        ["term"] | ["history"] => "GET",
        ["term", v, a] if !v.is_empty() && Action::parse(a).is_some() => "POST",
        ["term", _, _] => return Some(Err(error(404, "not found"))),
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
        [_, viewer, action] => Route::Action(id, viewer, Action::parse(action)?),
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
    picks: AtomicUsize,
}

struct State {
    registry: Registry,
    views: HashMap<String, View>,
    /// The token (name) each viewer id belongs to.
    tokens: HashMap<String, String>,
    next_gen: u64,
}

/// A viewed agent: lives exactly as long as its bridge.
struct View {
    gen: u64,
    viewers: HashMap<String, Seat>,
    /// The latest screen of each watched target ([`Target::Active`] always).
    screens: HashMap<Target, Screen>,
    /// The latest window list.
    windows: Vec<Window>,
    client: Option<String>,
    owner: Option<SyncSender<(Input, Option<PaneId>)>>,
}

/// One viewer of a view: its stream and what it watches.
struct Seat {
    tx: SyncSender<Vec<u8>>,
    target: Target,
}

impl View {
    /// The active pane of `target`: `None` for the agent's window; `Err` if the window is gone.
    fn pane(&self, target: Target) -> Result<Option<PaneId>, Reply> {
        match target {
            Target::Active => Ok(None),
            Target::Window(w) => self
                .windows
                .iter()
                .find(|x| x.id == w)
                .map(|x| Some(x.pane))
                .ok_or_else(|| error(409, "window closed")),
        }
    }

    /// Viewers of windows that `keep` rejects go back to the agent's window, with a full screen.
    fn fall_back(&mut self, keep: impl Fn(WindowId) -> bool) {
        let active = self
            .screens
            .get(&Target::Active)
            .map(|s| screen::full(s, &[]));
        for seat in self.viewers.values_mut() {
            if let Target::Window(w) = seat.target {
                if !keep(w) {
                    seat.target = Target::Active;
                    let _ = seat
                        .tx
                        .try_send(windows::target(Target::Active).into_bytes());
                    if let Some(full) = &active {
                        let _ = seat.tx.try_send(full.clone().into_bytes());
                    }
                }
            }
        }
        self.prune();
    }

    /// Forget screens of windows nobody watches, so a later pick starts from a fresh capture.
    fn prune(&mut self) {
        let watched: Vec<Target> = self.viewers.values().map(|s| s.target).collect();
        self.screens
            .retain(|t, _| *t == Target::Active || watched.contains(t));
    }
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
                    tokens: HashMap::new(),
                    next_gen: 0,
                }),
                recorder: Recorder {
                    dir: history_dir,
                    logs: Mutex::new(HashMap::new()),
                },
                picks: AtomicUsize::new(0),
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

    /// Answer a viewer route (`None`: not one) for the token named `who`. The caller has checked
    /// the token, its scope and Origin.
    pub fn handle(&self, method: &str, target: &str, body: &[u8], who: &str) -> Option<Answer> {
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let route = match route(method, path)? {
            Ok(r) => r,
            Err(reply) => return Some(Answer::Json(reply)),
        };
        Some(match route {
            Route::Stream(id) => self.open(&id, who),
            Route::History(id) => self.history(&id, query),
            Route::Action(id, viewer, Action::Window) => {
                Answer::Json(self.pick(&id, viewer, body, who))
            }
            Route::Action(id, viewer, action) => {
                Answer::Json(self.action(&id, viewer, action, query, body, who))
            }
        })
    }

    fn open(&self, id: &AgentId, who: &str) -> Answer {
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
                screens: HashMap::new(),
                windows: Vec::new(),
                client: None,
                owner: None,
            });
            let _ = tx.try_send(screen::hello(&viewer).into_bytes());
            let _ = tx.try_send(screen::mode(owner.as_deref()).into_bytes());
            if !view.windows.is_empty() {
                let _ = tx.try_send(windows::message(&view.windows).into_bytes());
            }
            // Sent under the lock, so no delta for a newer screen can overtake it.
            if let Some(s) = view.screens.get(&Target::Active) {
                let _ = tx.try_send(screen::full(s, &scroll).into_bytes());
            }
            let seat = Seat {
                tx,
                target: Target::Active,
            };
            view.viewers.insert(viewer.clone(), seat);
            let start = (view.gen == gen).then_some(gen);
            st.tokens.insert(viewer.clone(), who.to_owned());
            start
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
            st.tokens.remove(viewer);
            let event = st.registry.leave(id.as_str(), viewer);
            if let Some(e) = &event {
                self.inner.recorder.event(id, &e.to_string());
            }
            let Some(view) = st.views.get_mut(id.as_str()) else {
                return;
            };
            view.viewers.remove(viewer);
            view.prune();
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

    fn action(
        &self,
        id: &AgentId,
        viewer: &str,
        action: Action,
        query: &str,
        body: &[u8],
        who: &str,
    ) -> Reply {
        let mut st = self.inner.lock();
        if !st.owns(id, viewer, who) {
            return error(404, "no such viewer");
        }
        match action {
            Action::Takeover => {
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
            Action::Release => match st.registry.release(id.as_str(), viewer) {
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
            Action::Keys | Action::Text | Action::Paste | Action::Resize => {
                // View-only input is discarded here, before any command is built.
                if !st.registry.may_write(id.as_str(), viewer) {
                    return error(403, "view-only: input discarded");
                }
                let Some(view) = st.views.get(id.as_str()) else {
                    return error(409, "not attached");
                };
                let target = view
                    .viewers
                    .get(viewer)
                    .map_or(Target::Active, |s| s.target);
                let pane = match view.pane(target) {
                    Ok(p) => p,
                    Err(reply) => return reply,
                };
                let meta = view.screens.get(&target).map(|s| s.meta);
                let input = match action {
                    Action::Keys => input::keys(
                        &String::from_utf8_lossy(body),
                        meta.is_some_and(|m| m.app_cursor),
                    ),
                    Action::Text => input::text(body),
                    Action::Paste => input::paste(body, meta.is_some_and(|m| m.bracketed_paste)),
                    _ => size(query).and_then(|(c, r)| input::resize(c, r)),
                };
                let input = match input {
                    Ok(i) => i,
                    Err(InputError::TooLarge) => return error(413, "input too large"),
                    Err(e) => return error(400, &e.to_string()),
                };
                match view.owner.as_ref().map(|tx| tx.try_send((input, pane))) {
                    Some(Ok(())) => ok(r#"{"sent":true}"#),
                    Some(Err(TrySendError::Full(_))) => error(503, "input queue full"),
                    _ => error(503, "take-over client is gone"),
                }
            }
            Action::Window => error(500, "a window pick is not an input action"),
        }
    }

    /// Watch another window: `body` is exactly `active` (the agent's window) or the id of a window in
    /// the latest list, else 400. One exec captures it (with scrollback) for this viewer; from then
    /// on the bridge snapshots it with the others.
    fn pick(&self, id: &AgentId, viewer: &str, body: &[u8], who: &str) -> Reply {
        let target = {
            let st = self.inner.lock();
            if !st.owns(id, viewer, who) {
                return error(404, "no such viewer");
            }
            let Some(view) = st.views.get(id.as_str()) else {
                return error(409, "not attached");
            };
            match pick_target(body, &view.windows) {
                Some(t) => t,
                None => {
                    return error(
                        400,
                        "body must be `active` or the id (@<digits>) of a window in the list",
                    )
                }
            }
        };
        if self.inner.picks.fetch_add(1, Ordering::SeqCst) >= MAX_PICKS {
            self.inner.picks.fetch_sub(1, Ordering::SeqCst);
            return error(429, "too many window changes at once");
        }
        let captured = self.inner.term.capture(id, target);
        self.inner.picks.fetch_sub(1, Ordering::SeqCst);
        let Some((fresh, scroll)) = captured.ok().and_then(|raw| split_capture(&raw)) else {
            return error(409, "window closed");
        };
        let mut st = self.inner.lock();
        if !st.owns(id, viewer, who) {
            return error(404, "no such viewer");
        }
        let Some(view) = st.views.get_mut(id.as_str()) else {
            return error(409, "not attached");
        };
        if view.pane(target).is_err() {
            return error(409, "window closed");
        }
        // A screen someone already watches is the one later deltas follow: send that one.
        let full = screen::full(view.screens.entry(target).or_insert(fresh), &scroll);
        if let Some(seat) = view.viewers.get_mut(viewer) {
            seat.target = target;
            let _ = seat.tx.try_send(windows::target(target).into_bytes());
            let _ = seat.tx.try_send(full.into_bytes());
        }
        view.prune();
        let window = match target {
            Target::Active => "null".to_owned(),
            Target::Window(w) => format!("\"{w}\""),
        };
        ok(&format!(r#"{{"window":{window}}}"#))
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

impl State {
    /// Whether `viewer` watches `id` and was opened by the token named `who`.
    fn owns(&self, id: &AgentId, viewer: &str, who: &str) -> bool {
        self.registry.is_viewer(id.as_str(), viewer)
            && self.tokens.get(viewer).map(String::as_str) == Some(who)
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

    /// A new snapshot of `target`, for the viewers watching it: a full one (with scrollback) the
    /// first time, else only what changed.
    fn publish(
        &self,
        id: &AgentId,
        gen: u64,
        target: Target,
        next: Screen,
        scroll: Option<Vec<Line>>,
    ) {
        let mut st = self.lock();
        let Some(view) = st.views.get_mut(id.as_str()).filter(|v| v.gen == gen) else {
            return;
        };
        let msg = match view.screens.get(&target) {
            Some(prev) => screen::delta(prev, &next),
            None => Some(screen::full(&next, &scroll.unwrap_or_default())),
        };
        view.screens.insert(target, next);
        if let Some(m) = msg {
            view.viewers
                .retain(|_, s| s.target != target || s.tx.try_send(m.as_bytes().to_vec()).is_ok());
        }
    }

    /// A new window list: sent to every viewer if it changed; viewers of windows no longer in it
    /// fall back to the agent's window.
    fn set_windows(&self, id: &AgentId, gen: u64, list: Vec<Window>) {
        let mut st = self.lock();
        let Some(view) = st.views.get_mut(id.as_str()).filter(|v| v.gen == gen) else {
            return;
        };
        if view.windows != list {
            broadcast(view, &windows::message(&list));
            view.windows = list;
        }
        let ids: Vec<WindowId> = view.windows.iter().map(|w| w.id).collect();
        view.fall_back(|w| ids.contains(&w));
    }

    /// Window `w` could not be captured: it closed. Its viewers fall back to the agent's window.
    fn gone(&self, id: &AgentId, gen: u64, w: WindowId) {
        let mut st = self.lock();
        if let Some(view) = st.views.get_mut(id.as_str()).filter(|v| v.gen == gen) {
            view.windows.retain(|x| x.id != w);
            view.fall_back(|x| x != w);
        }
    }

    /// What to snapshot: the agent's window, then every window a viewer watches.
    fn targets(&self, id: &AgentId, gen: u64) -> Vec<Target> {
        let mut out = vec![Target::Active];
        if let Some(view) = self.lock().views.get(id.as_str()).filter(|v| v.gen == gen) {
            for seat in view.viewers.values() {
                if !out.contains(&seat.target) {
                    out.push(seat.target);
                }
            }
        }
        out
    }

    /// The bridge stopped: tell its viewers and drop them (their streams end; pages reconnect).
    fn ended(&self, id: &AgentId, gen: u64, reason: &str) {
        let mut st = self.lock();
        if st.views.get(id.as_str()).is_some_and(|v| v.gen == gen) {
            if let Some(view) = st.views.remove(id.as_str()) {
                for seat in view.viewers.values() {
                    let _ = seat.tx.try_send(screen::bye(reason).into_bytes());
                }
            }
        }
    }
}

/// Send to every viewer; one whose queue is full is dropped rather than waited for.
fn broadcast(view: &mut View, msg: &str) {
    view.viewers
        .retain(|_, s| s.tx.try_send(msg.as_bytes().to_vec()).is_ok());
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
    Windows,
    Meta(Target),
    Capture(Target),
}

fn bridge(inner: Arc<Inner>, id: AgentId, gen: u64) {
    let reason = match run_bridge(&inner, &id, gen) {
        Ok(()) => "session ended".to_owned(),
        Err(e) => e,
    };
    inner.ended(&id, gen, &reason);
}

/// Read the read-only client's lines until it exits or is no longer wanted. Output is recorded
/// and triggers a snapshot of every watched target; while one is in flight, further output only
/// marks it dirty, which batches bursts without a timer. A window notice refreshes the list and
/// the snapshots (switching windows prints nothing). The client is closed on every way out.
fn run_bridge(inner: &Inner, id: &AgentId, gen: u64) -> Result<(), String> {
    inner.term.prepare(id)?;
    let scroll = inner.term.scrollback(id).map(|r| screen::lines(&r)).ok();
    let mut client = inner.term.attach_view(id)?;
    let stdout = client.take_stdout().ok_or("control client has no stdout")?;
    let mut lines = Lines::new(stdout);
    let mut parser = Parser::default();
    let mut wants = VecDeque::from([Want::Name, Want::Windows]);
    client.send(ViewCommand::ClientName)?;
    client.send(ViewCommand::ListWindows)?;
    let (mut dirty, mut meta, mut scroll) = (true, None, scroll);
    loop {
        let capturing = wants.iter().any(|w| matches!(w, Want::Capture(_)));
        if dirty && !capturing {
            dirty = false;
            for t in inner.targets(id, gen) {
                client.send(ViewCommand::CursorInfo(t))?;
                client.send(ViewCommand::Snapshot(t))?;
                wants.extend([Want::Meta(t), Want::Capture(t)]);
            }
        }
        let Some(line) = lines.next_line() else { break };
        if !inner.alive(id, gen) {
            break;
        }
        match parser.line(&line) {
            Some(Notice::Output(bytes)) => {
                inner.recorder.output(id, &bytes);
                dirty = true;
            }
            Some(Notice::Windows) => {
                client.send(ViewCommand::ListWindows)?;
                wants.push_back(Want::Windows);
                dirty = true;
            }
            Some(Notice::Reply { ok, lines: reply }) => match wants.pop_front() {
                Some(Want::Name) => inner.set_client(id, gen, reply.into_iter().next()),
                Some(Want::Windows) => {
                    if ok {
                        inner.set_windows(id, gen, windows::parse_list(&reply));
                    }
                }
                Some(Want::Meta(t)) => {
                    meta = reply
                        .first()
                        .filter(|_| ok)
                        .and_then(|l| Meta::parse(l))
                        .map(|m| (t, m));
                }
                Some(Want::Capture(t)) => match (ok, meta.take()) {
                    (true, Some((mt, m))) if mt == t => {
                        let scroll = if t == Target::Active {
                            scroll.take()
                        } else {
                            None
                        };
                        inner.publish(id, gen, t, Screen::from_capture(m, &reply), scroll);
                    }
                    // A missing window: `display` prints blanks, `capture-pane` fails.
                    _ => {
                        if let Target::Window(w) = t {
                            inner.gone(id, gen, w);
                        }
                    }
                },
                None => {}
            },
            Some(Notice::Exit) => break,
            None => {}
        }
    }
    Ok(())
}

/// The window a pick names: exactly `active`, or a window id (`@<digits>`) in `list`.
fn pick_target(body: &[u8], list: &[Window]) -> Option<Target> {
    let text = std::str::from_utf8(body).ok()?;
    if text == "active" {
        return Some(Target::Active);
    }
    let w = WindowId::parse(text)?;
    list.iter().any(|x| x.id == w).then_some(Target::Window(w))
}

/// [`Terminal::capture`] output as the screen (the last `height` lines) and the scrollback above it.
fn split_capture(raw: &[String]) -> Option<(Screen, Vec<Line>)> {
    let (first, rest) = raw.split_first()?;
    let meta = Meta::parse(first)?;
    let at = rest.len().checked_sub(usize::from(meta.height))?;
    let (scroll, shown) = rest.split_at(at);
    Some((Screen::from_capture(meta, shown), screen::lines(scroll)))
}

/// The take-over client: forwards the lock holder's commands until the lock is released (the
/// sender is dropped), then closes.
fn writer(inner: Arc<Inner>, id: AgentId, rx: Receiver<(Input, Option<PaneId>)>) {
    let mut writer = match inner.term.attach_writer(&id) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("ac-gateway: viewer: {id}: take-over client: {e}");
            return;
        }
    };
    while let Ok((input, pane)) = rx.recv() {
        if writer.send(&input, pane).is_err() {
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
            Some(Some(Route::Action(id(), "v1", Action::Keys)))
        );
        assert_eq!(
            route("POST", "/api/v1/agents/a1/term/v1/explode").map(|r| r.err().map(|e| e.status)),
            Some(Some(404))
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

    fn list() -> Vec<Window> {
        windows::parse_list(&["@0 %0 0 1 bash".to_owned(), "@3 %5 1 0 job-1".to_owned()])
    }

    #[test]
    fn a_pick_is_active_or_a_listed_window_id_and_nothing_else() {
        let w3 = WindowId::parse("@3").map(Target::Window);
        assert_eq!(pick_target(b"@3", &list()), w3);
        assert_eq!(pick_target(b"active", &list()), Some(Target::Active));
        let huge = format!("@{}", "9".repeat(100_000));
        for bad in [
            "@1;kill-server",
            "@3;kill-server",
            "@3 ",
            " @3",
            "@3\n",
            "%5",
            "%1",
            "",
            "@",
            "@7",
            "@03",
            &huge,
            "@\u{0663}",
            "\u{ff20}3",
            "@3\u{0}",
            "Active",
            "main",
            "@0\tactive",
        ] {
            assert_eq!(pick_target(bad.as_bytes(), &list()), None, "{bad:?}");
        }
        assert_eq!(pick_target(b"@3\xff", &list()), None);
        assert_eq!(pick_target(b"@3", &[]), None, "not in the latest list");
    }

    #[test]
    fn view_commands_name_only_main_or_a_window_number() {
        let w = WindowId::parse("@3").map_or(Target::Active, Target::Window);
        assert_eq!(
            ViewCommand::Snapshot(Target::Active).line(),
            "capture-pane -p -e -t main\n"
        );
        assert_eq!(
            ViewCommand::Snapshot(w).line(),
            "capture-pane -p -e -t @3\n"
        );
        assert!(ViewCommand::CursorInfo(w)
            .line()
            .starts_with("display -p -t @3 '#{pane_width}"));
        assert_eq!(
            ViewCommand::ListWindows.line(),
            format!("list-windows -t main -F '{LIST_FORMAT}'\n")
        );
    }

    #[test]
    fn a_capture_splits_into_screen_and_scrollback() {
        let raw: Vec<String> = ["3 2 0 1 1 0 0", "old 1", "old 2", "row 1", "row 2"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let (shown, scroll) = split_capture(&raw).unwrap_or_else(|| panic!("split"));
        assert_eq!((shown.meta.width, shown.meta.height), (3, 2));
        assert_eq!(shown.lines.len(), 2);
        assert_eq!(scroll.len(), 2);
        assert!(
            split_capture(&raw[..2]).is_none(),
            "fewer lines than the screen"
        );
        assert!(
            split_capture(&["      ".to_owned()]).is_none(),
            "a missing window's blanks"
        );
        assert!(split_capture(&[]).is_none());
    }

    #[test]
    fn viewer_ids_are_hex_and_differ() {
        let (a, b) = (new_viewer_id(), new_viewer_id());
        assert!(a.len() == 16 && a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
