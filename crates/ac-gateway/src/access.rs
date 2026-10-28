//! Which scope each route needs (ADR-011), checked before any handler runs. A path under `/api/`
//! that matches no entry of [`ROUTES`] is 404 for every token, so a route added to a handler
//! without an entry here cannot be reached at all, let alone unprotected.
//!
//! A token restricted to some agents gets 404 for every other agent, whatever the route, so it
//! cannot tell which ids exist; only then is the scope checked (403).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::mcp::{Arg, Kind, Send, Tool};
use crate::tokens::{Scope, Token};
use crate::{error, Reply};

/// What a route needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    /// Any valid token.
    Token,
    /// A token with this scope.
    Scope(Scope),
}

/// One route: method, path pattern (`{id}` is the agent, `{job}`, `{viewer}` and `{cp}` any
/// segment), what it needs, the action name it is audited under (`None`: not audited) and the
/// MCP tool it is offered as, if any (ADR-017, ADR-020).
#[derive(Debug)]
pub struct Rule {
    /// HTTP method.
    pub method: &'static str,
    /// Path pattern.
    pub path: &'static str,
    /// Required scope.
    pub need: Need,
    /// Audit action name, for state-changing routes.
    pub audit: Option<&'static str>,
    /// The MCP tool that calls this route.
    pub tool: Option<Tool>,
}

const fn rule(
    method: &'static str,
    path: &'static str,
    need: Need,
    audit: Option<&'static str>,
) -> Rule {
    Rule {
        method,
        path,
        need,
        audit,
        tool: None,
    }
}

/// `r`, offered as the MCP tool `t`.
const fn offer(r: Rule, t: Tool) -> Rule {
    Rule { tool: Some(t), ..r }
}

const fn arg(name: &'static str, kind: Kind, required: bool, about: &'static str) -> Arg {
    Arg {
        name,
        kind,
        required,
        about,
    }
}

const AGENT: Arg = arg("agent_id", Kind::Agent, true, "The agent's id.");
const JOB: Arg = arg(
    "job_id",
    Kind::Job,
    true,
    "The job's id, as submit_job returned it.",
);
const FORCE: Arg = arg(
    "force",
    Kind::Bool,
    false,
    "Interrupt the agent's queued or running job first; without it the call is refused while one exists.",
);
const CHECKPOINT: Arg = arg(
    "checkpoint_id",
    Kind::Checkpoint,
    true,
    "The checkpoint's id, as list_checkpoints shows it.",
);

use Need::Scope as S;
use Scope::{AgentsRead, AgentsWrite, EgressWrite, JobsRead, JobsWrite, ViewerControl, ViewerRead};

const A: &str = "/api/v1/agents/{id}";

