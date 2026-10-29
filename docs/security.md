# Security

## Threat model

Single trusted owner, several semi-trusted agents. Agents run without approval prompts and
may execute untrusted code or be steered by prompt injection from web content.

| Threat | Mitigation |
|---|---|
| Agent escapes to host | Kata microVM (own guest kernel, KVM); no sudo; no containerd socket |
| Agent reads other agents' data | Separate microVMs and workspace volumes; no network path between agents |
| Agent exfiltrates data | Egress allowlist proxy (`ac-egress`), firewall makes it the only route; limits destinations, not content (ADR-007) |
| An agent claims another agent's wider egress | Profiles follow a per-boot proxy credential, not the source IP; only its SHA-256 is stored, a wrong or unknown one gets 407 (never the base list), and it changes on every create and resume (ADR-016) |
| An agent's proxy credential leaks | It grants only that agent's profiles, from any agent VM, until its next pause or resume; it is never on a host command line, only in the VM's environment and containerd's root-only record (ADR-016) |
| A token widens egress | `egress:write` attaches profiles root installed on the host; no API call defines hosts. Every change is audited (ADR-016) |
| A fork leaks the source agent's secrets | A fork copies the workspace, so it holds the source's Claude Code login and Git credentials; forking needs `agents:write` for both agents. Egress profiles are not copied (ADR-018) |
| Checkpoints keep data an agent deleted | Read-only snapshots in root-only `checkpoints/<agent>/` (0700), never mounted into a VM; capped per agent and deleted by purge (ADR-018) |
| Agent steals credentials | Control-plane secrets root-owned 0600; model logins confined to the agent's workspace volume |
| Prompt injection via browsing | Policy-locked Chromium, egress allowlist, human takeover |
| Agent attacks the viewer's browser | Only sanitised text and SGR colours reach the page; strict CSP; see [Viewer](#viewer) |
| Network attacker reaches the API | Gateway on a private bind only (loopback, RFC 1918, tailnet), scoped bearer tokens ([API tokens](#api-tokens)); no public port |
| A jobs token is stolen (`jobs:read,jobs:write` for some agents) | It runs prompts, so code as the agent user, in those agents only; it cannot pause, destroy, watch or type into them, and other agents answer 404. Revoke it alone (`ac-gateway token revoke`); see [API tokens](#api-tokens) |
| The admin token is stolen | Full control of every agent (create, destroy, purge, jobs, take-over), not the host. Keep it on the host (root, 0600), give every other caller a narrower token, revoke and re-issue it if it leaks |
| Runaway agent starves the host | Per-agent `cpu.max` and `memory.max` on the whole sandbox, a btrfs quota on its workspace (ADR-023), the job gate (ADR-013) |
| Destructive commands | Command guard in the shell container (a backstop, not a boundary) |

## Hardening checklist

- [ ] `/dev/kvm` present; every agent runs under the Kata runtime class, never plain runc
- [ ] SELinux enforcing; denials reviewed instead of disabling it
- [ ] Gateway reachable only over Tailscale with HTTPS and a bearer token
- [x] nftables egress rules: agents reach only the egress proxy (verified 2026-10-19 from a1:
  direct connections to the internet, the LAN, 169.254.169.254 and other host ports refused;
  no DNS)
- [x] Agents cannot reach each other: a `bridge`-family nft rule drops frames between `acbr0` ports
  (verified 2026-10-20: a1 reached 0 of a2's 65535 TCP ports, including a live listener; both
  still reached the proxy)
- [x] firewalld does not decide the proxy's reachability by accident: `acbr0` is in its own
  `agents` zone, which accepts only TCP 3128 (verified 2026-10-20 with the default zone's
  TCP 1025-3388 removed: a1 still reached the proxy, and failed with `acbr0` taken out of the zone)
- [x] Proxy allowlist limited to the model APIs and Git hosts actually in use (measured
  2026-10-19: `api.anthropic.com`, `platform.claude.com` for token refresh, GitHub)
- [ ] Per-agent memory and CPU limits (ADR-023: `AC_AGENT_CPUS`, `AC_AGENT_MEMORY_MIB`; built, to verify on
  BaileyServ); cap on concurrent builds (the job gate, ADR-013)
- [ ] Per-agent disk quota on the workspace volume (ADR-023: `AC_AGENT_DISK_GIB`, a btrfs simple quota; built, to
  verify on BaileyServ)
- [ ] Secrets in 0600 root-owned files; never in images, repo, prompts or logs
- [ ] Agent user has no sudo and no access to the containerd socket
- [ ] No git remotes with embedded tokens in workspaces
- [ ] Data directory (registry + event logs) owned by the control-plane user, mode 0700; one process per directory is enforced by a lock
- [ ] Backups for the data directory and workspace volumes, with a tested restore (workspaces: ADR-021, tested on
  loopback filesystems, to verify on BaileyServ; the rest of the data directory: not yet)
- [ ] Production desktop image: Chromium managed policy applied; DevTools policy decided
- [x] Terminal viewer: view-only by default, enforced in the gateway; one take-over holder;
  agent output sanitised to text and SGR before the browser (verified 2026-10-20, see [Viewer](#viewer))
- [x] No `ctr task exec` ever gets stdin EOF while its guest process lives, from a pipe, a file or
  `/dev/null` (it wedges the Kata shim); every gateway exec holds its stdin open (verified 2026-10-21:
  `/dev/null` wedged a1 at the 126th exec, held stdin passed 200 and 300); the watchdog recovers a wedge
  where killing `ctr` clients frees it, acting only on sustained failures on an idle host and logging every
  pid it kills (ADR-012; verified 2026-10-22: a deliberate wedge recovered in 117.5 s, and nothing was killed
  with 3 agents under load)
- [x] A create or resume whose guest would not fit `/dev/shm` is refused (507), for every token; guest RAM lives
  there and a full `/dev/shm` kills VMs (ADR-012, verified 2026-10-22)
- [x] Task API: job state in the host registry, agent output served as inert text, prompts never in an
  argv, permission mode and tool list set by the operator (verified 2026-10-21, see [Task API](#task-api))
- [x] One token per caller with the least scopes and agents it needs; tokens stored only as SHA-256 digests in
  a root-only 0600 file; every route mapped to a scope by a table a test checks (ADR-011, see
  [API tokens](#api-tokens); verified 2026-10-22: a jobs token for a1 ran a job, got 403 on pause and take-over,
  404 for a2, and 401 within 2 s of its revoke)
- [x] The agent image's PID 1 is `tini`, which reaps orphans, so killed jobs leave no zombies; Claude Code never
  self-updates (`DISABLE_AUTOUPDATER=1`) (verified 2026-10-22)

## Credentials for model access

Each agent uses its own login (for example a Codex or Claude Code login under the agent
user), as Cloudroom core does. Claude Code's login is kept on the agent's workspace
(`CLAUDE_CONFIG_DIR=/workspace/.claude`) so it survives pause and resume. On the host that
is `/var/lib/agent-cloud/workspaces/<id>/.claude`, owned by `AC_UID` (default 1000), so a
host user with that uid can read every agent's login. `AC_UID` must equal the image's agent
uid, so on a shared host rebuild the image with an otherwise unused uid and set `AC_UID` to it. Do not put inference credentials in control-plane
environment files. A request flow that asks the owner for a secret through a form, not
chat, is worth building (Cloudroom's `cloud-secrets` skill is a reference).

## API tokens

Every call under `/api/` needs a bearer token from `/etc/agent-cloud/tokens` (ADR-011). Each token has a name,
scopes and agents; the gateway stores only its SHA-256 digest, compares in constant time, and refuses to start
with a tokens file that is unreadable, group/world accessible, not owned by root or malformed (no fallback to
open). Logs carry the token's name, or for an unknown token the first 4 bytes of its digest; never the token
or the `Authorization` header.

What a stolen token gives its holder (on the private bind, until revoked):

| Token | Can | Cannot |
|---|---|---|
| `jobs:read,jobs:write` for `a1` | Run prompts in a1, which with the default tool list means running code as a1's agent user inside a1's VM (see [Task API](#task-api)); read a1's job output; cancel a1's jobs | Pause, resume, destroy or purge any agent; watch or type into any terminal; read terminal history; see that other agents exist (404 for every other id, the list shows only a1) |
| `viewer:read` | Watch the listed agents' terminals live (any of their tmux windows) and read their history, which shows whatever the agents printed, secrets included | Type, take over, run jobs or change any agent's state; picking a window changes only its own view |
| `viewer:control` (with `viewer:read`) | Take over the listed agents' shells: everything their agent users can do, including their logins | Change agent state or run jobs through the API (its shell access makes this moot inside those agents) |
| `egress:write` (with `agents:read`) | Attach any installed profile to the listed agents, so those agents can reach its hosts (for `rust`, fetch and build any published crate) | Define hosts, change the base list, or reach anything itself; every change is audited |
| `admin` (or the legacy `AC_TOKEN_FILE` token) | Everything on every agent: create, destroy and purge workspaces, run jobs, take over shells | Reach the host beyond what the agents' VMs and the egress allowlist allow |

Revoke with `sudo ac-gateway token revoke <name>`; the gateway stops honouring it within a second. Audit lines
(`audit token=<name> action=...`) in the journal and in each agent's history show what a token did.

## Viewer

The terminal viewer (ADR-006) shows an agent's tmux session `main` and lets one viewer take over
its input.

What it protects:
- **The token** travels only in the `Authorization` header. The page asks for it and keeps it in
  memory: never in the URL, a cookie or browser storage. Watching needs `viewer:read`, taking over and
  typing `viewer:control` (ADR-011); a viewer id works only with the token that opened it. There are no cookies, so a cross-site
  page cannot ride a session; Origin (and `Sec-Fetch-Site`) are checked on every request anyway.
  The page itself carries no data and is served without the token, since a browser cannot send a
  header when it navigates.
- **View-only is enforced by the gateway, not by tmux or the page.** A viewer without the
  take-over lock gets 403 for any input, before anything is built. The shared view client can
  only be sent a closed set of commands (`ViewCommand`: client name, list windows, cursor info and
  snapshot of `main` or of a window); the take-over client exists only while someone holds the
  lock and receives only `send-keys -H` hex and `refresh-client -C` lines, so no viewer byte can
  become a tmux command. Picking a window to watch sends only a window id the gateway parsed into
  a number and found in its own list (`@<digits>`; anything else is 400), and input then goes to
  that window's pane by the pane id from the same list. A pick never runs `select-window`, so it
  never moves the agent or other viewers. tmux's
  `read-only` flag is not used: it made tmux refuse the agent's own tmux commands while anyone
  watched. **An agent can always use its own tmux while someone watches.**
- **Agent to viewer:** the gateway turns screen snapshots into plain text plus colour numbers. OSC
  sequences (hyperlinks, clipboard writes, titles), DCS, APC, cursor movement, control characters
  and bidi overrides are dropped before anything is sent; the page sets text with
  `textContent`; its CSP allows only its own script, style and API (`default-src 'none'`). An
  agent can garble its own viewer (for example by printing lines that mimic tmux's protocol),
  never another agent's, and never run anything in the browser.
- **Resources:** 4 viewers per agent and 16 in all, below rusty_serve's 64 connections, so viewers
  cannot starve the lifecycle API (tested). A viewer that falls 256 messages behind is dropped;
  input is capped (64 keys, 4 KiB typed, 64 KiB pasted per request).
- **Recording:** while viewed, the agent's output and every take-over, forced take-over and
  release are logged per agent (`/var/lib/agent-cloud/history/<id>`, root, 0700, 16 MiB kept).
  **Keystrokes are not recorded**, because they can hold secrets typed during a take-over; a
  take-over appears only as its start and end. What the terminal echoes is output and is recorded
  (a typed command line appears; a password at a no-echo prompt does not).

What it does not protect:
- **A take-over holder has full control of the agent's shell**, and so of everything the agent
  user can reach: its workspace, its Claude Code login (`/workspace/.claude`), its Git credentials.
  Treat a `viewer:control` token, and a take-over, as equivalent to agent access.
- Anything the agent prints is shown and recorded, secrets included.
- The gateway listens on loopback here; reach it through an SSH tunnel (encrypted) or a tailnet
  address. There is no TLS of its own.
- A browser that vanishes without closing its connection holds its seat until TCP gives up; a
  closed one is noticed within about 5 s.

## Task API

Jobs (ADR-010) run a prompt in an agent with `claude -p`.

**Who can call it:** a token with `jobs:write` (submit, cancel) or `jobs:read` (status, output) for the agent,
on the private bind (ADR-011). Give a script or another agent a token with just those scopes and just its
agents: it cannot pause, destroy, watch or take over anything, nor learn which other agents exist.

**What a job can do:** it runs as the agent user in that agent's VM, with its login, its workspace and its
egress allowlist. A headless `claude` cannot ask for approval, so the gateway passes the operator's choice:
- default `AC_JOB_PERMISSION_MODE=acceptEdits` with the default `AC_JOB_ALLOWED_TOOLS`: read and search any
  file the agent user can read; create and edit files in `/workspace`; `git status|diff|log|add|commit`
  (not `push`); `ls`, `cat`, `sleep`; `python3` and `cargo build|test|check|fmt`. `acceptEdits` also
  approves Claude Code's common filesystem commands inside `/workspace` on its own (a job ran `rm -f` on a
  workspace file, 2026-10-21). Other commands are denied and the job carries on without them (a job's
  `curl` was refused and listed under `permission_denials` in its stream log).
- `python3` and `cargo` run arbitrary code (a `-c` argument, a build script), so with the default list a
  job can in effect do anything the agent user can do inside its VM. Drop them for jobs that only read and
  edit, if you want the list to mean more.
- `AC_JOB_UNSAFE=1` passes `--dangerously-skip-permissions`: no list at all. Tradeoff: the VM, the egress
  allowlist and the isolation between agents are the boundary in every mode; the permission list is a
  second layer that stops a confused or prompt-injected job from casually running commands outside it.
  Recommendation: keep `acceptEdits` with a list sized to the work.

**Shared start gate (ADR-013):** jobs of all agents share a host-wide cap and a CPU-pressure gate, so one
agent's jobs can delay another's: a token for several agents can occupy several slots (one per agent), and a
CPU-heavy job can hold every start at "host busy". A job's `queued_reason` and `GET /api/v1/capacity`
(`agents:read`) tell a caller that the host is full or busy and how many jobs run, never whose. Nothing about
what a job may do changes.

**Untrusted output:** the result and the stream log are written by the agent, and a prompt-injected agent
writes what it likes. The gateway caps them (256 KiB, the last 1 MiB), never parses them, and serves them as
`text/plain; charset=utf-8` with `X-Content-Type-Options: nosniff`, `Content-Security-Policy: default-src
'none'; sandbox` and `Cache-Control: no-store`, so a browser will not render or run them. A caller must treat
them as data from an untrusted source too (for example, never paste them into a shell).

**What the agent cannot change:** a job's state, times, timeout and exit code are in the host registry. The
agent can fake its own job's result and exit code (it runs the job), never another agent's. The gateway
(root) never writes, chowns or reads a path below the workspace root: the prompt goes in with
`O_CREAT|O_EXCL` at the root of the bind mount, which follows no symlink, and output is read in the VM, as
the agent user, by capped execs. Prompts are never in an argv; history records only job ids and states.

## Known gaps

- A container-in-VM boundary is only as strong as the hypervisor and guest kernel config.
  Keep Kata and the host kernel patched.
- The command guard only blocks known-bad patterns.
- Tokens expire only if created with `--expires` (ADR-022); otherwise revoke them. An expired token stays in
  the file (listed as expired) until revoked. `admin` (and the deprecated legacy token) can do everything.
- Failed authentications are limited per gateway (20 a minute are logged, then 429), not per source address:
  rusty_serve does not tell handlers the peer's address. Tokens are 256-bit, so this bounds log volume rather
  than guessing.
- Egress control limits destinations, not content: an agent can still send data to an allowed
  host (a GitHub gist, a model API request). TLS is not inspected, so domain fronting on shared
  CDN addresses is possible. See ADR-007.
- Egress profiles widen an agent's reach to whole package registries, which deliver code. Attach them to the
  agents that build, not by default (ADR-016).
- Screen streaming exposes whatever the agent's browser shows, including anything a logged-in
  session reveals. Treat viewer access as equivalent to agent access.
