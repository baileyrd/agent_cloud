//! `ac-ctl`: drive the agent fleet from the shell.
//!
//! Exit codes: 0 ok, 1 operation failed (or `status` of an unknown agent), 2 bad usage, 3 the
//! data directory is in use by another process (normally `ac-gateway`, which holds it while it
//! runs): use the gateway's API instead, or stop it. `reconcile` prints one `<id> paused` line
//! per agent it changed. `create` and `resume` are refused when the guest would not fit `/dev/shm`
//! (ADR-012; the gateway's `AC_SHM_MARGIN_MIB`, `AC_VM_MEMORY_MIB`, `AC_KATA_CONFIG` and
//! `AC_SHM_LIMIT_MIB` apply here too).

mod args;

use std::process::ExitCode;

use ac_ctr::capacity::{CapacityConfig, HostCapacity};
use ac_ctr::{CtrConfig, CtrRuntime, Network};
use ac_fleet::{Fleet, FleetError};
use ac_ports::PortError;
use ac_store::EngineStore;
use args::{Action, Cli, Job, USAGE};
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
    let capacity = HostCapacity::new(
        CtrRuntime::new(LinuxSpawner, cfg.clone()),
        CapacityConfig {
            shm: "/dev/shm".into(),
            kata_config: std::env::var("AC_KATA_CONFIG")
                .unwrap_or_else(|_| "/etc/kata-containers/configuration.toml".to_owned())
                .into(),
            guest_mib: env_mib("AC_VM_MEMORY_MIB")?,
            shm_limit_mib: env_mib("AC_SHM_LIMIT_MIB")?,
        },
    );
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

fn main() -> ExitCode {
    let cli = match args::parse(std::env::args().skip(1)) {
        Ok(cli) => cli,
        Err(e) => {
            eprintln!("ac-ctl: {e}\n{USAGE}");
            return ExitCode::from(2);
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
