//! `ac-gateway`: serves the lifecycle API over HTTP.
//!
//! Env: `AC_TOKEN_FILE` (file holding the bearer token; refused if group/world
//! readable), `AC_BIND` (default 127.0.0.1:8780), `AC_IMAGE` (required),
//! `AC_DATA` (default /var/lib/agent-cloud), `AC_CTR`, `AC_NAMESPACE`,
//! `AC_UID` (workspace owner, default 1000), `AC_NETWORK=1` (attach agents to CNI),
//! `AC_ALLOW_PUBLIC_BIND=1` to override the private-address check, `AC_PROBE_INTERVAL`
//! (seconds between watchdog probes, default 30; 0 turns the watchdog off).
//!
//! At startup it reconciles the registry with containerd once (see `Fleet::reconcile`): after a
//! host reboot, agents recorded running have no VM and become paused. A failed reconcile is
//! logged and the gateway serves anyway. It then SIGKILLs `ctr` clients an earlier gateway left
//! and detaches their tmux control clients in every running agent.
//!
//! The watchdog probes each running agent with a 2 s exec. If one does not answer (a wedged
//! Kata shim, ADR-006), it SIGKILLs the gateway's own `ctr` clients for that agent, then every
//! `ctr` exec client of it, probing after each. If the agent still does not answer it is
//! reported as `"health":"unhealthy"`; its VM is left running.

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ac_ctr::{CtrConfig, CtrRuntime, Network};
use ac_fleet::Fleet;
use ac_gateway::viewer::{Hub, Terminal};
use ac_gateway::web::App;
use ac_gateway::{bind, Api, Unhealthy};
use ac_store::EngineStore;
use platform_linux::LinuxSpawner;
use rusty_serve::Server;

type Gateway = App<CtrRuntime<LinuxSpawner>, EngineStore>;

/// Probe every running agent each `interval`; see the module docs.
fn watchdog(app: &Gateway, hub: &Hub, unhealthy: &Unhealthy, interval: Duration) {
    let term = hub.terminal();
    loop {
        std::thread::sleep(interval);
        let running = app.api().lock().map(|a| a.running()).unwrap_or_default();
        for id in running {
            let healthy = term.probe(&id) || recover(term, &id);
            let mut set = unhealthy.lock().unwrap_or_else(|e| e.into_inner());
            if healthy {
                set.remove(id.as_str());
            } else if set.insert(id.to_string()) {
                eprintln!(
                    "ac-gateway: watchdog: {id} still does not answer after killing every ctr exec \
                     client; marked unhealthy, VM left running"
                );
            }
        }
    }
}

fn recover(term: &dyn Terminal, id: &ac_domain::AgentId) -> bool {
    for (ours_only, which) in [
        (true, "the gateway's own ctr clients"),
        (false, "every ctr exec client"),
    ] {
        eprintln!("ac-gateway: watchdog: {id} did not answer a 2 s probe; SIGKILL {which} for it");
        if let Err(e) = term.kill_clients(id, ours_only) {
            eprintln!("ac-gateway: watchdog: {id}: {e}");
        }
        if term.probe(id) {
            eprintln!("ac-gateway: watchdog: {id} answers again");
            return true;
        }
    }
    false
}

/// The app behind an `Arc`, shared with the watchdog.
struct Shared(Arc<Gateway>);

impl rusty_serve::SharedHandler for Shared {
    fn handle(&self, r: &rusty_serve::Request<'_>) -> rusty_serve::Response {
        self.0.handle(r)
    }
}

fn env(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

fn read_token(path: &str) -> Result<String, String> {
    let mode = std::fs::metadata(path)
        .map_err(|e| format!("token file {path}: {e}"))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        return Err(format!(
            "token file {path} must not be group/world accessible (mode {:o})",
            mode & 0o777
        ));
    }
    let token = std::fs::read_to_string(path).map_err(|e| format!("token file {path}: {e}"))?;
    Ok(token.trim().to_owned())
}

fn run() -> Result<(), String> {
    let token =
        read_token(&std::env::var("AC_TOKEN_FILE").map_err(|_| "AC_TOKEN_FILE is required")?)?;
    let image = std::env::var("AC_IMAGE").map_err(|_| "AC_IMAGE is required")?;
    let addr: SocketAddr = env("AC_BIND", "127.0.0.1:8780")
        .parse()
        .map_err(|e| format!("AC_BIND: {e}"))?;
    if !bind::is_private(&addr) && env("AC_ALLOW_PUBLIC_BIND", "") != "1" {
        return Err(format!(
            "refusing to bind {addr}: not loopback, RFC 1918 or tailnet. Set AC_BIND to one of \
             those. (AC_ALLOW_PUBLIC_BIND=1 overrides this when run by hand; ac-gateway.service \
             unsets it.)"
        ));
    }
    let data = PathBuf::from(env("AC_DATA", "/var/lib/agent-cloud"));
    let uid: u32 = env("AC_UID", "1000")
        .parse()
        .map_err(|e| format!("AC_UID: {e}"))?;
    let network = if env("AC_NETWORK", "") == "1" {
        Some(Network::defaults(&data).map_err(|e| e.to_string())?)
    } else {
        None
    };
    let cfg = CtrConfig {
        ctr_bin: env("AC_CTR", "ctr"),
        namespace: env("AC_NAMESPACE", "agents"),
        image,
        workspace_root: data.join("workspaces"),
        workspace_owner: Some((uid, uid)),
        network,
        vc_root: "/run/vc".into(),
        task_root: "/run/containerd/io.containerd.runtime.v2.task".into(),
        command_timeout: Some(std::time::Duration::from_secs(120)),
        trace: std::env::var_os("AC_TRACE").is_some(),
    };
    let interval: u64 = env("AC_PROBE_INTERVAL", "30")
        .parse()
        .map_err(|e| format!("AC_PROBE_INTERVAL: {e}"))?;
    let runtime = CtrRuntime::new(LinuxSpawner, cfg.clone());
    let store = EngineStore::open(&data).map_err(|e| e.to_string())?;
    let fleet = Fleet::new(runtime, store);
    match fleet.reconcile() {
        Ok(changed) => {
            for id in changed {
                eprintln!("ac-gateway: reconcile: {id} had no VM; marked paused");
            }
        }
        Err(e) => eprintln!("ac-gateway: reconcile: {e}"),
    }
    let unhealthy = Unhealthy::default();
    let api = Api::new(fleet, &token)
        .ok_or("token file is empty")?
        .with_unhealthy(Arc::clone(&unhealthy));
    let hub = Hub::new(CtrRuntime::new(LinuxSpawner, cfg), data.join("history"));
    for id in api.running() {
        let term = hub.terminal();
        let cleared = term
            .kill_clients(&id, true)
            .and_then(|()| term.prepare(&id));
        if let Err(e) = cleared {
            eprintln!("ac-gateway: {id}: clearing old viewer clients: {e}");
        }
    }
    let app = Arc::new(App::new(api, hub.clone()));
    if interval > 0 {
        let app = Arc::clone(&app);
        let every = Duration::from_secs(interval);
        std::thread::spawn(move || watchdog(&app, &hub, &unhealthy, every));
    }
    let server = Server::bind_shared(addr, Shared(app)).map_err(|e| format!("bind {addr}: {e}"))?;
    eprintln!("ac-gateway listening on {addr}");
    server.run().map_err(|e| e.to_string())
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ac-gateway: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
