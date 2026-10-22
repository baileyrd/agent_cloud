//! `ac-gateway`: serves the lifecycle API over HTTP; `ac-gateway token create|list|revoke`
//! manages its API tokens (ADR-011) while it runs.
//!
//! Env: `AC_TOKENS` (the tokens file, default /etc/agent-cloud/tokens: owned by the gateway's
//! user, mode 0600; re-read on SIGHUP and when it changes, checked each second),
//! `AC_TOKEN_FILE` (deprecated: a file holding one token, served as the `admin` token `legacy`;
//! refused if group/world readable), `AC_BIND` (default 127.0.0.1:8780), `AC_IMAGE` (required),
//! `AC_DATA` (default /var/lib/agent-cloud), `AC_CTR`, `AC_NAMESPACE`,
//! `AC_UID` (workspace owner, default 1000), `AC_NETWORK=1` (attach agents to CNI),
//! `AC_ALLOW_PUBLIC_BIND=1` to override the private-address check, `AC_PROBE_INTERVAL`
//! (seconds between watchdog probes, default 30; 0 turns the watchdog off). Jobs (ADR-010):
//! `AC_JOB_PERMISSION_MODE` (default `acceptEdits`), `AC_JOB_ALLOWED_TOOLS` (comma-separated,
//! default [`jobs::DEFAULT_TOOLS`]), `AC_JOB_UNSAFE=1` (`--dangerously-skip-permissions` instead).
//!
//! It refuses to start without a token, or with a tokens file that is unreadable, too open, owned
//! by someone else or malformed. A bad later version is logged and the previous tokens stay.
//!
//! At startup it reconciles the registry with containerd once (see `Fleet::reconcile`): after a
//! host reboot, agents recorded running have no VM and become paused. A failed reconcile is
//! logged and the gateway serves anyway. Jobs of agents that are not running are then ended
//! (`Jobs::reconcile`); running jobs of running agents are re-attached by the job supervisor,
//! which ticks every second. It then SIGKILLs `ctr` clients an earlier gateway left
//! and detaches their tmux control clients in every running agent.
//!
//! The watchdog probes each running agent with a 2 s exec. If one does not answer (a wedged
//! Kata shim, ADR-006), it SIGKILLs the gateway's own `ctr` clients for that agent, then every
//! `ctr` exec client of it, probing after each. If the agent still does not answer it is
//! reported as `"health":"unhealthy"`; its VM is left running.

use std::net::SocketAddr;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ac_ctr::{CtrConfig, CtrRuntime, Network};
use ac_fleet::Fleet;
use ac_gateway::jobs::{self, Jobs};
use ac_gateway::tokens::{self, Agents, Keys, Scopes, Token, TokenFile, Tokens};
use ac_gateway::viewer::{Hub, Terminal};
use ac_gateway::web::App;
use ac_gateway::{bind, Api, Unhealthy};
use ac_store::EngineStore;
use platform::events::{SignalEvent, SignalSource};
use platform_linux::{LinuxSignalSource, LinuxSpawner};
use rusty_serve::Server;

const TOKENS: &str = "/etc/agent-cloud/tokens";

type Gateway = App<CtrRuntime<LinuxSpawner>, Arc<EngineStore>>;

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

/// The uid this process runs as (the owner of `/proc/self`), which must own the tokens file.
fn euid() -> Result<u32, String> {
    std::fs::metadata("/proc/self")
        .map(|m| m.uid())
        .map_err(|e| format!("/proc/self: {e}"))
}

fn tokens_file() -> Result<TokenFile, String> {
    Ok(TokenFile::new(env("AC_TOKENS", TOKENS), euid()?))
}

/// The legacy token joins every version of the file.
fn with_legacy(mut tokens: Tokens, legacy: Option<&Token>) -> Result<Tokens, String> {
    if let Some(l) = legacy {
        tokens.add(l.clone())?;
    }
    Ok(tokens)
}

/// The tokens at startup: the file (a bad one refuses the start) plus the legacy token.
fn load_tokens(file: &mut TokenFile) -> Result<(Tokens, Option<Token>), String> {
    let legacy = match std::env::var("AC_TOKEN_FILE") {
        Ok(path) => match read_token(&path)? {
            t if t.is_empty() => return Err(format!("token file {path} is empty")),
            t => Some(Token::admin(tokens::LEGACY, &t)),
        },
        Err(_) => None,
    };
    let from_file = file.reload(true)?.unwrap_or_default();
    let n = from_file.iter().count();
    let all = with_legacy(from_file, legacy.as_ref())?;
    if all.is_empty() {
        return Err(format!(
            "no API tokens: create one with `ac-gateway token create <name> --scopes admin` \
             (writes {}), or set AC_TOKEN_FILE",
            file.path().display()
        ));
    }
    eprintln!(
        "ac-gateway: tokens: {n} from {}{}",
        file.path().display(),
        if legacy.is_some() {
            ", plus legacy from AC_TOKEN_FILE (deprecated; see docs/install.md)"
        } else {
            ""
        }
    );
    Ok((all, legacy))
}

