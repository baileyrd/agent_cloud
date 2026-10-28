//! Which scope each route needs (ADR-011), checked before any handler runs. A path under `/api/`
//! that matches no entry of [`ROUTES`] is 404 for every token, so a route added to a handler
//! without an entry here cannot be reached at all, let alone unprotected.
//!
//! A token restricted to some agents gets 404 for every other agent, whatever the route, so it
//! cannot tell which ids exist; only then is the scope checked (403).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

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

/// One route: method, path pattern (`{id}` is the agent, `{job}` and `{viewer}` any segment),
/// what it needs, and the action name it is audited under (`None`: not audited).
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
    }
}

use Need::Scope as S;
use Scope::{AgentsRead, AgentsWrite, EgressWrite, JobsRead, JobsWrite, ViewerControl, ViewerRead};

const A: &str = "/api/v1/agents/{id}";

/// Every route under `/api/`. The MCP endpoint needs only a token: each tool call is then checked
/// as the route it names (ADR-017). Viewer input (keys, text, paste, resize) is not audited per call:
/// it comes from the viewer whose take-over was, and a viewer id works only with the token that
/// opened it. Picking a window changes only what that viewer sees, so it needs `viewer:read`.
/// Attaching egress profiles widens what an agent may reach, so it has its own scope (ADR-016);
/// a create that attaches some needs it too, which the handler checks.
pub const ROUTES: &[Rule] = &[
    rule("GET", "/api/v1/whoami", Need::Token, None),
    rule("POST", "/api/v1/mcp", Need::Token, None),
    rule("GET", "/api/v1/agents", S(AgentsRead), None),
    rule("GET", "/api/v1/capacity", S(AgentsRead), None),
    rule("GET", A, S(AgentsRead), None),
    rule("POST", A, S(AgentsWrite), Some("create")),
    rule("DELETE", A, S(AgentsWrite), Some("destroy")),
    rule(
        "POST",
        "/api/v1/agents/{id}/pause",
        S(AgentsWrite),
        Some("pause"),
    ),
    rule(
        "POST",
        "/api/v1/agents/{id}/resume",
        S(AgentsWrite),
        Some("resume"),
    ),
    rule("GET", "/api/v1/egress/profiles", S(AgentsRead), None),
    rule("GET", "/api/v1/agents/{id}/egress", S(AgentsRead), None),
    rule(
        "PUT",
        "/api/v1/agents/{id}/egress",
        S(EgressWrite),
        Some("egress"),
    ),
    rule("GET", "/api/v1/agents/{id}/jobs", S(JobsRead), None),
    rule(
        "POST",
        "/api/v1/agents/{id}/jobs",
        S(JobsWrite),
        Some("job-submit"),
    ),
    rule("GET", "/api/v1/agents/{id}/jobs/{job}", S(JobsRead), None),
    rule(
        "GET",
        "/api/v1/agents/{id}/jobs/{job}/output",
        S(JobsRead),
        None,
    ),
    rule(
        "POST",
        "/api/v1/agents/{id}/jobs/{job}/cancel",
        S(JobsWrite),
        Some("job-cancel"),
    ),
    rule("GET", "/api/v1/agents/{id}/term", S(ViewerRead), None),
    rule("GET", "/api/v1/agents/{id}/history", S(ViewerRead), None),
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
            26,
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
