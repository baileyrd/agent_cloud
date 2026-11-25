//! The desktop stream (ADR-006, desktop addendum): watch an agent's display `:0` and take over its
//! mouse and keyboard, on the terminal viewer's transport and take-over lock.
//!
//! One frame client per viewed agent (`ffmpeg -f x11grab`, an [`Exec`] with output) feeds every
//! viewer of its desktop: each JPEG goes out as an NDJSON `frame` message, base64-encoded, and a
//! frame equal to the last one is not sent again, so an idle desktop costs a SHA-256 per frame. A
//! second client, the input loop, exists only while one of the desktop's viewers holds the lock,
//! and takes only [`DeskInput`] lines. Desktop and terminal viewers of an agent share one
//! [`ac_viewer::registry::Registry`] entry: one lock between them, and one per-agent cap.
//!
//! Routes (scopes in [`crate::access`]):
//! - `GET /api/v1/agents/{id}/desk`: NDJSON stream (`hello`, `mode`, `frame`, `ping`, `bye`);
//! - `POST /api/v1/agents/{id}/desk/{viewer}/{takeover|release|pointer|keys|text}`.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::Arc;

use ac_ctr::{CtrRuntime, Exec};
use ac_domain::AgentId;
use ac_viewer::desk::{self, DeskError, DeskInput};
use ac_viewer::registry::{Full, Held};
use ac_viewer::screen;
use platform::process::Spawner;
use rusty_sha2::{Hash, Sha256};

use super::{new_viewer_id, ok, Hub, Inner, Kind, State, Stream, INPUT_QUEUE};
use crate::{error, Reply};

/// Messages queued per desktop viewer. Frames are large, so few; a frame that finds the queue full
/// is skipped for that viewer, which then sees the next one.
const QUEUE: usize = 8;
/// What the frame client runs: display `:0`, 4 frames a second, JPEG quality 10 of 31.
const FRAMES: [&str; 19] = [
    "ffmpeg",
    "-nostats",
    "-loglevel",
    "error",
    "-f",
    "x11grab",
    "-draw_mouse",
    "1",
    "-framerate",
    "4",
    "-i",
    ":0",
    "-f",
    "image2pipe",
    "-c:v",
    "mjpeg",
    "-q:v",
    "10",
    "-",
];
/// `q` on stdin makes ffmpeg finish and exit.
const FRAMES_QUIT: &[u8] = b"q\n";
/// The input loop: one `xdotool` per line, for exactly the forms [`DeskInput::lines`] renders.
/// Globbing is off and key names are checked by the gateway (letters, digits, `_`, `+`), so the
/// word splitting of `$rest` yields only key names.
const INPUT: &str = r#"set -f
while IFS=' ' read -r cmd rest; do
    case $cmd in
        move) set -- $rest; xdotool mousemove -- "$1" "$2" ;;
        down) xdotool mousedown "$rest" ;;
        up) xdotool mouseup "$rest" ;;
        click) xdotool click "$rest" ;;
        key) set -- $rest; xdotool key --clearmodifiers -- "$@" ;;
        quit) exit 0 ;;
    esac
done"#;
const INPUT_QUIT: &[u8] = b"quit\n";
/// Bytes read from the frame client at a time.
const READ: usize = 256 * 1024;

/// How the gateway reaches an agent's desktop. Implemented over `ctr` for [`CtrRuntime`]; tests
/// use a fake.
pub trait Desktop: Send + Sync + 'static {
    /// Start the frame client (JPEGs on stdout). Called on the thread that will own it.
    fn attach_frames(&self, id: &AgentId) -> Result<Exec, String>;
    /// Start the input loop (no output), likewise.
    fn attach_input(&self, id: &AgentId) -> Result<Exec, String>;
}

impl<S: Spawner + Send + Sync + 'static> Desktop for CtrRuntime<S> {
    fn attach_frames(&self, id: &AgentId) -> Result<Exec, String> {
        self.spawn_exec(id, &FRAMES, true, FRAMES_QUIT)
            .map_err(|e| e.to_string())
    }

    fn attach_input(&self, id: &AgentId) -> Result<Exec, String> {
        self.spawn_exec(id, &["sh", "-c", INPUT], false, INPUT_QUIT)
            .map_err(|e| e.to_string())
    }
}

/// What a desktop viewer asks for under `desk/{viewer}/`: a closed set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeskAction {
    Takeover,
    Release,
    Pointer,
    Keys,
    Text,
}

impl DeskAction {
    pub(crate) fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "takeover" => Self::Takeover,
            "release" => Self::Release,
            "pointer" => Self::Pointer,
            "keys" => Self::Keys,
            "text" => Self::Text,
            _ => return None,
        })
    }
}

