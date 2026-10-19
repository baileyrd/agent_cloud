//! `Runtime` over the containerd `ctr` CLI, spawned through rustils'
//! `Spawner` so it is testable without containerd or KVM.
//!
//! Networking: Kata decides a VM's interfaces at boot, so `ctr run --cni` (which attaches the
//! network after the task starts) gives the guest nothing. Instead the adapter creates a network
//! namespace, attaches it with the CNI bridge plugin, then starts the container in it.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::Duration;

use ac_domain::AgentId;
use ac_ports::{PortError, Runtime};
use platform::process::{Command, EnvSpec, ExitStatus, Spawner, Stdio};

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
    /// Kata's runtime state root (`/run/vc`). A failed create removes `vm/<id>` and `sbs/<id>`.
    pub vc_root: PathBuf,
    /// containerd's task bundle root (`/run/containerd/io.containerd.runtime.v2.task`). A shim that
    /// died uncleanly leaves `<namespace>/<id>` there, and the next start fails with `file exists`.
    pub task_root: PathBuf,
    /// Kill any external command (`ctr`, `ip`, the CNI plugin) that runs longer than this, via
    /// coreutils `timeout`. `None`: wait forever.
    pub command_timeout: Option<Duration>,
    /// Print each external command to stderr before running it.
    pub trace: bool,
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

    /// Run a program to completion and return its stdout; a non-zero exit carries its message
    /// (stderr, or stdout where CNI plugins write their errors) in the error.
    fn run(
        &self,
        op: &'static str,
        program: &OsStr,
        args: &[OsString],
        env: Option<BTreeMap<OsString, OsString>>,
    ) -> Result<Vec<u8>, PortError> {
        match self.exec(op, program, args, env)? {
            (status, stdout, _) if status.success() => Ok(stdout),
            (status, stdout, stderr) => Err(failure(op, status, &stdout, &stderr)),
        }
    }

    /// Run a program to completion and return its exit status, stdout and stderr. Stdin is the
    /// null device: anything a program must read comes from a file argument or its environment.
    /// Stdout is drained before stderr, which is fine for the small outputs of these programs.
    fn exec(
        &self,
        op: &'static str,
        program: &OsStr,
        args: &[OsString],
        env: Option<BTreeMap<OsString, OsString>>,
    ) -> Result<(ExitStatus, Vec<u8>, Vec<u8>), PortError> {
        let fail = |cause: String| PortError { op, cause };
        let mut argv: Vec<OsString> = Vec::new();
        let mut exe = program.to_owned();
        if let Some(limit) = self.cfg.command_timeout {
            argv.extend([
                OsString::from("--kill-after=5"),
                OsString::from(limit.as_secs().to_string()),
                program.to_owned(),
            ]);
            exe = OsString::from("timeout");
        }
        argv.extend_from_slice(args);
        if self.cfg.trace {
            let line: Vec<String> = std::iter::once(&exe)
                .chain(argv.iter())
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            eprintln!("ac-ctr: {op}: {}", line.join(" "));
        }
        let mut cmd = Command::new(&exe, "/").args(&argv);
        if let Some(env) = env {
            cmd = cmd.env(EnvSpec::Explicit(env));
        }
        cmd.stdin = Stdio::Null;
        cmd.stdout = Stdio::Pipe;
        cmd.stderr = Stdio::Pipe;
        let mut child = self
            .spawner
            .spawn(&cmd)
            .map_err(|e| fail(format!("spawn {}: {e:?}", exe.to_string_lossy())))?;
        let drain = |pipe: Option<Box<dyn platform::fs::File>>| match pipe {
            Some(mut pipe) => read_all(pipe.as_mut()).map_err(|e| fail(format!("read: {e:?}"))),
            None => Ok(Vec::new()),
        };
        let stdout = drain(child.take_stdout())?;
        let stderr = drain(child.take_stderr())?;
        let status = child.wait().map_err(|e| fail(format!("wait: {e:?}")))?;
        Ok((status, stdout, stderr))
    }

    /// Run `ctr -n <ns> <args...>` and return its stdout.
    fn ctr_output(&self, op: &'static str, args: &[OsString]) -> Result<Vec<u8>, PortError> {
        let mut full = os(&["-n", &self.cfg.namespace]);
        full.extend_from_slice(args);
        self.run(op, OsStr::new(&self.cfg.ctr_bin), &full, None)
    }

    /// Run `ctr -n <ns> <args...>`.
    fn ctr(&self, op: &'static str, args: &[OsString]) -> Result<(), PortError> {
        self.ctr_output(op, args).map(drop)
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
        if !net.config.is_file() {
            return Err(PortError {
                op,
                cause: format!("missing CNI config {}", net.config.display()),
            });
        }
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
        // The plugin reads its config from stdin until EOF. A file redirect guarantees the EOF;
        // a pipe held open by the child would deadlock.
        let args = [
            OsString::from("-c"),
            OsString::from(r#"exec "$0" <"$1""#),
            net.plugin.clone().into_os_string(),
            net.config.clone().into_os_string(),
        ];
        self.run(op, OsStr::new("sh"), &args, Some(env)).map(drop)
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
        )
        .map(drop)
    }

    /// Create the agent's namespace and attach it. Undoes itself on failure.
    fn network_up(&self, op: &'static str, net: &Network, id: &AgentId) -> Result<(), PortError> {
        self.netns_cmd(op, "add", net, id)?;
        self.cni(op, net, "ADD", id)
            .map_err(|e| with_cleanup(e, self.network_down(op, net, id)))
    }

    /// Detach the agent's network and remove its namespace. DEL runs even with no namespace:
    /// after a reboot /run/netns is empty but the IPAM lease on disk is not, and the next ADD
    /// would be refused as a duplicate allocation. The bridge plugin accepts DEL for a missing
    /// namespace.
    fn network_down(&self, op: &'static str, net: &Network, id: &AgentId) -> Result<(), PortError> {
        self.cni(op, net, "DEL", id)?;
        if net.netns(id).exists() {
            self.netns_cmd(op, "del", net, id)?;
        }
        Ok(())
    }

    /// Remove what an earlier failed run left under this id: its container and snapshot.
    fn remove_stale(&self, op: &'static str, id: &AgentId) -> Result<(), PortError> {
        self.ctr_gone_ok(op, &os(&["container", "delete", id.as_str()]))?;
        self.ctr_gone_ok(op, &os(&["snapshots", "rm", id.as_str()]))
    }

    /// Kill a sandbox's leftover Kata processes and remove its state dirs.
    fn kill_sandbox(&self, op: &'static str, id: &AgentId) -> Result<(), PortError> {
        for pattern in kata_patterns(id) {
            self.pkill(op, &pattern)?;
        }
        for kind in ["vm", "sbs"] {
            remove_dir_if_present(op, &self.cfg.vc_root.join(kind).join(id.as_str()))?;
        }
        let bundle = self
            .cfg
            .task_root
            .join(&self.cfg.namespace)
            .join(id.as_str());
        // `remove_dir` refuses a rootfs that is still mounted (or not empty), so nothing under a
        // live mount is ever deleted.
        match std::fs::remove_dir(bundle.join("rootfs")) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(PortError {
                op,
                cause: format!("rm {}/rootfs: {e}", bundle.display()),
            }),
            _ => remove_dir_if_present(op, &bundle),
        }
    }

    /// `pkill -KILL -f <pattern>`; exit 1 (nothing matched) is success.
    fn pkill(&self, op: &'static str, pattern: &str) -> Result<(), PortError> {
        let args = os(&["-KILL", "-f", pattern]);
        match self.exec(op, OsStr::new("pkill"), &args, None)? {
            (ExitStatus::Code(0 | 1), _, _) => Ok(()),
            (status, stdout, stderr) => Err(failure(op, status, &stdout, &stderr)),
        }
    }

    /// Undo a start that failed: every step runs; the first error is returned.
    fn undo_start(
        &self,
        op: &'static str,
        id: &AgentId,
        net: Option<&Network>,
    ) -> Result<(), PortError> {
        [
            self.kill_sandbox(op, id),
            self.remove_stale(op, id),
            net.map_or(Ok(()), |net| self.network_down(op, net, id)),
        ]
        .into_iter()
        .collect()
    }

    /// Boot the agent's VM on its existing workspace: clear stale state, attach the network,
    /// start the container. A failed start undoes itself.
    fn boot(&self, op: &'static str, id: &AgentId, dir: &Path) -> Result<(), PortError> {
        // Removing the container fails while a task still runs in it, so the kill below never
        // reaches a live VM; it only clears what a shim that died uncleanly left behind.
        self.remove_stale(op, id)?;
        self.kill_sandbox(op, id)?;
        let net = self.cfg.network.as_ref();
        if let Some(net) = net {
            // A VM that died without `pause` leaves a namespace (`ip netns add` would fail) or,
            // after a reboot, an IPAM lease (ADD would fail).
            self.network_down(op, net, id)?;
            self.network_up(op, net, id)?;
        }
        self.start_container(op, id, dir, net)
            .map_err(|e| with_cleanup(e, self.undo_start(op, id, net)))
    }

    /// Stop the VM and remove its container, Kata leftovers and network; the workspace stays.
    /// Anything already gone counts as done, so this also clears up after a VM that died.
    fn stop(&self, op: &'static str, id: &AgentId) -> Result<(), PortError> {
        self.ctr_gone_ok(op, &os(&["task", "delete", "--force", id.as_str()]))?;
        self.ctr_gone_ok(op, &os(&["container", "delete", id.as_str()]))?;
        self.kill_sandbox(op, id)?;
        match &self.cfg.network {
            Some(net) => self.network_down(op, net, id),
            None => Ok(()),
        }
    }
}