/// Every route under `/api/`, and so every operation of the REST and MCP interfaces, each
/// declared once (ADR-020). The MCP endpoint needs only a token: each tool call is then checked
/// as the route that offers it (ADR-017); `tools/list` follows this order. Viewer input (keys,
/// text, paste, resize) is not audited per call: it comes from the viewer whose take-over was,
/// and a viewer id works only with the token that opened it. Picking a window changes only what that viewer sees, so it needs `viewer:read`.
/// Attaching egress profiles widens what an agent may reach, so it has its own scope (ADR-016);
/// a create or fork that attaches some needs it too, which the handler checks. A fork's new agent
/// is in its body, so the handler checks the token is for it too (ADR-018).
pub const ROUTES: &[Rule] = &[
    offer(rule("GET", "/api/v1/whoami", Need::Token, None), Tool {
        name: "whoami",
        about: "The calling token's name, scopes and agents.",
        send: Send::Query,
        args: &[],
    }),
    rule("POST", "/api/v1/mcp", Need::Token, None),
    offer(rule("GET", "/api/v1/agents", S(AgentsRead), None), Tool {
        name: "list_agents",
        about: "The agents this token is for, with their state (running, paused, destroyed).",
        send: Send::Query,
        args: &[],
    }),
    offer(rule("GET", "/api/v1/capacity", S(AgentsRead), None), Tool {
        name: "get_capacity",
        about: "Admission figures: /dev/shm, guest size, how many more agents fit, job slots.",
        send: Send::Query,
        args: &[],
    }),
    offer(rule("GET", A, S(AgentsRead), None), Tool {
        name: "get_agent",
        about: "One agent's state and health.",
        send: Send::Query,
        args: &[AGENT],
    }),
    offer(rule("POST", A, S(AgentsWrite), Some("create")), Tool {
        name: "create_agent",
        about: "Create and boot an agent (a microVM with its own workspace).",
        send: Send::Body,
        args: &[
            AGENT,
            arg(
                "egress_profiles",
                Kind::Names,
                false,
                "Egress profiles to attach before boot; needs scope egress:write.",
            ),
        ],
    }),
    offer(rule("DELETE", A, S(AgentsWrite), Some("destroy")), Tool {
        name: "destroy_agent",
        about: "Destroy an agent's VM; its workspace is kept unless purge is true.",
        send: Send::Query,
        args: &[
            AGENT,
            arg(
                "purge",
                Kind::Bool,
                false,
                "Also remove the workspace and job records (also for an agent already destroyed).",
            ),
            FORCE,
        ],
    }),
    offer(rule(
        "POST",
        "/api/v1/agents/{id}/pause",
        S(AgentsWrite),
        Some("pause"),
    ), Tool {
        name: "pause_agent",
        about: "Stop an agent's VM, keeping its workspace.",
        send: Send::Query,
        args: &[AGENT, FORCE],
    }),
    offer(rule(
        "POST",
        "/api/v1/agents/{id}/resume",
        S(AgentsWrite),
        Some("resume"),
    ), Tool {
        name: "resume_agent",
        about: "Boot a paused agent again.",
        send: Send::Query,
        args: &[AGENT],
    }),
    offer(rule(
        "GET",
        "/api/v1/agents/{id}/checkpoints",
        S(AgentsRead),
        None,
    ), Tool {
        name: "list_checkpoints",
        about: "An agent's workspace checkpoints, oldest first, and how many it may keep.",
        send: Send::Query,
        args: &[AGENT],
    }),
    offer(rule(
        "POST",
        "/api/v1/agents/{id}/checkpoints",
        S(AgentsWrite),
        Some("checkpoint"),
    ), Tool {
        name: "create_checkpoint",
        about: "Checkpoint a running or paused agent's workspace; refused when the agent has as many as it may keep.",
        send: Send::Body,
        args: &[
            AGENT,
            arg("label", Kind::Text, false, "A note to keep with it, 1-80 characters."),
        ],
    }),
    offer(rule(
        "DELETE",
        "/api/v1/agents/{id}/checkpoints/{cp}",
        S(AgentsWrite),
        Some("checkpoint-delete"),
    ), Tool {
        name: "delete_checkpoint",
        about: "Delete one of an agent's checkpoints.",
        send: Send::Query,
        args: &[AGENT, CHECKPOINT],
    }),
    offer(rule(
        "POST",
        "/api/v1/agents/{id}/restore",
        S(AgentsWrite),
        Some("restore"),
    ), Tool {
        name: "restore_checkpoint",
        about: "Put a checkpoint back as a paused agent's workspace; what it replaces is kept as a new checkpoint.",
        send: Send::Body,
        args: &[
            AGENT,
            arg("checkpoint", Kind::Text, true, "The checkpoint's id, as list_checkpoints shows it."),
        ],
    }),
    offer(rule(
        "POST",
        "/api/v1/agents/{id}/fork",
        S(AgentsWrite),
        Some("fork"),
    ), Tool {
        name: "fork_agent",
        about: "Create a new agent on a copy of this agent's workspace, now or at a checkpoint. The token needs both agents.",
        send: Send::Body,
        args: &[
            AGENT,
            arg("id", Kind::Text, true, "The new agent's id."),
            arg("checkpoint", Kind::Text, false, "The checkpoint to fork from; without it, the workspace as it is now."),
            arg(
                "egress_profiles",
                Kind::Names,
                false,
                "Egress profiles to attach to the new agent; needs egress:write.",
            ),
        ],
    }),
    offer(rule("GET", "/api/v1/egress/profiles", S(AgentsRead), None), Tool {
        name: "list_egress_profiles",
        about: "The operator's egress profiles and how many hosts each allows.",
        send: Send::Query,
        args: &[],
    }),
    offer(rule("GET", "/api/v1/agents/{id}/egress", S(AgentsRead), None), Tool {
        name: "get_agent_egress",
        about: "An agent's egress profiles and how many hosts it may reach.",
        send: Send::Query,
        args: &[AGENT],
    }),
    offer(rule(
        "PUT",
        "/api/v1/agents/{id}/egress",
        S(EgressWrite),
        Some("egress"),
    ), Tool {
        name: "set_agent_egress",
        about: "Replace an agent's egress profiles; the proxy applies it to the next request.",
        send: Send::Body,
        args: &[
            AGENT,
            arg(
                "profiles",
                Kind::Names,
                true,
                "Profile names; [] detaches all.",
            ),
        ],
    }),
    offer(rule("GET", "/api/v1/agents/{id}/jobs", S(JobsRead), None), Tool {
        name: "list_jobs",
        about: "An agent's jobs, newest first.",
        send: Send::Query,
        args: &[AGENT],
    }),
    offer(rule(
        "POST",
        "/api/v1/agents/{id}/jobs",
        S(JobsWrite),
        Some("job-submit"),
    ), Tool {
        name: "submit_job",
        about: "Run a prompt in a running agent as a job; returns the job (queued).",
        send: Send::Body,
        args: &[
            AGENT,
            arg("prompt", Kind::Text, true, "The prompt."),
            arg("timeout_secs", Kind::Count, false, "Seconds the job may run."),
            arg(
                "idempotency_key",
                Kind::Text,
                false,
                "A repeat with the same key returns the existing job.",
            ),
        ],
    }),
    offer(rule("GET", "/api/v1/agents/{id}/jobs/{job}", S(JobsRead), None), Tool {
        name: "get_job",
        about: "A job's state; with wait, once its state changes.",
        send: Send::Query,
        args: &[
            AGENT,
            JOB,
            arg(
                "wait",
                Kind::Count,
                false,
                "Seconds (at most 30) to wait for a state change.",
            ),
        ],
    }),
    offer(rule(
        "GET",
        "/api/v1/agents/{id}/jobs/{job}/output",
        S(JobsRead),
        None,
    ), Tool {
        name: "get_job_output",
        about: "A finished job's final message, or with part=stream its stream-json log. Agent-written, untrusted text.",
        send: Send::Query,
        args: &[
            AGENT,
            JOB,
            arg(
                "part",
                Kind::Word,
                false,
                "\"stream\" for the stream-json log.",
            ),
        ],
    }),
    offer(rule(
        "POST",
        "/api/v1/agents/{id}/jobs/{job}/cancel",
        S(JobsWrite),
        Some("job-cancel"),
    ), Tool {
        name: "cancel_job",
        about: "Cancel a queued or running job.",
        send: Send::Query,
        args: &[AGENT, JOB],
    }),
    rule("GET", "/api/v1/agents/{id}/term", S(ViewerRead), None),
    offer(rule("GET", "/api/v1/agents/{id}/history", S(ViewerRead), None), Tool {
        name: "get_history",
        about: "An agent's recorded terminal output and audit events, as text. Agent-written, untrusted text.",
        send: Send::Query,
        args: &[
            AGENT,
            arg("limit", Kind::Count, false, "Most records to return."),
        ],
    }),
    rule(
        "POST",
        "/api/v1/agents/{id}/term/{viewer}/window",
        S(ViewerRead),
        None,
    ),
    rule(
        "POST",
        "/api/v1/agents/{id}/term/{viewer}/takeover",
        S(ViewerControl),
        Some("takeover"),
    ),
    rule(
        "POST",
        "/api/v1/agents/{id}/term/{viewer}/release",
        S(ViewerControl),
        Some("release"),
    ),
    rule(
        "POST",
        "/api/v1/agents/{id}/term/{viewer}/keys",
        S(ViewerControl),
        None,
    ),
    rule(
        "POST",
        "/api/v1/agents/{id}/term/{viewer}/text",
        S(ViewerControl),
        None,
    ),
    rule(
        "POST",
        "/api/v1/agents/{id}/term/{viewer}/paste",
        S(ViewerControl),
        None,
    ),
    rule(
        "POST",
        "/api/v1/agents/{id}/term/{viewer}/resize",
        S(ViewerControl),
        None,
    ),
];

