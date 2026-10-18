//! `ac-ctl`: drive the agent fleet from the shell.
//!
//! Exit codes: 0 ok, 1 operation failed (or `status` of an unknown agent), 2 bad usage.

mod args;

use std::process::ExitCode;

use ac_ctr::{CtrConfig, CtrRuntime};
use ac_fleet::{Fleet, FleetError};
use ac_store::EngineStore;
use args::{Action, Cli, USAGE};
use platform_linux::LinuxSpawner;

fn run(cli: &Cli) -> Result<String, Box<dyn std::error::Error>> {
    let resolv_conf = if cli.network {
        Some(ac_ctr::default_resolv_conf(&cli.data)?)
    } else {
        None
    };
    let runtime = CtrRuntime::new(
        LinuxSpawner,
        CtrConfig {
            ctr_bin: cli.ctr.clone(),
            namespace: cli.namespace.clone(),
            image: cli.image.clone(),
            workspace_root: cli.workspaces.clone(),
            workspace_owner: Some((cli.uid, cli.uid)),
            network: cli.network,
            resolv_conf,
        },
    );
    let fleet = Fleet::new(runtime, EngineStore::open(&cli.data)?);
    let id = &cli.id;
    let state = match cli.action {
        Action::Create => fleet.create(id)?,
        Action::Pause => fleet.pause(id)?,
        Action::Resume => fleet.resume(id)?,
        Action::Destroy => fleet.destroy(id)?,
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
        Err(e) => {
            eprintln!("ac-ctl: {e}");
            ExitCode::FAILURE
        }
    }
}