/// A watched desktop: lives exactly as long as its frame client.
pub(crate) struct Desk {
    gen: u64,
    pub(crate) viewers: HashMap<String, SyncSender<Vec<u8>>>,
    /// The latest frame message, for a viewer that joins.
    frame: Option<Vec<u8>>,
    pub(crate) owner: Option<SyncSender<DeskInput>>,
}

/// Send a control message to every viewer; one whose queue is full is dropped (it reconnects).
pub(crate) fn broadcast(desk: &mut Desk, msg: &str) {
    desk.viewers
        .retain(|_, tx| tx.try_send(msg.as_bytes().to_vec()).is_ok());
}

impl Hub {
    pub(crate) fn desk_open(&self, id: &AgentId, who: &str) -> super::Answer {
        if self.inner.desktop.is_none() {
            return super::Answer::Json(error(404, "no desktop stream on this gateway"));
        }
        let viewer = new_viewer_id();
        let (tx, rx) = mpsc::sync_channel(QUEUE);
        let start = {
            let mut st = self.inner.lock();
            match st.registry.join(id.as_str(), &viewer) {
                Err(Full::Agent) => {
                    return super::Answer::Json(error(429, "too many viewers of this agent"))
                }
                Err(Full::Total) => return super::Answer::Json(error(503, "too many viewers")),
                Ok(()) => {}
            }
            st.next_gen += 1;
            let gen = st.next_gen;
            let owner = st.registry.owner(id.as_str()).map(str::to_owned);
            let desk = st.desks.entry(id.to_string()).or_insert_with(|| Desk {
                gen,
                viewers: HashMap::new(),
                frame: None,
                owner: None,
            });
            let _ = tx.try_send(screen::hello(&viewer).into_bytes());
            let _ = tx.try_send(screen::mode(owner.as_deref()).into_bytes());
            if let Some(frame) = &desk.frame {
                let _ = tx.try_send(frame.clone());
            }
            desk.viewers.insert(viewer.clone(), tx);
            let start = (desk.gen == gen).then_some(gen);
            st.tokens.insert(viewer.clone(), who.to_owned());
            start
        };
        if let Some(gen) = start {
            let inner = Arc::clone(&self.inner);
            let id = id.clone();
            std::thread::spawn(move || frames(inner, id, gen));
        }
        super::Answer::Stream(Stream {
            rx,
            hub: self.clone(),
            id: id.clone(),
            viewer,
            kind: Kind::Desk,
        })
    }

    pub(crate) fn desk_leave(&self, id: &AgentId, viewer: &str) {
        let mut st = self.inner.lock();
        st.tokens.remove(viewer);
        let event = st.registry.leave(id.as_str(), viewer);
        if let Some(desk) = st.desks.get_mut(id.as_str()) {
            desk.viewers.remove(viewer);
        }
        if let Some(e) = &event {
            self.inner.recorder.event(id, &e.to_string());
            self.owner_changed(&mut st, id, None);
        }
        // The frame client sees it is no longer wanted at its next frame and closes.
        if st
            .desks
            .get(id.as_str())
            .is_some_and(|d| d.viewers.is_empty())
        {
            st.desks.remove(id.as_str());
        }
    }

