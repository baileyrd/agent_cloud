//! `Runtime` over the containerd `ctr` CLI, spawned through rustils'
//! `Spawner` so it is testable without containerd or KVM.
//!
//! The `ctr` flag spellings below follow `ctr --help` from memory and are
//! **unverified** against a live host; the M0 smoke test confirms them.

use std::ffi::OsString;
use std::path::PathBuf;

use ac_domain::AgentId;
use ac_ports::{PortError, Runtime};
use platform::process::{Command, Spawner, Stdio};

const KATA: &str = "io.containerd.kata.v2";
const WORKSPACE_MOUNT: &str = "/workspace";

/// Static configuration for the adapter.
#[derive(Debug, Clone)]
pub struct CtrConfig {
    /// Path or name of the `ctr` binary.
    pub ctr_bin: String,
    /// containerd namespace holding agent containers.
    pub namespace: String,
    /// Image every agent boots from.
    pub image: String,
    /// Parent directory of per-agent workspace volumes.
    pub workspace_root: PathBuf,
}

/// Runs each agent as a detached Kata container with a bind-mounted workspace.
pub struct CtrRuntime<S> {
    spawner: S,
    cfg: CtrConfig,
}

impl<S: Spawner> CtrRuntime<S> {
    /// Build from a process spawner and configuration.
    pub fn new(spawner: S, cfg: CtrConfig) -> Self {
        Self { spawner, cfg }
    }

    fn workspace(&self, id: &AgentId) -> PathBuf {
        self.cfg.workspace_root.join(id.as_str())
    }

    /// Run `ctr -n <ns> <args...>`; a non-zero exit carries stderr in the error.
    fn ctr(&self, op: &'static str, args: &[OsString]) -> Result<(), PortError> {
        let fail = |cause: String| PortError { op, cause };
        let mut cmd = Command::new(&self.cfg.ctr_bin, "/")
            .arg("-n")
            .arg(&self.cfg.namespace)
            .args(args);
        cmd.stdout = Stdio::Null;
        cmd.stderr = Stdio::Pipe;
        let mut child = self
            .spawner
            .spawn(&cmd)
            .map_err(|e| fail(format!("spawn: {e:?}")))?;
        let stderr = match child.take_stderr() {
            Some(mut pipe) => read_all(pipe.as_mut()).map_err(|e| fail(format!("read: {e:?}")))?,
            None => Vec::new(),
        };
        let status = child.wait().map_err(|e| fail(format!("wait: {e:?}")))?;
        if status.success() {
            return Ok(());
        }
        Err(fail(format!(
            "{status:?}: {}",
            String::from_utf8_lossy(&stderr).trim()
        )))
    }
}

fn read_all(file: &mut dyn platform::fs::File) -> platform::error::Result<Vec<u8>> {
    let (mut out, mut buf) = (Vec::new(), [0u8; 4096]);
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok(out);
        }
        out.extend_from_slice(&buf[..n]);
    }
}

fn os(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

impl<S: Spawner> Runtime for CtrRuntime<S> {
    fn create(&self, id: &AgentId) -> Result<(), PortError> {
        let dir = self.workspace(id);
        std::fs::create_dir_all(&dir).map_err(|e| PortError {
            op: "create",
            cause: format!("mkdir {}: {e}", dir.display()),
        })?;
        let mount = format!(
            "type=bind,src={},dst={WORKSPACE_MOUNT},options=rbind:rw",
            dir.display()
        );
        self.ctr(
            "create",
            &os(&[
                "run",
                "-d",
                "--runtime",
                KATA,
                "--mount",
                &mount,
                &self.cfg.image,
                id.as_str(),
            ]),
        )
    }

    fn pause(&self, id: &AgentId) -> Result<(), PortError> {
        self.ctr("pause", &os(&["task", "pause", id.as_str()]))
    }

    fn resume(&self, id: &AgentId) -> Result<(), PortError> {
        self.ctr("resume", &os(&["task", "resume", id.as_str()]))
    }

    fn destroy(&self, id: &AgentId) -> Result<(), PortError> {
        self.ctr("destroy", &os(&["task", "delete", "--force", id.as_str()]))?;
        self.ctr("destroy", &os(&["container", "delete", id.as_str()]))?;
        let dir = self.workspace(id);
        std::fs::remove_dir_all(&dir).map_err(|e| PortError {
            op: "destroy",
            cause: format!("rm {}: {e}", dir.display()),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use platform::process::ExitStatus;
    use platform_mock::process::SpawnRecord;
    use platform_mock::MockSpawner;

    use super::*;

    type Log = Arc<Mutex<Vec<SpawnRecord>>>;

    fn rt(status: ExitStatus) -> (CtrRuntime<MockSpawner>, Log, PathBuf) {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("ac-ctr-{}-{n}", std::process::id()));
        let spawner = MockSpawner::new().script("ctr", status);
        let log = Arc::clone(&spawner.spawned);
        let cfg = CtrConfig {
            ctr_bin: "ctr".into(),
            namespace: "agents".into(),
            image: "agent:latest".into(),
            workspace_root: root.clone(),
        };
        (CtrRuntime::new(spawner, cfg), log, root)
    }

    fn id() -> AgentId {
        AgentId::parse("a1").unwrap_or_else(|e| panic!("{e}"))
    }

    fn argv(log: &Log) -> Vec<Vec<String>> {
        let log = log.lock().unwrap_or_else(|e| e.into_inner());
        log.iter()
            .map(|r| {
                r.argv
                    .iter()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn create_runs_kata_with_workspace_mount() {
        let (rt, log, root) = rt(ExitStatus::Code(0));
        assert!(rt.create(&id()).is_ok());
        let calls = argv(&log);
        assert_eq!(
            &calls[0][..6],
            ["-n", "agents", "run", "-d", "--runtime", KATA]
        );
        assert!(calls[0].iter().any(|a| a.contains("dst=/workspace")));
        assert_eq!(calls[0].last().map(String::as_str), Some("a1"));
        assert!(root.join("a1").is_dir());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn pause_and_resume_use_task_verbs() {
        let (rt, log, _) = rt(ExitStatus::Code(0));
        assert!(rt.pause(&id()).is_ok() && rt.resume(&id()).is_ok());
        let calls = argv(&log);
        assert_eq!(calls[0][2..], ["task", "pause", "a1"]);
        assert_eq!(calls[1][2..], ["task", "resume", "a1"]);
    }

    #[test]
    fn destroy_deletes_task_container_and_volume() {
        let (rt, log, root) = rt(ExitStatus::Code(0));
        let _ = std::fs::create_dir_all(root.join("a1"));
        assert!(rt.destroy(&id()).is_ok());
        let calls = argv(&log);
        assert_eq!(calls[0][2..], ["task", "delete", "--force", "a1"]);
        assert_eq!(calls[1][2..], ["container", "delete", "a1"]);
        assert!(!root.join("a1").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn nonzero_exit_is_an_error_and_stops_destroy() {
        let (rt, log, _) = rt(ExitStatus::Code(1));
        let err = rt.destroy(&id()).err().map(|e| e.op);
        assert_eq!(err, Some("destroy"));
        assert_eq!(argv(&log).len(), 1);
    }
}