/// Report a failed create, plus its cleanup's error if cleanup failed too.
fn with_cleanup(original: PortError, cleanup: Result<(), PortError>) -> PortError {
    match cleanup {
        Ok(()) => original,
        Err(e) => PortError {
            op: original.op,
            cause: format!("{}; cleanup also failed: {e}", original.cause),
        },
    }
}

/// Error for a program that exited unsuccessfully, quoting stderr, or stdout if stderr is empty.
/// Exits 124 and 137 come from `timeout`.
fn failure(op: &'static str, status: ExitStatus, stdout: &[u8], stderr: &[u8]) -> PortError {
    let hint = if matches!(status, ExitStatus::Code(124 | 137)) {
        " (timed out)"
    } else {
        ""
    };
    let message = String::from_utf8_lossy(if stderr.trim_ascii().is_empty() {
        stdout
    } else {
        stderr
    });
    PortError {
        op,
        cause: format!("{status:?}{hint}: {}", message.trim()),
    }
}

/// `pkill -f` patterns for a sandbox's QEMU, shim and virtiofsd. The bracketed first letter keeps
/// a pattern from matching its own command line (or the `timeout` wrapper running it).
fn kata_patterns(id: &AgentId) -> [String; 3] {
    [
        format!("[s]andbox-{id},"),
        format!("[c]ontainerd-shim-kata-v2.* -id {id}( |$)"),
        format!("[s]hared/sandboxes/{id}/shared"),
    ]
}

