//! The viewer end to end over a loopback socket: real rusty_serve, real gateway routing, a fake
//! tmux behind the `Terminal` seam (platform `Child`/`File` fakes speaking control mode).

use std::cell::Cell;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ac_ctr::Exec;
use ac_domain::{AgentId, State};
use ac_fleet::Fleet;
use ac_gateway::egress::EgressStore;
use ac_gateway::tokens::{self, Agents, Keys, Scopes, Token, TokenFile, Tokens};
use ac_gateway::viewer::{Hub, Terminal, ViewClient, ViewCommand, Writer};
use ac_gateway::web::App;
use ac_gateway::Api;
use ac_ports::{PortError, Runtime, SessionStore};
use ac_viewer::windows::{Target, WindowId};
use platform::error::{ErrorKind, OsCode, PlatformError};
use platform::fs::File;
use platform::process::{Child, ExitStatus, Signal};
use rusty_serve::Server;

const TOKEN: &str = "s3cret";

// ---- the fake tmux ---------------------------------------------------------------------------

/// A window besides the agent's own (`@0 %0`, index 0, `bash`).
struct FakeWindow {
    id: &'static str,
    pane: &'static str,
    name: &'static str,
    screen: Vec<String>,
}

#[derive(Default)]
struct World {
    meta: String,
    screen: Vec<String>,
    windows: Vec<FakeWindow>,
    /// Targets of every `Terminal::capture` exec.
    captures: Vec<String>,
    /// (writable client, command line) for every command any client received.
    commands: Vec<(bool, String)>,
    detached: Vec<String>,
    /// stdout of each live read-only client.
    outputs: Vec<Sender<Vec<u8>>>,
    /// Set if any client's stdin was closed while it was alive (must never happen).
    eof_while_alive: bool,
    /// Clients attached, and clients whose stdin has been closed.
    attached: usize,
    closed: usize,
    n: u64,
}

#[derive(Clone)]
struct Fake(Arc<Mutex<World>>);

impl Fake {
    fn new() -> Self {
        let w = World {
            meta: "20 3 2 1 1 0 0".into(),
            screen: vec![
                "\x1b[32m$\x1b[0m hello".into(),
                String::new(),
                String::new(),
            ],
            ..World::default()
        };
        Self(Arc::new(Mutex::new(w)))
    }
    fn world(&self) -> std::sync::MutexGuard<'_, World> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
    /// The pane printed something: every read-only client gets `%output`.
    fn output(&self, escaped: &str) {
        let line = format!("%output %0 {escaped}\n").into_bytes();
        self.world()
            .outputs
            .retain(|tx| tx.send(line.clone()).is_ok());
    }
    /// A tmux notification on every read-only client.
    fn notify(&self, line: &str) {
        let line = format!("{line}\n").into_bytes();
        self.world()
            .outputs
            .retain(|tx| tx.send(line.clone()).is_ok());
    }
    fn sent(&self, writable: bool) -> Vec<String> {
        let w = self.world();
        w.commands
            .iter()
            .filter(|(rw, _)| *rw == writable)
            .map(|(_, c)| c.clone())
            .collect()
    }
}

struct FakeIn {
    fake: Fake,
    writable: bool,
    alive: Rc<Cell<bool>>,
    out: Option<Sender<Vec<u8>>>,
}

