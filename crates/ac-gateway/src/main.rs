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
//! Job start gate (ADR-013; defaults in [`Limits::for_cores`]): `AC_MAX_RUNNING_JOBS` (host-wide,
//! default the core count), `AC_JOB_CPU_PSI_MAX` (start only while cpu PSI some avg10 is below
//! this, 40), `AC_JOB_START_SPACING` (least seconds between starts, 15).
//! Settings (ADR-014): `AC_PROFILE` (`idle`, `build` or `heavy`, default build); at startup the
//! gateway logs one line if its settings differ from what `ac-ctl doctor` recommends.
//! Egress profiles (ADR-016, with `AC_NETWORK=1`): `AC_EGRESS_AGENTS` (the agents file it writes,
//! default /etc/agent-cloud/egress/agents), `AC_EGRESS_PROFILES` (default
//! /etc/agent-cloud/egress/profiles), `AC_EGRESS_ALLOW` (the base list, for host counts; default
//! /etc/agent-cloud/egress-allow.txt), `AC_EGRESS_PROXY` (what credentials name, default
//! 10.89.0.1:3128). Every create and resume boots with a new credential; a malformed agents file
//! refuses the start.
//! Checkpoints (ADR-018): `AC_CHECKPOINT_MAX` (checkpoints each agent may keep, default 8),
//! `AC_BTRFS` (the btrfs binary, default `btrfs`). Checkpoints live under `$AC_DATA/checkpoints`;
//! at startup the gateway logs whether the workspace root is on btrfs and which workspaces are
//! still plain directories.
//! Admission (ADR-012): `AC_SHM_MARGIN_MIB` (MiB of `/dev/shm` kept free, default 512),
//! `AC_VM_MEMORY_MIB` (guest size, instead of `default_memory` in `AC_KATA_CONFIG`, default
//! /etc/kata-containers/configuration.toml), `AC_SHM_LIMIT_MIB` (testing only: treat `/dev/shm` as
//! this big; logged as a warning at startup).
//!
//! It refuses to start without a token, or with a tokens file that is unreadable, too open, owned
//! by someone else or malformed. A bad later version is logged and the previous tokens stay.
//!
//! Readiness (ADR-015): with `NOTIFY_SOCKET` set (`Type=notify`) it sends `READY=1` once the
//! tokens are loaded, the reconcile has run, admission is set up and the listener is bound; a
//! start that fails exits non-zero first. SIGTERM or SIGINT sends `STOPPING=1` and exits 0
//! within a second.
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
use ac_ctr::facts::{collect, LinuxHost, Paths, Scope};
use ac_ctr::host::{Monotonic, ProcPressure};
use ac_ctr::{CtrConfig, CtrRuntime, Network};
use ac_domain::health::Thresholds;
use ac_domain::schedule::Limits;
use ac_domain::tune::rules::drift;
use ac_domain::tune::{recommend, Fact, Profile};
use ac_domain::{GuestMemory, State};
use ac_fleet::Fleet;
use ac_gateway::egress::EgressStore;
use ac_gateway::jobs::{self, Jobs};
use ac_gateway::notify::{self, Notify};
use ac_gateway::tokens::{self, Agents, Keys, Scopes, Token, TokenFile, Tokens};
use ac_gateway::viewer::Hub;
use ac_gateway::watchdog::Watchdog;
use ac_gateway::web::App;
use ac_gateway::{bind, Api, Unhealthy};
use ac_store::EngineStore;
use platform::events::{SignalEvent, SignalSource};
use platform_linux::{LinuxSignalSource, LinuxSpawner};
use rusty_serve::{Server, ShutdownHandle};

const TOKENS: &str = "/etc/agent-cloud/tokens";

type Gateway = App<CtrRuntime<LinuxSpawner>, Arc<EngineStore>>;

/// A number >= 0 from the environment, `default` when unset.
fn num(key: &str, default: f64) -> Result<f64, String> {
    match std::env::var(key) {
        Ok(v) => v
            .parse::<f64>()
            .ok()
            .filter(|n| n.is_finite() && *n >= 0.0)
            .ok_or_else(|| format!("{key}={v}: want a number >= 0")),
        Err(_) => Ok(default),
    }
}

