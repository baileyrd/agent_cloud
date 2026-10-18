//! `Runtime` over the containerd `ctr` CLI, spawned through rustils'
//! `Spawner` so it is testable without containerd or KVM.
//!
//! Networking: Kata decides a VM's interfaces at boot, so `ctr run --cni` (which attaches the
//! network after the task starts) gives the guest nothing. Instead the adapter creates a network
//! namespace, attaches it with the CNI bridge plugin, then starts the container in it.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use ac_domain::AgentId;
use ac_ports::{PortError, Runtime};
use platform::process::{Command, EnvSpec, Spawner, Stdio};

const KATA: &str = "io.containerd.kata.v2";
const WORKSPACE_MOUNT: &str = "/workspace";
const CNI_PATH_ENV: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Public resolvers used for agents until the egress proxy owns DNS (ADR-007).
const DEFAULT_RESOLV_CONF: &str = "nameserver 1.1.1.1\nnameserver 9.9.9.9\n";

/// Single-plugin CNI config: a NAT bridge, 10.89.0.0/16. Edit the written copy to change it.
const DEFAULT_CNI_CONFIG: &str = r#"{
  "cniVersion": "1.0.0",
  "name": "agents",
  "type": "bridge",
  "bridge": "acbr0",
  "isGateway": true,
  "ipMasq": true,
  "ipam": {
    "type": "host-local",
    "ranges": [[{ "subnet": "10.89.0.0/16" }]],
    "routes": [{ "dst": "0.0.0.0/0" }]
  }
}
"#;

/// How agents are attached to a network.
#[derive(Debug, Clone)]
pub struct Network {
    /// CNI plugin binary that wires the namespace (the `bridge` plugin).
    pub plugin: PathBuf,
    /// Single-plugin CNI config (JSON) fed to the plugin on stdin.
    pub config: PathBuf,
    /// Host file bind-mounted read-only as the guest's `/etc/resolv.conf`. A VM cannot use the
    /// host's loopback resolver stub, so networked agents need one of their own.
    pub resolv_conf: PathBuf,
    /// Directory holding named network namespaces (`ip netns` uses `/run/netns`).
    pub netns_dir: PathBuf,
}

impl Network {
    /// Defaults under `dir`: writes `cni-agents.json` and `resolv.conf` there if absent.
    pub fn defaults(dir: &Path) -> Result<Self, PortError> {
        let fail = |e: std::io::Error| PortError {
            op: "network config",
            cause: e.to_string(),
        };
        std::fs::create_dir_all(dir).map_err(fail)?;
        let write_once = |name: &str, body: &str| -> Result<PathBuf, PortError> {
            let path = dir.join(name);
            if !path.exists() {
                std::fs::write(&path, body).map_err(fail)?;
            }
            Ok(path)
        };
        Ok(Self {
            plugin: PathBuf::from("/usr/libexec/cni/bridge"),
            config: write_once("cni-agents.json", DEFAULT_CNI_CONFIG)?,
            resolv_conf: write_once("resolv.conf", DEFAULT_RESOLV_CONF)?,
            netns_dir: PathBuf::from("/run/netns"),
        })
    }

