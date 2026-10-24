//! Readiness notifications (ADR-015) from the real binary: a fake `NOTIFY_SOCKET` bound here, a
//! fake `ctr` that takes a second to answer the reconcile, and a tokens file and data directory
//! under a temp dir.

use std::fmt::Display;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{SocketAddr as UnixAddr, UnixDatagram};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_ac-gateway");

fn ok<T, E: Display>(r: Result<T, E>) -> T {
    r.unwrap_or_else(|e| panic!("{e}"))
}

fn unique() -> String {
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    format!("ac-notify-{}-{n}", std::process::id())
}

/// A temp dir with a tokens file and a `ctr` that lists no tasks after a second.
fn setup() -> PathBuf {
    let dir = std::env::temp_dir().join(unique());
    let _ = std::fs::remove_dir_all(&dir);
    ok(std::fs::create_dir_all(&dir));
    let ctr = dir.join("ctr");
    ok(std::fs::write(
        &ctr,
        "#!/bin/sh\nsleep 1\necho 'TASK PID STATUS'\n",
    ));
    ok(std::fs::set_permissions(
        &ctr,
        std::fs::Permissions::from_mode(0o755),
    ));
    let made = ok(Command::new(BIN)
        .args(["token", "create", "t", "--scopes", "admin"])
        .env("AC_TOKENS", dir.join("tokens"))
        .output());
    assert!(made.status.success(), "{made:?}");
    dir
}

fn free_port() -> SocketAddr {
    ok(ok(TcpListener::bind("127.0.0.1:0")).local_addr())
}

/// The gateway on `addr`, `NOTIFY_SOCKET` set to `notify` if given.
fn gateway(dir: &Path, addr: SocketAddr, notify: Option<&str>) -> Child {
    let mut c = Command::new(BIN);
    c.env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("AC_TOKENS", dir.join("tokens"))
        .env("AC_BIND", addr.to_string())
        .env("AC_IMAGE", "test:latest")
        .env("AC_DATA", dir.join("data"))
        .env("AC_CTR", dir.join("ctr"))
        .env("AC_KATA_CONFIG", dir.join("kata.toml"))
        .env("AC_VM_MEMORY_MIB", "2048")
        .env("AC_PROBE_INTERVAL", "0")
        .stderr(Stdio::piped());
    if let Some(n) = notify {
        c.env("NOTIFY_SOCKET", n);
    }
    ok(c.spawn())
}

/// The next datagram within `wait`, if any.
fn recv(sock: &UnixDatagram, wait: Duration) -> Option<String> {
    ok(sock.set_read_timeout(Some(wait)));
    let mut buf = [0u8; 4096];
    let n = sock.recv(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf[..n]).into_owned())
}

fn listening(addr: SocketAddr) -> bool {
    TcpStream::connect_timeout(&addr, Duration::from_secs(1)).is_ok()
}

fn wait_listening(addr: SocketAddr) {
    let start = Instant::now();
    while !listening(addr) {
        assert!(start.elapsed() < Duration::from_secs(30), "never listened");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// SIGTERM, then the exit and what it logged.
fn terminate(child: Child) -> Output {
    let killed = ok(Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status());
    assert!(killed.success());
    ok(child.wait_with_output())
}

/// READY=1 once, with STATUS, and only once the listener is bound; STOPPING=1 on SIGTERM, then
/// exit 0.
fn ready_then_stopping(sock: &UnixDatagram, notify: &str) {
    let dir = setup();
    let addr = free_port();
    let child = gateway(&dir, addr, Some(notify));
    let ready = recv(sock, Duration::from_secs(30)).unwrap_or_default();
    assert!(listening(addr), "READY before the listener was bound");
    assert_eq!(
        ready,
        format!("READY=1\nSTATUS=listening on {addr}; 0 agents; reconcile done\n")
    );
    assert_eq!(recv(sock, Duration::from_millis(1500)), None, "READY twice");
    let out = terminate(child);
    assert_eq!(
        recv(sock, Duration::from_secs(5)).as_deref(),
        Some("STOPPING=1\n")
    );
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("Terminate: stopping"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn path_socket() {
    let path = std::env::temp_dir().join(format!("{}.sock", unique()));
    let sock = ok(UnixDatagram::bind(&path));
    ready_then_stopping(&sock, &path.display().to_string());
    let _ = std::fs::remove_file(path);
}

#[test]
fn abstract_socket() {
    let name = unique();
    let sock = ok(UnixDatagram::bind_addr(&ok(UnixAddr::from_abstract_name(
        &name,
    ))));
    ready_then_stopping(&sock, &format!("@{name}"));
}

#[test]
fn nothing_without_notify_socket() {
    let path = std::env::temp_dir().join(format!("{}.sock", unique()));
    let sock = ok(UnixDatagram::bind(&path));
    let dir = setup();
    let addr = free_port();
    let child = gateway(&dir, addr, None);
    wait_listening(addr);
    assert_eq!(recv(&sock, Duration::from_millis(1500)), None);
    assert!(terminate(child).status.success());
    assert_eq!(recv(&sock, Duration::from_millis(500)), None);
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_failed_send_is_logged_and_the_gateway_serves() {
    let dir = setup();
    let addr = free_port();
    let missing = dir.join("no-such.sock");
    let mut child = gateway(&dir, addr, Some(&missing.display().to_string()));
    wait_listening(addr);
    std::thread::sleep(Duration::from_millis(500));
    assert!(ok(child.try_wait()).is_none(), "the gateway exited");
    assert!(listening(addr));
    let out = terminate(child);
    assert!(out.status.success(), "{out:?}");
    let log = String::from_utf8_lossy(&out.stderr);
    assert!(
        log.contains("WARNING: systemd notification READY=1 not sent"),
        "{log}"
    );
    assert!(
        log.contains("WARNING: systemd notification STOPPING=1 not sent"),
        "{log}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_failed_start_sends_no_ready() {
    let path = std::env::temp_dir().join(format!("{}.sock", unique()));
    let sock = ok(UnixDatagram::bind(&path));
    let dir = setup();
    ok(std::fs::set_permissions(
        dir.join("tokens"),
        std::fs::Permissions::from_mode(0o644),
    ));
    let addr = free_port();
    let out = ok(gateway(&dir, addr, Some(&path.display().to_string())).wait_with_output());
    assert!(!out.status.success(), "{out:?}");
    assert!(!listening(addr));
    assert_eq!(recv(&sock, Duration::from_millis(500)), None);
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_dir_all(dir);
}
