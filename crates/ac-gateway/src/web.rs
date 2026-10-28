//! The whole HTTP surface, in order: the Origin check (every request), the viewer page and its
//! script and style (no data in them, so no token: a browser cannot send a header when it
//! navigates), then under `/api/`: the token, the route's scope and agents ([`access`]), and the
//! viewer, job, lifecycle or MCP handler; an MCP tool call goes through the same checks as the
//! route it names ([`mcp`]). State-changing calls are audited: one line in the gateway's
//! log and one event in the agent's history, with the token's name, never the token.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ac_domain::AgentId;
use ac_ports::{Runtime, SessionStore};
use rusty_http::{HeaderMap, Method, StatusCode};
use rusty_serve::{Request, Response, SharedHandler};

use crate::access::{self, Failures, Rule, Verdict};
use crate::jobs::{self, Jobs, Route};
use crate::mcp::{self, Message};
use crate::tokens::{self, Keys, Presented, Scope, Token};
use crate::viewer::{Answer, Hub};
use crate::{error, escape, Api, Reply};

/// Who the caller is: name, scopes and agents, no secrets.
pub const WHOAMI: &str = "/api/v1/whoami";
/// Failed authentications logged per source per window; past it they get 429, unlogged.
const FAIL_LIMIT: u32 = 20;
const FAIL_WINDOW: Duration = Duration::from_secs(60);
/// Sources tracked at once.
const FAIL_SOURCES: usize = 1024;
/// rusty_serve does not tell a handler the peer's address, so every failure counts against one
/// source: the limit bounds log volume and does not single out an address (ADR-011).
const SOURCE: &str = "any";

const PAGE: &str = include_str!("../assets/viewer.html");
const SCRIPT: &str = include_str!("../assets/viewer.js");
const STYLE: &str = include_str!("../assets/viewer.css");

/// For agent-written text: never sniffed, never rendered as a page.
const UNTRUSTED: [(&str, &str); 4] = [
    ("Content-Type", "text/plain; charset=utf-8"),
    ("X-Content-Type-Options", "nosniff"),
    ("Content-Security-Policy", "default-src 'none'; sandbox"),
    ("Cache-Control", "no-store"),
];

/// For the page: nothing but this origin's own script, style and API.
pub const CSP: &str =
    "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; \
    img-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// The gateway's handler. Lifecycle calls take the API's lock; token checks and viewer calls
/// never do.
pub struct App<R, S> {
    api: Mutex<Api<R, S>>,
    keys: Keys,
    failures: Failures,
    hub: Hub,
    jobs: Option<Arc<Jobs>>,
}

impl<R: Runtime, S: SessionStore> App<R, S> {
    /// Serve `api` and `hub` to holders of `keys`.
    pub fn new(api: Api<R, S>, hub: Hub, keys: Keys) -> Self {
        Self {
            api: Mutex::new(api),
            keys,
            failures: Failures::new(FAIL_WINDOW, FAIL_LIMIT, FAIL_SOURCES),
            hub,
            jobs: None,
        }
    }

    /// Serve the task API too.
    pub fn with_jobs(mut self, jobs: Arc<Jobs>) -> Self {
        self.jobs = Some(jobs);
        self
    }

    /// The lifecycle API (for the watchdog).
    pub fn api(&self) -> &Mutex<Api<R, S>> {
        &self.api
    }
}

impl<R: Runtime + Send, S: SessionStore + Send> SharedHandler for App<R, S> {
    fn handle(&self, r: &Request<'_>) -> Response {
        if !same_origin(r.headers) {
            return json(error(403, "cross-origin request refused"));
        }
        let method = match r.method {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Delete => "DELETE",
            _ => "OTHER",
        };
        let path = r.target.split('?').next().unwrap_or_default();
        if let Some(asset) = asset(method, path) {
            return asset;
        }
        if !path.starts_with("/api/") {
            return json(error(404, "not found"));
        }
        let keys = self.keys.get();
        let token = match keys.find(r.authorization) {
            Presented::Known(t) if !t.live(tokens::unix_now()) => {
                return self.refuse(method, path, &format!("token {} expired", t.name))
            }
            Presented::Known(t) => t,
            Presented::Missing => return self.refuse(method, path, "no bearer token"),
            Presented::Unknown(prefix) => {
                return self.refuse(method, path, &format!("unknown token sha256:{prefix}"))
            }
        };
        if path == mcp::PATH && !mcp_version_ok(r.headers) {
            return json(error(400, "unsupported MCP-Protocol-Version"));
        }
        self.serve(method, r.target, r.body, token)
    }
}

