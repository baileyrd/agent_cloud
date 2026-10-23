//! `ac-ctl`: drive the agent fleet from the shell, and check (`doctor`) or set (`tune`) the
//! host's settings from its facts (ADR-014). `doctor` and `tune` never open the registry.
//!
//! Exit codes: 0 ok, 1 operation failed (or `status` of an unknown agent), 2 bad usage, 3 the
//! data directory is in use by another process (normally `ac-gateway`, which holds it while it
//! runs): use the gateway's API instead, or stop it. `reconcile` prints one `<id> paused` line
//! per agent it changed. `create` and `resume` are refused when the guest would not fit `/dev/shm`
//! (ADR-012; the gateway's `AC_SHM_MARGIN_MIB`, `AC_VM_MEMORY_MIB`, `AC_KATA_CONFIG` and
//! `AC_SHM_LIMIT_MIB` apply here too). `doctor`: 0 all ok, 1 warnings, 2 errors (or it could not
//! run). `tune`: 0 shown or applied, 1 refused or failed.

mod args;
mod doctor;
mod factsjson;
mod tune;

use std::io;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ac_ctr::capacity::{CapacityConfig, HostCapacity};
use ac_ctr::facts::{collect, kata_sandboxes, HostRead, LinuxHost, Paths, Scope};
use ac_ctr::{CtrConfig, CtrRuntime, Network};
use ac_domain::tune::edit::stamp;
use ac_domain::tune::{recommend, HostFacts};
use ac_fleet::{Fleet, FleetError};
use ac_ports::PortError;
use ac_store::EngineStore;
use args::{Action, Cli, Command, Doctor, Job, Tune, USAGE};
use platform_linux::LinuxSpawner;

/// Exit code when another process holds the data directory.
const IN_USE: u8 = 3;
/// Where `ac-gateway.service` reads `AC_BIND` from.
const GATEWAY_ENV: &str = "/etc/agent-cloud/gateway.env";

/// The gateway's address: `AC_BIND`, else the last `AC_BIND=` line of `gateway_env` (the
/// service's settings), else its default.
fn gateway_bind(var: Option<String>, gateway_env: Option<String>) -> String {
    var.or_else(|| {
        gateway_env?
            .lines()
            .rev()
            .find_map(|l| l.trim().strip_prefix("AC_BIND="))
            .map(str::to_owned)
    })
    .unwrap_or_else(|| "127.0.0.1:8780".to_owned())
}

/// An optional MiB figure from the environment.
fn env_mib(key: &str) -> Result<Option<u64>, String> {
    std::env::var(key)
        .ok()
        .map(|v| v.parse::<u64>().map_err(|e| format!("{key}={v}: {e}")))
        .transpose()
}

fn run(cli: &Cli) -> Result<String, Box<dyn std::error::Error>> {
    let network = if cli.network {
        Some(Network::defaults(&cli.data)?)
    } else {
        None
    };
    let cfg = CtrConfig {
        ctr_bin: cli.ctr.clone(),
        namespace: cli.namespace.clone(),
        image: cli.image.clone(),
        workspace_root: cli.workspaces.clone(),
        workspace_owner: Some((cli.uid, cli.uid)),
        network,
        vc_root: "/run/vc".into(),
        task_root: "/run/containerd/io.containerd.runtime.v2.task".into(),
        command_timeout: Some(std::time::Duration::from_secs(120)),
        trace: std::env::var_os("AC_TRACE").is_some(),
    };
    let capacity = HostCapacity::new(CapacityConfig {
        shm: "/dev/shm".into(),
        kata_config: std::env::var("AC_KATA_CONFIG")
            .unwrap_or_else(|_| "/etc/kata-containers/configuration.toml".to_owned())
            .into(),
        guest_mib: env_mib("AC_VM_MEMORY_MIB")?,
        shm_limit_mib: env_mib("AC_SHM_LIMIT_MIB")?,
    });
    let margin = env_mib("AC_SHM_MARGIN_MIB")?.unwrap_or(512);
    let fleet = Fleet::new(
        CtrRuntime::new(LinuxSpawner, cfg),
        EngineStore::open(&cli.data)?,
    )
    .with_admission(Box::new(capacity), margin);
    let (action, id) = match &cli.job {
        Job::Agent(action, id) => (*action, id),
        Job::Reconcile => {
            let changed = fleet.reconcile()?;
            if changed.is_empty() {
                return Ok("nothing to reconcile".to_owned());
            }
            let lines: Vec<String> = changed.iter().map(|id| format!("{id} paused")).collect();
            return Ok(lines.join("\n"));
        }
    };
    let state = match action {
        Action::Create => fleet.create(id)?,
        Action::Pause => fleet.pause(id)?,
        Action::Resume => fleet.resume(id)?,
        Action::Destroy { purge } => fleet.destroy(id, purge)?,
        Action::Status => fleet
            .state(id)?
            .ok_or_else(|| FleetError::NotFound(id.clone()))?,
    };
    Ok(format!("{id} {state:?}").to_lowercase())
}