    fn netns(&self, id: &AgentId) -> PathBuf {
        self.netns_dir.join(format!("ac-{id}"))
    }
}

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
    /// Owner (uid, gid) given to each new workspace so the unprivileged agent user can write.
    /// `None` leaves the directory owned by whoever runs the adapter.
    pub workspace_owner: Option<(u32, u32)>,
    /// Network for agents. `None`: no network.
    pub network: Option<Network>,
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

    /// Run a program to completion; a non-zero exit carries stderr in the error.
    fn run(
        &self,
        op: &'static str,
        program: &OsStr,
        args: &[OsString],
        env: Option<BTreeMap<OsString, OsString>>,
        stdin: Option<&[u8]>,
    ) -> Result<(), PortError> {
        let fail = |cause: String| PortError { op, cause };
        let mut cmd = Command::new(program, "/").args(args);
        if let Some(env) = env {
            cmd = cmd.env(EnvSpec::Explicit(env));
        }
        cmd.stdout = Stdio::Null;
        cmd.stderr = Stdio::Pipe;
        if stdin.is_some() {
            cmd.stdin = Stdio::Pipe;
        }
        let mut child = self
            .spawner
            .spawn(&cmd)
            .map_err(|e| fail(format!("spawn {}: {e:?}", program.to_string_lossy())))?;
        if let Some(mut bytes) = stdin {
            let mut pipe = child
                .take_stdin()
                .ok_or_else(|| fail("no stdin pipe".into()))?;
            while !bytes.is_empty() {
                let n = pipe
                    .write(bytes)
                    .map_err(|e| fail(format!("write: {e:?}")))?;
                if n == 0 {
                    return Err(fail("stdin closed early".into()));
                }
                bytes = &bytes[n..];
            }
        }
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

    /// Run `ctr -n <ns> <args...>`.
    fn ctr(&self, op: &'static str, args: &[OsString]) -> Result<(), PortError> {
        let mut full = os(&["-n", &self.cfg.namespace]);
        full.extend_from_slice(args);
        self.run(op, OsStr::new(&self.cfg.ctr_bin), &full, None, None)
    }

    /// Like [`Self::ctr`], but a "not found" failure is success.
    fn ctr_gone_ok(&self, op: &'static str, args: &[OsString]) -> Result<(), PortError> {
        match self.ctr(op, args) {
            Err(e) if is_already_gone(&e.cause) => Ok(()),
            other => other,
        }
    }

    /// Run the CNI plugin (`ADD` or `DEL`) against the agent's namespace.
    fn cni(
        &self,
        op: &'static str,
        net: &Network,
        verb: &str,
        id: &AgentId,
    ) -> Result<(), PortError> {
        let config = std::fs::read(&net.config).map_err(|e| PortError {
            op,
            cause: format!("read {}: {e}", net.config.display()),
        })?;
        let plugin_dir = net.plugin.parent().unwrap_or(Path::new("/"));
        let env = BTreeMap::from(
            [
                ("CNI_COMMAND", verb.to_owned()),
                ("CNI_CONTAINERID", id.to_string()),
                ("CNI_NETNS", net.netns(id).display().to_string()),
                ("CNI_IFNAME", "eth0".to_owned()),
                ("CNI_PATH", plugin_dir.display().to_string()),
                ("PATH", CNI_PATH_ENV.to_owned()),
            ]
            .map(|(k, v)| (OsString::from(k), OsString::from(v))),
        );
        self.run(op, net.plugin.as_os_str(), &[], Some(env), Some(&config))
    }

    fn netns_cmd(
        &self,
        op: &'static str,
        verb: &str,
        net: &Network,
        id: &AgentId,
    ) -> Result<(), PortError> {
        let name = net.netns(id);
        let name = name.file_name().unwrap_or_default().to_owned();
        self.run(
            op,
            OsStr::new("ip"),
            &[OsString::from("netns"), OsString::from(verb), name],
            None,
            None,
        )
    }

    /// Create the agent's namespace and attach it. Undoes itself on failure.
    fn network_up(&self, net: &Network, id: &AgentId) -> Result<(), PortError> {
        self.netns_cmd("create", "add", net, id)?;
        self.cni("create", net, "ADD", id)
            .map_err(|e| self.with_cleanup(e, net, id))
    }

    /// Detach and remove the agent's namespace. No namespace means nothing to do.
    fn network_down(&self, net: &Network, id: &AgentId) -> Result<(), PortError> {
        if !net.netns(id).exists() {
            return Ok(());
        }
        self.cni("destroy", net, "DEL", id)?;
        self.netns_cmd("destroy", "del", net, id)
    }

    /// Tear the network down after a failed create; report both errors if cleanup fails too.
    fn with_cleanup(&self, original: PortError, net: &Network, id: &AgentId) -> PortError {
        match self.network_down(net, id) {
            Ok(()) => original,
            Err(e) => PortError {
                op: original.op,
                cause: format!("{}; cleanup also failed: {e}", original.cause),
            },
        }
    }
}

/// Whether a `ctr` failure says the thing it was asked to remove no longer exists.
fn is_already_gone(cause: &str) -> bool {
    cause.contains("not found")
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
        if let Some((uid, gid)) = self.cfg.workspace_owner {
            std::os::unix::fs::chown(&dir, Some(uid), Some(gid)).map_err(|e| PortError {
                op: "create",
                cause: format!("chown {}: {e}", dir.display()),
            })?;
        }
        let net = self.cfg.network.as_ref();
        if let Some(net) = net {
            self.network_up(net, id)?;
        }
        let started = self.start_container(id, &dir, net);
        match (started, net) {
            (Err(e), Some(net)) => Err(self.with_cleanup(e, net, id)),
            (result, _) => result,
        }
    }

    fn pause(&self, id: &AgentId) -> Result<(), PortError> {
        self.ctr("pause", &os(&["task", "pause", id.as_str()]))
    }

    fn resume(&self, id: &AgentId) -> Result<(), PortError> {
        self.ctr("resume", &os(&["task", "resume", id.as_str()]))
    }

    fn destroy(&self, id: &AgentId) -> Result<(), PortError> {
        // Already gone counts as done, so destroy can be retried after a crash or reboot.
        self.ctr_gone_ok("destroy", &os(&["task", "delete", "--force", id.as_str()]))?;
        self.ctr_gone_ok("destroy", &os(&["container", "delete", id.as_str()]))?;
        if let Some(net) = &self.cfg.network {
            self.network_down(net, id)?;
        }
        let dir = self.workspace(id);
        std::fs::remove_dir_all(&dir).map_err(|e| PortError {
            op: "destroy",
            cause: format!("rm {}: {e}", dir.display()),
        })
    }
}

