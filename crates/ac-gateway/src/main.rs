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
//! (seconds between watchdog probes, default 30; 0 turns the watchdog off). Watchdog thresholds
//! (ADR-012; defaults in [`Thresholds`]): `AC_PROBE_BUDGET` (seconds one probe may take, 10),
//! `AC_PROBE_FAILURES` (consecutive failures before acting, 3), `AC_PROBE_WINDOW` (seconds they must
//! span, 90), `AC_BUSY_LOAD_PER_CORE` (1.0), `AC_BUSY_IO_SOME` (20), `AC_BUSY_CPU_SOME` (50). Jobs (ADR-010):
//! `AC_JOB_PERMISSION_MODE` (default `acceptEdits`), `AC_JOB_ALLOWED_TOOLS` (comma-separated,
//! default [`jobs::DEFAULT_TOOLS`]), `AC_JOB_UNSAFE=1` (`--dangerously-skip-permissions` instead).
//! Admission (ADR-012): `AC_SHM_MARGIN_MIB` (MiB of `/dev/shm` kept free, default 512),
//! `AC_VM_MEMORY_MIB` (guest size, instead of `default_memory` in `AC_KATA_CONFIG`, default
//! /etc/kata-containers/configuration.toml), `AC_SHM_LIMIT_MIB` (testing only: treat `/dev/shm` as
//! this big; logged as a warning at startup).
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
//! The watchdog ([`ac_gateway::watchdog`]) probes each running agent. It acts only on sustained
//! failures while the host is not busy, then kills the gateway's own exec clients, then stale
//! foreign ones, then reports the agent `"health":"unhealthy"` (its VM left running), re-probing
//! after each step.

use std::net::SocketAddr;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ac_ctr::capacity::{CapacityConfig, GuestSource, HostCapacity};
use ac_ctr::host::{Monotonic, ProcPressure};
use ac_ctr::{CtrConfig, CtrRuntime, Network};
use ac_domain::health::Thresholds;
use ac_domain::GuestMemory;
use ac_fleet::Fleet;
use ac_gateway::jobs::{self, Jobs};
use ac_gateway::tokens::{self, Agents, Keys, Scopes, Token, TokenFile, Tokens};
use ac_gateway::viewer::Hub;
use ac_gateway::watchdog::Watchdog;
use ac_gateway::web::App;
use ac_gateway::{bind, Api, Unhealthy};
use ac_store::EngineStore;
use platform::events::{SignalEvent, SignalSource};
use platform_linux::{LinuxSignalSource, LinuxSpawner};
use rusty_serve::Server;

const TOKENS: &str = "/etc/agent-cloud/tokens";

type Gateway = App<CtrRuntime<LinuxSpawner>, Arc<EngineStore>>;

