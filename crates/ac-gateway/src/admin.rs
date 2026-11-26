//! Host-wide admin routes (ADR-031): API tokens, written through the CLI's own [`TokenFile`], and
//! backups ([`Backups`]). The
//! caller is a host admin by now ([`crate::access::Need::Host`]); this module only parses, writes
//! and answers. Nothing here is offered over MCP, so a secret never lands in a tool result.

use rusty_json::Value;

use crate::backups::Backups;
use crate::tokens::{self, Agents, Keys, Scopes, Token, TokenError, TokenFile, Tokens};
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

    /// Serve `call` from `caller` with `body` at `now` (Unix seconds). A caller only creates or
    /// revokes tokens that do not outlive its own (ADR-031), so a short-lived admin token cannot
    /// mint a lasting one or revoke the owner's.
    pub fn serve(&self, call: Call<'_>, body: &[u8], now: u64, caller: &Token) -> Done {
        let read = |reply| Done {
            reply,
            audit: String::new(),
        };
        match call {
            Call::Tokens => read(self.list(now)),
            Call::Create(name) => self.create(name, body, now, caller),
            Call::Revoke(name) => self.revoke(name, now, caller),
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

    fn create(&self, name: &str, body: &[u8], now: u64, caller: &Token) -> Done {
        let Some(named) = named(name) else {
            return invalid(name);
        };
        let done = |reply, audit: String| Done { reply, audit };
        let grant = match Grant::parse(body) {
            Ok(g) => g,
            Err(reply) => return done(reply, named),
        };
        let audit = format!("{named}{}", grant.audit(now));
        let expires = grant.lifetime.map(|l| now.saturating_add(l));
        if expires.unwrap_or(u64::MAX) > caller.end() {
            let until = caller.expires.map_or_else(String::new, tokens::utc);
            let why = format!(
                "token {} expires at {until}: a token it creates must expire no later",
                caller.name
            );
            return done(error(403, &why), audit);
        }
        let made = self
            .file
            .create_new(name, grant.scopes, grant.agents, now, grant.lifetime);
        let reply = match made {
            Ok(secret) => {
                self.refresh();
                created(name, &secret, expires)
            }
            Err(e) => refused(name, e),
        };
        done(reply, audit)
    }

    fn revoke(&self, name: &str, now: u64, caller: &Token) -> Done {
        let Some(audit) = named(name) else {
            return invalid(name);
        };
        let reply = match self.file.revoke_keeping_admin(name, now, caller.end()) {
            Ok(()) => {
                self.refresh();
                Reply {
                    status: 200,
                    body: format!(r#"{{"revoked":"{name}"}}"#),
                }
            }
            Err(e) => refused(name, e),
        };
        Done { reply, audit }
    }

    /// Put the file's tokens in force now, read and published under the file's lock so a
    /// concurrent refresh never publishes an older set; on failure the reload loop retries
    /// within a second.
    fn refresh(&self) {
        let published = self.file.lock().and_then(|_held| {
            let tokens = self.file.list()?;
            self.keys.set(tokens);
            Ok(())
        });
        if let Err(e) = published {
            eprintln!("ac-gateway: admin: tokens not refreshed: {e}");
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
            Some(v) if v.is_null() => None,
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
        let expires = self.lifetime.map_or_else(String::new, |l| {
            format!(" expires={}", now.saturating_add(l))
        });
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

/// ` name=<name>` for the audit line, if the name is valid.
fn named(name: &str) -> Option<String> {
    tokens::check_name(name)
        .ok()
        .map(|()| format!(" name={name}"))
}

/// 400 for a name from the path that is not a token name; the audit line does not repeat it.
fn invalid(name: &str) -> Done {
    let e = tokens::check_name(name).err().unwrap_or_default();
    Done {
        reply: error(400, &e),
        audit: " name=<invalid>".into(),
    }
}

/// The answer for a write the tokens file refused.
fn refused(name: &str, e: TokenError) -> Reply {
    match e {
        TokenError::Invalid(e) => error(400, &e),
        TokenError::Exists => error(409, &format!("a token named {name} exists")),
        TokenError::Full => error(507, "the tokens file is full: revoke unused tokens first"),
        TokenError::Unknown => error(404, &format!("no token named {name}")),
        TokenError::Outlives => error(
            403,
            &format!(
                "token {name} outlives the token revoking it: revoke it with a longer-lived one"
            ),
        ),
        TokenError::LastAdmin => error(
            409,
            "refused: no other live token with admin for * lasts as long (use the host's CLI)",
        ),
        TokenError::Failed(e) => failed(&e),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;
    use std::sync::{Arc, Barrier};

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

    /// The token named `name` in the file.
    fn token(a: &Admin, name: &str) -> Token {
        let all = a.file.list().unwrap_or_else(|e| panic!("{e}"));
        let found = all.iter().find(|t| t.name == name).cloned();
        found.unwrap_or_else(|| panic!("no {name}"))
    }

    /// As `owner`, which never expires.
    fn status(a: &Admin, call: Call<'_>, body: &str) -> (u16, String) {
        status_as(a, "owner", call, body)
    }

    fn status_as(a: &Admin, caller: &str, call: Call<'_>, body: &str) -> (u16, String) {
        let d = a.serve(call, body.as_bytes(), 200, &token(a, caller));
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
        let d = a.serve(
            Call::Create("ci"),
            body.as_bytes(),
            200,
            &token(&a, "owner"),
        );
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
        let never = r#"{"scopes":["viewer:read"],"agents":"*","expires":null}"#;
        assert_eq!(
            status(&a, Call::Create("watch"), never).0,
            201,
            "null: never"
        );
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
        let d = a.serve(Call::Revoke("Bad\n!"), b"", 200, &token(&a, "owner"));
        assert_eq!(d.audit, " name=<invalid>", "an invalid name is not logged");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Nano's review of #56: a short-lived admin token can neither mint a lasting one nor revoke
    /// the owner's, so the API is never left without a host admin once it expires.
    #[test]
    fn a_token_never_outlives_the_one_that_made_it() {
        let (a, _, dir) = admin();
        let brief = r#"{"scopes":["admin"],"agents":"*","expires":"1h"}"#;
        assert_eq!(status(&a, Call::Create("brief"), brief).0, 201);
        for body in [
            r#"{"scopes":["admin"],"agents":"*"}"#,
            r#"{"scopes":["jobs:read"],"agents":"*","expires":"2h"}"#,
        ] {
            let (code, why) = status_as(&a, "brief", Call::Create("more"), body);
            assert_eq!(code, 403, "{body}");
            assert!(why.contains("no later"), "{why}");
        }
        let shorter = r#"{"scopes":["jobs:read"],"agents":"*","expires":"30m"}"#;
        assert_eq!(status_as(&a, "brief", Call::Create("job"), shorter).0, 201);
        assert_eq!(status_as(&a, "brief", Call::Revoke("owner"), "").0, 403);
        assert_eq!(status_as(&a, "brief", Call::Revoke("job"), "").0, 200);
        assert_eq!(
            status_as(&a, "brief", Call::Revoke("brief"), "").0,
            200,
            "itself: owner stays"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_host_admin_goes_only_if_another_lasts_as_long() {
        let (a, _, dir) = admin();
        assert_eq!(status(&a, Call::Revoke("owner"), "").0, 409, "the only one");
        let restricted = r#"{"scopes":["admin"],"agents":["a1"]}"#;
        assert_eq!(status(&a, Call::Create("half"), restricted).0, 201);
        assert_eq!(status(&a, Call::Revoke("owner"), "").0, 409, "not for *");
        let expiring = r#"{"scopes":["admin"],"agents":"*","expires":"1h"}"#;
        assert_eq!(status(&a, Call::Create("brief"), expiring).0, 201);
        assert_eq!(
            status(&a, Call::Revoke("owner"), "").0,
            409,
            "brief ends sooner"
        );
        let lasting = r#"{"scopes":["admin"],"agents":"*"}"#;
        assert_eq!(status(&a, Call::Create("spare"), lasting).0, 201);
        assert_eq!(status(&a, Call::Revoke("owner"), "").0, 200);
        assert_eq!(
            status_as(&a, "spare", Call::Revoke("brief"), "").0,
            200,
            "spare covers it"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Both checks run under the file's lock: of two revokes of the last two lasting host admins,
    /// one wins; of two creates of one name, one gets 409 (not a 500).
    #[test]
    fn concurrent_writes_keep_the_rules() {
        let (a, _, dir) = admin();
        let lasting = r#"{"scopes":["admin"],"agents":"*"}"#;
        assert_eq!(status(&a, Call::Create("spare"), lasting).0, 201);
        let a = Arc::new(a);
        let race = |calls: [(&'static str, Call<'static>, &'static str); 2]| -> Vec<u16> {
            let gate = Arc::new(Barrier::new(2));
            let runs: Vec<_> = calls
                .into_iter()
                .map(|(caller, call, body)| {
                    let (a, gate) = (Arc::clone(&a), Arc::clone(&gate));
                    std::thread::spawn(move || {
                        let me = token(&a, caller);
                        gate.wait();
                        a.serve(call, body.as_bytes(), 200, &me).reply.status
                    })
                })
                .collect();
            let mut codes: Vec<u16> = runs.into_iter().map(|r| r.join().unwrap_or(0)).collect();
            codes.sort_unstable();
            codes
        };
        let revokes = race([
            ("owner", Call::Revoke("spare"), ""),
            ("spare", Call::Revoke("owner"), ""),
        ]);
        assert_eq!(revokes, [200, 409]);
        let one = r#"{"scopes":["jobs:read"],"agents":"*"}"#;
        let left = a.file.list().unwrap_or_default();
        let who = left
            .iter()
            .next()
            .map(|t| t.name.clone())
            .unwrap_or_default();
        let who: &'static str = Box::leak(who.into_boxed_str());
        let creates = race([
            (who, Call::Create("dup"), one),
            (who, Call::Create("dup"), one),
        ]);
        assert_eq!(creates, [201, 409]);
        let names = |t: &Tokens| t.iter().map(|t| t.name.clone()).collect::<Vec<_>>();
        let file = a.file.list().unwrap_or_default();
        assert_eq!(
            names(&a.keys.get()),
            names(&file),
            "the newest set is in force"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_unreadable_file_is_500_and_a_full_one_507() {
        let (a, _, dir) = admin();
        let owner = token(&a, "owner");
        let path = dir.join("tokens");
        let full = format!(
            "{}\n{}",
            std::fs::read_to_string(&path).unwrap_or_default(),
            "#".repeat(64 * 1024 - 300)
        );
        std::fs::write(&path, full).unwrap_or_else(|e| panic!("{e}"));
        let body = br#"{"scopes":["admin"],"agents":"*"}"#;
        let made: Vec<u16> = (0..4)
            .map(|i| {
                let name = format!("t{i}");
                let name: &'static str = Box::leak(name.into_boxed_str());
                a.serve(Call::Create(name), body, 200, &owner).reply.status
            })
            .collect();
        assert!(made.contains(&507), "{made:?}");
        std::fs::write(&path, "not a token line\n").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(a.serve(Call::Tokens, b"", 200, &owner).reply.status, 500);
        assert_eq!(
            a.serve(Call::Revoke("owner"), b"", 200, &owner)
                .reply
                .status,
            500
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