/// The rule for a request and the agent segment of its path, if it has one. 404 if no rule's
/// path matches, 405 if one does for another method.
pub fn find<'a>(method: &str, path: &'a str) -> Result<(&'static Rule, Option<&'a str>), Reply> {
    let mut other_method = false;
    for r in ROUTES {
        if let Some(agent) = matches(r.path, path) {
            if r.method == method {
                return Ok((r, agent));
            }
            other_method = true;
        }
    }
    Err(if other_method {
        error(405, "method not allowed")
    } else {
        error(404, "not found")
    })
}

/// `Some(agent segment)` if `path` fits `pattern`; placeholders match one non-empty segment.
fn matches<'a>(pattern: &str, path: &'a str) -> Option<Option<&'a str>> {
    let (mut want, mut got) = (pattern.split('/'), path.split('/'));
    let mut agent = None;
    loop {
        match (want.next(), got.next()) {
            (None, None) => return Some(agent),
            (Some(w), Some(g)) if w.starts_with('{') && !g.is_empty() => {
                if w == "{id}" {
                    agent = Some(g);
                }
            }
            (Some(w), Some(g)) if w == g => {}
            _ => return None,
        }
    }
}

/// Whether `token` may use `rule` on `agent`: 404 for an agent it is not for, 403 without the
/// scope.
pub fn check(token: &Token, rule: &Rule, agent: Option<&str>) -> Result<(), Reply> {
    if agent.is_some_and(|a| !token.agents.contains(a)) {
        return Err(error(404, "not found"));
    }
    match rule.need {
        Need::Scope(s) if !token.scopes.allows(s) => Err(error(
            403,
            &format!("token {} lacks scope {}", token.name, s.name()),
        )),
        _ => Ok(()),
    }
}

