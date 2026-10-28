//! Hand-rolled argument parsing (a dozen flags do not justify a dependency).

use std::fmt;
use std::path::PathBuf;

use ac_domain::backup::{DEFAULT_FULL_EVERY, DEFAULT_KEEP};
use ac_domain::checkpoint::LabelError;
use ac_domain::tune::Profile;
use ac_domain::{AgentId, BackupId, CheckpointId, IdError, Label};

/// Usage text.
pub const USAGE: &str = "usage: ac-ctl --image IMAGE [--data DIR] [--workspaces DIR] [--ctr PATH] \
[--namespace NS] [--uid N] [--network] [--purge] <create|pause|resume|destroy|status> <agent-id>
       ac-ctl --image IMAGE [...] [--network] reconcile
       ac-ctl --image IMAGE [...] checkpoints <agent-id>
       ac-ctl --image IMAGE [...] [--label TEXT] checkpoint <agent-id>
       ac-ctl --image IMAGE [...] delete-checkpoint <agent-id> <checkpoint>
       ac-ctl --image IMAGE [...] restore <agent-id> <checkpoint>
       ac-ctl --image IMAGE [...] [--network] [--from CHECKPOINT] fork <agent-id> <new-agent-id>
       ac-ctl --image IMAGE [...] workspace migrate <agent-id>
       ac-ctl [--data DIR] [--workspaces DIR] [--keep N] [--full-every N] backup <dest> [<agent-id>...]
       ac-ctl [--data DIR] backups <dest> [<agent-id>]
       ac-ctl [--data DIR] [--workspaces DIR] restore-backup <dest> <agent-id> [<backup>]
       ac-ctl doctor [--profile idle|build|heavy] [--json] [--facts-file F] [--root DIR] [--explain]
       ac-ctl tune [--profile idle|build|heavy] [--facts-file F] [--root DIR] [--apply [--force] \
[--restart-gateway]]
  destroy keeps the agent's workspace; destroy --purge removes it (also after a plain destroy).
  reconcile marks agents recorded running but with no VM (after a reboot) paused, for resume.
  Checkpoints (ADR-018; workspaces must be btrfs subvolumes): checkpoint snapshots a running or
    paused agent's workspace read-only (AC_CHECKPOINT_MAX, default 8, per agent); restore (paused
    agents only) puts one back and keeps the workspace it replaces as a new checkpoint; fork
    creates and boots a new agent on a copy of the workspace, or of checkpoint --from. workspace
    migrate turns a paused agent's plain-directory workspace into a subvolume.
  Backups (ADR-021; root; dest any directory, best on another machine or disk): backup sends
    each subvolume workspace (or those named) as a btrfs send stream file to <dest>/<agent>/,
    incrementally against the last one, in full every --full-every (default 7), and keeps the
    newest --keep (default 7) there with the streams they need; exit 1 if any agent was not
    backed up (a plain directory needs workspace migrate). backups lists them. restore-backup
    replays one (default the newest) in place of a workspace with no VM, keeping the old one as
    a checkpoint. None of the three opens the registry: they work while ac-gateway runs.
  doctor compares this host's settings with what its facts recommend for the profile (ADR-014):
    read-only; exit 0 all ok, 1 warnings, 4 errors. --facts-file plans a host from a JSON file of
    facts (deploy/hosts/) instead of this one; --explain lists every rule and its evidence.
  tune prints the edits doctor recommends as diffs; --apply (root) makes them, with backups, and
    refuses while a Kata VM runs unless --force. --root DIR edits copies under DIR instead.
  The profile defaults to AC_PROFILE in gateway.env, else build. Neither command opens the
  registry: both work while ac-gateway runs.";

/// What to do to the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Provision and start.
    Create,
    /// Stop the VM, keep the volume.
    Pause,
    /// Start a paused VM.
    Resume,
    /// Tear down, keeping the workspace unless `purge`.
    Destroy {
        /// Also remove the workspace.
        purge: bool,
    },
    /// Print the lifecycle state.
    Status,
}

/// What the command line asks for.
#[derive(Debug)]
pub enum Command {
    /// Act on the fleet (needs the registry).
    Fleet(Cli),
    /// Check the host's settings.
    Doctor(Doctor),
    /// Make the host's settings what doctor recommends.
    Tune(Tune),
    /// Back up workspaces, list backups, or restore one (no registry).
    Backup(Backup),
}

