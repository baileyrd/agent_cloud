//! The proxy: accept, read the head, identify the caller and decide, resolve and vet, connect,
//! then splice bytes both ways until either side closes or the connection goes idle. One thread
//! per direction. Before each decision the egress files are re-checked (a stat each), so a
//! changed attachment applies to the next request.

use std::io::{self, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use ac_domain::Allowlist;

use crate::auth;
use crate::egressfile::EgressFiles;
use crate::head::{read_head, Head, HeadLimits};
use crate::policy::{judge, vet, would_allow, Decision, Policy, Reason, Verdict};

/// Name resolution and connecting, injectable so tests can point names anywhere.
pub trait Upstream: Send + Sync + 'static {
    /// Resolve `host` once; the proxy connects only to addresses from this answer.
    fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>>;

    /// Connect to one vetted address.
    fn connect(&self, addr: SocketAddr, timeout: Duration) -> io::Result<TcpStream> {
        TcpStream::connect_timeout(&addr, timeout)
    }
}

/// The host's resolver (`getaddrinfo`) and plain TCP.
#[derive(Debug, Default)]
pub struct System;

impl Upstream for System {
    fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        Ok((host, port).to_socket_addrs()?.collect())
    }
}

/// Resource bounds for the proxy.
#[derive(Debug, Clone)]
pub struct Limits {
    /// Bounds on the request head.
    pub head: HeadLimits,
    /// Time allowed for the client to send its head.
    pub head_timeout: Duration,
    /// Most connections served at once; more are answered 503.
    pub max_conns: usize,
    /// A tunnel with no bytes either way for this long is closed.
    pub idle: Duration,
    /// Time allowed for each upstream connect attempt.
    pub connect_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            head: HeadLimits::default(),
            head_timeout: Duration::from_secs(10),
            max_conns: 256,
            idle: Duration::from_secs(600),
            connect_timeout: Duration::from_secs(10),
        }
    }
}

/// The allowlist proxy.
pub struct Proxy<U> {
    policy: RwLock<Policy>,
    files: Option<Mutex<EgressFiles>>,
    upstream: U,
    limits: Limits,
    active: Arc<AtomicUsize>,
}

/// One log line per decision: the source address, the agent (`-` without a credential, `<id>?`
/// for a refused one), the target and the outcome. Never headers, credentials or bodies.
pub fn log_line(
    src: &str,
    who: &str,
    host: &str,
    port: u16,
    outcome: &Result<(), String>,
) -> String {
    let host = host.escape_debug();
    match outcome {
        Ok(()) => format!("ac-egress: allow {src} agent={who} {host}:{port}"),
        Err(reason) => format!("ac-egress: deny {src} agent={who} {host}:{port}: {reason}"),
    }
}

/// A refused request: the status, the client's text and the log's (which may say more).
struct Denied {
    status: u16,
    client: String,
    log: String,
}

impl Denied {
    fn policy(reason: &Reason) -> Self {
        Self {
            status: 403,
            client: format!("denied: {reason}"),
            log: reason.to_string(),
        }
    }
}

/// Holds one of the `max_conns` slots until dropped.
struct Slot(Arc<AtomicUsize>);