fn remove_dir_if_present(op: &'static str, dir: &Path) -> Result<(), PortError> {
    match std::fs::remove_dir_all(dir) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(PortError {
            op,
            cause: format!("rm {}: {e}", dir.display()),
        }),
        _ => Ok(()),
    }
}

/// Agent ids in `ctr task ls` output (`TASK PID STATUS`, after a header) whose task has not
/// stopped. `UNKNOWN` means the shim is gone, so the VM is too. Non-agent ids are skipped.
fn live_tasks(out: &str) -> Vec<AgentId> {
    out.lines()
        .skip(1)
        .filter_map(|line| {
            let mut cols = line.split_whitespace();
            let (task, status) = (cols.next()?, cols.nth(1)?);
            let live = !matches!(status, "STOPPED" | "UNKNOWN");
            live.then(|| AgentId::parse(task).ok()).flatten()
        })
        .collect()
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
        self.boot("create", id, &dir)
    }

    // Pause stops the VM rather than freezing it (`ctr task pause`): a frozen Kata VM keeps its
    // memory, and nothing could bring it back after a host reboot. Resume boots a fresh VM.
    fn pause(&self, id: &AgentId) -> Result<(), PortError> {
        self.stop("pause", id)
    }

    fn resume(&self, id: &AgentId) -> Result<(), PortError> {
        let dir = self.workspace(id);
        if !dir.is_dir() {
            return Err(PortError {
                op: "resume",
                cause: format!("workspace {} is missing", dir.display()),
            });
        }
        self.boot("resume", id, &dir)
    }

    fn destroy(&self, id: &AgentId, purge: bool) -> Result<(), PortError> {
        // Already gone counts as done, so destroy can be retried after a crash or reboot.
        self.stop("destroy", id)?;
        if purge {
            remove_dir_if_present("destroy", &self.workspace(id))?;
        }
        Ok(())
    }

    fn running(&self) -> Result<Vec<AgentId>, PortError> {
        let out = self.ctr_output("reconcile", &os(&["task", "ls"]))?;
        Ok(live_tasks(&String::from_utf8_lossy(&out)))
    }
}