/// `ac-ctl backup`, `backups` and `restore-backup`.
#[derive(Debug, PartialEq, Eq)]
pub struct Backup {
    /// Data directory: backup parents are kept under `<data>/backups`, checkpoints under
    /// `<data>/checkpoints`.
    pub data: PathBuf,
    /// Parent of per-agent workspace volumes.
    pub workspaces: PathBuf,
    /// Where backups go: a directory on another btrfs filesystem.
    pub dest: PathBuf,
    /// What to do.
    pub op: BackupOp,
}

/// A backup operation.
#[derive(Debug, PartialEq, Eq)]
pub enum BackupOp {
    /// Back up these agents (empty: every workspace), keeping the newest `keep` at `dest`.
    Send {
        /// The agents.
        agents: Vec<AgentId>,
        /// Backups kept per agent.
        keep: usize,
        /// A full send at least this often.
        full_every: usize,
    },
    /// List the backups of one agent, or of every agent at `dest`.
    List(Option<AgentId>),
    /// Restore one backup (`None`: the newest) of the agent.
    Restore(AgentId, Option<BackupId>),
}

/// `ac-ctl doctor`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Doctor {
    /// The workload profile; `None`: `AC_PROFILE`, else build.
    pub profile: Option<Profile>,
    /// JSON instead of a table.
    pub json: bool,
    /// Facts from this file instead of this host.
    pub facts_file: Option<PathBuf>,
    /// The edited configuration files under this directory instead of `/`.
    pub root: Option<PathBuf>,
    /// List the rules instead.
    pub explain: bool,
}

/// `ac-ctl tune`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Tune {
    /// The workload profile; `None`: `AC_PROFILE`, else build.
    pub profile: Option<Profile>,
    /// Facts from this file instead of this host (dry run only).
    pub facts_file: Option<PathBuf>,
    /// The edited configuration files under this directory instead of `/`.
    pub root: Option<PathBuf>,
    /// Make the edits.
    pub apply: bool,
    /// Apply even while Kata VMs run.
    pub force: bool,
    /// Restart ac-gateway after applying, and wait for its port.
    pub restart_gateway: bool,
}

/// What the fleet command line asks for.
#[derive(Debug)]
pub enum Job {
    /// Act on one agent.
    Agent(Action, AgentId),
    /// Bring the registry in line with containerd.
    Reconcile,
    /// A workspace checkpoint, restore, fork or migration (ADR-018).
    Snap(Snap),
}

/// A workspace operation.
#[derive(Debug, PartialEq, Eq)]
pub enum Snap {
    /// List the agent's checkpoints.
    List(AgentId),
    /// Make a checkpoint.
    Take(AgentId, Option<Label>),
    /// Delete a checkpoint.
    Delete(AgentId, CheckpointId),
    /// Put a checkpoint back.
    Restore(AgentId, CheckpointId),
    /// Create agent `to` on a copy of `from`'s workspace, or of its checkpoint.
    Fork {
        /// The source agent.
        from: AgentId,
        /// The new agent.
        to: AgentId,
        /// The checkpoint to copy; `None`: the workspace now.
        checkpoint: Option<CheckpointId>,
    },
    /// Turn a plain-directory workspace into a subvolume.
    Migrate(AgentId),
}

/// Parsed command line.
#[derive(Debug)]
pub struct Cli {
    /// Registry directory.
    pub data: PathBuf,
    /// Parent of per-agent workspace volumes.
    pub workspaces: PathBuf,
    /// `ctr` binary.
    pub ctr: String,
    /// containerd namespace.
    pub namespace: String,
    /// Agent image.
    pub image: String,
    /// Workspace owner uid/gid (the unprivileged agent user in the image).
    pub uid: u32,
    /// Attach agents to the CNI network.
    pub network: bool,
    /// What to do.
    pub job: Job,
}