    pub(crate) fn desk_action(
        &self,
        id: &AgentId,
        viewer: &str,
        action: DeskAction,
        query: &str,
        body: &[u8],
        who: &str,
    ) -> Reply {
        let mut st = self.inner.lock();
        if !st.owns(id, viewer, who) || !is_desk_viewer(&st, id, viewer) {
            return error(404, "no such viewer");
        }
        match action {
            DeskAction::Takeover => {
                let force = query.split('&').any(|p| p == "force=true");
                match st.registry.take_over(id.as_str(), viewer, force) {
                    Err(Held(o)) => {
                        error(409, &format!("viewer {o} has control; force to take over"))
                    }
                    Ok(event) => {
                        if let Some(e) = event {
                            self.inner.recorder.event(id, &e.to_string());
                        }
                        self.owner_changed(&mut st, id, Some(viewer));
                        ok(&format!(r#"{{"owner":"{viewer}"}}"#))
                    }
                }
            }
            DeskAction::Release => match st.registry.release(id.as_str(), viewer) {
                None => error(409, "you do not have control"),
                Some(e) => {
                    self.inner.recorder.event(id, &e.to_string());
                    self.owner_changed(&mut st, id, None);
                    ok(r#"{"owner":null}"#)
                }
            },
            DeskAction::Pointer | DeskAction::Keys | DeskAction::Text => {
                // View-only input is discarded here, before any command is built.
                if !st.registry.may_write(id.as_str(), viewer) {
                    return error(403, "view-only: input discarded");
                }
                let input = match action {
                    DeskAction::Pointer => DeskInput::pointer(body),
                    DeskAction::Keys => DeskInput::keys(body),
                    _ => DeskInput::text(body),
                };
                let input = match input {
                    Ok(i) => i,
                    Err(DeskError::TooLarge) => return error(413, "input too large"),
                    Err(e) => return error(400, &e.to_string()),
                };
                let owner = st.desks.get(id.as_str()).and_then(|d| d.owner.as_ref());
                match owner.map(|tx| tx.try_send(input)) {
                    Some(Ok(())) => ok(r#"{"sent":true}"#),
                    Some(Err(TrySendError::Full(_))) => error(503, "input queue full"),
                    _ => error(503, "input client is gone"),
                }
            }
        }
    }

    /// Start the desktop's input loop for a new lock holder.
    pub(crate) fn desk_writer(&self, id: &AgentId) -> SyncSender<DeskInput> {
        let (tx, rx) = mpsc::sync_channel(INPUT_QUEUE);
        let inner = Arc::clone(&self.inner);
        let id = id.clone();
        std::thread::spawn(move || input(inner, id, rx));
        tx
    }
}

fn is_desk_viewer(st: &State, id: &AgentId, viewer: &str) -> bool {
    st.desks
        .get(id.as_str())
        .is_some_and(|d| d.viewers.contains_key(viewer))
}

impl Inner {
    fn desk_alive(&self, id: &AgentId, gen: u64) -> bool {
        self.lock()
            .desks
            .get(id.as_str())
            .is_some_and(|d| d.gen == gen)
    }

    /// A new frame for every viewer of desktop `gen`; skipped for a viewer that is behind.
    fn desk_frame(&self, id: &AgentId, gen: u64, msg: Vec<u8>) {
        let mut st = self.lock();
        let Some(desk) = st.desks.get_mut(id.as_str()).filter(|d| d.gen == gen) else {
            return;
        };
        desk.viewers.retain(|_, tx| match tx.try_send(msg.clone()) {
            Ok(()) | Err(TrySendError::Full(_)) => true,
            Err(TrySendError::Disconnected(_)) => false,
        });
        desk.frame = Some(msg);
    }

    /// The frame client stopped: tell the desktop's viewers and drop them.
    fn desk_ended(&self, id: &AgentId, gen: u64, reason: &str) {
        let mut st = self.lock();
        if st.desks.get(id.as_str()).is_some_and(|d| d.gen == gen) {
            if let Some(desk) = st.desks.remove(id.as_str()) {
                for tx in desk.viewers.values() {
                    let _ = tx.try_send(screen::bye(reason).into_bytes());
                }
            }
        }
    }
}

fn frames(inner: Arc<Inner>, id: AgentId, gen: u64) {
    let reason = match run_frames(&inner, &id, gen) {
        Ok(()) => "desktop stream ended".to_owned(),
        Err(e) => e,
    };
    inner.desk_ended(&id, gen, &reason);
}

/// Read frames until the client exits or the desktop is no longer watched; the client is closed
/// (quit, then signals) on every way out, when `exec` drops.
fn run_frames(inner: &Inner, id: &AgentId, gen: u64) -> Result<(), String> {
    let desktop = inner.desktop.as_ref().ok_or("no desktop stream")?;
    let mut exec = desktop.attach_frames(id)?;
    let mut out = exec.take_stdout().ok_or("frame client has no stdout")?;
    let mut splitter = desk::Frames::new();
    let mut last: Option<Vec<u8>> = None;
    let mut buf = vec![0u8; READ];
    loop {
        let n = out
            .read(&mut buf)
            .map_err(|e| format!("frame client: {e:?}"))?;
        if n == 0 || !inner.desk_alive(id, gen) {
            return Ok(());
        }
        for frame in splitter.feed(&buf[..n]) {
            let digest = Sha256::digest(&frame).to_vec();
            if last.as_ref() == Some(&digest) {
                continue;
            }
            last = Some(digest);
            let msg = desk::frame_message(&rusty_base64::encode_standard(&frame));
            inner.desk_frame(id, gen, msg.into_bytes());
        }
    }
}

/// The input loop: forwards the lock holder's commands until the lock moves (the sender is
/// dropped), then closes.
fn input(inner: Arc<Inner>, id: AgentId, rx: Receiver<DeskInput>) {
    let Some(desktop) = inner.desktop.as_ref() else {
        return;
    };
    let mut exec = match desktop.attach_input(&id) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("ac-gateway: viewer: {id}: desktop input client: {e}");
            return;
        }
    };
    while let Ok(cmd) = rx.recv() {
        for line in cmd.lines() {
            if exec.send(line.as_bytes()).is_err() {
                return;
            }
        }
    }
}
