//! Host-wide admin routes (ADR-031): API tokens, written through the CLI's own [`TokenFile`], and
//! backups ([`Backups`]). The
//! caller is a host admin by now ([`crate::access::Need::Host`]); this module only parses, writes
//! and answers. Nothing here is offered over MCP, so a secret never lands in a tool result.

use rusty_json::Value;

use crate::backups::Backups;
use crate::tokens::{self, Agents, Keys, RevokeError, Scopes, Token, TokenFile, Tokens};
use crate::{error, escape, members, Reply};

/// One admin call, from its method and path.
#[derive(Debug, PartialEq, Eq)]
pub enum Call<'a> {
    /// `GET /api/v1/tokens`.
    Tokens,
    /// `POST /api/v1/tokens/{name}`.
    Create(&'a str),
    /// `DELETE /api/v1/tokens/{name}`.
    Revoke(&'a str),
    /// `GET /api/v1/backups`.
    Backups,
    /// `POST /api/v1/backups`.
    BackUp,
    /// `POST /api/v1/agents/{id}/restore-backup`: needs the lifecycle lock, so the transport
    /// serves it ([`crate::web`]).
    RestoreBackup(&'a str),
}

/// The call `method` and `path` name, if any.
pub fn route<'a>(method: &str, path: &'a str) -> Option<Call<'a>> {
    match (method, path) {
        ("GET", "/api/v1/backups") => return Some(Call::Backups),
        ("POST", "/api/v1/backups") => return Some(Call::BackUp),
        _ => {}
    }
    let agent = path
        .strip_prefix("/api/v1/agents/")
        .and_then(|p| p.strip_suffix("/restore-backup"))
        .filter(|id| !id.is_empty() && !id.contains('/'));
    if let Some(id) = agent {
        return (method == "POST").then_some(Call::RestoreBackup(id));
    }
    let rest = path.strip_prefix("/api/v1/tokens")?;
    if rest.is_empty() {
        return (method == "GET").then_some(Call::Tokens);
    }
    let name = rest
        .strip_prefix('/')
        .filter(|n| !n.is_empty() && !n.contains('/'))?;
    match method {
        "POST" => Some(Call::Create(name)),
        "DELETE" => Some(Call::Revoke(name)),
        _ => None,
    }
}

/// A call's answer and what the audit line adds after `action=` (empty for a read).
#[derive(Debug)]
pub struct Done {
    /// The answer.
    pub reply: Reply,
    /// ` name=... [scopes=...] ...`, validated values only.
    pub audit: String,
}

/// The tokens file and the set in force, which a write refreshes at once rather than at the next
/// one-second reload; the backups, if a destination is set.
pub struct Admin {
    file: TokenFile,
    keys: Keys,
    backups: Option<Backups>,
}

impl Admin {
    /// Write `file` and refresh `keys` after each change.
    pub fn new(file: TokenFile, keys: Keys) -> Self {
        Self {
            file,
            keys,
            backups: None,
        }
    }

    /// Serve the backup routes from `backups`; without it they answer 404.
    pub fn with_backups(mut self, backups: Backups) -> Self {
        self.backups = Some(backups);
        self
    }

    /// Serve `call` with `body` at `now` (Unix seconds).
    pub fn serve(&self, call: Call<'_>, body: &[u8], now: u64) -> Done {
        let read = |reply| Done {
            reply,
            audit: String::new(),
        };
        match call {
            Call::Tokens => read(self.list(now)),
            Call::Create(name) => self.create(name, body, now),
            Call::Revoke(name) => self.revoke(name, now),
            Call::Backups => read(self.backups().map_or_else(|r| r, Backups::list)),
            Call::BackUp => read(self.backups().map_or_else(|r| r, Backups::start)),
            Call::RestoreBackup(_) => read(error(404, "not found")),
        }
    }

    /// The backups, or 404 without a destination.
    pub fn backups(&self) -> Result<&Backups, Reply> {
        self.backups
            .as_ref()
            .ok_or_else(|| error(404, "no backup destination (set AC_BACKUP_DEST)"))
    }

    fn list(&self, now: u64) -> Reply {
        match self.file.list() {
            Ok(tokens) => Reply {
                status: 200,
                body: list_json(&tokens, now),
            },
            Err(e) => failed(&e),
        }
    }