impl<R: Runtime, S: SessionStore> App<R, S> {
    /// 401, or 429 once a source has failed too often; logged with the token's digest prefix,
    /// never the token or the header.
    fn refuse(&self, method: &str, path: &str, why: &str) -> Response {
        let path = shown(path);
        match self.failures.fail(SOURCE, Instant::now()) {
            Verdict::Log => {
                eprintln!("ac-gateway: auth: {why} for {method} {path}");
                json(error(401, "unauthorized"))
            }
            Verdict::Limit => {
                eprintln!(
                    "ac-gateway: auth: {why} for {method} {path}; {FAIL_LIMIT} failures in \
                     {}s, further ones answered 429 and not logged",
                    FAIL_WINDOW.as_secs()
                );
                json(error(429, "too many failed attempts"))
            }
            Verdict::Quiet => json(error(429, "too many failed attempts")),
        }
    }

    /// An authenticated request, from HTTP or an MCP tool call: the route's scope and agents,
    /// then its handler, then the audit.
    fn serve(&self, method: &str, target: &str, body: &[u8], token: &Token) -> Response {
        let path = target.split('?').next().unwrap_or_default();
        let (rule, agent) = match access::find(method, path) {
            Ok(found) => found,
            Err(reply) => return json(reply),
        };
        if let Err(reply) = access::check(token, rule, agent) {
            eprintln!(
                "ac-gateway: auth: token {} refused ({}) for {method} {}",
                token.name,
                reply.status,
                shown(path)
            );
            return json(reply);
        }
        let response = self.dispatch(method, path, target, body, token);
        if let Some(agent) = agent {
            self.audit(token, rule, agent, target, body, &response);
        }
        response
    }

    /// One MCP message (ADR-017). A tool call is served as its REST request, by [`Self::serve`].
    fn mcp(&self, body: &[u8], token: &Token) -> Response {
        let answer = match mcp::parse(body) {
            Message::Accepted => return Response::json(StatusCode::from_u16(202), Vec::new()),
            Message::Initialize(id, asked) => {
                mcp::initialized(id, mcp::negotiate(asked.as_deref()))
            }
            Message::Ping(id) => mcp::empty(id),
            Message::List(id) => mcp::list(id, |rule| access::check(token, rule, None).is_ok()),
            Message::Call(id, name, args) => {
                let Some(tool) = mcp::tool(&name) else {
                    return rpc(mcp::unknown_tool(id, &name));
                };
                match mcp::build(tool, &args) {
                    Ok(call) => {
                        let r = self.serve(call.method, &call.target, &call.body, token);
                        let text = match &r.body {
                            rusty_serve::Body::Json(b) => String::from_utf8_lossy(b).into_owned(),
                            _ => "a stream: not available over MCP".to_owned(),
                        };
                        mcp::called(id, r.status.as_u16(), &text)
                    }
                    Err(e) => mcp::called(id, 400, &error(400, &e).body),
                }
            }
            Message::Error(id, code, message) => {
                let status = if id.is_null() { 400 } else { 200 };
                let body = mcp::failed(id, code, &message).into_bytes();
                return Response::json(StatusCode::from_u16(status), body);
            }
        };
        rpc(answer)
    }

    fn dispatch(
        &self,
        method: &str,
        path: &str,
        target: &str,
        body: &[u8],
        token: &Token,
    ) -> Response {
        if path == WHOAMI {
            return json(whoami(token));
        }
        if path == mcp::PATH {
            return self.mcp(body, token);
        }
        match self.hub.handle(method, target, body, &token.name) {
            Some(Answer::Json(reply)) => json(reply),
            Some(Answer::Stream(stream)) => Response::stream(
                "application/x-ndjson",
                Revocable {
                    stream,
                    keys: self.keys.clone(),
                    token: token.clone(),
                },
            ),
            Some(Answer::Text(text)) => untrusted(text.into_bytes()),
            None => match jobs::route(method, path) {
                Some(route) => self.job(route, target, body),
                None => {
                    let api = self.api.lock().unwrap_or_else(|e| e.into_inner());
                    let may_attach = token.scopes.allows(Scope::EgressWrite);
                    json(api.request(method, target, body, &token.agents, may_attach))
                }
            },
        }
    }

