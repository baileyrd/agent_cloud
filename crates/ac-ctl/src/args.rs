//! Hand-rolled argument parsing (six flags do not justify a dependency).

use std::fmt;
use std::path::PathBuf;

use ac_domain::{AgentId, IdError};

/// Usage text.
pub const USAGE: &str = "usage: ac-ctl --image IMAGE [--data DIR] [--workspaces DIR] [--ctr PATH] \
[--namespace NS] [--uid N] [--network] <create|pause|resume|destroy|status> <agent-id>";

/// What to do to the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Provision and start.
    Create,
    /// Stop the VM, keep the volume.
    Pause,
    /// Start a paused VM.
    Resume,
    /// Tear down.
    Destroy,
    /// Print the lifecycle state.
    Status,
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
    /// Requested action.
    pub action: Action,
    /// Target agent.
    pub id: AgentId,
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
        }
    }
}

impl std::error::Error for ArgError {}

fn action(word: &str) -> Result<Action, ArgError> {
    Ok(match word {
        "create" => Action::Create,
        "pause" => Action::Pause,
        "resume" => Action::Resume,
        "destroy" => Action::Destroy,
        "status" => Action::Status,
        other => return Err(ArgError::UnknownAction(other.to_owned())),
    })
}

/// Parse `args` (without the program name).
pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Cli, ArgError> {
    let (mut data, mut workspaces) = (PathBuf::from("/var/lib/agent-cloud"), None::<PathBuf>);
    let (mut ctr, mut namespace, mut image) = ("ctr".to_owned(), "agents".to_owned(), None);
    let (mut uid, mut network) = (1000u32, false);
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
            _ => return Err(ArgError::UnknownFlag(arg)),
        }
    }
    let mut pos = positional.into_iter();
    let action = action(&pos.next().ok_or(ArgError::Missing("action"))?)?;
    let id = AgentId::parse(&pos.next().ok_or(ArgError::Missing("agent id"))?)
        .map_err(ArgError::BadId)?;
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
        action,
        id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<Cli, ArgError> {
        parse(s.split_whitespace().map(str::to_owned))
    }

    #[test]
    fn parses_with_defaults() {
        let cli = p("--image agent:1 create a1").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(cli.action, Action::Create);
        assert_eq!(cli.id.as_str(), "a1");
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