/// Kata's configuration: `AC_KATA_CONFIG`, else the installed one.
fn kata_config() -> PathBuf {
    std::env::var("AC_KATA_CONFIG")
        .unwrap_or_else(|_| "/etc/kata-containers/configuration.toml".to_owned())
        .into()
}

fn paths(root: Option<&Path>) -> Paths {
    let p = Paths::system(kata_config());
    match root {
        Some(r) => p.rooted(r),
        None => p,
    }
}

/// The facts, from a facts file or this host, and where they came from.
fn host_facts(
    facts_file: Option<&Path>,
    root: Option<&Path>,
    scope: Scope,
) -> Result<(HostFacts, String), String> {
    if let Some(file) = facts_file {
        let text = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
        let facts = factsjson::from_json(&text).map_err(|e| format!("{}: {e}", file.display()))?;
        return Ok((facts, format!("facts file {}", file.display())));
    }
    let facts = collect(&LinuxHost::new(LinuxSpawner), &paths(root), scope)?;
    let source = match root {
        Some(r) => format!("this host, configuration under {}", r.display()),
        None => "this host".to_owned(),
    };
    Ok((facts, source))
}

fn run_doctor(d: &Doctor) -> Result<u8, String> {
    if d.explain {
        print!("{}", doctor::explain());
        return Ok(0);
    }
    let (facts, source) = host_facts(d.facts_file.as_deref(), d.root.as_deref(), Scope::Full)?;
    let report = doctor::report(facts, d.profile, &source)?;
    if d.json {
        println!("{}", doctor::json(&report));
    } else {
        print!("{}", doctor::table(&report));
    }
    Ok(doctor::exit_code(&report.findings))
}

/// A file tune may edit, or why it cannot be read.
fn text_of(path: &Path) -> (PathBuf, Result<String, String>) {
    let text = std::fs::read_to_string(path).map_err(|e| match e.kind() {
        io::ErrorKind::PermissionDenied => "root-only: run with sudo".to_owned(),
        _ => e.to_string(),
    });
    (path.to_owned(), text)
}