/// The watchdog's thresholds: [`Thresholds::default`], each overridable from the environment.
fn thresholds() -> Result<Thresholds, String> {
    let d = Thresholds::default();
    let num = |key: &str, default: f64| -> Result<f64, String> {
        match std::env::var(key) {
            Ok(v) => v
                .parse::<f64>()
                .ok()
                .filter(|n| n.is_finite() && *n >= 0.0)
                .ok_or_else(|| format!("{key}={v}: want a number >= 0")),
            Err(_) => Ok(default),
        }
    };
    let secs = |key: &str, default: Duration| -> Result<Duration, String> {
        Ok(Duration::from_secs_f64(num(key, default.as_secs_f64())?))
    };
    let failures = num("AC_PROBE_FAILURES", f64::from(d.failures))?;
    Ok(Thresholds {
        probe_budget: secs("AC_PROBE_BUDGET", d.probe_budget)?.max(Duration::from_secs(1)),
        failures: (failures as u32).max(1),
        window: secs("AC_PROBE_WINDOW", d.window)?,
        load_per_core: num("AC_BUSY_LOAD_PER_CORE", d.load_per_core)?,
        io_some: num("AC_BUSY_IO_SOME", d.io_some)?,
        cpu_some: num("AC_BUSY_CPU_SOME", d.cpu_some)?,
    })
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

/// An optional MiB figure from the environment.
fn env_mib(key: &str) -> Result<Option<u64>, String> {
    std::env::var(key)
        .ok()
        .map(|v| v.parse::<u64>().map_err(|e| format!("{key}={v}: {e}")))
        .transpose()
}

/// `/dev/shm` and the guest size, read from this host; says where the guest size came from.
fn host_capacity() -> Result<HostCapacity, String> {
    let capacity = CapacityConfig {
        shm: "/dev/shm".into(),
        kata_config: env("AC_KATA_CONFIG", "/etc/kata-containers/configuration.toml").into(),
        guest_mib: env_mib("AC_VM_MEMORY_MIB")?,
        shm_limit_mib: env_mib("AC_SHM_LIMIT_MIB")?,
    };
    if let Some(l) = capacity.shm_limit_mib {
        eprintln!(
            "ac-gateway: WARNING: AC_SHM_LIMIT_MIB={l} is set (testing only): admission treats \
             /dev/shm as {l} MiB, not its real size"
        );
    }
    let path = capacity.kata_config.display().to_string();
    let host = HostCapacity::new(capacity);
    let (mib, source) = host.guest().map_err(|e| e.to_string())?;
    let source = match source {
        GuestSource::Override => "AC_VM_MEMORY_MIB".to_owned(),
        GuestSource::Kata(GuestMemory::Configured(_)) => format!("default_memory in {path}"),
        GuestSource::Kata(GuestMemory::KataDefault) => {
            format!("Kata's default: {path} does not set default_memory")
        }
    };
    eprintln!("ac-gateway: admission: guests count {mib} MiB each ({source})");
    Ok(host)
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
    let margin = env_mib("AC_SHM_MARGIN_MIB")?.unwrap_or(512);
    let runtime = CtrRuntime::new(LinuxSpawner, cfg.clone());
    let store = Arc::new(EngineStore::open(&data).map_err(|e| e.to_string())?);
    let fleet =
        Fleet::new(runtime, Arc::clone(&store)).with_admission(Box::new(host_capacity()?), margin);
    match fleet.reconcile() {
        Ok(changed) => {
            for id in changed {
                eprintln!("ac-gateway: reconcile: {id} had no VM; marked paused");
            }
        }
        Err(e) => eprintln!("ac-gateway: reconcile: {e}"),
    }
    match fleet.capacity() {
        Ok(Some(r)) => eprintln!(
            "ac-gateway: admission: /dev/shm {} MiB ({} free), {} agent(s) running commit {} MiB, \
             margin {} MiB, {} more fit",
            r.shm_total_mib,
            r.shm_free_mib,
            r.running_agents,
            r.committed_mib,
            r.margin_mib,
            r.max_more_agents
        ),
        Ok(None) => {}
        Err(e) => eprintln!("ac-gateway: admission: {e}"),
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
            CtrRuntime::new(LinuxSpawner, cfg.clone()),
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
        let t = thresholds()?;
        eprintln!(
            "ac-gateway: watchdog: every {interval}s; acts after {} failed probes over {}s (budget \
             {}s each) while load <= {} x cores, io PSI some <= {}, cpu PSI some <= {}",
            t.failures,
            t.window.as_secs(),
            t.probe_budget.as_secs(),
            t.load_per_core,
            t.io_some,
            t.cpu_some
        );
        let notes = hub.clone();
        let mut dog = Watchdog::new(
            CtrRuntime::new(LinuxSpawner, cfg),
            ProcPressure,
            Monotonic::new(),
            t,
            unhealthy,
            Box::new(move |id, line| notes.note(id, line)),
        );
        std::thread::spawn(move || loop {
            std::thread::sleep(every);
            let running = app.api().lock().map(|a| a.running()).unwrap_or_default();
            dog.check(&running);
        });
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