/// Failed authentications per source, counted in fixed windows; at most `max_sources` tracked
/// (expired windows go first, then the oldest).
pub struct Failures {
    window: Duration,
    limit: u32,
    max_sources: usize,
    seen: Mutex<HashMap<String, (Instant, u32)>>,
}

/// What to do about one more failure.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Under the limit: answer 401 and log it.
    Log,
    /// The limit was just reached: answer 429 and log that further failures are not logged.
    Limit,
    /// Over the limit: answer 429, log nothing.
    Quiet,
}

impl Failures {
    /// At most `limit` logged failures per source per `window`.
    pub fn new(window: Duration, limit: u32, max_sources: usize) -> Self {
        Self {
            window,
            limit,
            max_sources,
            seen: Mutex::new(HashMap::new()),
        }
    }

    /// Count a failure from `source` at `now`.
    pub fn fail(&self, source: &str, now: Instant) -> Verdict {
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        if !seen.contains_key(source) && seen.len() >= self.max_sources {
            seen.retain(|_, (start, _)| now.duration_since(*start) < self.window);
            let oldest = seen
                .iter()
                .min_by_key(|(_, (t, _))| *t)
                .map(|(k, _)| k.clone());
            if let (true, Some(k)) = (seen.len() >= self.max_sources, oldest) {
                seen.remove(&k);
            }
        }
        let entry = seen.entry(source.to_owned()).or_insert((now, 0));
        if now.duration_since(entry.0) >= self.window {
            *entry = (now, 0);
        }
        entry.1 = entry.1.saturating_add(1);
        match entry.1.cmp(&self.limit) {
            std::cmp::Ordering::Less => Verdict::Log,
            std::cmp::Ordering::Equal => Verdict::Limit,
            std::cmp::Ordering::Greater => Verdict::Quiet,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::Tokens;

    const DIGEST: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"; // "test"

    fn token(scopes: &str, agents: &str) -> Token {
        let t = Tokens::parse(&format!("t {scopes} {agents} {DIGEST}"))
            .unwrap_or_else(|e| panic!("{e}"));
        let first = t.iter().next().cloned();
        first.unwrap_or_else(|| panic!("none"))
    }

    fn need(method: &str, path: &str) -> Option<Need> {
        find(method, path).ok().map(|(r, _)| r.need)
    }

    /// The table in ADR-011, route by route.
    #[test]
    fn route_to_scope_table() {
        let job = "/api/v1/agents/a1/jobs/0123456789abcdef";
        let term = "/api/v1/agents/a1/term/0011223344556677";
        for (method, path, want) in [
            ("GET", "/api/v1/whoami", Need::Token),
            ("POST", "/api/v1/mcp", Need::Token),
            ("GET", "/api/v1/agents", S(AgentsRead)),
            ("GET", "/api/v1/capacity", S(AgentsRead)),
            ("GET", "/api/v1/agents/a1", S(AgentsRead)),
            ("POST", "/api/v1/agents/a1", S(AgentsWrite)),
            ("DELETE", "/api/v1/agents/a1", S(AgentsWrite)),
            ("POST", "/api/v1/agents/a1/pause", S(AgentsWrite)),
            ("POST", "/api/v1/agents/a1/resume", S(AgentsWrite)),
            ("GET", "/api/v1/agents/a1/checkpoints", S(AgentsRead)),
            ("POST", "/api/v1/agents/a1/checkpoints", S(AgentsWrite)),
            ("DELETE", "/api/v1/agents/a1/checkpoints/x", S(AgentsWrite)),
            ("POST", "/api/v1/agents/a1/restore", S(AgentsWrite)),
            ("POST", "/api/v1/agents/a1/fork", S(AgentsWrite)),
            ("GET", "/api/v1/egress/profiles", S(AgentsRead)),
            ("GET", "/api/v1/agents/a1/egress", S(AgentsRead)),
            ("PUT", "/api/v1/agents/a1/egress", S(EgressWrite)),
            ("GET", "/api/v1/agents/a1/jobs", S(JobsRead)),
            ("POST", "/api/v1/agents/a1/jobs", S(JobsWrite)),
            ("GET", job, S(JobsRead)),
            ("GET", &format!("{job}/output"), S(JobsRead)),
            ("POST", &format!("{job}/cancel"), S(JobsWrite)),
            ("GET", "/api/v1/agents/a1/term", S(ViewerRead)),
            ("GET", "/api/v1/agents/a1/history", S(ViewerRead)),
            ("POST", &format!("{term}/window"), S(ViewerRead)),
            ("POST", &format!("{term}/takeover"), S(ViewerControl)),
            ("POST", &format!("{term}/release"), S(ViewerControl)),
            ("POST", &format!("{term}/keys"), S(ViewerControl)),
            ("POST", &format!("{term}/text"), S(ViewerControl)),
            ("POST", &format!("{term}/paste"), S(ViewerControl)),
            ("POST", &format!("{term}/resize"), S(ViewerControl)),
        ] {
            assert_eq!(need(method, path), Some(want), "{method} {path}");
        }
        assert_eq!(
            ROUTES.len(),
            31,
            "a new route needs a row above and in ADR-011"
        );
    }

    #[test]
    fn unknown_paths_and_methods() {
        for (method, path, status) in [
            ("GET", "/api/v1/agents/a1/explode", 404),
            ("GET", "/api/v1/agents/", 404),
            ("GET", "/api/v1/agents//pause", 404),
            ("GET", "/api/v1/agents/a1/term/v1/keys/x", 404),
            ("POST", "/api/v1/agents/a1/term/v1/explode", 404),
            ("GET", "/api/v2/agents", 404),
            ("GET", "/api/v1/agents/a1/pause", 405),
            ("PUT", "/api/v1/agents/a1", 405),
            ("POST", "/api/v1/agents/a1/egress", 405),
            ("PUT", "/api/v1/egress/profiles", 405),
            ("GET", "/api/v1/egress", 404),
            ("POST", "/api/v1/agents", 405),
            ("GET", "/api/v1/agents/a1/term/v1/keys", 405),
            ("GET", "/api/v1/mcp", 405),
        ] {
            assert_eq!(
                find(method, path).err().map(|r| r.status),
                Some(status),
                "{method} {path}"
            );
        }
        assert_eq!(
            find("GET", "/api/v1/agents/..%2Fx/jobs").map(|(_, a)| a),
            Ok(Some("..%2Fx")),
            "the handler rejects a bad id with 400"
        );
    }

    #[test]
    fn agent_restriction_comes_before_scope() {
        let t = token("jobs:read,jobs:write", "a1");
        let (pause, _) =
            find("POST", "/api/v1/agents/a1/pause").unwrap_or_else(|r| panic!("{r:?}"));
        assert_eq!(
            check(&t, pause, Some("a1")).err().map(|r| r.status),
            Some(403)
        );
        assert_eq!(
            check(&t, pause, Some("a2")).err().map(|r| r.status),
            Some(404)
        );
        let (egress, _) =
            find("PUT", "/api/v1/agents/a1/egress").unwrap_or_else(|r| panic!("{r:?}"));
        let writer = token("agents:read,agents:write", "*");
        assert_eq!(
            check(&writer, egress, Some("a1")).err().map(|r| r.status),
            Some(403)
        );
        assert!(check(&token("egress:write", "a1"), egress, Some("a1")).is_ok());
        assert_eq!(
            check(&token("egress:write", "a1"), egress, Some("a2"))
                .err()
                .map(|r| r.status),
            Some(404)
        );
        let (jobs, _) = find("POST", "/api/v1/agents/a2/jobs").unwrap_or_else(|r| panic!("{r:?}"));
        assert!(check(&t, jobs, Some("a1")).is_ok());
        for other in ["a2", "zz", "..%2Fx", "A1", "a1 "] {
            assert_eq!(
                check(&t, jobs, Some(other)).err().map(|r| r.status),
                Some(404),
                "{other}"
            );
        }
        let (list, _) = find("GET", "/api/v1/agents").unwrap_or_else(|r| panic!("{r:?}"));
        assert_eq!(check(&t, list, None).err().map(|r| r.status), Some(403));
        let (who, _) = find("GET", "/api/v1/whoami").unwrap_or_else(|r| panic!("{r:?}"));
        assert!(check(&t, who, None).is_ok());
        let admin = token("admin", "*");
        assert!(ROUTES.iter().all(|r| check(&admin, r, Some("any")).is_ok()));
    }

    #[test]
    fn failures_are_limited_per_source_in_bounded_memory() {
        let f = Failures::new(Duration::from_secs(60), 3, 2);
        let t0 = Instant::now();
        assert_eq!(f.fail("x", t0), Verdict::Log);
        assert_eq!(f.fail("x", t0), Verdict::Log);
        assert_eq!(f.fail("x", t0), Verdict::Limit);
        assert_eq!(f.fail("x", t0), Verdict::Quiet);
        assert_eq!(f.fail("y", t0), Verdict::Log, "another source");
        assert_eq!(
            f.fail("x", t0 + Duration::from_secs(61)),
            Verdict::Log,
            "new window"
        );
        for i in 0..1000 {
            let _ = f.fail(&format!("s{i}"), t0);
        }
        assert!(f.seen.lock().is_ok_and(|s| s.len() <= 2));
    }
}