/// Wait up to `limit` for something to listen on `bind`: the gateway reports active before it
/// listens.
fn wait_for_port(bind: &str, limit: Duration) -> Result<Duration, String> {
    let addr: SocketAddr = bind.parse().map_err(|e| format!("AC_BIND={bind}: {e}"))?;
    let start = Instant::now();
    while start.elapsed() < limit {
        if TcpStream::connect_timeout(&addr, Duration::from_secs(1)).is_ok() {
            return Ok(start.elapsed());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err(format!(
        "nothing listens on {bind} after {}s",
        limit.as_secs()
    ))
}

fn run_tune(t: &Tune) -> Result<u8, String> {
    let host = LinuxHost::new(LinuxSpawner);
    let (facts, source) = host_facts(t.facts_file.as_deref(), t.root.as_deref(), Scope::Settings)?;
    let configured = doctor::profile(None, &facts)?;
    let profile = t.profile.unwrap_or(configured);
    let p = paths(t.root.as_deref());
    let texts = if t.facts_file.is_some() {
        let none = |path: &Path| {
            (
                path.to_owned(),
                Err("facts file: no file to edit".to_owned()),
            )
        };
        tune::Texts {
            kata: none(&p.kata),
            env: none(&p.gateway_env),
            fstab: none(&p.fstab),
        }
    } else {
        tune::Texts {
            kata: text_of(&p.kata),
            env: text_of(&p.gateway_env),
            fstab: text_of(&p.fstab),
        }
    };
    let options = facts
        .shm
        .known()
        .map(|s| s.options.clone())
        .unwrap_or_default();
    let findings = recommend(&facts, profile);
    let plan = tune::plan(
        &findings,
        t.profile.map(|p| (p, configured)),
        &texts,
        &options,
    );
    println!("ac-ctl tune: profile {}, {source}", profile.name());
    print!("{}", tune::render(&plan));
    if t.facts_file.is_none() {
        for (path, text) in [&texts.kata, &texts.env, &texts.fstab] {
            if let Err(why) = text {
                println!("not checked: {}: {why}", path.display());
            }
        }
    }
    if !t.apply {
        if !plan.edits.is_empty() {
            println!("dry run: nothing written (--apply makes these edits)");
        }
        return Ok(0);
    }
    if plan.edits.is_empty() {
        return Ok(0);
    }
    if host.euid() != 0 && t.root.is_none() {
        return Err("--apply needs root (sudo), or --root DIR to edit copies".into());
    }
    let running = kata_sandboxes(&host.cmdlines());
    let used = facts.shm_used_mib.known().copied();
    if let Some(why) = tune::refusal(&plan, &running, t.force, used) {
        return Err(format!("refused: {why}"));
    }
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    for backup in tune::apply(&plan, &stamp(secs))? {
        println!("backup: {}", backup.display());
    }
    for line in tune::afterwards(&plan, &running, t.restart_gateway) {
        println!("{line}");
    }
    if t.restart_gateway {
        host.run(&["systemctl", "restart", "ac-gateway"])
            .map_err(|e| format!("systemctl restart ac-gateway: {e}"))?;
        let bind = gateway_bind(None, std::fs::read_to_string(&p.gateway_env).ok());
        let took = wait_for_port(&bind, Duration::from_secs(60))?;
        println!(
            "gateway: restarted; listening on {bind} after {:.1}s",
            took.as_secs_f64()
        );
    }
    println!("\nafter applying:");
    let (facts, source) = host_facts(None, t.root.as_deref(), Scope::Full)?;
    print!(
        "{}",
        doctor::table(&doctor::report(facts, Some(profile), &source)?)
    );
    Ok(0)
}

fn main() -> ExitCode {
    let command = match args::parse(std::env::args().skip(1)) {
        Ok(command) => command,
        Err(e) => {
            eprintln!("ac-ctl: {e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let cli = match command {
        Command::Fleet(cli) => cli,
        Command::Doctor(d) => {
            return run_doctor(&d).map_or_else(
                |e| {
                    eprintln!("ac-ctl doctor: {e}");
                    ExitCode::from(2)
                },
                ExitCode::from,
            );
        }
        Command::Tune(t) => {
            return run_tune(&t).map_or_else(
                |e| {
                    eprintln!("ac-ctl tune: {e}");
                    ExitCode::FAILURE
                },
                ExitCode::from,
            );
        }
    };
    match run(&cli) {
        Ok(line) => {
            println!("{line}");
            ExitCode::SUCCESS
        }
        Err(e) => match e
            .downcast_ref::<PortError>()
            .filter(|p| p.op == ac_store::IN_USE)
        {
            Some(held) => {
                let bind = gateway_bind(
                    std::env::var("AC_BIND").ok(),
                    std::fs::read_to_string(GATEWAY_ENV).ok(),
                );
                eprintln!(
                    "ac-ctl: {}, normally ac-gateway, which holds it while it runs: use the \
                     gateway API at http://{bind} with a token (see docs/install.md), or stop \
                     ac-gateway (sudo systemctl stop ac-gateway; its agents keep running)",
                    held.cause
                );
                ExitCode::from(IN_USE)
            }
            None => {
                eprintln!("ac-ctl: {e}");
                ExitCode::FAILURE
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_bind_from_env_then_file_then_default() {
        let file =
            || Some("# x\nAC_TOKEN_FILE=/t\nAC_BIND=10.0.0.1:1\n AC_BIND=100.64.0.2:8780\n".into());
        assert_eq!(
            gateway_bind(Some("127.0.0.1:9".into()), file()),
            "127.0.0.1:9"
        );
        assert_eq!(gateway_bind(None, file()), "100.64.0.2:8780");
        assert_eq!(
            gateway_bind(None, Some("#AC_BIND=x\n".into())),
            "127.0.0.1:8780"
        );
        assert_eq!(gateway_bind(None, None), "127.0.0.1:8780");
    }
}
