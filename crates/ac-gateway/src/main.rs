//! `ac-gateway`: serves the lifecycle API over HTTP.
//!
//! Env: `AC_TOKEN_FILE` (file holding the bearer token; refused if group/world
//! readable), `AC_BIND` (default 127.0.0.1:8780), `AC_IMAGE` (required),
//! `AC_DATA` (default /var/lib/agent-cloud), `AC_CTR`, `AC_NAMESPACE`,
//! `AC_UID` (workspace owner, default 1000), `AC_NETWORK=1` (attach agents to CNI),
//! `AC_ALLOW_PUBLIC_BIND=1` to override the private-address check.

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use ac_ctr::{CtrConfig, CtrRuntime, Network};
use ac_fleet::Fleet;
use ac_gateway::{bind, Api};
use ac_store::EngineStore;
use platform_linux::LinuxSpawner;
use rusty_http::{Method, StatusCode};
use rusty_serve::{Handler, Request, Response, Server};

type Gateway = Api<CtrRuntime<LinuxSpawner>, EngineStore>;

struct Http(Gateway);

impl Handler for Http {
    fn handle(&mut self, r: &Request<'_>) -> Response {
        let method = match r.method {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Delete => "DELETE",
            _ => "OTHER",
        };
        let reply = self.0.handle(method, r.target, r.authorization);
        Response::json(StatusCode::from_u16(reply.status), reply.body.into_bytes())
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
        return Err(format!("refusing to bind {addr}: not a private address (set AC_ALLOW_PUBLIC_BIND=1 to override)"));
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
    let runtime = CtrRuntime::new(
        LinuxSpawner,
        CtrConfig {
            ctr_bin: env("AC_CTR", "ctr"),
            namespace: env("AC_NAMESPACE", "agents"),
            image,
            workspace_root: data.join("workspaces"),
            workspace_owner: Some((uid, uid)),
            network,
            vc_root: "/run/vc".into(),
            command_timeout: Some(std::time::Duration::from_secs(120)),
            trace: std::env::var_os("AC_TRACE").is_some(),
        },
    );
    let store = EngineStore::open(&data).map_err(|e| e.to_string())?;
    let api = Api::new(Fleet::new(runtime, store), &token).ok_or("token file is empty")?;
    let server = Server::bind(addr, Http(api)).map_err(|e| format!("bind {addr}: {e}"))?;
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