    /// One line in the gateway log and an event in the agent's history: token name, action, its
    /// flags, the job a submit created, the egress profiles a create, fork or attach named
    /// (validated names only), a fork's new agent and checkpoint, a deleted checkpoint, the
    /// status. No prompt, body or terminal content.
    fn audit(
        &self,
        token: &Token,
        rule: &Rule,
        agent: &str,
        target: &str,
        body: &[u8],
        response: &Response,
    ) {
        let Some(action) = rule.audit else {
            return;
        };
        let mut line = format!("audit token={} action={action}", token.name);
        let key = match action {
            "create" => Some("egress_profiles"),
            "egress" => Some("profiles"),
            _ => None,
        };
        let mut profiles = key.map(|k| crate::profiles_in(body, k));
        if action == "fork" {
            if let Ok(f) = crate::Fork::parse(body) {
                line.push_str(&format!(" to={}", f.to));
                if let Some(cp) = &f.checkpoint {
                    line.push_str(&format!(" checkpoint={cp}"));
                }
                profiles = Some(Ok(f.profiles));
            }
        }
        if let Some(Ok(Some(p))) = profiles {
            let names = ac_domain::egress::names(&p);
            line.push_str(&format!(
                " profiles={}",
                if names.is_empty() { "-" } else { &names }
            ));
        }
        let query = target.split_once('?').map_or("", |(_, q)| q);
        for flag in ["force=true", "purge=true"] {
            if query.split('&').any(|p| p == flag) {
                line.push_str(&format!(" {flag}"));
            }
        }
        if rule.path.contains("{cp}") {
            let cp = target.split('/').nth(6).unwrap_or_default();
            line.push_str(&format!(" checkpoint={}", shown(cp)));
        }
        if rule.path.contains("{job}") {
            let job = target.split('/').nth(6).unwrap_or_default();
            line.push_str(&format!(" job={}", shown(job)));
        } else if let Some(job) = created_job(response) {
            line.push_str(&format!(" job={job}"));
        }
        let status = response.status.as_u16();
        line.push_str(&format!(" status={status}"));
        eprintln!("ac-gateway: {line} agent={}", shown(agent));
        let known = !matches!(status, 400 | 404);
        if let (true, Ok(id)) = (known, AgentId::parse(agent)) {
            self.hub.note(&id, &line);
        }
    }

    /// A job route. A submit takes the lifecycle lock around the agent's state check and the
    /// queueing, so a pause cannot slip between them; the others never take it.
    fn job(&self, route: Result<Route, Reply>, target: &str, body: &[u8]) -> Response {
        let Some(jobs) = &self.jobs else {
            return json(error(404, "not found"));
        };
        let route = match route {
            Ok(route) => route,
            Err(reply) => return json(reply),
        };
        if let Route::Submit(id) = &route {
            let api = self.api.lock().unwrap_or_else(|e| e.into_inner());
            return json(match api.state(id) {
                Ok(state) => jobs.submit(id, state, body),
                Err(e) => error(500, &e.to_string()),
            });
        }
        let query = target.split_once('?').map_or("", |(_, q)| q);
        match jobs.handle(route, query) {
            jobs::Answer::Json(reply) => json(reply),
            jobs::Answer::Output(bytes) => untrusted(bytes),
        }
    }
}

/// A viewer stream that ends once its token is revoked, changed or expired (checked at every
/// message and keep-alive, so within a few seconds of the reload or the expiry). Ending it gives up the viewer's seat.
struct Revocable {
    stream: crate::viewer::Stream,
    keys: Keys,
    token: Token,
}

impl Iterator for Revocable {
    type Item = Vec<u8>;

    fn next(&mut self) -> Option<Vec<u8>> {
        let msg = self.stream.next()?;
        let live = self.token.live(tokens::unix_now()) && self.keys.get().holds(&self.token);
        live.then_some(msg)
    }
}