/// The watchdog's thresholds: [`Thresholds::default`], each overridable from the environment.
fn thresholds() -> Result<Thresholds, String> {
    let d = Thresholds::default();
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

/// The job start gate: [`Limits::for_cores`] for this host, each overridable from the environment.
fn limits() -> Result<Limits, String> {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let d = Limits::for_cores(u32::try_from(cores).unwrap_or(u32::MAX));
    let max = num("AC_MAX_RUNNING_JOBS", f64::from(d.max_running))?;
    if max < 1.0 || max.fract() != 0.0 {
        return Err(format!(
            "AC_MAX_RUNNING_JOBS={max}: want a whole number >= 1"
        ));
    }
    Ok(Limits {
        max_running: max as u32,
        cpu_some_max: num("AC_JOB_CPU_PSI_MAX", d.cpu_some_max)?,
        spacing_ms: (num("AC_JOB_START_SPACING", d.spacing_ms as f64 / 1000.0)? * 1000.0) as u64,
        ..d
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

/// Each second: on SIGTERM or SIGINT, send `STOPPING=1` and stop the server; otherwise re-read
/// the tokens file if it changed or on SIGHUP (a bad version keeps the current tokens). The
/// handlers are installed by the caller.
fn watch(
    signals: LinuxSignalSource,
    mut file: TokenFile,
    legacy: Option<Token>,
    keys: Keys,
    notify: Notify,
    stop: ShutdownHandle,
) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(1));
        let signal = signals.take();
        if let Some(s @ (SignalEvent::Terminate | SignalEvent::Interrupt)) = signal {
            eprintln!("ac-gateway: {s:?}: stopping");
            notify.send(notify::STOPPING);
            return stop.shutdown();
        }
        let force = signal == Some(SignalEvent::Hangup);
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
}

/// The agents file and profiles (ADR-016), read once now: a malformed agents file refuses the
/// start, since rewriting it would lose the attachments it holds.
fn egress_store() -> Result<EgressStore, String> {
    let store = EgressStore::new(
        env("AC_EGRESS_AGENTS", "/etc/agent-cloud/egress/agents"),
        env("AC_EGRESS_PROFILES", "/etc/agent-cloud/egress/profiles"),
        env("AC_EGRESS_ALLOW", "/etc/agent-cloud/egress-allow.txt"),
        &env("AC_EGRESS_PROXY", "10.89.0.1:3128"),
        euid()?,
    );
    let agents = store.load()?;
    let attached = agents.iter().filter(|a| !a.profiles.is_empty()).count();
    let profiles = ac_gateway::egress::Profiles::list(&store).unwrap_or_default();
    eprintln!(
        "ac-gateway: egress: {} agent line(s) in {} ({attached} with profiles), {} profile(s); \
         every boot gets a new proxy credential",
        agents.len(),
        store.agents_path().display(),
        profiles.len()
    );
    Ok(store)
}

/// One log line when this gateway's settings (its environment, Kata's configuration, `/dev/shm`)
/// differ from what `ac-ctl doctor` recommends for this host and `AC_PROFILE` (ADR-014). It
/// only warns; it never changes anything.
fn settings_check() {
    let kata = env("AC_KATA_CONFIG", "/etc/kata-containers/configuration.toml");
    let mut facts = match collect(
        &LinuxHost::new(LinuxSpawner),
        &Paths::system(kata.into()),
        Scope::Settings,
    ) {
        Ok(f) => f,
        Err(e) => return eprintln!("ac-gateway: settings check skipped: {e}"),
    };
    facts.gateway = Fact::Known(
        std::env::vars()
            .filter(|(k, _)| k.starts_with("AC_"))
            .collect(),
    );
    let name = env("AC_PROFILE", "build");
    let Some(profile) = Profile::parse(&name) else {
        return eprintln!(
            "ac-gateway: AC_PROFILE={name}: want idle, build or heavy; run ac-ctl doctor"
        );
    };
    if let Some(line) = drift(&recommend(&facts, profile)) {
        eprintln!(
            "ac-gateway: settings differ from the recommendation for this host (profile {name}): \
             {line}; run ac-ctl doctor"
        );
    }
}

/// `AC_CHECKPOINT_MAX`, default 8, at least 1.
fn checkpoint_max() -> Result<usize, String> {
    match env("AC_CHECKPOINT_MAX", "8").parse::<usize>() {
        Ok(n) if n > 0 => Ok(n),
        Ok(_) => Err("AC_CHECKPOINT_MAX must be at least 1".to_owned()),
        Err(e) => Err(format!("AC_CHECKPOINT_MAX: {e}")),
    }
}

/// One line on whether workspaces can be checkpointed, naming those still to migrate.
fn log_volumes(rt: &CtrRuntime<LinuxSpawner>) {
    match rt.plain_workspaces() {
        Ok(None) => eprintln!("ac-gateway: checkpoints: the workspace root is not on btrfs; off"),
        Ok(Some(plain)) if plain.is_empty() => eprintln!("ac-gateway: checkpoints: on"),
        Ok(Some(plain)) => {
            let names: Vec<&str> = plain.iter().map(ac_domain::AgentId::as_str).collect();
            eprintln!(
                "ac-gateway: checkpoints: on; plain-directory workspaces to migrate (paused, \
                 ac-ctl workspace migrate <id>): {}",
                names.join(" ")
            )
        }
        Err(e) => eprintln!("ac-gateway: checkpoints: {e}"),
    }
}

fn run() -> Result<(), String> {
    let notify = Notify::from_env();
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
    let networked = env("AC_NETWORK", "") == "1";
    let network = if networked {
        Some(Network::defaults(&data).map_err(|e| e.to_string())?)
    } else {
        None
    };
    let cfg = CtrConfig {
        ctr_bin: env("AC_CTR", "ctr"),
        namespace: env("AC_NAMESPACE", "agents"),
        image,
        workspace_root: data.join("workspaces"),
        checkpoint_root: data.join("checkpoints"),
        backup_root: data.join("backups"),
        btrfs_bin: env("AC_BTRFS", "btrfs"),
        env_dir: data.join("boot"),
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
    let gate = limits()?;
    settings_check();
    eprintln!(
        "ac-gateway: jobs: at most {} running; a job starts while cpu PSI some < {} and load < {} x \
         cores, at least {}s after the last start",
        gate.max_running,
        gate.cpu_some_max,
        gate.load_per_core,
        gate.spacing_ms as f64 / 1000.0
    );
    let runtime = CtrRuntime::new(LinuxSpawner, cfg.clone());
    let store = Arc::new(EngineStore::open(&data).map_err(|e| e.to_string())?);
    let snapshots = CtrRuntime::new(LinuxSpawner, cfg.clone());
    log_volumes(&snapshots);
    let mut fleet = Fleet::new(runtime, Arc::clone(&store))
        .with_admission(Box::new(host_capacity()?), margin)
        .with_snapshots(Arc::new(snapshots), checkpoint_max()?);
    let egress = match networked {
        true => Some(Arc::new(egress_store()?)),
        false => None,
    };
    if let Some(e) = &egress {
        fleet = fleet.with_egress(e.clone());
    }
    let reconciled = match fleet.reconcile() {
        Ok(changed) => {
            for id in changed {
                eprintln!("ac-gateway: reconcile: {id} had no VM; marked paused");
            }
            "reconcile done"
        }
        Err(e) => {
            eprintln!("ac-gateway: reconcile: {e}");
            "reconcile failed (see the log)"
        }
    };
    let listed = fleet.list().unwrap_or_default();
    let agents = listed
        .iter()
        .filter(|(_, s)| *s != State::Destroyed)
        .count();
    let running = listed.iter().filter(|(_, s)| *s == State::Running).count();
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
        .with_gate(gate, ProcPressure)
        .with_changes(CtrRuntime::new(LinuxSpawner, cfg.clone()))
        .with_notes(move |id, text| notes.note(id, text)),
    );
    jobs.reconcile();
    let unhealthy = Unhealthy::default();
    let mut api = Api::new(fleet)
        .with_unhealthy(Arc::clone(&unhealthy))
        .with_jobs(jobs.clone());
    if let Some(e) = egress {
        api = api.with_egress(e);
    }
    for id in api.running() {
        let term = hub.terminal();
        let cleared = term
            .kill_clients(&id, true)
            .and_then(|()| term.prepare(&id));
        if let Err(e) = cleared {
            eprintln!("ac-gateway: {id}: clearing old viewer clients: {e}");
        }
    }
    let app = Arc::new(App::new(api, hub.clone(), keys.clone()).with_jobs(Arc::clone(&jobs)));
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
    let bound = server
        .local_addr()
        .map_err(|e| format!("bind {addr}: {e}"))?;
    let stop = server
        .shutdown_handle()
        .map_err(|e| format!("bind {addr}: {e}"))?;
    let signals = LinuxSignalSource;
    signals
        .install(&[
            SignalEvent::Hangup,
            SignalEvent::Terminate,
            SignalEvent::Interrupt,
        ])
        .map_err(|e| format!("install signal handlers: {e:?}"))?;
    // Ready: everything above that can fail has succeeded, and the listener is bound (connections
    // queue until `run` accepts them).
    eprintln!("ac-gateway listening on {bound}");
    notify.send(&notify::ready(&format!(
        "listening on {bound}; {agents} agents ({running} running); {reconciled}"
    )));
    watch(signals, file, legacy, keys, notify, stop);
    server.run().map_err(|e| e.to_string())
}

const USAGE: &str = "usage: ac-gateway token create <name> --scopes <s,...> [--agents <id,...>|*]
       ac-gateway token list
       ac-gateway token revoke <name>
scopes: agents:read agents:write jobs:read jobs:write viewer:read viewer:control egress:write admin
  egress:write attaches egress profiles to agents (widens what they may reach); admin implies it";

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