/// Why parsing failed.
#[derive(Debug)]
pub enum ArgError {
    /// A flag that needs a value had none.
    MissingValue(String),
    /// Unknown flag.
    UnknownFlag(String),
    /// Required piece absent.
    Missing(&'static str),
    /// Unknown action word.
    UnknownAction(String),
    /// Extra positional argument.
    Extra(String),
    /// Agent id rejected.
    BadId(IdError),
    /// `--uid` was not a number.
    BadUid(String),
    /// `--purge` given with an action other than `destroy`.
    PurgeWithout(String),
    /// `--profile` other than idle, build or heavy.
    BadProfile(String),
    /// A flag that needs another one.
    Needs(&'static str, &'static str),
    /// Two flags that exclude each other.
    Conflict(&'static str, &'static str),
    /// A flag given with an action it does not apply to.
    OnlyWith(&'static str, &'static str),
    /// Checkpoint label rejected.
    BadLabel(LabelError),
    /// `--keep` or `--full-every` was not a number of at least 1.
    BadCount(&'static str, String),
}

impl fmt::Display for ArgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingValue(k) => write!(f, "{k} needs a value"),
            Self::UnknownFlag(k) => write!(f, "unknown flag {k}"),
            Self::Missing(what) => write!(f, "missing {what}"),
            Self::UnknownAction(a) => write!(f, "unknown action {a}"),
            Self::Extra(a) => write!(f, "unexpected argument {a}"),
            Self::BadId(e) => e.fmt(f),
            Self::BadUid(v) => write!(f, "--uid must be a number, got {v}"),
            Self::PurgeWithout(a) => write!(f, "--purge only applies to destroy, not {a}"),
            Self::BadProfile(p) => write!(f, "--profile must be idle, build or heavy, not {p}"),
            Self::Needs(a, b) => write!(f, "{a} needs {b}"),
            Self::Conflict(a, b) => write!(f, "{a} and {b} exclude each other"),
            Self::OnlyWith(flag, action) => write!(f, "{flag} only applies to {action}"),
            Self::BadLabel(e) => e.fmt(f),
            Self::BadCount(flag, v) => write!(f, "{flag} must be a number of at least 1, got {v}"),
        }
    }
}

impl std::error::Error for ArgError {}

fn action(word: &str) -> Result<Action, ArgError> {
    Ok(match word {
        "create" => Action::Create,
        "pause" => Action::Pause,
        "resume" => Action::Resume,
        "destroy" => Action::Destroy { purge: false },
        "status" => Action::Status,
        other => return Err(ArgError::UnknownAction(other.to_owned())),
    })
}

/// Parse `args` (without the program name): `doctor` or `tune` first, else a fleet command.
pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Command, ArgError> {
    let mut args = args.into_iter().peekable();
    match args.peek().map(String::as_str) {
        Some("doctor") => host_tool(args.skip(1))
            .and_then(doctor)
            .map(Command::Doctor),
        Some("tune") => host_tool(args.skip(1)).and_then(tune).map(Command::Tune),
        _ => match backup(args.by_ref())? {
            Ok(b) => Ok(Command::Backup(b)),
            Err(args) => fleet(args).map(Command::Fleet),
        },
    }
}

/// `value` of `flag` as a number of at least 1.
fn count(flag: &'static str, value: String) -> Result<usize, ArgError> {
    match value.parse::<usize>() {
        Ok(n) if n >= 1 => Ok(n),
        _ => Err(ArgError::BadCount(flag, value)),
    }
}

