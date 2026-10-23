//! Hand-rolled argument parsing (a dozen flags do not justify a dependency).

use std::fmt;
use std::path::PathBuf;

use ac_domain::tune::Profile;
use ac_domain::{AgentId, IdError};

/// Usage text.
pub const USAGE: &str = "usage: ac-ctl --image IMAGE [--data DIR] [--workspaces DIR] [--ctr PATH] \
[--namespace NS] [--uid N] [--network] [--purge] <create|pause|resume|destroy|status> <agent-id>
       ac-ctl --image IMAGE [...] [--network] reconcile
       ac-ctl doctor [--profile idle|build|heavy] [--json] [--facts-file F] [--root DIR] [--explain]
       ac-ctl tune [--profile idle|build|heavy] [--facts-file F] [--root DIR] [--apply [--force] \
[--restart-gateway]]
  destroy keeps the agent's workspace; destroy --purge removes it (also after a plain destroy).
  reconcile marks agents recorded running but with no VM (after a reboot) paused, for resume.
  doctor compares this host's settings with what its facts recommend for the profile (ADR-014):
    read-only; exit 0 all ok, 1 warnings, 2 errors. --facts-file plans a host from a JSON file of
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
        _ => fleet(args).map(Command::Fleet),
    }
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
            _ => return Err(ArgError::UnknownFlag(arg)),
        }
    }
    let mut pos = positional.into_iter();
    let word = pos.next().ok_or(ArgError::Missing("action"))?;
    let job = if word == "reconcile" {
        Job::Reconcile
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
        ] {
            assert!(USAGE.contains(flag), "{flag}");
        }
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
}