/// Re-read the tokens file on SIGHUP and whenever it changes; a bad version keeps the current
/// tokens.
fn watch_tokens(mut file: TokenFile, legacy: Option<Token>, keys: Keys) -> Result<(), String> {
    let signals = LinuxSignalSource;
    signals
        .install(&[SignalEvent::Hangup])
        .map_err(|e| format!("install SIGHUP handler: {e:?}"))?;
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(1));
        let force = signals.take() == Some(SignalEvent::Hangup);
        let next = file
            .reload(force)
            .and_then(|t| t.map(|t| with_legacy(t, legacy.as_ref())).transpose());
        match next {
            Ok(Some(tokens)) => {
                let n = tokens.iter().filter(|t| t.name != tokens::LEGACY).count();
                keys.set(tokens);
                eprintln!("ac-gateway: tokens reloaded: {n} from the file");
            }
            Ok(None) => {}
            Err(e) => eprintln!("ac-gateway: tokens not reloaded, keeping the current ones: {e}"),
        }
    });
    Ok(())
}

fn run() -> Result<(), String> {
    let mut file = tokens_file()?;
    let (tokens, legacy) = load_tokens(&mut file)?;
    let keys = Keys::new(tokens);
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
    let claude = jobs::claude_flags(
        std::env::var("AC_JOB_PERMISSION_MODE").ok().as_deref(),
        std::env::var("AC_JOB_ALLOWED_TOOLS").ok().as_deref(),
        env("AC_JOB_UNSAFE", "") == "1",
    )?;
    let runtime = CtrRuntime::new(LinuxSpawner, cfg.clone());
    let store = Arc::new(EngineStore::open(&data).map_err(|e| e.to_string())?);
    let fleet = Fleet::new(runtime, Arc::clone(&store));
    match fleet.reconcile() {
        Ok(changed) => {
            for id in changed {
                eprintln!("ac-gateway: reconcile: {id} had no VM; marked paused");
            }
        }
        Err(e) => eprintln!("ac-gateway: reconcile: {e}"),
    }
    let hub = Hub::new(
        CtrRuntime::new(LinuxSpawner, cfg.clone()),
        data.join("history"),
    );
    let notes = hub.clone();
    let jobs = Arc::new(
        Jobs::new(
            store.clone(),
            store,
            CtrRuntime::new(LinuxSpawner, cfg),
            data.join("jobs"),
            claude,
        )
        .with_notes(move |id, text| notes.note(id, text)),
    );
    jobs.reconcile();
    let unhealthy = Unhealthy::default();
    let api = Api::new(fleet)
        .with_unhealthy(Arc::clone(&unhealthy))
        .with_jobs(jobs.clone());
    for id in api.running() {
        let term = hub.terminal();
        let cleared = term
            .kill_clients(&id, true)
            .and_then(|()| term.prepare(&id));
        if let Err(e) = cleared {
            eprintln!("ac-gateway: {id}: clearing old viewer clients: {e}");
        }
    }
    watch_tokens(file, legacy, keys.clone())?;
    let app = Arc::new(App::new(api, hub.clone(), keys).with_jobs(Arc::clone(&jobs)));
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(1));
        jobs.tick();
    });
    if interval > 0 {
        let app = Arc::clone(&app);
        let every = Duration::from_secs(interval);
        std::thread::spawn(move || watchdog(&app, &hub, &unhealthy, every));
    }
    let server = Server::bind_shared(addr, Shared(app)).map_err(|e| format!("bind {addr}: {e}"))?;
    eprintln!("ac-gateway listening on {addr}");
    server.run().map_err(|e| e.to_string())
}

const USAGE: &str = "usage: ac-gateway token create <name> --scopes <s,...> [--agents <id,...>|*]
       ac-gateway token list
       ac-gateway token revoke <name>
scopes: agents:read agents:write jobs:read jobs:write viewer:read viewer:control admin";

/// `ac-gateway token ...`: edits the tokens file only (never the registry), so it works while the
/// gateway runs; the gateway picks the change up within a second.
fn token_cli(args: &[String]) -> Result<(), String> {
    let file = tokens_file()?;
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args[..] {
        ["create", name, ref rest @ ..] => {
            let (mut scopes, mut agents) = (None, Agents::All);
            let mut rest = rest.iter();
            while let Some(flag) = rest.next() {
                let value = rest.next().ok_or(USAGE)?;
                match *flag {
                    "--scopes" => scopes = Some(Scopes::parse(value)?),
                    "--agents" => agents = Agents::parse(value)?,
                    _ => return Err(USAGE.into()),
                }
            }
            let scopes = scopes.ok_or(USAGE)?;
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let secret = file.create(name, scopes, agents.clone(), now)?;
            println!("{secret}");
            eprintln!(
                "token {name} ({scopes} for {agents}) added to {}; shown once, never stored",
                file.path().display()
            );
            Ok(())
        }
        ["list"] => {
            println!("{:<20} {:<40} {:<20} CREATED", "NAME", "SCOPES", "AGENTS");
            for t in file.list()?.iter() {
                let created = t.created.map_or_else(|| "-".to_owned(), tokens::utc);
                let (scopes, agents) = (t.scopes.to_string(), t.agents.to_string());
                println!("{:<20} {scopes:<40} {agents:<20} {created}", t.name);
            }
            Ok(())
        }
        ["revoke", name] => {
            file.revoke(name)?;
            eprintln!("token {name} removed from {}", file.path().display());
            Ok(())
        }
        _ => Err(USAGE.into()),
    }
}

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("token") {
        return match token_cli(&args[1..]) {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("ac-gateway token: {e}");
                std::process::ExitCode::FAILURE
            }
        };
    }
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ac-gateway: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