/// A backup command, or every argument back when the first word after the flags names none.
fn backup(args: impl Iterator<Item = String>) -> Result<Result<Backup, Vec<String>>, ArgError> {
    let all: Vec<String> = args.collect();
    let (mut flags, mut pos) = (Vec::new(), Vec::new());
    let mut it = all.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            // Every flag that takes a value, the fleet's too, so its value is not a word.
            "--data" | "--workspaces" | "--keep" | "--full-every" | "--image" | "--ctr"
            | "--namespace" | "--uid" | "--label" | "--from" => flags.push((arg, it.next())),
            a if a.starts_with("--") => flags.push((arg, None)),
            _ => pos.push(arg.clone()),
        }
    }
    let word = pos.first().cloned().unwrap_or_default();
    if !matches!(word.as_str(), "backup" | "backups" | "restore-backup") {
        return Ok(Err(all));
    }
    let (mut data, mut workspaces) = (PathBuf::from("/var/lib/agent-cloud"), None::<PathBuf>);
    let (mut keep, mut full_every) = (None::<usize>, None::<usize>);
    for (flag, value) in flags {
        let value = || {
            value
                .cloned()
                .ok_or_else(|| ArgError::MissingValue(flag.clone()))
        };
        match flag.as_str() {
            "--data" => data = value()?.into(),
            "--workspaces" => workspaces = Some(value()?.into()),
            "--keep" => keep = Some(count("--keep", value()?)?),
            "--full-every" => full_every = Some(count("--full-every", value()?)?),
            _ => return Err(ArgError::UnknownFlag(flag.clone())),
        }
    }
    let mut rest = pos.into_iter().skip(1);
    let dest = PathBuf::from(rest.next().ok_or(ArgError::Missing("backup destination"))?);
    let op = match word.as_str() {
        "backup" => BackupOp::Send {
            agents: rest
                .by_ref()
                .map(|a| AgentId::parse(&a).map_err(ArgError::BadId))
                .collect::<Result<_, _>>()?,
            keep: keep.unwrap_or(DEFAULT_KEEP),
            full_every: full_every.unwrap_or(DEFAULT_FULL_EVERY),
        },
        "backups" => BackupOp::List(
            rest.next()
                .map(|a| AgentId::parse(&a))
                .transpose()
                .map_err(ArgError::BadId)?,
        ),
        _ => BackupOp::Restore(
            agent_arg(&mut rest, "agent id")?,
            rest.next()
                .map(|b| BackupId::parse(&b))
                .transpose()
                .map_err(ArgError::BadId)?,
        ),
    };
    if !matches!(op, BackupOp::Send { .. }) {
        if keep.is_some() {
            return Err(ArgError::OnlyWith("--keep", "backup"));
        }
        if full_every.is_some() {
            return Err(ArgError::OnlyWith("--full-every", "backup"));
        }
    }
    if let Some(extra) = rest.next() {
        return Err(ArgError::Extra(extra));
    }
    Ok(Ok(Backup {
        workspaces: workspaces.unwrap_or_else(|| data.join("workspaces")),
        data,
        dest,
        op,
    }))
}

/// The flags `doctor` and `tune` share, plus tune's own, before checking which apply.
#[derive(Default)]
struct TuneFlags {
    apply: bool,
    force: bool,
    restart_gateway: bool,
}

fn host_tool(args: impl Iterator<Item = String>) -> Result<(Doctor, TuneFlags), ArgError> {
    let (mut d, mut t) = (Doctor::default(), TuneFlags::default());
    let mut it = args;
    while let Some(arg) = it.next() {
        let mut value = || it.next().ok_or_else(|| ArgError::MissingValue(arg.clone()));
        match arg.as_str() {
            "--profile" => {
                let v = value()?;
                d.profile = Some(Profile::parse(&v).ok_or(ArgError::BadProfile(v))?);
            }
            "--json" => d.json = true,
            "--explain" => d.explain = true,
            "--facts-file" => d.facts_file = Some(value()?.into()),
            "--root" => d.root = Some(value()?.into()),
            "--apply" => t.apply = true,
            "--force" => t.force = true,
            "--restart-gateway" => t.restart_gateway = true,
            a if a.starts_with("--") => return Err(ArgError::UnknownFlag(arg)),
            _ => return Err(ArgError::Extra(arg)),
        }
    }
    if d.facts_file.is_some() && d.root.is_some() {
        return Err(ArgError::Conflict("--facts-file", "--root"));
    }
    Ok((d, t))
}

fn doctor((d, t): (Doctor, TuneFlags)) -> Result<Doctor, ArgError> {
    let tune_only = [
        (t.apply, "--apply"),
        (t.force, "--force"),
        (t.restart_gateway, "--restart-gateway"),
    ];
    match tune_only.iter().find(|(set, _)| *set) {
        Some((_, flag)) => Err(ArgError::UnknownFlag((*flag).into())),
        None => Ok(d),
    }
}

fn tune((d, t): (Doctor, TuneFlags)) -> Result<Tune, ArgError> {
    if d.json || d.explain {
        let flag = if d.json { "--json" } else { "--explain" };
        return Err(ArgError::UnknownFlag(flag.into()));
    }
    if t.force && !t.apply {
        return Err(ArgError::Needs("--force", "--apply"));
    }
    if t.restart_gateway && !t.apply {
        return Err(ArgError::Needs("--restart-gateway", "--apply"));
    }
    if t.apply && d.facts_file.is_some() {
        return Err(ArgError::Conflict("--apply", "--facts-file"));
    }
    if t.restart_gateway && d.root.is_some() {
        return Err(ArgError::Conflict("--restart-gateway", "--root"));
    }
    Ok(Tune {
        profile: d.profile,
        facts_file: d.facts_file,
        root: d.root,
        apply: t.apply,
        force: t.force,
        restart_gateway: t.restart_gateway,
    })
}