/// `{"name","scopes","agents","expires"}` of the caller's token; `expires` is Unix seconds or
/// `null` (until revoked).
fn whoami(token: &Token) -> Reply {
    let quoted = |v: Vec<String>| {
        v.iter()
            .map(|s| format!(r#""{}""#, escape(s)))
            .collect::<Vec<_>>()
            .join(",")
    };
    let scopes = quoted(
        token
            .scopes
            .names()
            .into_iter()
            .map(str::to_owned)
            .collect(),
    );
    let agents = match &token.agents {
        crate::tokens::Agents::All => r#""*""#.to_owned(),
        crate::tokens::Agents::Only(ids) => {
            format!(
                "[{}]",
                quoted(ids.iter().map(ToString::to_string).collect())
            )
        }
    };
    Reply {
        status: 200,
        body: format!(
            r#"{{"name":"{}","scopes":[{scopes}],"agents":{agents},"expires":{}}}"#,
            escape(&token.name),
            token
                .expires
                .map_or_else(|| "null".to_owned(), |e| e.to_string())
        ),
    }
}

/// The job id in a submit's reply, if it is one.
fn created_job(response: &Response) -> Option<String> {
    let rusty_serve::Body::Json(body) = &response.body else {
        return None;
    };
    let text = std::str::from_utf8(body).ok()?;
    let id = text.split_once(r#""job_id":""#)?.1.split('"').next()?;
    ac_domain::JobId::parse(id).ok().map(|j| j.to_string())
}

/// A request path for the log: control characters out, at most 200 characters.
fn shown(path: &str) -> String {
    escape(&path.chars().take(200).collect::<String>())
}

/// Agent-written bytes as plain text (see [`UNTRUSTED`]).
fn untrusted(body: Vec<u8>) -> Response {
    with(Response::json(StatusCode::OK, body), &UNTRUSTED)
}

/// A browser request from another site is refused. `Origin` (sent on POSTs and cross-origin
/// requests) must name this host; `Sec-Fetch-Site`, when present, must be `same-origin` or
/// `none`. Clients that send neither (curl) pass; they still need the token.
pub fn same_origin(headers: &HeaderMap) -> bool {
    if let Some(site) = headers.get("sec-fetch-site") {
        if site != "same-origin" && site != "none" {
            return false;
        }
    }
    let Some(origin) = headers.get("origin") else {
        return true;
    };
    let Some(host) = headers.get("host") else {
        return false;
    };
    origin == format!("http://{host}") || origin == format!("https://{host}")
}

fn asset(method: &str, path: &str) -> Option<Response> {
    let name = path.strip_prefix("/ui/")?;
    if method != "GET" {
        return Some(json(error(405, "method not allowed")));
    }
    let (body, kind) = match name {
        "viewer.js" => (SCRIPT, "text/javascript; charset=utf-8"),
        "viewer.css" => (STYLE, "text/css; charset=utf-8"),
        id if ac_domain::AgentId::parse(id).is_ok() => (PAGE, "text/html; charset=utf-8"),
        _ => return Some(json(error(404, "not found"))),
    };
    Some(with(
        Response::json(StatusCode::OK, body.as_bytes().to_vec()),
        &[
            ("Content-Type", kind),
            ("Content-Security-Policy", CSP),
            ("Cache-Control", "no-store"),
            ("X-Frame-Options", "DENY"),
            ("Referrer-Policy", "no-referrer"),
            ("X-Content-Type-Options", "nosniff"),
        ],
    ))
}

/// `MCP-Protocol-Version`, when sent, must be one this gateway serves.
fn mcp_version_ok(headers: &HeaderMap) -> bool {
    headers
        .get("mcp-protocol-version")
        .is_none_or(|v| mcp::VERSIONS.contains(&v))
}

/// A JSON-RPC answer.
fn rpc(body: String) -> Response {
    Response::json(StatusCode::OK, body.into_bytes())
}

fn json(reply: Reply) -> Response {
    Response::json(StatusCode::from_u16(reply.status), reply.body.into_bytes())
}

fn with(mut response: Response, headers: &[(&str, &str)]) -> Response {
    for (name, value) in headers {
        response = match response.with_header(name, value) {
            Ok(r) => r,
            Err(_) => return Response::internal_error(),
        };
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            let _ = h.insert(k, v);
        }
        h
    }

    #[test]
    fn origin_must_be_this_host() {
        let host = ("Host", "127.0.0.1:8780");
        assert!(same_origin(&headers(&[host])));
        assert!(same_origin(&headers(&[
            host,
            ("Origin", "http://127.0.0.1:8780")
        ])));
        assert!(same_origin(&headers(&[
            host,
            ("Sec-Fetch-Site", "same-origin")
        ])));
        for bad in [
            vec![host, ("Origin", "http://evil.example")],
            vec![host, ("Origin", "null")],
            vec![host, ("Origin", "http://127.0.0.1:8780.evil")],
            vec![("Origin", "http://127.0.0.1:8780")],
            vec![host, ("Sec-Fetch-Site", "cross-site")],
            vec![host, ("Sec-Fetch-Site", "same-site")],
        ] {
            assert!(!same_origin(&headers(&bad)), "{bad:?}");
        }
    }

    #[test]
    fn page_assets() {
        let page = asset("GET", "/ui/a1");
        assert!(page
            .is_some_and(|r| r.status == StatusCode::OK
                && r.headers.get("content-security-policy") == Some(CSP)));
        assert!(asset("GET", "/ui/viewer.js").is_some_and(
            |r| r.headers.get("content-type") == Some("text/javascript; charset=utf-8")
        ));
        assert!(asset("GET", "/ui/..%2Fetc").is_some_and(|r| r.status == StatusCode::from_u16(404)));
        assert!(asset("POST", "/ui/a1").is_some_and(|r| r.status == StatusCode::from_u16(405)));
        assert!(asset("GET", "/api/v1/agents").is_none());
    }

    #[test]
    fn page_makes_no_external_requests_and_keeps_the_token_in_memory() {
        for (name, text) in [("html", PAGE), ("js", SCRIPT), ("css", STYLE)] {
            for banned in [
                "http://",
                "https://",
                "//cdn",
                "localStorage",
                "sessionStorage",
                "document.cookie",
                "innerHTML",
                "eval(",
                "new Function",
            ] {
                assert!(!text.contains(banned), "{name} contains {banned}");
            }
        }
        assert!(
            !PAGE.contains("<script>") && !PAGE.contains("style="),
            "no inline script or style"
        );
    }
}
