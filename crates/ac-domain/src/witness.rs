//! A job's witness record (ADR-029): what the host saw of a job, built only from data the
//! agent cannot write (the job's registry record, its changed paths from btrfs snapshots, and
//! the egress proxy's decisions for its agent during the job), in one canonical text form so
//! a digest over it can be checked by anyone holding the same record. Pure: facts in, text out.

use std::collections::BTreeMap;

use crate::{AgentId, Job};

/// The record's first line; a new layout takes a new version.
pub const HEADER: &str = "agent_cloud.witness v1";

/// What the host knows of the paths a job changed (ADR-019).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Changes {
    /// Listed: sorted, unique, escaped as btrfs prints them (no whitespace or control bytes).
    Listed(Vec<String>),
    /// More than the listing cap.
    TooMany,
    /// The compare failed.
    Failed,
    /// Not compared yet (or the job ended before this record was kept).
    Pending,
    /// Never compared: the job did not start, or change tracking is off.
    None,
}

/// Where the egress lines came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EgressSource {
    /// The proxy's journal for the job's window.
    Journal(Vec<Decision>),
    /// The journal could not be read; no egress facts.
    Unavailable,
}

/// The proxy allowed or refused a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verdict {
    /// Allowed.
    Allow,
    /// Refused.
    Deny,
}

impl Verdict {
    /// Its word in the proxy log and the record.
    pub fn name(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }
}

/// How often the proxy gave one verdict for one target.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Decision {
    /// Allowed or refused.
    pub verdict: Verdict,
    /// `host:port`, as the proxy logged it.
    pub target: String,
    /// How many times.
    pub count: u64,
}

/// Count the proxy's `allow` and `deny` lines for `agent`, by verdict and target, sorted.
/// Lines for other agents, unattributed lines (`agent=-`), `error` and `refused` lines, and
/// anything else are skipped. A target holding whitespace cannot be logged as one word; such
/// a request is refused by the proxy, so at worst a `deny` is counted under a truncated name.
pub fn decisions(log: &str, agent: &AgentId) -> Vec<Decision> {
    let who = format!("agent={agent}");
    let mut counts: BTreeMap<(Verdict, String), u64> = BTreeMap::new();
    for line in log.lines() {
        let Some(rest) = line.trim_start().strip_prefix("ac-egress: ") else {
            continue;
        };
        let words: Vec<&str> = rest.split(' ').collect();
        let [verdict, _src, by, target, ..] = words[..] else {
            continue;
        };
        let verdict = match verdict {
            "allow" => Verdict::Allow,
            "deny" => Verdict::Deny,
            _ => continue,
        };
        if by != who {
            continue;
        }
        let target = match verdict {
            Verdict::Deny => target.strip_suffix(':').unwrap_or(target),
            Verdict::Allow => target,
        };
        *counts.entry((verdict, target.to_owned())).or_default() += 1;
    }
    counts
        .into_iter()
        .map(|((verdict, target), count)| Decision {
            verdict,
            target,
            count,
        })
        .collect()
}

/// The canonical record: one `key value` fact per line, `\n`-terminated, in a fixed order,
/// lists sorted. A missing value is `-`. Nothing the agent wrote (prompt, output, reason) is in
/// it. The job's `reason` is left out since it may quote a command's error text.
pub fn render(job: &Job, changes: &Changes, egress: &EgressSource) -> String {
    let opt = |v: Option<String>| v.unwrap_or_else(|| "-".to_owned());
    let mut out = format!(
        "{HEADER}\nagent {}\njob {}\nstate {}\nexit_code {}\ncreated_ms {}\nstarted_ms {}\nfinished_ms {}\n",
        job.agent,
        job.id,
        job.state.name(),
        opt(job.exit_code.map(|c| c.to_string())),
        job.created_ms,
        opt(job.started_ms.map(|t| t.to_string())),
        opt(job.finished_ms.map(|t| t.to_string())),
    );
    match changes {
        Changes::Listed(paths) => {
            let mut paths = paths.clone();
            paths.sort();
            paths.dedup();
            out += &format!("changes listed {}\n", paths.len());
            for p in paths {
                out += &format!("changed {p}\n");
            }
        }
        Changes::TooMany => out += "changes too_many\n",
        Changes::Failed => out += "changes failed\n",
        Changes::Pending => out += "changes pending\n",
        Changes::None => out += "changes none\n",
    }
    match egress {
        EgressSource::Journal(all) => {
            let mut all = all.clone();
            all.sort();
            out += &format!("egress journal {}\n", all.len());
            for d in all {
                out += &format!("egress_{} {} {}\n", d.verdict.name(), d.target, d.count);
            }
        }
        EgressSource::Unavailable => out += "egress unavailable\n",
    }
    out
}

/// The stored form of a job's changes, kept by the gateway in the job's host directory.
pub fn render_changes(changes: &Changes) -> String {
    match changes {
        Changes::Listed(paths) => paths
            .iter()
            .fold("listed\n".to_owned(), |s, p| s + p + "\n"),
        Changes::TooMany => "too_many\n".to_owned(),
        Changes::Failed => "failed\n".to_owned(),
        Changes::Pending => "pending\n".to_owned(),
        Changes::None => "none\n".to_owned(),
    }
}