fn fleet(args: impl IntoIterator<Item = String>) -> Result<Cli, ArgError> {
    let (mut data, mut workspaces) = (PathBuf::from("/var/lib/agent-cloud"), None::<PathBuf>);
    let (mut ctr, mut namespace, mut image) = ("ctr".to_owned(), "agents".to_owned(), None);
    let (mut uid, mut network, mut purge) = (1000u32, false, false);
    let (mut label, mut from) = (None::<String>, None::<String>);
    let mut positional = Vec::new();
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        if !arg.starts_with("--") {
            positional.push(arg);
            continue;
        }
        let mut value = || it.next().ok_or_else(|| ArgError::MissingValue(arg.clone()));
        match arg.as_str() {
            "--data" => data = value()?.into(),
            "--workspaces" => workspaces = Some(value()?.into()),
            "--ctr" => ctr = value()?,
            "--namespace" => namespace = value()?,
            "--image" => image = Some(value()?),
            "--uid" => {
                let v = value()?;
                uid = v.parse().map_err(|_| ArgError::BadUid(v))?;
            }
            "--network" => network = true,
            "--purge" => purge = true,
            "--label" => label = Some(value()?),
            "--from" => from = Some(value()?),
            _ => return Err(ArgError::UnknownFlag(arg)),
        }
    }
    let mut pos = positional.into_iter();
    let word = pos.next().ok_or(ArgError::Missing("action"))?;
    let job = if word == "reconcile" {
        Job::Reconcile
    } else if let Some(op) = snap(&word, &mut pos, &mut label, &mut from)? {
        Job::Snap(op)
    } else {
        let action = match action(&word)? {
            Action::Destroy { .. } => Action::Destroy { purge },
            other => other,
        };
        let id = AgentId::parse(&pos.next().ok_or(ArgError::Missing("agent id"))?)
            .map_err(ArgError::BadId)?;
        Job::Agent(action, id)
    };
    if purge && !matches!(job, Job::Agent(Action::Destroy { .. }, _)) {
        return Err(ArgError::PurgeWithout(word));
    }
    if label.is_some() {
        return Err(ArgError::OnlyWith("--label", "checkpoint"));
    }
    if from.is_some() {
        return Err(ArgError::OnlyWith("--from", "fork"));
    }
    if let Some(extra) = pos.next() {
        return Err(ArgError::Extra(extra));
    }
    Ok(Cli {
        workspaces: workspaces.unwrap_or_else(|| data.join("workspaces")),
        data,
        ctr,
        namespace,
        image: image.ok_or(ArgError::Missing("--image"))?,
        uid,
        network,
        job,
    })
}

fn agent_arg(
    pos: &mut impl Iterator<Item = String>,
    what: &'static str,
) -> Result<AgentId, ArgError> {
    AgentId::parse(&pos.next().ok_or(ArgError::Missing(what))?).map_err(ArgError::BadId)
}

fn checkpoint_arg(s: &str) -> Result<CheckpointId, ArgError> {
    CheckpointId::parse(s).map_err(ArgError::BadId)
}