impl<S: Spawner> CtrRuntime<S> {
    fn start_container(
        &self,
        op: &'static str,
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
            "--hostname".to_owned(),
            id.to_string(),
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
        self.ctr(op, &os(&args))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use platform::process::{Child, ExitStatus, GroupHandle};
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

    fn mock(ctr: ExitStatus, plugin: ExitStatus) -> MockSpawner {
        MockSpawner::new()
            .script("ctr", ctr)
            .script("ip", ExitStatus::Code(0))
            .script("sh", plugin)
            // 1 is pkill's "nothing matched", which cleanup must accept.
            .script("pkill", ExitStatus::Code(1))
            .script("timeout", ExitStatus::Code(0))
    }

    fn cfg(root: &Path) -> CtrConfig {
        CtrConfig {
            ctr_bin: "ctr".into(),
            namespace: "agents".into(),
            image: "agent:latest".into(),
            workspace_root: root.to_path_buf(),
            workspace_owner: None,
            network: None,
            vc_root: root.join("vc"),
            task_root: root.join("task"),
            command_timeout: None,
            trace: false,
        }
    }

    fn rt_with(ctr: ExitStatus, plugin: ExitStatus) -> (CtrRuntime<MockSpawner>, Log, PathBuf) {
        let root = tmp();
        let spawner = mock(ctr, plugin);
        let log = Arc::clone(&spawner.spawned);
        (CtrRuntime::new(spawner, cfg(&root)), log, root)
    }

    fn rt(status: ExitStatus) -> (CtrRuntime<MockSpawner>, Log, PathBuf) {
        rt_with(status, ExitStatus::Code(0))
    }

    /// A runtime with a network whose config files and netns dir live under `root`.
    fn rt_net(ctr: ExitStatus, plugin: ExitStatus) -> (CtrRuntime<MockSpawner>, Log, PathBuf) {
        let (mut rt, log, root) = rt_with(ctr, plugin);
        with_network(&mut rt.cfg, &root);
        (rt, log, root)
    }

    fn with_network(cfg: &mut CtrConfig, root: &Path) {
        let mut net = Network::defaults(&root.join("etc")).unwrap_or_else(|e| panic!("{e}"));
        net.plugin = PathBuf::from(PLUGIN);
        net.netns_dir = root.join("netns");
        let _ = std::fs::create_dir_all(&net.netns_dir);
        cfg.network = Some(net);
    }

    /// `ctr run` is scripted by `run`, commands before it by `ok` and after it (the cleanup) by
    /// `after`; one log for all.
    struct RunFails {
        ok: MockSpawner,
        run: MockSpawner,
        after: MockSpawner,
        ran: std::sync::atomic::AtomicBool,
        log: Log,
    }

    impl Spawner for RunFails {
        fn spawn(&self, cmd: &Command) -> platform::error::Result<Box<dyn Child>> {
            let mut log = self.log.lock().unwrap_or_else(|e| e.into_inner());
            log.push(SpawnRecord::from(cmd));
            if cmd.argv.iter().any(|a| a == "run") {
                self.ran.store(true, Ordering::Relaxed);
                return self.run.spawn(cmd);
            }
            if self.ran.load(Ordering::Relaxed) {
                return self.after.spawn(cmd);
            }
            self.ok.spawn(cmd)
        }

        fn resolve(&self, program: &OsStr) -> platform::error::Result<OsString> {
            self.ok.resolve(program)
        }

        fn adopt(&self, pid: u32) -> platform::error::Result<Box<dyn GroupHandle>> {
            self.ok.adopt(pid)
        }

        fn is_alive(&self, pid: u32) -> platform::error::Result<bool> {
            self.ok.is_alive(pid)
        }

        fn is_zombie(&self, pid: u32) -> platform::error::Result<bool> {
            self.ok.is_zombie(pid)
        }
    }

    /// A networked runtime whose `ctr run` exits with `run` (also when wrapped in `timeout`) and
    /// whose cleanup `pkill` exits with `pkill`, with a netns and Kata state dirs left behind as
    /// a real failed start leaves them.
    fn rt_run_fails(run: ExitStatus, pkill: ExitStatus) -> (CtrRuntime<RunFails>, Log, PathBuf) {
        let root = tmp();
        let spawner = RunFails {
            ok: mock(ExitStatus::Code(0), ExitStatus::Code(0)),
            run: MockSpawner::new().script("ctr", run).script("timeout", run),
            after: mock(ExitStatus::Code(0), ExitStatus::Code(0)).script("pkill", pkill),
            ran: Default::default(),
            log: Log::default(),
        };
        let log = Arc::clone(&spawner.log);
        let mut cfg = cfg(&root);
        with_network(&mut cfg, &root);
        let _ = std::fs::write(root.join("netns/ac-a1"), "");
        for kind in ["vm", "sbs"] {
            let _ = std::fs::create_dir_all(root.join("vc").join(kind).join("a1"));
        }
        let _ = std::fs::create_dir_all(root.join("task/agents/a1/rootfs"));
        (CtrRuntime::new(spawner, cfg), log, root)
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
            &calls[5][..6],
            ["-n", "agents", "run", "-d", "--runtime", KATA]
        );
        assert!(calls[5].iter().any(|a| a.contains("dst=/workspace")));
        assert!(!calls[5].iter().any(|a| a == "--with-ns"));
        assert!(calls[5].windows(2).any(|w| w == ["--hostname", "a1"]));
        assert_eq!(calls[5].last().map(String::as_str), Some("a1"));
        assert!(root.join("a1").is_dir());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn create_removes_a_stale_container_and_snapshot_first() {
        let (rt, log, root) = rt(ExitStatus::Code(0));
        assert!(rt.create(&id()).is_ok());
        let calls = argv(&log);
        assert_eq!(calls[0][2..], ["container", "delete", "a1"]);
        assert_eq!(calls[1][2..], ["snapshots", "rm", "a1"]);
        assert_eq!(calls.len(), 6);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn stale_removal_failure_stops_create() {
        let (rt, log, root) = rt_net(ExitStatus::Code(1), ExitStatus::Code(0));
        assert_eq!(rt.create(&id()).err().map(|e| e.op), Some("create"));
        assert_eq!(programs(&log), ["ctr"]);
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
        // A DEL first releases any lease a reboot left behind; no namespace, so no `ip netns del`.
        assert_eq!(
            programs(&log),
            ["ctr", "ctr", "pkill", "pkill", "pkill", "sh", "ip", "sh", "ctr"]
        );
        assert_eq!(env_of(&log, 5, "CNI_COMMAND").as_deref(), Some("DEL"));
        let cni_args = &argv(&log)[7];
        assert_eq!(cni_args[2], PLUGIN);
        assert!(cni_args[3].ends_with("cni-agents.json"));
        assert_eq!(argv(&log)[6], ["netns", "add", "ac-a1"]);
        assert_eq!(env_of(&log, 7, "CNI_COMMAND").as_deref(), Some("ADD"));
        assert_eq!(env_of(&log, 7, "CNI_CONTAINERID").as_deref(), Some("a1"));
        assert_eq!(env_of(&log, 7, "CNI_IFNAME").as_deref(), Some("eth0"));
        assert_eq!(env_of(&log, 7, "CNI_PATH").as_deref(), Some("/fake/cni"));
        let ns = root.join("netns/ac-a1").display().to_string();
        assert_eq!(env_of(&log, 7, "CNI_NETNS").as_deref(), Some(ns.as_str()));
        let ctr = &argv(&log)[8];
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
    fn cni_failure_never_starts_the_container() {
        let (rt, log, root) = rt_net(ExitStatus::Code(0), ExitStatus::Code(1));
        let err = rt.create(&id()).err();
        assert_eq!(err.map(|e| e.op), Some("create"));
        // The plugin fails on the first call, the DEL that clears stale leases.
        assert_eq!(
            programs(&log),
            ["ctr", "ctr", "pkill", "pkill", "pkill", "sh"]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn start_failure_kills_kata_leftovers_and_undoes_everything() {
        let (rt, log, root) = rt_run_fails(ExitStatus::Code(1), ExitStatus::Code(1));
        assert_eq!(rt.create(&id()).err().map(|e| e.op), Some("create"));
        assert_eq!(
            programs(&log),
            [
                "ctr", "ctr", "pkill", "pkill", "pkill", "sh", "ip", "ip", "sh", "ctr", "pkill",
                "pkill", "pkill", "ctr", "ctr", "sh", "ip"
            ]
        );
        let calls = argv(&log);
        // The namespace the fixture left behind is removed before a fresh one is added.
        assert_eq!(env_of(&log, 5, "CNI_COMMAND").as_deref(), Some("DEL"));
        assert_eq!(calls[6], ["netns", "del", "ac-a1"]);
        assert_eq!(calls[7], ["netns", "add", "ac-a1"]);
        assert_eq!(calls[10], ["-KILL", "-f", "[s]andbox-a1,"]);
        assert_eq!(calls[11][2], "[c]ontainerd-shim-kata-v2.* -id a1( |$)");
        assert_eq!(calls[12][2], "[s]hared/sandboxes/a1/shared");
        assert_eq!(calls[13][2..], ["container", "delete", "a1"]);
        assert_eq!(calls[14][2..], ["snapshots", "rm", "a1"]);
        assert_eq!(env_of(&log, 15, "CNI_COMMAND").as_deref(), Some("DEL"));
        assert_eq!(calls[16], ["netns", "del", "ac-a1"]);
        assert!(!root.join("vc/vm/a1").exists() && !root.join("vc/sbs/a1").exists());
        assert!(!root.join("task/agents/a1").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_bundle_with_a_non_empty_rootfs_is_left_alone() {
        let (rt, _, root) = rt(ExitStatus::Code(0));
        let rootfs = root.join("task/agents/a1/rootfs");
        let _ = std::fs::create_dir_all(rootfs.join("etc"));
        let err = rt.pause(&id()).err().map(|e| e.cause).unwrap_or_default();
        assert!(err.contains("rootfs"), "{err}");
        assert!(rootfs.join("etc").is_dir());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn start_timeout_is_reported_and_cleaned_up() {
        let (mut rt, log, root) = rt_run_fails(ExitStatus::Code(124), ExitStatus::Code(1));
        rt.cfg.command_timeout = Some(Duration::from_secs(30));
        let err = rt.create(&id()).err().map(|e| e.cause).unwrap_or_default();
        assert!(
            err.contains("timed out") && !err.contains("cleanup"),
            "{err}"
        );
        let killed = argv(&log).iter().filter(|a| a[2] == "pkill").count();
        assert_eq!(killed, 6);
        assert!(!root.join("vc/vm/a1").exists() && !root.join("vc/sbs/a1").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn failed_cleanup_is_reported_and_the_network_still_comes_down() {
        let (rt, log, root) = rt_run_fails(ExitStatus::Code(1), ExitStatus::Code(2));
        let err = rt.create(&id()).err().map(|e| e.cause).unwrap_or_default();
        assert!(err.contains("cleanup also failed"), "{err}");
        assert_eq!(programs(&log)[10..], ["pkill", "ctr", "ctr", "sh", "ip"]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn kata_patterns_never_match_their_own_command_line() {
        for pattern in kata_patterns(&id()) {
            let head: String = pattern
                .chars()
                .filter(|c| !"[]".contains(*c))
                .take(8)
                .collect();
            assert!(!pattern.contains(&head), "{pattern}");
        }
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
    fn pause_stops_the_vm_and_keeps_the_workspace() {
        let (rt, log, root) = rt_net(ExitStatus::Code(0), ExitStatus::Code(0));
        let _ = std::fs::create_dir_all(root.join("a1"));
        let _ = std::fs::write(root.join("netns/ac-a1"), "");
        let _ = std::fs::create_dir_all(root.join("vc/vm/a1"));
        assert!(rt.pause(&id()).is_ok());
        assert_eq!(
            programs(&log),
            ["ctr", "ctr", "pkill", "pkill", "pkill", "sh", "ip"]
        );
        let calls = argv(&log);
        assert_eq!(calls[0][2..], ["task", "delete", "--force", "a1"]);
        assert_eq!(calls[1][2..], ["container", "delete", "a1"]);
        assert_eq!(env_of(&log, 5, "CNI_COMMAND").as_deref(), Some("DEL"));
        assert!(!root.join("vc/vm/a1").exists());
        assert!(root.join("a1").is_dir());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn resume_boots_a_fresh_vm_on_the_kept_workspace() {
        let (rt, log, root) = rt_net(ExitStatus::Code(0), ExitStatus::Code(0));
        let _ = std::fs::create_dir_all(root.join("a1"));
        assert!(rt.resume(&id()).is_ok());
        assert_eq!(
            programs(&log),
            ["ctr", "ctr", "pkill", "pkill", "pkill", "sh", "ip", "sh", "ctr"]
        );
        let run = &argv(&log)[8];
        assert_eq!(run[2..4], ["run", "-d"]);
        let src = format!("src={}", root.join("a1").display());
        assert!(run.iter().any(|a| a.contains(&src)));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn resume_without_a_workspace_is_refused_before_any_command() {
        let (rt, log, root) = rt(ExitStatus::Code(0));
        let err = rt.resume(&id()).err();
        assert_eq!(err.map(|e| e.op), Some("resume"));
        assert!(programs(&log).is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn purge_deletes_task_container_and_volume() {
        let (rt, log, root) = rt(ExitStatus::Code(0));
        let _ = std::fs::create_dir_all(root.join("a1"));
        assert!(rt.destroy(&id(), true).is_ok());
        let calls = argv(&log);
        assert_eq!(calls[0][2..], ["task", "delete", "--force", "a1"]);
        assert_eq!(calls[1][2..], ["container", "delete", "a1"]);
        assert!(!root.join("a1").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn destroy_keeps_the_volume_unless_purged() {
        let (rt, _, root) = rt(ExitStatus::Code(0));
        let _ = std::fs::create_dir_all(root.join("a1"));
        assert!(rt.destroy(&id(), false).is_ok());
        assert!(root.join("a1").is_dir());
        assert!(rt.destroy(&id(), true).is_ok());
        assert!(!root.join("a1").exists());
        // Purging again, with the volume already gone, is still success.
        assert!(rt.destroy(&id(), true).is_ok());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn destroy_detaches_the_network_when_the_namespace_exists() {
        let (rt, log, root) = rt_net(ExitStatus::Code(0), ExitStatus::Code(0));
        let _ = std::fs::create_dir_all(root.join("a1"));
        let _ = std::fs::write(root.join("netns/ac-a1"), "");
        assert!(rt.destroy(&id(), false).is_ok());
        assert_eq!(
            programs(&log),
            ["ctr", "ctr", "pkill", "pkill", "pkill", "sh", "ip"]
        );
        assert_eq!(env_of(&log, 5, "CNI_COMMAND").as_deref(), Some("DEL"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn destroy_without_a_namespace_still_releases_the_lease() {
        let (rt, log, root) = rt_net(ExitStatus::Code(0), ExitStatus::Code(0));
        let _ = std::fs::create_dir_all(root.join("a1"));
        assert!(rt.destroy(&id(), false).is_ok());
        assert_eq!(
            programs(&log),
            ["ctr", "ctr", "pkill", "pkill", "pkill", "sh"]
        );
        assert_eq!(env_of(&log, 5, "CNI_COMMAND").as_deref(), Some("DEL"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn commands_are_wrapped_in_timeout_when_configured() {
        let (mut rt, log, _) = rt(ExitStatus::Code(0));
        rt.cfg.command_timeout = Some(Duration::from_secs(30));
        assert!(rt.pause(&id()).is_ok());
        assert!(programs(&log).iter().all(|p| p == "timeout"));
        assert_eq!(
            argv(&log)[0],
            [
                "--kill-after=5",
                "30",
                "ctr",
                "-n",
                "agents",
                "task",
                "delete",
                "--force",
                "a1"
            ]
        );
    }

    #[test]
    fn timeout_exit_is_reported_as_a_timeout() {
        let (mut rt, _, _) = rt(ExitStatus::Code(0));
        rt.cfg.command_timeout = Some(Duration::from_secs(1));
        let spawner = MockSpawner::new().script("timeout", ExitStatus::Code(124));
        let slow = CtrRuntime::new(spawner, rt.cfg.clone());
        let err = slow.pause(&id()).err().map(|e| e.cause).unwrap_or_default();
        assert!(err.contains("timed out"), "{err}");
    }

    #[test]
    fn running_lists_agents_with_a_live_task() {
        let out =
            "TASK    PID      STATUS    \na1      20972    RUNNING\nb2      0        STOPPED\n\
                   c3      7        PAUSED\nd4      0        UNKNOWN\nNOT_AN_ID 9 RUNNING\n";
        let spawner = MockSpawner::new().script_with_output("ctr", ExitStatus::Code(0), out);
        let rt = CtrRuntime::new(spawner, cfg(&tmp()));
        let live = rt.running().unwrap_or_else(|e| panic!("{e}"));
        let names: Vec<&str> = live.iter().map(AgentId::as_str).collect();
        assert_eq!(names, ["a1", "c3"]);
        assert!(live_tasks("TASK    PID      STATUS    \n").is_empty());
    }

    #[test]
    fn running_fails_when_ctr_does() {
        let (rt, log, _) = rt(ExitStatus::Code(1));
        assert_eq!(rt.running().err().map(|e| e.op), Some("reconcile"));
        assert_eq!(argv(&log)[0][2..], ["task", "ls"]);
    }

    #[test]
    fn failure_quotes_stdout_when_stderr_is_empty() {
        let cni = br#"{"code": 999, "msg": "duplicate allocation is not allowed"}"#;
        let e = failure("resume", ExitStatus::Code(1), cni, b"\n");
        assert!(e.cause.contains("duplicate allocation"), "{}", e.cause);
        let e = failure("resume", ExitStatus::Code(1), b"ignored", b"real error");
        assert_eq!(e.cause, "Code(1): real error");
    }

    #[test]
    fn already_gone_is_recognised() {
        assert!(is_already_gone(
            "Code(1): ctr: no running task found: task a1 not found"
        ));
        assert!(is_already_gone(
            r#"Code(1): container "a1" in namespace "agents": not found"#
        ));
        assert!(is_already_gone(
            r#"Code(1): ctr: failed to remove "a1": snapshot a1 does not exist: not found"#
        ));
        assert!(!is_already_gone("Code(1): permission denied"));
    }

    #[test]
    fn nonzero_exit_is_an_error_and_stops_destroy() {
        let (rt, log, _) = rt(ExitStatus::Code(1));
        let err = rt.destroy(&id(), false).err().map(|e| e.op);
        assert_eq!(err, Some("destroy"));
        assert_eq!(argv(&log).len(), 1);
    }
}