impl<S: Spawner> CtrRuntime<S> {
    fn start_container(
        &self,
        id: &AgentId,
        dir: &Path,
        net: Option<&Network>,
    ) -> Result<(), PortError> {
        let workspace = format!(
            "type=bind,src={},dst={WORKSPACE_MOUNT},options=rbind:rw",
            dir.display()
        );
        let mut args = vec![
            "run".to_owned(),
            "-d".to_owned(),
            "--runtime".to_owned(),
            KATA.to_owned(),
            "--mount".to_owned(),
            workspace,
        ];
        if let Some(net) = net {
            args.extend([
                "--with-ns".to_owned(),
                format!("network:{}", net.netns(id).display()),
                "--mount".to_owned(),
                format!(
                    "type=bind,src={},dst=/etc/resolv.conf,options=rbind:ro",
                    net.resolv_conf.display()
                ),
            ]);
        }
        args.extend([self.cfg.image.clone(), id.to_string()]);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        self.ctr("create", &os(&args))
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

    const PLUGIN: &str = "/fake/cni/bridge";

    fn tmp() -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("ac-ctr-{}-{n}", std::process::id()))
    }

    fn rt_with(ctr: ExitStatus, plugin: ExitStatus) -> (CtrRuntime<MockSpawner>, Log, PathBuf) {
        let root = tmp();
        let spawner = MockSpawner::new()
            .script("ctr", ctr)
            .script("ip", ExitStatus::Code(0))
            .script(PLUGIN, plugin);
        let log = Arc::clone(&spawner.spawned);
        let cfg = CtrConfig {
            ctr_bin: "ctr".into(),
            namespace: "agents".into(),
            image: "agent:latest".into(),
            workspace_root: root.clone(),
            workspace_owner: None,
            network: None,
        };
        (CtrRuntime::new(spawner, cfg), log, root)
    }

    fn rt(status: ExitStatus) -> (CtrRuntime<MockSpawner>, Log, PathBuf) {
        rt_with(status, ExitStatus::Code(0))
    }

    /// A runtime with a network whose config files and netns dir live under `root`.
    fn rt_net(ctr: ExitStatus, plugin: ExitStatus) -> (CtrRuntime<MockSpawner>, Log, PathBuf) {
        let (mut rt, log, root) = rt_with(ctr, plugin);
        let mut net = Network::defaults(&root.join("etc")).unwrap_or_else(|e| panic!("{e}"));
        net.plugin = PathBuf::from(PLUGIN);
        net.netns_dir = root.join("netns");
        let _ = std::fs::create_dir_all(&net.netns_dir);
        rt.cfg.network = Some(net);
        (rt, log, root)
    }

    fn id() -> AgentId {
        AgentId::parse("a1").unwrap_or_else(|e| panic!("{e}"))
    }

    fn programs(log: &Log) -> Vec<String> {
        let log = log.lock().unwrap_or_else(|e| e.into_inner());
        log.iter()
            .map(|r| r.program.to_string_lossy().into_owned())
            .collect()
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

    fn env_of(log: &Log, index: usize, key: &str) -> Option<String> {
        let log = log.lock().unwrap_or_else(|e| e.into_inner());
        match &log.get(index)?.env {
            EnvSpec::Explicit(m) => m
                .get(OsStr::new(key))
                .map(|v| v.to_string_lossy().into_owned()),
            EnvSpec::Inherit => None,
        }
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
        assert!(!calls[0].iter().any(|a| a == "--with-ns"));
        assert_eq!(calls[0].last().map(String::as_str), Some("a1"));
        assert!(root.join("a1").is_dir());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn owner_is_applied_to_the_workspace() {
        use std::os::unix::fs::MetadataExt;
        let (mut rt, _, root) = rt(ExitStatus::Code(0));
        let _ = std::fs::create_dir_all(&root);
        let meta = std::fs::metadata(&root).unwrap_or_else(|e| panic!("{e}"));
        rt.cfg.workspace_owner = Some((meta.uid(), meta.gid()));
        assert!(rt.create(&id()).is_ok());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn network_create_makes_namespace_attaches_cni_then_starts_in_it() {
        let (rt, log, root) = rt_net(ExitStatus::Code(0), ExitStatus::Code(0));
        assert!(rt.create(&id()).is_ok());
        assert_eq!(programs(&log), ["ip", PLUGIN, "ctr"]);
        assert_eq!(argv(&log)[0], ["netns", "add", "ac-a1"]);
        assert_eq!(env_of(&log, 1, "CNI_COMMAND").as_deref(), Some("ADD"));
        assert_eq!(env_of(&log, 1, "CNI_CONTAINERID").as_deref(), Some("a1"));
        assert_eq!(env_of(&log, 1, "CNI_IFNAME").as_deref(), Some("eth0"));
        assert_eq!(env_of(&log, 1, "CNI_PATH").as_deref(), Some("/fake/cni"));
        let ns = root.join("netns/ac-a1").display().to_string();
        assert_eq!(env_of(&log, 1, "CNI_NETNS").as_deref(), Some(ns.as_str()));
        let ctr = &argv(&log)[2];
        let at = ctr
            .iter()
            .position(|a| a == "--with-ns")
            .unwrap_or(usize::MAX);
        assert_eq!(
            ctr.get(at + 1).map(String::as_str),
            Some(format!("network:{ns}").as_str())
        );
        assert!(ctr
            .iter()
            .any(|a| a.contains("dst=/etc/resolv.conf,options=rbind:ro")));
        assert_eq!(ctr.last().map(String::as_str), Some("a1"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cni_failure_cleans_up_and_never_starts_the_container() {
        let (rt, log, root) = rt_net(ExitStatus::Code(0), ExitStatus::Code(1));
        let err = rt.create(&id()).err();
        assert_eq!(err.map(|e| e.op), Some("create"));
        // namespace added, plugin failed; the namespace file was never created by the mock,
        // so cleanup has nothing to detach and the container is not started.
        assert_eq!(programs(&log), ["ip", PLUGIN]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn container_failure_tears_the_network_down() {
        let (rt, log, root) = rt_net(ExitStatus::Code(1), ExitStatus::Code(0));
        let _ = std::fs::write(root.join("netns/ac-a1"), "");
        assert!(rt.create(&id()).is_err());
        assert_eq!(programs(&log), ["ip", PLUGIN, "ctr", PLUGIN, "ip"]);
        assert_eq!(env_of(&log, 3, "CNI_COMMAND").as_deref(), Some("DEL"));
        assert_eq!(argv(&log)[4], ["netns", "del", "ac-a1"]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn default_network_config_is_written_once() {
        let dir = tmp();
        let net = Network::defaults(&dir).unwrap_or_else(|e| panic!("{e}"));
        assert!(std::fs::read_to_string(&net.resolv_conf)
            .unwrap_or_default()
            .contains("nameserver 1.1.1.1"));
        assert!(std::fs::read_to_string(&net.config)
            .unwrap_or_default()
            .contains("\"bridge\": \"acbr0\""));
        let _ = std::fs::write(&net.resolv_conf, "nameserver 10.0.0.1\n");
        let again = Network::defaults(&dir).unwrap_or_else(|e| panic!("{e}"));
        assert!(std::fs::read_to_string(again.resolv_conf)
            .unwrap_or_default()
            .contains("10.0.0.1"));
        let _ = std::fs::remove_dir_all(dir);
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
    fn destroy_detaches_the_network_when_the_namespace_exists() {
        let (rt, log, root) = rt_net(ExitStatus::Code(0), ExitStatus::Code(0));
        let _ = std::fs::create_dir_all(root.join("a1"));
        let _ = std::fs::write(root.join("netns/ac-a1"), "");
        assert!(rt.destroy(&id()).is_ok());
        assert_eq!(programs(&log), ["ctr", "ctr", PLUGIN, "ip"]);
        assert_eq!(env_of(&log, 2, "CNI_COMMAND").as_deref(), Some("DEL"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn destroy_skips_network_teardown_when_there_is_no_namespace() {
        let (rt, log, root) = rt_net(ExitStatus::Code(0), ExitStatus::Code(0));
        let _ = std::fs::create_dir_all(root.join("a1"));
        assert!(rt.destroy(&id()).is_ok());
        assert_eq!(programs(&log), ["ctr", "ctr"]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn already_gone_is_recognised() {
        assert!(is_already_gone(
            "Code(1): ctr: no running task found: task a1 not found"
        ));
        assert!(is_already_gone(
            r#"Code(1): container "a1" in namespace "agents": not found"#
        ));
        assert!(!is_already_gone("Code(1): permission denied"));
    }

    #[test]
    fn nonzero_exit_is_an_error_and_stops_destroy() {
        let (rt, log, _) = rt(ExitStatus::Code(1));
        let err = rt.destroy(&id()).err().map(|e| e.op);
        assert_eq!(err, Some("destroy"));
        assert_eq!(argv(&log).len(), 1);
    }
}