/// A workspace operation's words after `word`, taking `--label` and `--from` where they apply
/// (what is left, the caller refuses). `None` if `word` names no such operation.
fn snap(
    word: &str,
    pos: &mut impl Iterator<Item = String>,
    label: &mut Option<String>,
    from: &mut Option<String>,
) -> Result<Option<Snap>, ArgError> {
    Ok(Some(match word {
        "checkpoints" => Snap::List(agent_arg(pos, "agent id")?),
        "checkpoint" => {
            let label = label.take().map(|l| Label::parse(&l)).transpose();
            Snap::Take(
                agent_arg(pos, "agent id")?,
                label.map_err(ArgError::BadLabel)?,
            )
        }
        "delete-checkpoint" | "restore" => {
            let id = agent_arg(pos, "agent id")?;
            let cp = checkpoint_arg(&pos.next().ok_or(ArgError::Missing("checkpoint"))?)?;
            match word {
                "restore" => Snap::Restore(id, cp),
                _ => Snap::Delete(id, cp),
            }
        }
        "fork" => Snap::Fork {
            from: agent_arg(pos, "agent id")?,
            to: agent_arg(pos, "new agent id")?,
            checkpoint: from.take().map(|c| checkpoint_arg(&c)).transpose()?,
        },
        "workspace" => match pos.next().as_deref() {
            Some("migrate") => Snap::Migrate(agent_arg(pos, "agent id")?),
            Some(other) => return Err(ArgError::UnknownAction(format!("workspace {other}"))),
            None => return Err(ArgError::Missing("workspace action (migrate)")),
        },
        _ => return Ok(None),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<Cli, ArgError> {
        match parse(s.split_whitespace().map(str::to_owned))? {
            Command::Fleet(cli) => Ok(cli),
            other => panic!("not a fleet command: {other:?}"),
        }
    }

    fn host(s: &str) -> Result<Command, ArgError> {
        parse(s.split_whitespace().map(str::to_owned))
    }

    #[test]
    fn doctor_flags() {
        let d = |s| match host(s) {
            Ok(Command::Doctor(d)) => d,
            other => panic!("{s}: {other:?}"),
        };
        assert_eq!(d("doctor"), Doctor::default());
        let all = d("doctor --profile heavy --json --facts-file h.json --explain");
        assert_eq!(
            all,
            Doctor {
                profile: Some(Profile::Heavy),
                json: true,
                facts_file: Some("h.json".into()),
                root: None,
                explain: true,
            }
        );
        assert_eq!(d("doctor --root /tmp/r").root, Some("/tmp/r".into()));
        assert!(matches!(
            host("doctor --profile huge"),
            Err(ArgError::BadProfile(_))
        ));
        assert!(matches!(
            host("doctor --apply"),
            Err(ArgError::UnknownFlag(_))
        ));
        assert!(matches!(
            host("doctor --profile"),
            Err(ArgError::MissingValue(_))
        ));
        assert!(matches!(host("doctor extra"), Err(ArgError::Extra(_))));
        assert!(matches!(
            host("doctor --facts-file f --root r"),
            Err(ArgError::Conflict(..))
        ));
        // No --image needed, and "doctor" is not an agent action.
        assert!(matches!(
            host("--image i doctor"),
            Err(ArgError::UnknownAction(_))
        ));
    }

    #[test]
    fn backup_commands() {
        let b = |s| match host(s) {
            Ok(Command::Backup(b)) => b,
            other => panic!("{s}: {other:?}"),
        };
        let a = |s: &str| AgentId::parse(s).unwrap_or_else(|e| panic!("{e}"));
        let all = b("backup /mnt/backup");
        assert_eq!(all.dest, PathBuf::from("/mnt/backup"));
        assert_eq!(
            all.workspaces,
            PathBuf::from("/var/lib/agent-cloud/workspaces")
        );
        assert_eq!(
            all.op,
            BackupOp::Send {
                agents: vec![],
                keep: DEFAULT_KEEP,
                full_every: DEFAULT_FULL_EVERY
            }
        );
        let some = b("--data /d --keep 3 --full-every 4 backup /b a1 a2");
        assert_eq!(some.workspaces, PathBuf::from("/d/workspaces"));
        assert_eq!(
            some.op,
            BackupOp::Send {
                agents: vec![a("a1"), a("a2")],
                keep: 3,
                full_every: 4
            }
        );
        assert_eq!(b("backups /b").op, BackupOp::List(None));
        assert_eq!(b("backups /b a1").op, BackupOp::List(Some(a("a1"))));
        assert_eq!(
            b("restore-backup /b a1").op,
            BackupOp::Restore(a("a1"), None)
        );
        let cp = BackupId::parse("20261028-130512").ok();
        assert_eq!(
            b("--workspaces /w restore-backup /b a1 20261028-130512").op,
            BackupOp::Restore(a("a1"), cp)
        );
        for (line, want) in [
            ("backup", "missing backup destination"),
            (
                "--keep 0 backup /b",
                "--keep must be a number of at least 1, got 0",
            ),
            ("--keep 2 backups /b", "--keep only applies to backup"),
            (
                "--full-every 2 restore-backup /b a1",
                "--full-every only applies to backup",
            ),
            (
                "--full-every x backup /b",
                "--full-every must be a number of at least 1, got x",
            ),
            ("--image i backup /b", "unknown flag --image"),
            ("backup /b ../x", "id must match"),
            ("restore-backup /b", "missing agent id"),
            ("backups /b a1 a2", "unexpected argument a2"),
            ("backup /b --keep", "--keep needs a value"),
        ] {
            let got = host(line).err().map(|e| e.to_string()).unwrap_or_default();
            assert!(got.contains(want), "{line}: {got}");
        }
        // Not a backup word: still the fleet command line.
        assert!(matches!(host("--image i status a1"), Ok(Command::Fleet(_))));
    }

    #[test]
    fn tune_flags() {
        let t = |s| match host(s) {
            Ok(Command::Tune(t)) => t,
            other => panic!("{s}: {other:?}"),
        };
        assert_eq!(t("tune"), Tune::default());
        let all = t("tune --profile idle --apply --force --restart-gateway");
        assert_eq!(
            all,
            Tune {
                profile: Some(Profile::Idle),
                facts_file: None,
                root: None,
                apply: true,
                force: true,
                restart_gateway: true,
            }
        );
        assert_eq!(t("tune --facts-file f").facts_file, Some("f".into()));
        assert_eq!(t("tune --root /r --apply").root, Some("/r".into()));
        assert!(matches!(
            host("tune --force"),
            Err(ArgError::Needs("--force", "--apply"))
        ));
        assert!(matches!(
            host("tune --restart-gateway"),
            Err(ArgError::Needs("--restart-gateway", "--apply"))
        ));
        assert!(matches!(
            host("tune --apply --facts-file f"),
            Err(ArgError::Conflict(..))
        ));
        assert!(matches!(
            host("tune --apply --restart-gateway --root /r"),
            Err(ArgError::Conflict(..))
        ));
        assert!(matches!(host("tune --json"), Err(ArgError::UnknownFlag(_))));
        assert!(matches!(
            host("tune --explain"),
            Err(ArgError::UnknownFlag(_))
        ));
    }

    #[test]
    fn usage_names_every_flag() {
        for flag in [
            "--profile",
            "--json",
            "--facts-file",
            "--root",
            "--explain",
            "--apply",
            "--force",
            "--restart-gateway",
            "--purge",
            "--network",
            "--uid",
            "--label",
            "--from",
        ] {
            assert!(USAGE.contains(flag), "{flag}");
        }
        // doctor's codes stay clear of bad usage (2) and "data dir in use" (3).
        assert!(USAGE.contains("exit 0 all ok, 1 warnings, 4 errors"));
    }

    #[test]
    fn parses_with_defaults() {
        let cli = p("--image agent:1 create a1").unwrap_or_else(|e| panic!("{e}"));
        assert!(matches!(&cli.job, Job::Agent(Action::Create, id) if id.as_str() == "a1"));
        assert_eq!(
            cli.workspaces,
            PathBuf::from("/var/lib/agent-cloud/workspaces")
        );
        assert_eq!(
            (cli.ctr.as_str(), cli.namespace.as_str()),
            ("ctr", "agents")
        );
    }

    #[test]
    fn uid_and_network_flags() {
        let cli = p("--image i --uid 1234 --network status a1").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((cli.uid, cli.network), (1234, true));
        let cli = p("--image i status a1").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((cli.uid, cli.network), (1000, false));
        assert!(matches!(
            p("--image i --uid x status a1"),
            Err(ArgError::BadUid(_))
        ));
    }

    #[test]
    fn purge_only_with_destroy() {
        let action = |s| match p(s).map(|c| c.job) {
            Ok(Job::Agent(a, _)) => Some(a),
            _ => None,
        };
        assert_eq!(
            action("--image i destroy a1"),
            Some(Action::Destroy { purge: false })
        );
        assert_eq!(
            action("--image i --purge destroy a1"),
            Some(Action::Destroy { purge: true })
        );
        for bad in ["pause a1", "reconcile"] {
            let r = p(&format!("--image i --purge {bad}"));
            assert!(matches!(r, Err(ArgError::PurgeWithout(_))), "{bad}");
        }
    }

    #[test]
    fn reconcile_takes_no_agent_id() {
        let cli = p("--image i --network reconcile").unwrap_or_else(|e| panic!("{e}"));
        assert!(matches!(cli.job, Job::Reconcile) && cli.network);
        assert!(matches!(
            p("--image i reconcile a1"),
            Err(ArgError::Extra(_))
        ));
    }

    #[test]
    fn workspaces_follow_data_unless_given() {
        let cli = p("--data /d --image i status a1").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(cli.workspaces, PathBuf::from("/d/workspaces"));
        let cli =
            p("--data /d --workspaces /w --image i status a1").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(cli.workspaces, PathBuf::from("/w"));
    }

    #[test]
    fn rejects_bad_input() {
        assert!(matches!(p("create a1"), Err(ArgError::Missing("--image"))));
        assert!(matches!(p("--image i"), Err(ArgError::Missing("action"))));
        assert!(matches!(
            p("--image i create"),
            Err(ArgError::Missing("agent id"))
        ));
        assert!(matches!(
            p("--image i fly a1"),
            Err(ArgError::UnknownAction(_))
        ));
        assert!(matches!(
            p("--image i create ../x"),
            Err(ArgError::BadId(_))
        ));
        assert!(matches!(
            p("--image i create a1 b2"),
            Err(ArgError::Extra(_))
        ));
        assert!(matches!(p("--bogus x"), Err(ArgError::UnknownFlag(_))));
        assert!(matches!(p("--image"), Err(ArgError::MissingValue(_))));
    }

    fn snap_of(s: &str) -> Result<Snap, ArgError> {
        match p(s)?.job {
            Job::Snap(op) => Ok(op),
            other => panic!("{s}: {other:?}"),
        }
    }

    #[test]
    fn workspace_operations() {
        let a = |s: &str| AgentId::parse(s).unwrap_or_else(|e| panic!("{e}"));
        let c = |s: &str| CheckpointId::parse(s).unwrap_or_else(|e| panic!("{e}"));
        let ok = |s: &str| snap_of(s).unwrap_or_else(|e| panic!("{s}: {e}"));
        assert_eq!(ok("--image i checkpoints a1"), Snap::List(a("a1")));
        assert_eq!(ok("--image i checkpoint a1"), Snap::Take(a("a1"), None));
        let labelled = parse(
            [
                "--image",
                "i",
                "--label",
                "before refactor",
                "checkpoint",
                "a1",
            ]
            .map(str::to_owned),
        );
        let Ok(Command::Fleet(cli)) = labelled else {
            panic!("{labelled:?}")
        };
        let label = Label::parse("before refactor").ok();
        assert!(matches!(cli.job, Job::Snap(Snap::Take(_, l)) if l == label));
        assert_eq!(
            ok("--image i restore a1 20261028-130512"),
            Snap::Restore(a("a1"), c("20261028-130512"))
        );
        assert_eq!(
            ok("--image i delete-checkpoint a1 x1"),
            Snap::Delete(a("a1"), c("x1"))
        );
        assert_eq!(
            ok("--image i --from x1 fork a1 b1"),
            Snap::Fork {
                from: a("a1"),
                to: a("b1"),
                checkpoint: Some(c("x1"))
            }
        );
        assert_eq!(ok("--image i workspace migrate a1"), Snap::Migrate(a("a1")));
        for (bad, want) in [
            (
                "--image i --label x restore a1 c1",
                "--label only applies to checkpoint",
            ),
            (
                "--image i --from c1 checkpoint a1",
                "--from only applies to fork",
            ),
            (
                "--image i --label x status a1",
                "--label only applies to checkpoint",
            ),
            ("--image i restore a1", "missing checkpoint"),
            ("--image i restore a1 ../x", "id must match"),
            ("--image i fork a1", "missing new agent id"),
            ("--image i workspace", "missing workspace action (migrate)"),
            (
                "--image i workspace grow a1",
                "unknown action workspace grow",
            ),
            ("--image i checkpoint a1 extra", "unexpected argument extra"),
        ] {
            let got = p(bad)
                .map(|c| format!("{:?}", c.job))
                .unwrap_or_else(|e| e.to_string());
            assert!(got.contains(want), "{bad}: {got}");
        }
    }
}
