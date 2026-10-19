//! `ac-egress`: serves the egress proxy on the agent bridge.
//!
//! Env: `AC_EGRESS_BIND` (default 10.89.0.1:3128, the CNI bridge gateway; must be a non-public
//! address, never 0.0.0.0), `AC_EGRESS_ALLOW` (default /etc/agent-cloud/egress-allow.txt, mode
//! 0600). The allowlist is re-read on SIGHUP and whenever the file changes (checked each second);
//! a bad new version is logged and the previous list stays in force.
//!
//! The bridge address exists only once the first agent's CNI ADD creates `acbr0`, so binding is
//! retried every two seconds until it does.

use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::time::Duration;

use ac_egress::allowfile::AllowFile;
use ac_egress::{is_public, Limits, Proxy, System};
use platform::events::{SignalEvent, SignalSource};
use platform_linux::LinuxSignalSource;

fn env(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

fn bind_when_ready(addr: SocketAddr) -> Result<TcpListener, String> {
    let mut waiting = false;
    loop {
        match TcpListener::bind(addr) {
            Ok(listener) => return Ok(listener),
            Err(e) if e.kind() == std::io::ErrorKind::AddrNotAvailable => {
                if !waiting {
                    eprintln!(
                        "ac-egress: {} not up yet (the first agent creates it); waiting",
                        addr.ip()
                    );
                    waiting = true;
                }
                std::thread::sleep(Duration::from_secs(2));
            }
            Err(e) => return Err(format!("bind {addr}: {e}")),
        }
    }
}

fn run() -> Result<(), String> {
    let addr: SocketAddr = env("AC_EGRESS_BIND", "10.89.0.1:3128")
        .parse()
        .map_err(|e| format!("AC_EGRESS_BIND: {e}"))?;
    if addr.ip().is_unspecified() || is_public(addr.ip()) {
        return Err(format!(
            "refusing to bind {addr}: use the agent bridge's private address (AC_EGRESS_BIND)"
        ));
    }
    let mut file = AllowFile::new(env("AC_EGRESS_ALLOW", "/etc/agent-cloud/egress-allow.txt"));
    let (list, n) = file.reload(true)?.ok_or("allowlist: nothing read")?;
    eprintln!(
        "ac-egress: allowlist {}: {n} entries",
        file.path().display()
    );
    let proxy = Arc::new(Proxy::new(list, System, Limits::default()));

    let signals = LinuxSignalSource;
    signals
        .install(&[SignalEvent::Hangup])
        .map_err(|e| format!("install SIGHUP handler: {e:?}"))?;
    let reloading = Arc::clone(&proxy);
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(1));
        let force = signals.take() == Some(SignalEvent::Hangup);
        match file.reload(force) {
            Ok(Some((list, n))) => {
                reloading.set_allowlist(list);
                eprintln!("ac-egress: allowlist reloaded: {n} entries");
            }
            Ok(None) => {}
            Err(e) => eprintln!("ac-egress: allowlist not reloaded, keeping the previous one: {e}"),
        }
    });

    let listener = bind_when_ready(addr)?;
    eprintln!("ac-egress listening on {addr}");
    proxy.serve(listener);
    Ok(())
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ac-egress: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