impl File for FakeIn {
    fn read(&mut self, _: &mut [u8]) -> platform::error::Result<usize> {
        Ok(0)
    }
    fn write(&mut self, buf: &[u8]) -> platform::error::Result<usize> {
        if !self.alive.get() {
            return Err(PlatformError::new(
                ErrorKind::BrokenPipe,
                OsCode::None,
                "write",
            ));
        }
        for cmd in String::from_utf8_lossy(buf).lines() {
            let mut w = self.fake.world();
            w.commands.push((self.writable, cmd.to_owned()));
            w.n += 1;
            let n = w.n;
            if cmd == "detach-client" {
                self.alive.set(false);
                self.out = None;
                continue;
            }
            let reply = if cmd.contains("client_name") {
                Some(vec!["client-7".to_owned()])
            } else {
                w.answer(cmd)
            };
            if let Some(out) = &self.out {
                let mut text = format!("%begin 1 {n} 1\n");
                for l in reply.iter().flatten() {
                    text.push_str(l);
                    text.push('\n');
                }
                let end = if reply.is_some() { "end" } else { "error" };
                text.push_str(&format!("%{end} 1 {n} 1\n"));
                let _ = out.send(text.into_bytes());
            }
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> platform::error::Result<()> {
        Ok(())
    }
    fn sync_all(&mut self) -> platform::error::Result<()> {
        Ok(())
    }
    fn try_clone(&self) -> platform::error::Result<Box<dyn File>> {
        Err(PlatformError::new(
            ErrorKind::Unsupported,
            OsCode::None,
            "clone",
        ))
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl World {
    /// tmux's reply to a view or capture command (`None`: `%error`). `display` of a missing
    /// window prints blanks; `capture-pane` of one fails.
    fn answer(&self, cmd: &str) -> Option<Vec<String>> {
        if cmd.starts_with("list-windows") {
            let mut list = vec!["@0 %0 0 1 bash".to_owned()];
            for (i, x) in self.windows.iter().enumerate() {
                list.push(format!("{} {} {} 0 {}", x.id, x.pane, i + 1, x.name));
            }
            return Some(list);
        }
        let target = cmd
            .split(' ')
            .skip_while(|a| *a != "-t")
            .nth(1)
            .unwrap_or("main");
        let screen = match target {
            "main" => Some(self.screen.clone()),
            t => self
                .windows
                .iter()
                .find(|x| x.id == t)
                .map(|x| x.screen.clone()),
        };
        if cmd.starts_with("display") {
            Some(vec![
                screen.map_or("      ".to_owned(), |_| self.meta.clone())
            ])
        } else if cmd.starts_with("capture-pane") {
            screen
        } else {
            Some(Vec::new())
        }
    }
}

impl Drop for FakeIn {
    fn drop(&mut self) {
        let mut w = self.fake.world();
        w.closed += 1;
        if self.alive.get() {
            w.eof_while_alive = true;
        }
    }
}

struct FakeOut {
    rx: Receiver<Vec<u8>>,
    pending: Vec<u8>,
}

impl File for FakeOut {
    fn read(&mut self, buf: &mut [u8]) -> platform::error::Result<usize> {
        if self.pending.is_empty() {
            match self.rx.recv() {
                Ok(b) => self.pending = b,
                Err(_) => return Ok(0),
            }
        }
        let n = buf.len().min(self.pending.len());
        buf[..n].copy_from_slice(&self.pending[..n]);
        self.pending.drain(..n);
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
    fn try_clone(&self) -> platform::error::Result<Box<dyn File>> {
        Err(PlatformError::new(
            ErrorKind::Unsupported,
            OsCode::None,
            "clone",
        ))
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

struct FakeChild {
    alive: Rc<Cell<bool>>,
    stdin: Option<Box<dyn File>>,
    stdout: Option<Box<dyn File>>,
}

impl Child for FakeChild {
    fn wait(self: Box<Self>) -> platform::error::Result<ExitStatus> {
        Ok(ExitStatus::Code(0))
    }
    fn id(&self) -> u32 {
        1
    }
    fn kill_tree(&self, s: Signal) -> platform::error::Result<()> {
        self.kill_single(s)
    }
    fn kill_single(&self, _: Signal) -> platform::error::Result<()> {
        self.alive.set(false);
        Ok(())
    }
    fn try_wait(&mut self) -> platform::error::Result<Option<ExitStatus>> {
        Ok((!self.alive.get()).then_some(ExitStatus::Code(0)))
    }
    fn wait_job(&mut self) -> platform::error::Result<ExitStatus> {
        Ok(ExitStatus::Code(0))
    }
    fn try_wait_job(&mut self) -> platform::error::Result<Option<ExitStatus>> {
        self.try_wait()
    }
    fn take_stdin(&mut self) -> Option<Box<dyn File>> {
        self.stdin.take()
    }
    fn take_stdout(&mut self) -> Option<Box<dyn File>> {
        self.stdout.take()
    }
    fn take_stderr(&mut self) -> Option<Box<dyn File>> {
        None
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

impl Terminal for Fake {
    fn prepare(&self, _: &AgentId) -> Result<(), String> {
        Ok(())
    }
    fn attach_view(&self, _: &AgentId) -> Result<ViewClient, String> {
        Ok(ViewClient::new(self.spawn(false)))
    }
    fn attach_writer(&self, _: &AgentId) -> Result<Writer, String> {
        Ok(Writer::new(self.spawn(true)))
    }
    fn scrollback(&self, _: &AgentId) -> Result<Vec<String>, String> {
        Ok(vec!["older line".into()])
    }
    fn capture(&self, _: &AgentId, target: Target) -> Result<Vec<String>, String> {
        let mut w = self.world();
        let t = target.to_string();
        w.captures.push(t.clone());
        let screen = w
            .answer(&format!("capture-pane -p -e -t {t}"))
            .ok_or("can't find window")?;
        let mut out = vec![w.meta.clone(), format!("scrollback of {t}")];
        out.extend(screen);
        Ok(out)
    }
    fn detach(&self, _: &AgentId, client: &str) -> Result<(), String> {
        let mut w = self.world();
        w.detached.push(client.to_owned());
        // tmux tells the detached client, which then exits.
        w.outputs.retain(|tx| tx.send(b"%exit\n".to_vec()).is_ok());
        Ok(())
    }
    fn kill_clients(&self, _: &AgentId, _: bool) -> Result<(), String> {
        Ok(())
    }
}

impl Fake {
    fn spawn(&self, write: bool) -> Exec {
        let alive = Rc::new(Cell::new(true));
        let (tx, rx) = mpsc::channel();
        self.world().attached += 1;
        if !write {
            self.world().outputs.push(tx.clone());
        }
        let stdin = FakeIn {
            fake: self.clone(),
            writable: write,
            alive: Rc::clone(&alive),
            out: (!write).then_some(tx),
        };
        let child = FakeChild {
            alive,
            stdin: Some(Box::new(stdin)),
            stdout: (!write).then(|| {
                Box::new(FakeOut {
                    rx,
                    pending: Vec::new(),
                }) as Box<dyn File>
            }),
        };
        Exec::new(
            Box::new(child),
            b"detach-client\n",
            Duration::from_millis(100),
        )
    }
}

// ---- a fleet that always works ----------------------------------------------------------------

#[derive(Default)]
struct Store(Mutex<HashMap<String, State>>);
impl SessionStore for Store {
    fn state(&self, id: &AgentId) -> Result<Option<State>, PortError> {
        Ok(self
            .0
            .lock()
            .map(|m| m.get(id.as_str()).copied())
            .unwrap_or(None))
    }
    fn set_state(&self, id: &AgentId, s: State) -> Result<(), PortError> {
        let _ = self.0.lock().map(|mut m| m.insert(id.to_string(), s));
        Ok(())
    }
    fn list(&self) -> Result<Vec<(AgentId, State)>, PortError> {
        let m = self.0.lock().map_err(|_| PortError {
            op: "list",
            cause: "poisoned".into(),
        })?;
        Ok(m.iter()
            .filter_map(|(k, s)| Some((AgentId::parse(k).ok()?, *s)))
            .collect())
    }
}

struct Rt;
impl Runtime for Rt {
    fn create(&self, _: &AgentId, _: &ac_ports::Boot) -> Result<(), PortError> {
        Ok(())
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

fn serve(fake: &Fake, history: &std::path::Path, per_agent: usize, total: usize) -> SocketAddr {
    let mut t = Tokens::default();
    let _ = t.add(Token::admin(tokens::LEGACY, TOKEN));
    serve_with(fake, history, per_agent, total, Keys::new(t))
}

fn serve_with(
    fake: &Fake,
    history: &std::path::Path,
    per_agent: usize,
    total: usize,
    keys: Keys,
) -> SocketAddr {
    let api = Api::new(Fleet::new(Rt, Store::default()));
    serve_api(fake, history, per_agent, total, keys, api)
}

fn serve_api(
    fake: &Fake,
    history: &std::path::Path,
    per_agent: usize,
    total: usize,
    keys: Keys,
    api: Api<Rt, Store>,
) -> SocketAddr {
    let hub = Hub::with_caps(fake.clone(), history.to_path_buf(), per_agent, total);
    let server = Server::bind_shared(
        "127.0.0.1:0".parse().unwrap_or_else(|e| panic!("{e}")),
        App::new(api, hub, keys),
    )
    .unwrap_or_else(|e| panic!("bind: {e}"));
    let addr = server.local_addr().unwrap_or_else(|e| panic!("{e}"));
    std::thread::spawn(move || server.run());
    addr
}

// ---- a tiny HTTP client -----------------------------------------------------------------------

fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    extra: &[&str],
    body: &str,
) -> (u16, String) {
    request_as(addr, TOKEN, method, path, extra, body)
}

fn request_as(
    addr: SocketAddr,
    token: &str,
    method: &str,
    path: &str,
    extra: &[&str],
    body: &str,
) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap_or_else(|e| panic!("{e}"));
    let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for h in extra {
        head.push_str(h);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    let _ = s.write_all(head.as_bytes());
    let _ = s.write_all(body.as_bytes());
    let mut raw = String::new();
    let _ = s.read_to_string(&mut raw);
    let status = raw.get(9..12).and_then(|c| c.parse().ok()).unwrap_or(0);
    let body = raw
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_owned())
        .unwrap_or_default();
    (status, body)
}

struct Viewer {
    r: BufReader<TcpStream>,
    id: String,
    token: String,
}

impl Viewer {
    fn open(addr: SocketAddr, agent: &str) -> Result<Self, u16> {
        Self::open_as(addr, TOKEN, agent)
    }

    fn open_as(addr: SocketAddr, token: &str, agent: &str) -> Result<Self, u16> {
        let s = TcpStream::connect(addr).unwrap_or_else(|e| panic!("{e}"));
        let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
        let mut w = s.try_clone().unwrap_or_else(|e| panic!("{e}"));
        let _ = write!(
            w,
            "GET /api/v1/agents/{agent}/term HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\n\r\n"
        );
        let mut r = BufReader::new(s);
        let mut status = String::new();
        let _ = r.read_line(&mut status);
        let code: u16 = status.get(9..12).and_then(|c| c.parse().ok()).unwrap_or(0);
        loop {
            let mut h = String::new();
            if r.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                break;
            }
        }
        if code != 200 {
            return Err(code);
        }
        let mut v = Self {
            r,
            id: String::new(),
            token: token.to_owned(),
        };
        let hello = v.next().unwrap_or_default();
        v.id = hello.split('"').nth(7).unwrap_or_default().to_owned();
        Ok(v)
    }

    /// The next NDJSON message, keep-alives included (chunked framing removed).
    fn next_any(&mut self) -> Option<String> {
        let mut size = String::new();
        self.r.read_line(&mut size).ok()?;
        let n = usize::from_str_radix(size.trim(), 16).ok()?;
        if n == 0 {
            return None;
        }
        let mut chunk = vec![0u8; n + 2];
        self.r.read_exact(&mut chunk).ok()?;
        Some(String::from_utf8_lossy(&chunk[..n]).into_owned())
    }

    /// The next NDJSON message; keep-alives skipped.
    fn next(&mut self) -> Option<String> {
        loop {
            let msg = self.next_any()?;
            if !msg.contains("\"t\":\"ping\"") {
                return Some(msg);
            }
        }
    }

    /// Read until a message containing `needle`; fails after 5 s (checked at keep-alives too).
    fn until(&mut self, needle: &str) -> String {
        let end = Instant::now() + Duration::from_secs(5);
        while Instant::now() < end {
            match self.next_any() {
                Some(m) if m.contains(needle) => return m,
                Some(_) => {}
                None => break,
            }
        }
        panic!("no message containing {needle}");
    }

    fn post(&self, addr: SocketAddr, action: &str, body: &str) -> (u16, String) {
        self.post_as(addr, &self.token, action, body)
    }

    fn post_as(&self, addr: SocketAddr, token: &str, action: &str, body: &str) -> (u16, String) {
        let path = format!("/api/v1/agents/a1/term/{}/{action}", self.id);
        request_as(
            addr,
            token,
            "POST",
            &path,
            &[&format!("Origin: http://{addr}")],
            body,
        )
    }
}

/// Every line the shared view client may be sent, for the agent's window and `windows`.
fn fixed_commands(windows: &[&str]) -> Vec<String> {
    let mut targets = vec![Target::Active];
    targets.extend(
        windows
            .iter()
            .filter_map(|w| WindowId::parse(w).map(Target::Window)),
    );
    let mut all = vec![ViewCommand::ClientName, ViewCommand::ListWindows];
    for t in targets {
        all.extend([ViewCommand::CursorInfo(t), ViewCommand::Snapshot(t)]);
    }
    all.iter().map(|c| c.line().trim_end().to_owned()).collect()
}

fn tmp(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("ac-gateway-viewer-{}-{name}", std::process::id()))
}

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(5);
    while !f() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ---- the tests --------------------------------------------------------------------------------

#[test]
fn watch_take_over_release_and_history() {
    let (fake, dir) = (Fake::new(), tmp("main"));
    let addr = serve(&fake, &dir, 4, 16);
    let mut a = Viewer::open(addr, "a1").unwrap_or_else(|c| panic!("open {c}"));
    assert_eq!(a.id.len(), 16);
    assert!(a.until("\"t\":\"mode\"").contains("null"));
    let full = a.until("\"t\":\"full\"");
    assert!(
        full.contains(r#"["$",2,-1,0]"#) && full.contains("older line"),
        "{full}"
    );
    assert!(full.contains(r#""w":20,"h":3,"cx":2,"cy":1"#), "{full}");

    // Output triggers a snapshot; only the changed line is sent.
    fake.world().screen[1] = "count 1".into();
    fake.output(r"count 1\015\012");
    let delta = a.until("\"t\":\"delta\"");
    assert_eq!(
        delta,
        "{\"t\":\"delta\",\"cx\":2,\"cy\":1,\"cv\":true,\"lines\":[[1,[[\"count 1\",-1,-1,0]]]]}\n"
    );

    // View-only: hand-crafted input is refused and nothing reaches tmux.
    for (action, body) in [
        ("keys", "Enter"),
        ("text", "touch /tmp/pwned\n"),
        ("paste", "x"),
        ("resize?cols=80&rows=24", ""),
    ] {
        assert_eq!(a.post(addr, action, body).0, 403, "{action}");
    }
    assert!(fake.sent(true).is_empty());
    assert!(fake
        .sent(false)
        .iter()
        .all(|c| !c.starts_with("send-keys") && !c.contains("refresh-client")));

    // Take over, type, and a second viewer is refused until it forces.
    let mut b = Viewer::open(addr, "a1").unwrap_or_else(|c| panic!("open {c}"));
    assert_eq!(a.post(addr, "takeover", "").0, 200);
    b.until(&format!("\"owner\":\"{}\"", a.id));
    assert_eq!(a.post(addr, "keys", "C-c Enter").0, 200);
    assert_eq!(a.post(addr, "text", "ls\n").0, 200);
    assert_eq!(a.post(addr, "keys", "Enter;kill-server").0, 400);
    assert_eq!(b.post(addr, "takeover", "").0, 409);
    assert_eq!(b.post(addr, "text", "x").0, 403);
    wait_for("typed commands", || fake.sent(true).len() >= 2);
    assert_eq!(
        fake.sent(true),
        [
            "send-keys -t main -H 03 0d",
            "send-keys -t main -H 6c 73 0a"
        ]
    );
    // Hostile input reaches the pane byte for byte and never as tmux syntax.
    let hostile = "x\nkill-server\n; run-shell 'rm -rf ~' \"q\" %exit\0\r\\ #{pane_pid}";
    assert_eq!(a.post(addr, "text", hostile).0, 200);
    assert_eq!(a.post(addr, "paste", hostile).0, 200);
    assert_eq!(a.post(addr, "resize?cols=100&rows=30", "").0, 200);
    assert_eq!(
        a.post(addr, "resize?cols=100;kill-server&rows=30", "").0,
        400
    );
    wait_for("hostile input", || fake.sent(true).len() >= 5);
    let sent = fake.sent(true);
    let decode = |line: &str| -> Vec<u8> {
        let hex = line
            .strip_prefix("send-keys -t main -H ")
            .unwrap_or_else(|| panic!("{line}"));
        hex.split(' ')
            .map(|h| u8::from_str_radix(h, 16).unwrap_or_else(|e| panic!("{h}: {e}")))
            .collect()
    };
    assert_eq!(decode(&sent[2]), hostile.as_bytes());
    assert_eq!(decode(&sent[3]), hostile.as_bytes());
    assert_eq!(sent[4], "refresh-client -C 100x30");
    assert_eq!(b.post(addr, "takeover?force=true", "").0, 200);
    assert_eq!(
        a.post(addr, "text", "x").0,
        403,
        "displaced viewer is view-only"
    );
    assert_eq!(b.post(addr, "release", "").0, 200);
    assert_eq!(
        b.post(addr, "text", "x").0,
        403,
        "released: view-only again"
    );

    // Disconnecting the holder releases the lock (noticed at a keep-alive write).
    assert_eq!(a.post(addr, "takeover", "").0, 200);
    drop(a);
    let end = Instant::now() + Duration::from_secs(15);
    while !request(addr, "GET", "/api/v1/agents/a1/history", &[], "")
        .1
        .contains("reason=disconnected")
    {
        assert!(Instant::now() < end, "disconnect not noticed");
        std::thread::sleep(Duration::from_millis(200));
    }
    let id = b.id.clone();
    assert_eq!(b.post(addr, "takeover", "").0, 200, "lock is free again");
    assert_eq!(b.post(addr, "release", "").0, 200, "{id}");

    let (status, text) = request(addr, "GET", "/api/v1/agents/a1/history?limit=50", &[], "");
    assert_eq!(status, 200);
    assert!(text.contains(" output\ncount 1\n"), "{text}");
    for event in [
        "takeover viewer=",
        "forced=true displaced=",
        "release viewer=",
        "reason=disconnected",
    ] {
        assert!(text.contains(event), "{event} missing from {text}");
    }
    assert!(
        !text.contains("ls") && !text.contains("C-c"),
        "keystrokes must not be recorded"
    );

    // The last viewer leaving detaches the bridge's client, which closes cleanly.
    drop(b);
    let end = Instant::now() + Duration::from_secs(15);
    while fake.world().detached != ["client-7"] {
        assert!(Instant::now() < end, "bridge not detached");
        std::thread::sleep(Duration::from_millis(100));
    }
    wait_for("every client closed", || {
        let w = fake.world();
        w.attached == w.closed
    });
    let w = fake.world();
    assert!(
        w.attached >= 4,
        "bridge plus three take-over clients: {}",
        w.attached
    );
    assert!(!w.eof_while_alive, "stdin closed while a client was alive");
    drop(w);
    // The shared client heard nothing but the fixed view commands; the writer nothing but hex
    // keys and resizes.
    let fixed = fixed_commands(&[]);
    let shared = fake.sent(false);
    assert!(shared.len() >= 5, "{shared:?}");
    assert!(
        shared
            .iter()
            .all(|c| fixed.contains(c) || c == "detach-client"),
        "{shared:?}"
    );
    for line in fake.sent(true) {
        let hex = line.strip_prefix("send-keys -t main -H ").is_some_and(|h| {
            h.split(' ')
                .all(|x| x.len() == 2 && x.bytes().all(|b| b.is_ascii_hexdigit()))
        });
        let resize = line.strip_prefix("refresh-client -C ").is_some_and(|r| {
            r.split_once('x')
                .is_some_and(|(c, r)| c.parse::<u16>().is_ok() && r.parse::<u16>().is_ok())
        });
        assert!(
            hex || resize || line == "detach-client",
            "writer got {line:?}"
        );
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn hostile_output_never_reaches_the_browser() {
    let (fake, dir) = (Fake::new(), tmp("hostile"));
    fake.world().screen = vec![
        "\x1b]52;c;c2VjcmV0\x07\x1b]8;;https://evil.example/\x1b\\click\x1b]8;;\x1b\\".into(),
        "\x1b]0;fake title\x07\x1bP1$qm\x1b\\\x1b_Gf=100;AAAA\x1b\\\u{202e}gnp.exe".into(),
        "y".repeat(200_000),
    ];
    let addr = serve(&fake, &dir, 4, 16);
    let mut v = Viewer::open(addr, "a1").unwrap_or_else(|c| panic!("open {c}"));
    let full = v.until("\"t\":\"full\"");
    for banned in [
        "\x1b",
        "\\u001b",
        "evil",
        "c2VjcmV0",
        "fake title",
        "Gf=100",
        "\u{202e}",
        "\\u202e",
    ] {
        assert!(!full.contains(banned), "{banned:?} reached the client");
    }
    assert!(full.contains("click") && full.contains("gnp.exe"));
    assert!(
        full.len() < 4000,
        "a 200 000-character line must be cut: {}",
        full.len()
    );
    // History strips the same way.
    fake.output(r"\033]52;c;c2VjcmV0\007\033]8;;https://evil.example/\033\134ok\033]0;t\007");
    wait_for("recorded output", || {
        request(addr, "GET", "/api/v1/agents/a1/history", &[], "")
            .1
            .contains("ok")
    });
    let text = request(addr, "GET", "/api/v1/agents/a1/history", &[], "").1;
    assert!(
        !text.contains('\x1b') && !text.contains("evil") && !text.contains("c2VjcmV0"),
        "{text}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn caps_never_starve_the_lifecycle_api() {
    let (fake, dir) = (Fake::new(), tmp("caps"));
    let addr = serve(&fake, &dir, 4, 16);
    for i in 1..=5 {
        assert_eq!(
            request(addr, "POST", &format!("/api/v1/agents/a{i}"), &[], "").0,
            201
        );
    }
    let mut viewers = Vec::new();
    for i in 1..=4 {
        for _ in 0..4 {
            viewers
                .push(Viewer::open(addr, &format!("a{i}")).unwrap_or_else(|c| panic!("open {c}")));
        }
    }
    assert_eq!(viewers.len(), 16);
    assert_eq!(Viewer::open(addr, "a1").err(), Some(429), "per-agent cap");
    assert_eq!(Viewer::open(addr, "a5").err(), Some(503), "total cap");
    let t0 = Instant::now();
    let (status, body) = request(addr, "GET", "/api/v1/agents", &[], "");
    assert_eq!(status, 200);
    assert!(body.contains(r#"{"id":"a5","state":"running"}"#), "{body}");
    assert!(
        t0.elapsed() < Duration::from_secs(1),
        "lifecycle API took {:?}",
        t0.elapsed()
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn cross_origin_and_tokenless_requests_are_refused() {
    let (fake, dir) = (Fake::new(), tmp("origin"));
    let addr = serve(&fake, &dir, 4, 16);
    let v = Viewer::open(addr, "a1").unwrap_or_else(|c| panic!("open {c}"));
    let path = format!("/api/v1/agents/a1/term/{}/takeover", v.id);
    assert_eq!(
        request(addr, "POST", &path, &["Origin: http://evil.example"], "").0,
        403
    );
    assert_eq!(
        request(addr, "POST", &path, &["Sec-Fetch-Site: cross-site"], "").0,
        403
    );
    assert_eq!(
        request(
            addr,
            "GET",
            "/api/v1/agents/a1/history",
            &["Origin: null"],
            ""
        )
        .0,
        403
    );
    let mut s = TcpStream::connect(addr).unwrap_or_else(|e| panic!("{e}"));
    let _ = write!(
        s,
        "GET /api/v1/agents/a1/term HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    );
    let mut raw = String::new();
    let _ = s.read_to_string(&mut raw);
    assert!(raw.starts_with("HTTP/1.1 401"), "{raw}");
    let (status, page) = request(addr, "GET", "/ui/a1", &[], "");
    assert_eq!(status, 200);
    assert!(page.contains("/ui/viewer.js"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn viewer_scopes_and_viewer_ids_belong_to_their_token() {
    use std::os::unix::fs::MetadataExt;
    let (fake, dir) = (Fake::new(), tmp("scopes"));
    let _ = std::fs::create_dir_all(&dir);
    let me = std::fs::metadata("/proc/self").map_or(0, |m| m.uid());
    let file = TokenFile::new(dir.join("tokens"), me);
    let make = |name: &str, scopes: &str| {
        let scopes = Scopes::parse(scopes).unwrap_or_else(|e| panic!("{e}"));
        file.create(name, scopes, Agents::All, 1)
            .unwrap_or_else(|e| panic!("{e}"))
    };
    let (watch, control) = (
        make("watch", "viewer:read"),
        make("ctl", "viewer:read,viewer:control"),
    );
    let mut t = file.list().unwrap_or_else(|e| panic!("{e}"));
    let _ = t.add(Token::admin(tokens::LEGACY, TOKEN));
    let keys = Keys::new(t);
    let addr = serve_with(&fake, &dir, 4, 16, keys.clone());

    // viewer:read watches but never types or takes over.
    let mut v = Viewer::open_as(addr, &watch, "a1").unwrap_or_else(|c| panic!("open {c}"));
    v.until("\"t\":\"full\"");
    for action in [
        "takeover",
        "takeover?force=true",
        "release",
        "keys",
        "text",
        "paste",
        "resize?cols=80&rows=24",
    ] {
        assert_eq!(v.post(addr, action, "Enter").0, 403, "{action}");
    }
    // Another token cannot use this viewer's id, even with viewer:control (or admin).
    assert_eq!(
        v.post_as(addr, &control, "takeover", "").1,
        r#"{"error":"no such viewer"}"#
    );
    assert_eq!(v.post_as(addr, TOKEN, "takeover", "").0, 404);
    // Its own viewer, it can.
    let c = Viewer::open_as(addr, &control, "a1").unwrap_or_else(|c| panic!("open {c}"));
    assert_eq!(c.post(addr, "takeover", "").0, 200);
    assert_eq!(c.post(addr, "text", "ls\n").0, 200);
    assert_eq!(c.post(addr, "release", "").0, 200);
    assert!(fake
        .sent(true)
        .iter()
        .all(|l| l != "send-keys -t main -H 45 6e 74 65 72"));
    let (_, history) = request(addr, "GET", "/api/v1/agents/a1/history", &[], "");
    for line in [
        "audit token=ctl action=takeover status=200",
        "audit token=ctl action=release status=200",
    ] {
        assert!(history.contains(line), "{line} missing from {history}");
    }
    assert!(
        !history.contains("action=text"),
        "input is not audited per call"
    );

    // Revoking a token ends its open streams too (at the next message or keep-alive).
    assert!(file.revoke("watch").is_ok());
    let mut t = file.list().unwrap_or_else(|e| panic!("{e}"));
    let _ = t.add(Token::admin(tokens::LEGACY, TOKEN));
    keys.set(t);
    let end = Instant::now() + Duration::from_secs(15);
    while v.next().is_some() {
        assert!(Instant::now() < end, "revoked token's stream still open");
    }
    assert_eq!(
        c.post(addr, "takeover", "").0,
        200,
        "other tokens unaffected"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_viewer_picks_a_window_without_moving_the_agent_and_falls_back_when_it_closes() {
    let (fake, dir) = (Fake::new(), tmp("windows"));
    fake.world().windows.push(FakeWindow {
        id: "@3",
        pane: "%5",
        name: "job-6b941feec45a4c25",
        screen: vec!["job says hi".into(), String::new(), String::new()],
    });
    let addr = serve(&fake, &dir, 4, 16);
    let mut a = Viewer::open(addr, "a1").unwrap_or_else(|c| panic!("open {c}"));
    let list = a.until("\"t\":\"windows\"");
    assert_eq!(
        list,
        "{\"t\":\"windows\",\"list\":[{\"id\":\"@0\",\"index\":0,\"name\":\"bash\",\"active\":true,\"job\":false},\
         {\"id\":\"@3\",\"index\":1,\"name\":\"job-6b941feec45a4c25\",\"active\":false,\"job\":true}]}\n"
    );
    a.until("\"t\":\"full\"");
    let mut b = Viewer::open(addr, "a1").unwrap_or_else(|c| panic!("open {c}"));
    assert!(
        b.until("\"t\":\"windows\"").contains("@3"),
        "late viewers get the list"
    );
    b.until("\"t\":\"full\"");

    // Hostile or unknown ids are refused before anything is built; nothing reaches tmux.
    let huge = format!("@{}", "9".repeat(100_000));
    for body in [
        "@1;kill-server",
        "@3;kill-server",
        "@3 ",
        "%5",
        "%1",
        "",
        "@7",
        "@03",
        huge.as_str(),
        "@\u{0663}",
        "\u{ff20}3",
        "Active",
    ] {
        assert_eq!(a.post(addr, "window", body).0, 400, "{body:?}");
    }
    assert!(fake.world().captures.is_empty());

    // a watches the job's window; b still watches the agent's.
    assert_eq!(
        a.post(addr, "window", "@3"),
        (200, r#"{"window":"@3"}"#.into())
    );
    assert!(a.until("\"t\":\"target\"").contains("\"@3\""));
    let full = a.until("\"t\":\"full\"");
    assert!(
        full.contains("job says hi") && full.contains("scrollback of @3"),
        "{full}"
    );
    fake.world().windows[0].screen[1] = "job line 2".into();
    fake.world().screen[1] = "agent line 2".into();
    fake.output(r"x");
    assert!(a.until("\"t\":\"delta\"").contains("job line 2"));
    assert!(b.until("\"t\":\"delta\"").contains("agent line 2"));

    // A taker types into the active pane of the window it watches, by the pane id from the list.
    assert_eq!(a.post(addr, "takeover", "").0, 200);
    assert_eq!(a.post(addr, "text", "q").0, 200);
    wait_for("typed into the job's pane", || !fake.sent(true).is_empty());
    assert_eq!(fake.sent(true), ["send-keys -t %5 -H 71"]);

    // The window closes: the new list, then a falls back to the agent's window, and so does its
    // input.
    fake.world().windows.clear();
    fake.notify("%window-close @3");
    assert_eq!(a.until("\"t\":\"windows\"").matches("\"id\"").count(), 1);
    assert!(a.until("\"t\":\"target\"").contains("null"));
    assert!(a.until("\"t\":\"full\"").contains("agent line 2"));
    assert_eq!(a.post(addr, "text", "r").0, 200);
    wait_for("typed into main", || fake.sent(true).len() >= 2);
    assert_eq!(fake.sent(true)[1], "send-keys -t main -H 72");
    assert_eq!(a.post(addr, "window", "@3").0, 400, "no longer listed");
    assert_eq!(a.post(addr, "window", "active").0, 200);

    // Closed between the list and the pick: the capture fails, the viewer stays put.
    fake.world().windows.push(FakeWindow {
        id: "@4",
        pane: "%6",
        name: "w",
        screen: vec![String::new(); 3],
    });
    fake.notify("%window-add @4");
    a.until("\"id\":\"@4\"");
    fake.world().windows.clear();
    assert_eq!(a.post(addr, "window", "@4").0, 409);

    // Nobody ran select-window; the shared client heard only the fixed set; picks captured only
    // `main` and listed ids.
    let fixed = fixed_commands(&["@3", "@4"]);
    for line in fake.sent(false) {
        assert!(
            fixed.contains(&line) || line == "detach-client",
            "shared client got {line:?}"
        );
    }
    assert!(fake
        .sent(true)
        .iter()
        .all(|l| l.starts_with("send-keys -t ") && !l.contains("select")));
    assert_eq!(fake.world().captures, ["@3", "main", "@4"]);
    drop((a, b));
    let _ = std::fs::remove_dir_all(dir);
}

/// The egress routes over HTTP with the real agents file: `egress:write` attaches (for its own
/// agents only), `agents:write` alone cannot, not even through a create, and every change is
/// audited with the profiles it named.
#[test]
fn egress_routes_need_egress_write_and_are_audited() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let (fake, dir) = (Fake::new(), tmp("egress"));
    let _ = std::fs::create_dir_all(dir.join("profiles"));
    let me = std::fs::metadata("/proc/self").map_or(0, |m| m.uid());
    for (name, text) in [
        ("profiles/rust.txt", "index.crates.io\nstatic.crates.io\n"),
        ("allow.txt", "api.anthropic.com\n"),
    ] {
        let _ = std::fs::write(dir.join(name), text);
        let _ = std::fs::set_permissions(dir.join(name), std::fs::Permissions::from_mode(0o600));
    }
    let store = Arc::new(EgressStore::new(
        dir.join("egress/agents"),
        dir.join("profiles"),
        dir.join("allow.txt"),
        "10.89.0.1:3128",
        me,
    ));
    let file = TokenFile::new(dir.join("tokens"), me);
    let make = |name: &str, scopes: &str, agents: &str| {
        let scopes = Scopes::parse(scopes).unwrap_or_else(|e| panic!("{e}"));
        let agents = Agents::parse(agents).unwrap_or_else(|e| panic!("{e}"));
        file.create(name, scopes, agents, 1)
            .unwrap_or_else(|e| panic!("{e}"))
    };
    let life = make("life", "agents:read,agents:write", "*");
    let attach = make("attach", "agents:read,egress:write", "a1");
    let mut t = file.list().unwrap_or_else(|e| panic!("{e}"));
    let _ = t.add(Token::admin(tokens::LEGACY, TOKEN));
    let fleet = Fleet::new(Rt, Store::default()).with_egress(store.clone());
    let api = Api::new(fleet).with_egress(store.clone());
    let addr = serve_api(&fake, &dir, 4, 16, Keys::new(t), api);
    let agents_file = || std::fs::read_to_string(dir.join("egress/agents")).unwrap_or_default();

    let rust = r#"{"egress_profiles":["rust"]}"#;
    let (status, body) = request_as(addr, &life, "POST", "/api/v1/agents/a2", &[], rust);
    assert_eq!(status, 403, "{body}");
    assert_eq!(
        request_as(addr, &life, "POST", "/api/v1/agents/a1", &[], "").0,
        201
    );
    assert!(
        agents_file().contains("a1 ") && agents_file().ends_with(" -\n"),
        "{}",
        agents_file()
    );
    assert_eq!(request(addr, "POST", "/api/v1/agents/a2", &[], rust).0, 201);
    let put = r#"{"profiles":["rust"]}"#;
    assert_eq!(
        request_as(addr, &life, "PUT", "/api/v1/agents/a1/egress", &[], put).0,
        403
    );
    assert_eq!(
        request_as(addr, &attach, "PUT", "/api/v1/agents/a2/egress", &[], put).0,
        404
    );
    assert_eq!(
        request_as(addr, &attach, "PUT", "/api/v1/agents/a1/egress", &[], put),
        (
            200,
            r#"{"id":"a1","profiles":["rust"],"effective_hosts":3}"#.into()
        )
    );
    assert_eq!(
        request_as(addr, &attach, "GET", "/api/v1/egress/profiles", &[], ""),
        (200, r#"{"profiles":[{"name":"rust","hosts":2}]}"#.into())
    );
    assert_eq!(
        request_as(
            addr,
            &attach,
            "PUT",
            "/api/v1/agents/a1/egress",
            &[],
            r#"{"profiles":["go"]}"#
        )
        .0,
        400
    );
    let lines: Vec<String> = agents_file()
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(str::to_owned)
        .collect();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines
        .iter()
        .all(|l| l.ends_with(" rust") && l.split(' ').nth(1).is_some_and(|d| d.len() == 64)));
    let (_, a1) = request(addr, "GET", "/api/v1/agents/a1/history", &[], "");
    let (_, a2) = request(addr, "GET", "/api/v1/agents/a2/history", &[], "");
    for (history, line) in [
        (
            &a2,
            "audit token=legacy action=create profiles=rust status=201",
        ),
        (
            &a1,
            "audit token=attach action=egress profiles=rust status=200",
        ),
    ] {
        assert!(history.contains(line), "{line} missing from {history}");
    }
    // A refused change (400) is in the gateway's log only, as for every route.
    assert!(!a1.contains("status=400"), "{a1}");
    let mode = std::fs::metadata(dir.join("egress/agents")).map(|m| (m.mode() & 0o777, m.uid()));
    assert_eq!(mode.ok(), Some((0o600, me)));
    let _ = std::fs::remove_dir_all(dir);
}

/// One MCP tool call as `token`: `(isError, text)`.
fn tool(addr: SocketAddr, token: &str, name: &str, args: &str) -> (bool, String) {
    let body = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"{name}","arguments":{args}}}}}"#
    );
    let (status, reply) = request_as(addr, token, "POST", "/api/v1/mcp", &[], &body);
    assert_eq!(status, 200, "{reply}");
    let v = rusty_json::Value::parse(&reply).unwrap_or_else(|e| panic!("{e}: {reply}"));
    let text = v.pointer("/result/content/0/text").and_then(|t| t.as_str());
    let error = v.pointer("/result/isError").and_then(|e| e.as_bool());
    (
        error.unwrap_or_else(|| panic!("{reply}")),
        text.unwrap_or_default().to_owned(),
    )
}

/// The MCP endpoint (ADR-017): a tool call is served as the REST route it names, with that
/// route's scope, the token's agents and the audit; `tools/list` shows what the scopes allow.
#[test]
fn mcp_tools_follow_the_token_scopes() {
    use std::os::unix::fs::MetadataExt;
    let (fake, dir) = (Fake::new(), tmp("mcp"));
    let _ = std::fs::create_dir_all(&dir);
    let me = std::fs::metadata("/proc/self").map_or(0, |m| m.uid());
    let file = TokenFile::new(dir.join("tokens"), me);
    let scopes = Scopes::parse("jobs:read,jobs:write").unwrap_or_else(|e| panic!("{e}"));
    let agents = Agents::parse("a1").unwrap_or_else(|e| panic!("{e}"));
    let jobs = file
        .create("worker", scopes, agents, 1)
        .unwrap_or_else(|e| panic!("{e}"));
    let mut t = file.list().unwrap_or_else(|e| panic!("{e}"));
    let _ = t.add(Token::admin(tokens::LEGACY, TOKEN));
    let addr = serve_with(&fake, &dir, 4, 16, Keys::new(t));
    let rpc = |token: &str, extra: &[&str], body: &str| {
        request_as(addr, token, "POST", "/api/v1/mcp", extra, body)
    };

    let init = r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#;
    let (status, body) = rpc(&jobs, &[], init);
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains(r#""protocolVersion":"2025-06-18""#) && body.contains(r#""tools":"#),
        "{body}"
    );
    let note = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    assert_eq!(rpc(&jobs, &[], note).0, 202);

    let names = |token: &str| {
        let (_, body) = rpc(
            token,
            &[],
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        );
        let v = rusty_json::Value::parse(&body).unwrap_or_else(|e| panic!("{e}: {body}"));
        let tools = v
            .pointer("/result/tools")
            .and_then(|t| t.as_array())
            .cloned();
        tools
            .unwrap_or_default()
            .iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(str::to_owned))
            .collect::<Vec<_>>()
    };
    let worker = names(&jobs);
    assert_eq!(
        worker,
        [
            "whoami",
            "list_jobs",
            "submit_job",
            "get_job",
            "get_job_output",
            "cancel_job"
        ]
    );
    assert_eq!(names(TOKEN).len(), ac_gateway::mcp::offers().count());

    // Admin creates a1 over MCP; the call is audited like the REST one.
    let (failed, text) = tool(addr, TOKEN, "create_agent", r#"{"agent_id":"a1"}"#);
    assert!(!failed && text.contains(r#""state":"running""#), "{text}");
    let (_, history) = tool(addr, TOKEN, "get_history", r#"{"agent_id":"a1"}"#);
    assert!(
        history.contains("audit token=legacy action=create status=201"),
        "{history}"
    );

    // The jobs token: not its scope is 403, not its agent is 404. (This gateway has no task API,
    // so its job routes are 404 for a1 too; whoami shows a call that passes.)
    let (failed, text) = tool(addr, &jobs, "create_agent", r#"{"agent_id":"a1"}"#);
    assert!(
        failed && text.starts_with("HTTP 403") && text.contains("agents:write"),
        "{text}"
    );
    let (failed, text) = tool(addr, &jobs, "list_jobs", r#"{"agent_id":"a2"}"#);
    assert!(failed && text.starts_with("HTTP 404"), "{text}");
    let (failed, text) = tool(addr, &jobs, "list_jobs", r#"{"agent_id":"a1/../a2"}"#);
    assert!(failed && text.contains("agent_id:"), "{text}");
    let (failed, text) = tool(addr, &jobs, "whoami", "{}");
    assert!(!failed && text.contains(r#""name":"worker""#), "{text}");

    // Protocol errors.
    let (status, body) = rpc(
        &jobs,
        &[],
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"nope"}}"#,
    );
    assert!(status == 200 && body.contains("-32602"), "{body}");
    assert_eq!(rpc(&jobs, &[], "{").0, 400);
    assert_eq!(
        rpc(&jobs, &["MCP-Protocol-Version: 1999-01-01"], init).0,
        400
    );
    assert_eq!(
        rpc(&jobs, &["MCP-Protocol-Version: 2025-06-18"], init).0,
        200
    );
    assert_eq!(rpc("wrong", &[], init).0, 401);
    assert_eq!(
        request_as(addr, &jobs, "GET", "/api/v1/mcp", &[], "").0,
        405
    );
    let cross = ["Origin: http://evil.example"];
    assert_eq!(rpc(&jobs, &cross, init).0, 403);
    let _ = std::fs::remove_dir_all(dir);
}