    fn create(&self, name: &str, body: &[u8], now: u64) -> Done {
        let named = format!(" name={}", shown(name));
        let done = |reply| Done {
            reply,
            audit: named.clone(),
        };
        if let Err(e) = tokens::check_name(name) {
            return done(error(400, &e));
        }
        let grant = match Grant::parse(body) {
            Ok(g) => g,
            Err(reply) => return done(reply),
        };
        let audit = format!("{named}{}", grant.audit(now));
        match self.file.list() {
            Ok(t) if t.iter().any(|t| t.name == name) => {
                let reply = error(409, &format!("a token named {name} exists"));
                return Done { reply, audit };
            }
            Ok(_) => {}
            Err(e) => {
                return Done {
                    reply: failed(&e),
                    audit,
                }
            }
        }
        let made = self
            .file
            .create(name, grant.scopes, grant.agents, now, grant.lifetime);
        let reply = match made {
            Ok(secret) => {
                self.refresh();
                created(name, &secret, grant.lifetime.map(|l| now + l))
            }
            Err(e) => failed(&e),
        };
        Done { reply, audit }
    }

    fn revoke(&self, name: &str, now: u64) -> Done {
        let audit = format!(" name={}", shown(name));
        if let Err(e) = tokens::check_name(name) {
            return Done {
                reply: error(400, &e),
                audit,
            };
        }
        let reply = match self.file.revoke_keeping_admin(name, now) {
            Ok(()) => {
                self.refresh();
                Reply {
                    status: 200,
                    body: format!(r#"{{"revoked":"{name}"}}"#),
                }
            }
            Err(RevokeError::Unknown) => error(404, &format!("no token named {name}")),
            Err(RevokeError::LastAdmin) => error(
                409,
                "refused: no live token with admin for * would remain (use the host's CLI)",
            ),
            Err(RevokeError::Failed(e)) => failed(&e),
        };
        Done { reply, audit }
    }

    /// Put the file's tokens in force now; on failure the reload loop retries within a second.
    fn refresh(&self) {
        match self.file.list() {
            Ok(tokens) => self.keys.set(tokens),
            Err(e) => eprintln!("ac-gateway: admin: tokens not refreshed: {e}"),
        }
    }
}

/// What a create grants, from its body.
#[derive(Debug, PartialEq, Eq)]
struct Grant {
    scopes: Scopes,
    agents: Agents,
    lifetime: Option<u64>,
}

impl Grant {
    /// `{"scopes": [...], "agents": "*" | [...], "expires": "<n>m|h|d"?}`, by the CLI's parsers.
    fn parse(body: &[u8]) -> Result<Self, Reply> {
        let bad = |m: &str| error(400, m);
        let value = members(body, &["scopes", "agents", "expires"])?
            .ok_or_else(|| bad("body must name scopes and agents"))?;
        let scopes = match value.get("scopes").map(strings) {
            Some(Some(list)) if !list.is_empty() => {
                Scopes::parse(&list.join(",")).map_err(|e| bad(&e))?
            }
            _ => return Err(bad("scopes must be a non-empty list of scope names")),
        };
        let agents = match value.get("agents") {
            Some(v) if v.as_str() == Some("*") => Agents::All,
            Some(v) => match strings(v) {
                Some(list) if !list.is_empty() => {
                    Agents::parse(&list.join(",")).map_err(|e| bad(&e))?
                }
                _ => {
                    return Err(bad(
                        r#"agents must be "*" or a non-empty list of agent ids"#,
                    ))
                }
            },
            None => return Err(bad("agents is required")),
        };
        let lifetime = match value.get("expires") {
            None => None,
            Some(v) => {
                let text = v.as_str().ok_or_else(|| bad("expires must be a string"))?;
                Some(tokens::parse_lifetime(text).map_err(|e| bad(&e))?)
            }
        };
        Ok(Self {
            scopes,
            agents,
            lifetime,
        })
    }