impl Slot {
    fn take(active: &Arc<AtomicUsize>, max: usize) -> Option<Self> {
        let prev = active.fetch_add(1, Ordering::AcqRel);
        if prev >= max {
            active.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(Self(Arc::clone(active)))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl<U: Upstream> Proxy<U> {
    /// A proxy enforcing `list` for every agent.
    pub fn new(list: Allowlist, upstream: U, limits: Limits) -> Self {
        Self {
            policy: RwLock::new(Policy {
                base: list,
                ..Policy::default()
            }),
            files: None,
            upstream,
            limits,
            active: Arc::default(),
        }
    }

    /// Also enforce per-agent profiles from `files` (ADR-016), re-checked before each decision.
    pub fn with_files(mut self, files: EgressFiles) -> Self {
        self.files = Some(Mutex::new(files));
        self
    }

    /// Replace the base allowlist; connections already open keep running.
    pub fn set_allowlist(&self, list: Allowlist) {
        self.policy.write().unwrap_or_else(|e| e.into_inner()).base = list;
    }

    /// Re-read the egress files if they changed (always with `force`). A bad version is logged
    /// once and the current profiles and agents stay.
    pub fn refresh(&self, force: bool) {
        let Some(files) = &self.files else {
            return;
        };
        let mut files = files.lock().unwrap_or_else(|e| e.into_inner());
        match files.reload(force) {
            Ok(Some(loaded)) => {
                for w in &loaded.warnings {
                    eprintln!("ac-egress: warning: {w}");
                }
                eprintln!(
                    "ac-egress: egress files read: {} profile(s) from {}, {} agent(s) from {}",
                    loaded.profiles.len(),
                    files.profiles_path().display(),
                    loaded.agents.len(),
                    files.agents_path().display()
                );
                let mut policy = self.policy.write().unwrap_or_else(|e| e.into_inner());
                policy.profiles = loaded.profiles;
                policy.agents = loaded.agents;
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("ac-egress: egress files not reloaded, keeping the previous ones: {e}")
            }
        }
    }

    /// Serve `listener` forever, one thread per connection.
    pub fn serve(self: Arc<Self>, listener: TcpListener) {
        for conn in listener.incoming() {
            let client = match conn {
                Ok(c) => c,
                Err(e) => {
                    // Typically out of file descriptors: back off instead of spinning.
                    eprintln!("ac-egress: accept: {e}");
                    thread::sleep(Duration::from_millis(100));
                    continue;
                }
            };
            let Some(slot) = Slot::take(&self.active, self.limits.max_conns) else {
                let _ = client.set_write_timeout(Some(self.limits.head_timeout));
                respond(&client, 503, "too many connections");
                continue;
            };
            let proxy = Arc::clone(&self);
            thread::spawn(move || {
                let _slot = slot;
                proxy.handle(client);
            });
        }
    }

    fn handle(&self, client: TcpStream) {
        let src = client
            .peer_addr()
            .map(|a| a.ip().to_string())
            .unwrap_or_else(|_| "?".to_owned());
        let timeout = Some(self.limits.head_timeout);
        let Ok(read_half) = client.try_clone() else {
            return;
        };
        if read_half.set_read_timeout(timeout).is_err()
            || client.set_write_timeout(timeout).is_err()
        {
            return;
        }
        let mut reader = BufReader::new(read_half);
        let head = match read_head(&mut reader, &self.limits.head) {
            Ok(head) => head,
            Err(e) => {
                if let Some(status) = e.status() {
                    eprintln!("ac-egress: refused {src}: {e}");
                    respond(&client, status, &e.to_string());
                }
                return;
            }
        };
        self.refresh(false);
        let (who, addrs) = self.admit(&head);
        let outcome = addrs.as_ref().map(|_| ()).map_err(|d| d.log.clone());
        eprintln!("{}", log_line(&src, &who, &head.host, head.port, &outcome));
        let addrs = match addrs {
            Ok(addrs) => addrs,
            Err(denied) => return respond(&client, denied.status, &denied.client),
        };
        let Some(mut upstream) = self.connect(&addrs) else {
            eprintln!(
                "ac-egress: error {src} agent={who} {}:{}: connect failed",
                head.host.escape_debug(),
                head.port
            );
            return respond(&client, 502, "upstream connect failed");
        };
        let opened = match head.upstream() {
            None => (&client).write_all(b"HTTP/1.1 200 Connection established\r\n\r\n"),
            Some(text) => upstream.write_all(text.as_bytes()),
        };
        // Bytes the client sent after its head (an early TLS hello, a request body).
        let early = reader.buffer().to_vec();
        if opened.and_then(|()| upstream.write_all(&early)).is_err() {
            return;
        }
        splice(reader.into_inner(), client, upstream, self.limits.idle);
    }

    /// Identify the caller and decide, then resolve once and vet: the caller's log label and the
    /// addresses this request may use.
    fn admit(&self, head: &Head) -> (String, Result<Vec<SocketAddr>, Denied>) {
        let presented = auth::parse(&head.proxy_authorization);
        let policy = self.policy.read().unwrap_or_else(|e| e.into_inner());
        let caller = match judge(&policy, &presented, &head.host, head.port) {
            Verdict::Refused(refused) => {
                let denied = Denied {
                    status: 407,
                    client: "proxy credential refused".to_owned(),
                    log: refused.to_string(),
                };
                return (refused.label(), Err(denied));
            }
            Verdict::Decided(caller, Decision::Deny(reason)) => {
                let mut denied = Denied::policy(&reason);
                let names: Vec<&str> = would_allow(&policy, &head.host)
                    .into_iter()
                    .map(|p| p.as_str())
                    .collect();
                if reason == Reason::NotAllowed && !names.is_empty() {
                    denied.log = format!("{reason} (profile {} would allow it)", names.join(","));
                }
                return (caller.label(), Err(denied));
            }
            Verdict::Decided(caller, Decision::Allow) => caller,
        };
        drop(policy);
        let addrs = self
            .upstream
            .resolve(&head.host, head.port)
            .map_err(|_| Reason::Lookup)
            .and_then(vet);
        (caller.label(), addrs.map_err(|r| Denied::policy(&r)))
    }

    fn connect(&self, addrs: &[SocketAddr]) -> Option<TcpStream> {
        addrs
            .iter()
            .find_map(|&a| self.upstream.connect(a, self.limits.connect_timeout).ok())
    }
}

/// Answer and close: a short plain-text body naming the problem. A 407 asks for the agent's
/// credential (`Proxy-Authenticate`).
fn respond(mut client: &TcpStream, status: u16, message: &str) {
    let text = match status {
        400 => "Bad Request",
        403 => "Forbidden",
        407 => "Proxy Authentication Required",
        408 => "Request Timeout",
        431 => "Request Header Fields Too Large",
        502 => "Bad Gateway",
        _ => "Service Unavailable",
    };
    let body = format!("ac-egress: {message}\n");
    let challenge = match status {
        407 => "Proxy-Authenticate: Basic realm=\"agent-cloud\"\r\n",
        _ => "",
    };
    let reply = format!(
        "HTTP/1.1 {status} {text}\r\n{challenge}Content-Type: text/plain\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = client.write_all(reply.as_bytes());
    let _ = client.shutdown(Shutdown::Both);
}

/// Copy client->upstream on a second thread and upstream->client on this one. A side's EOF
/// half-closes the other; no bytes either way for `idle` closes both.
fn splice(client_in: TcpStream, client_out: TcpStream, upstream: TcpStream, idle: Duration) {
    let clock = Instant::now();
    let last = Arc::new(AtomicU64::new(0));
    let Ok(upstream_in) = upstream.try_clone() else {
        return;
    };
    let (l, c) = (Arc::clone(&last), clock);
    let up = thread::spawn(move || pump(client_in, upstream, &l, c, idle));
    pump(upstream_in, client_out, &last, clock, idle);
    let _ = up.join();
}

fn pump(mut from: TcpStream, mut to: TcpStream, last: &AtomicU64, clock: Instant, idle: Duration) {
    let millis = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
    let poll = idle.min(Duration::from_secs(5));
    if from.set_read_timeout(Some(poll)).is_err() || to.set_write_timeout(Some(idle)).is_err() {
        return;
    }
    let mut buf = [0u8; 16 * 1024];
    loop {
        match from.read(&mut buf) {
            Ok(0) => {
                let _ = to.shutdown(Shutdown::Write);
                return;
            }
            Ok(n) => {
                if to.write_all(&buf[..n]).is_err() {
                    break;
                }
                last.store(millis(clock.elapsed()), Ordering::Relaxed);
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                let quiet = millis(clock.elapsed()).saturating_sub(last.load(Ordering::Relaxed));
                if quiet >= millis(idle) {
                    break;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    let _ = from.shutdown(Shutdown::Both);
    let _ = to.shutdown(Shutdown::Both);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_line_names_source_agent_target_and_outcome_only() {
        assert_eq!(
            log_line("10.89.0.21", "a1", "api.anthropic.com", 443, &Ok(())),
            "ac-egress: allow 10.89.0.21 agent=a1 api.anthropic.com:443"
        );
        let why = Err(Reason::NotAllowed.to_string());
        assert_eq!(
            log_line("10.89.0.21", "-", "example.com", 443, &why),
            "ac-egress: deny 10.89.0.21 agent=- example.com:443: not on allowlist"
        );
        assert!(log_line("s", "-", "a\u{1b}[2J", 80, &Ok(())).contains("\\u{1b}"));
    }

    #[test]
    fn slots_are_bounded_and_returned() {
        let active = Arc::default();
        let a = Slot::take(&active, 2);
        let b = Slot::take(&active, 2);
        assert!(a.is_some() && b.is_some() && Slot::take(&active, 2).is_none());
        drop(a);
        assert!(Slot::take(&active, 2).is_some());
    }
}
