//! The whole HTTP surface, in order: the Origin check (every request), the viewer page and its
//! script and style (no data in them, so no token: a browser cannot send a header when it
//! navigates), the viewer routes, the job routes, then the lifecycle API. Everything under `/api`
//! needs the token.

use std::sync::{Arc, Mutex};

use ac_ports::{Runtime, SessionStore};
use rusty_http::{HeaderMap, Method, StatusCode};
use rusty_serve::{Request, Response, SharedHandler};

use crate::jobs::{self, Jobs, Route};
use crate::viewer::{Answer, Hub};
use crate::{error, Api, Auth, Reply};

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

/// The gateway's handler. Lifecycle calls take the API's lock; viewer calls never do.
pub struct App<R, S> {
    api: Mutex<Api<R, S>>,
    auth: Auth,
    hub: Hub,
    jobs: Option<Arc<Jobs>>,
}

impl<R: Runtime, S: SessionStore> App<R, S> {
    /// Serve `api` and `hub`.
    pub fn new(api: Api<R, S>, hub: Hub) -> Self {
        Self {
            auth: api.auth(),
            api: Mutex::new(api),
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
            Method::Delete => "DELETE",
            _ => "OTHER",
        };
        let path = r.target.split('?').next().unwrap_or_default();
        if let Some(asset) = asset(method, path) {
            return asset;
        }
        if path.starts_with("/api/") && !self.auth.check(r.authorization) {
            return json(error(401, "unauthorized"));
        }
        match self.hub.handle(method, r.target, r.body) {
            Some(Answer::Json(reply)) => json(reply),
            Some(Answer::Stream(stream)) => Response::stream("application/x-ndjson", stream),
            Some(Answer::Text(text)) => untrusted(text.into_bytes()),
            None => match jobs::route(method, path) {
                Some(route) => self.job(route, r),
                None => {
                    let api = self.api.lock().unwrap_or_else(|e| e.into_inner());
                    json(api.handle(method, r.target, r.authorization))
                }
            },
        }
    }
}

impl<R: Runtime, S: SessionStore> App<R, S> {
    /// A job route. A submit takes the lifecycle lock around the agent's state check and the
    /// queueing, so a pause cannot slip between them; the others never take it.
    fn job(&self, route: Result<Route, Reply>, r: &Request<'_>) -> Response {
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
                Ok(state) => jobs.submit(id, state, r.body),
                Err(e) => error(500, &e.to_string()),
            });
        }
        let query = r.target.split_once('?').map_or("", |(_, q)| q);
        match jobs.handle(route, query) {
            jobs::Answer::Json(reply) => json(reply),
            jobs::Answer::Output(bytes) => untrusted(bytes),
        }
    }
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