    fn audit(&self, now: u64) -> String {
        let expires = self
            .lifetime
            .map_or_else(String::new, |l| format!(" expires={}", now + l));
        format!(" scopes={} agents={}{expires}", self.scopes, self.agents)
    }
}

/// The strings of a JSON array, or `None` if it is not one of strings only. A comma inside one
/// would split it, so it is refused too.
fn strings(value: &Value) -> Option<Vec<&str>> {
    value
        .as_array()?
        .iter()
        .map(|v| v.as_str().filter(|s| !s.contains(',')))
        .collect()
}

/// `"name":…,"scopes":[…],"agents":…,"expires":…` (the members, no braces): what `whoami` and the
/// list share. Never the digest.
pub(crate) fn grants_json(token: &Token) -> String {
    let scopes: Vec<String> = token
        .scopes
        .names()
        .into_iter()
        .map(|s| format!(r#""{s}""#))
        .collect();
    let agents = match &token.agents {
        Agents::All => r#""*""#.to_owned(),
        Agents::Only(ids) => {
            let ids: Vec<String> = ids.iter().map(|id| format!(r#""{id}""#)).collect();
            format!("[{}]", ids.join(","))
        }
    };
    format!(
        r#""name":"{}","scopes":[{}],"agents":{agents},"expires":{}"#,
        escape(&token.name),
        scopes.join(","),
        number(token.expires)
    )
}

fn list_json(tokens: &Tokens, now: u64) -> String {
    let items: Vec<String> = tokens
        .iter()
        .map(|t| {
            format!(
                r#"{{{},"created":{},"live":{}}}"#,
                grants_json(t),
                number(t.created),
                t.live(now)
            )
        })
        .collect();
    format!(r#"{{"tokens":[{}]}}"#, items.join(","))
}

fn created(name: &str, secret: &str, expires: Option<u64>) -> Reply {
    Reply {
        status: 201,
        body: format!(
            r#"{{"name":"{name}","token":"{secret}","expires":{}}}"#,
            number(expires)
        ),
    }
}

fn number(n: Option<u64>) -> String {
    n.map_or_else(|| "null".to_owned(), |n| n.to_string())
}

/// 500 for a tokens file that could not be read or written; the detail goes to the journal.
fn failed(e: &str) -> Reply {
    eprintln!("ac-gateway: admin: tokens file: {e}");
    error(
        500,
        "the tokens file could not be read or written (see the gateway's journal)",
    )
}

/// A name from the path, for the log: at most 40 characters, control characters out.
fn shown(name: &str) -> String {
    escape(&name.chars().take(40).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    fn admin() -> (Admin, Keys, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "ac-admin-{}-{}",
            std::process::id(),
            tokens::random_hex().unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("{e}"));
        let uid = std::fs::metadata(&dir)
            .map(|m| m.uid())
            .unwrap_or_else(|e| panic!("{e}"));
        let file = TokenFile::new(dir.join("tokens"), uid);
        file.create(
            "owner",
            Scopes::parse("admin").unwrap_or_else(|e| panic!("{e}")),
            Agents::All,
            100,
            None,
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let keys = Keys::default();
        (Admin::new(file, keys.clone()), keys, dir)
    }

    fn status(a: &Admin, call: Call<'_>, body: &str) -> (u16, String) {
        let d = a.serve(call, body.as_bytes(), 200);
        (d.reply.status, d.reply.body)
    }

    #[test]
    fn routes() {
        assert_eq!(route("GET", "/api/v1/tokens"), Some(Call::Tokens));
        assert_eq!(route("POST", "/api/v1/tokens/ci"), Some(Call::Create("ci")));
        assert_eq!(
            route("DELETE", "/api/v1/tokens/ci"),
            Some(Call::Revoke("ci"))
        );
        assert_eq!(route("GET", "/api/v1/tokens/ci"), None);
        assert_eq!(route("POST", "/api/v1/tokens"), None);
        assert_eq!(route("GET", "/api/v1/tokensx"), None);
        assert_eq!(route("POST", "/api/v1/tokens/"), None);
        assert_eq!(route("POST", "/api/v1/tokens/a/b"), None);
        assert_eq!(route("GET", "/api/v1/backups"), Some(Call::Backups));
        assert_eq!(route("POST", "/api/v1/backups"), Some(Call::BackUp));
        assert_eq!(route("DELETE", "/api/v1/backups"), None);
        let restore = "/api/v1/agents/a1/restore-backup";
        assert_eq!(route("POST", restore), Some(Call::RestoreBackup("a1")));
        assert_eq!(route("GET", restore), None);
        assert_eq!(route("POST", "/api/v1/agents//restore-backup"), None);
        assert_eq!(route("POST", "/api/v1/agents/a/b/restore-backup"), None);
    }

    #[test]
    fn create_list_revoke() {
        let (a, keys, dir) = admin();
        let body = r#"{"scopes":["jobs:read","jobs:write"],"agents":["a1"],"expires":"30d"}"#;
        let d = a.serve(Call::Create("ci"), body.as_bytes(), 200);
        assert_eq!(d.reply.status, 201, "{}", d.reply.body);
        assert!(d.reply.body.contains(r#""token":""#));
        assert_eq!(
            d.audit,
            format!(
                " name=ci scopes=jobs:read,jobs:write agents=a1 expires={}",
                200 + 30 * 86_400
            )
        );
        let secret = d.reply.body.split('"').nth(7).unwrap_or_default();
        assert_eq!(secret.len(), 64);
        assert!(!d.audit.contains(secret), "no secret in the audit");
        assert_eq!(keys.get().iter().count(), 2, "in force at once");
        let (s, list) = status(&a, Call::Tokens, "");
        assert_eq!(s, 200);
        assert!(list.contains(r#""name":"ci","scopes":["jobs:read","jobs:write"],"agents":["a1"]"#));
        assert!(list.contains(r#""created":200,"live":true"#));
        assert!(!list.contains(secret), "never the secret");
        assert_eq!(status(&a, Call::Create("ci"), body).0, 409, "duplicate");
        assert_eq!(status(&a, Call::Revoke("ci"), "").0, 200);
        assert_eq!(status(&a, Call::Revoke("ci"), "").0, 404);
        assert_eq!(keys.get().iter().count(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn backups_without_a_destination_are_404() {
        let (a, _, dir) = admin();
        assert_eq!(status(&a, Call::Backups, "").0, 404);
        assert_eq!(status(&a, Call::BackUp, "").0, 404);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn bad_bodies_and_names_are_400() {
        let (a, _, dir) = admin();
        for (name, body) in [
            ("Bad!", r#"{"scopes":["admin"],"agents":"*"}"#),
            ("x", ""),
            ("x", r#"{"scopes":[],"agents":"*"}"#),
            ("x", r#"{"scopes":["root"],"agents":"*"}"#),
            ("x", r#"{"scopes":["jobs:read,admin"],"agents":"*"}"#),
            ("x", r#"{"scopes":["admin"]}"#),
            ("x", r#"{"scopes":["admin"],"agents":[]}"#),
            ("x", r#"{"scopes":["admin"],"agents":"a1"}"#),
            ("x", r#"{"scopes":["admin"],"agents":["../x"]}"#),
            ("x", r#"{"scopes":["admin"],"agents":"*","expires":"0d"}"#),
            ("x", r#"{"scopes":["admin"],"agents":"*","expires":30}"#),
            ("x", r#"{"scopes":["admin"],"agents":"*","hash":"00"}"#),
        ] {
            assert_eq!(status(&a, Call::Create(name), body).0, 400, "{name} {body}");
        }
        assert_eq!(status(&a, Call::Revoke("Bad!"), "").0, 400);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_last_host_admin_stays() {
        let (a, _, dir) = admin();
        assert_eq!(status(&a, Call::Revoke("owner"), "").0, 409);
        let restricted = r#"{"scopes":["admin"],"agents":["a1"]}"#;
        assert_eq!(status(&a, Call::Create("half"), restricted).0, 201);
        assert_eq!(status(&a, Call::Revoke("owner"), "").0, 409, "not for *");
        let expiring = r#"{"scopes":["admin"],"agents":"*","expires":"1m"}"#;
        assert_eq!(status(&a, Call::Create("brief"), expiring).0, 201);
        let later = a.serve(Call::Revoke("owner"), b"", 200 + 61);
        assert_eq!(later.reply.status, 409, "expired by then");
        assert_eq!(
            status(&a, Call::Revoke("owner"), "").0,
            200,
            "brief is live"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