/// Read back [`render_changes`]; anything else is `Err` with what was wrong.
pub fn parse_changes(text: &str) -> Result<Changes, String> {
    let mut lines = text.lines();
    let changes = match lines.next() {
        Some("listed") => return Ok(Changes::Listed(lines.map(str::to_owned).collect())),
        Some("too_many") => Changes::TooMany,
        Some("failed") => Changes::Failed,
        Some("pending") => Changes::Pending,
        Some("none") => Changes::None,
        other => return Err(format!("unknown changes kind {other:?}")),
    };
    match lines.next() {
        None => Ok(changes),
        Some(_) => Err("trailing lines after the changes kind".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{JobId, JobState};

    fn job() -> Job {
        let agent = AgentId::parse("a1").unwrap_or_else(|e| panic!("{e}"));
        let id = JobId::parse("0123456789abcdef").unwrap_or_else(|e| panic!("{e}"));
        let mut j = Job::new(agent, id, 1_000, 60, Some("key".into()));
        j.state = JobState::Succeeded;
        j.started_ms = Some(2_000);
        j.finished_ms = Some(9_000);
        j.exit_code = Some(0);
        j.reason = Some("agent text".into());
        j
    }

    fn egress() -> EgressSource {
        EgressSource::Journal(vec![
            Decision {
                verdict: Verdict::Deny,
                target: "example.com:443".into(),
                count: 1,
            },
            Decision {
                verdict: Verdict::Allow,
                target: "api.anthropic.com:443".into(),
                count: 3,
            },
        ])
    }

    #[test]
    fn the_record_has_a_fixed_layout() {
        let changes = Changes::Listed(vec!["src/b.rs".into(), "a.txt".into()]);
        assert_eq!(
            render(&job(), &changes, &egress()),
            "agent_cloud.witness v1\nagent a1\njob 0123456789abcdef\nstate succeeded\n\
             exit_code 0\ncreated_ms 1000\nstarted_ms 2000\nfinished_ms 9000\n\
             changes listed 2\nchanged a.txt\nchanged src/b.rs\n\
             egress journal 2\negress_allow api.anthropic.com:443 3\negress_deny example.com:443 1\n"
        );
    }

    #[test]
    fn the_same_facts_in_any_order_render_the_same() {
        let a = Changes::Listed(vec!["b".into(), "a".into(), "a".into()]);
        let b = Changes::Listed(vec!["a".into(), "b".into()]);
        let EgressSource::Journal(mut d) = egress() else {
            unreachable!()
        };
        d.reverse();
        assert_eq!(
            render(&job(), &a, &egress()),
            render(&job(), &b, &EgressSource::Journal(d))
        );
    }

    #[test]
    fn any_changed_fact_changes_the_record() {
        let base = render(&job(), &Changes::TooMany, &egress());
        let edits: [fn(&mut Job); 5] = [
            |j| j.state = JobState::Failed,
            |j| j.exit_code = Some(1),
            |j| j.started_ms = None,
            |j| j.finished_ms = Some(9_001),
            |j| j.created_ms = 1_001,
        ];
        for edit in edits {
            let mut j = job();
            edit(&mut j);
            assert_ne!(render(&j, &Changes::TooMany, &egress()), base);
        }
        assert_ne!(render(&job(), &Changes::Failed, &egress()), base);
        assert_ne!(
            render(&job(), &Changes::TooMany, &EgressSource::Unavailable),
            base
        );
    }

    #[test]
    fn agent_written_text_is_not_in_the_record() {
        let mut j = job();
        let base = render(&j, &Changes::None, &EgressSource::Unavailable);
        j.reason = Some("something else".into());
        j.key = Some("other".into());
        let again = render(&j, &Changes::None, &EgressSource::Unavailable);
        assert_eq!(base, again);
        assert!(!base.contains("agent text"));
        assert!(base.ends_with("exit_code 0\ncreated_ms 1000\nstarted_ms 2000\nfinished_ms 9000\nchanges none\negress unavailable\n"));
    }

    #[test]
    fn decisions_count_only_this_agents_allow_and_deny_lines() {
        let log = "ac-egress: allow 10.89.0.21 agent=a1 api.anthropic.com:443\n\
                   ac-egress: allow 10.89.0.21 agent=a1 api.anthropic.com:443\n\
                   ac-egress: deny 10.89.0.21 agent=a1 example.com:443: not on allowlist\n\
                   ac-egress: allow 10.89.0.22 agent=a12 crates.io:443\n\
                   ac-egress: deny 10.89.0.21 agent=- example.com:443: no credential\n\
                   ac-egress: error 10.89.0.21 agent=a1 api.anthropic.com:443: connect failed\n\
                   ac-egress: refused 10.89.0.21: bad request\n\
                   something else\n";
        let a1 = AgentId::parse("a1").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            decisions(log, &a1),
            vec![
                Decision {
                    verdict: Verdict::Allow,
                    target: "api.anthropic.com:443".into(),
                    count: 2
                },
                Decision {
                    verdict: Verdict::Deny,
                    target: "example.com:443".into(),
                    count: 1
                },
            ]
        );
    }

    #[test]
    fn stored_changes_round_trip_and_junk_is_refused() {
        for c in [
            Changes::Listed(vec!["a".into(), "b\\ c".into()]),
            Changes::Listed(Vec::new()),
            Changes::TooMany,
            Changes::Failed,
            Changes::Pending,
            Changes::None,
        ] {
            assert_eq!(parse_changes(&render_changes(&c)), Ok(c));
        }
        assert!(parse_changes("").is_err());
        assert!(parse_changes("maybe\n").is_err());
        assert!(parse_changes("failed\nx\n").is_err());
    }
}
