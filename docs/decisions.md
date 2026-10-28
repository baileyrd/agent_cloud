# Architecture decision records

Status values: **accepted**, **proposed** (default until built and tested), **open**.

## ADR-001: Per-agent isolation with Kata Containers on KVM
**Status:** proposed.
**Context:** Agents run untrusted code without approval prompts. Containers share the host
kernel; full VMs are heavy to build and manage by hand.
**Decision:** One Kata microVM per agent, via containerd runtime class.
**Consequences:** Hardware isolation with a container workflow. Requires KVM. Kata's
Fedora packaging and SELinux interaction are **[unverified]**.
**Evidence:** ChatGPT Dots' sandbox boots under Kata on KVM ([research-notes.md](research-notes.md)).
**Alternative:** gVisor (no virtualization needed, weaker boundary, emulated kernel).

## ADR-002: Fedora Server as host
**Status:** accepted (owner's choice).
**Consequences:** SELinux enforcing by default; cgroup v2 by default. Cloudroom's Ubuntu-only
installer cannot be used directly.

## ADR-003: Do not run Cloudroom core as-is
**Status:** proposed.
**Context:** cloudroom-core is a pre-release Rust service (Apache-2.0) for running coding
harnesses with Postgres-backed history (we use an embedded store instead; ADR-009). Its installer assumes Ubuntu 24.04, systemd, ext4
filesystem quotas and `/etc/fstab`, and its docs put containers out of scope. Its README
claims Codex, Claude Code, Pi and Cursor; the setup guide documents Codex and Pi.
**Decision:** Borrow its API shape, session model and security ideas; build a thin runtime
and history layer instead. Keep one Cloudroom-in-a-full-VM instance as an optional fallback.
**Consequences:** More to build; no Ubuntu/quota porting.

## ADR-004: Pod shape
**Status:** open.
**Options:** (a) one container per agent running shell and desktop under a supervisor;
(b) two containers in one pod (shell, desktop) sharing a workspace volume.
**Leaning:** (a) first, since it needs no pod tooling (no `crictl runp` or k3s).
**Revisit when:** you need to restart or upgrade the desktop independently of the shell.

## ADR-005: Control-plane deployment shape
**Status:** accepted.
**Decision:** One modular Rust process, built as a Cargo workspace: `ac-domain` (pure logic),
`ac-ports` (traits), then adapter crates (spawner, store, gateway). Extract a separate service
only for a concrete forcing function (independent scaling, language boundary, fault isolation).
**Why Rust:** single static binary on the host, lifecycle and allowlist rules as types, and
reuse of ports-and-adapters patterns from the owner's existing Rust tooling.
**Consequences:** Sync ports throughout; the viewer uses threads and a streamed HTTP response, not
async (ADR-006).

## ADR-006: Viewer (terminal now, desktop later)
**Status:** accepted for the terminal (2026-10-20, verified on BaileyServ); the desktop stream
is still open.
**Decision (terminal):** the owner watches an agent's tmux session `main` in a browser and can
take over its input, while the agent keeps running. All of it lives in `ac-gateway` (one
process owns the registry lock); bearer-token auth and the private-bind check are unchanged.
- *Session:* the image has tmux. The gateway creates `main` in `/workspace` as the agent user
  (with the image's environment) if it is missing. It survives browsers closing and is shared by
  the agent and every viewer.
- *Bridge:* the host reaches the VM only through `ctr task exec`, and `unsafe_code` is forbidden,
  so there is no host PTY. One `tmux -C attach -f ignore-size` control client per viewed agent,
  over plain pipes, feeds every viewer. On `%output` it asks tmux for the cursor and a
  `capture-pane -e` snapshot (one in flight at a time, which batches bursts). tmux is the
  terminal emulator, so resize, UTF-8, colours, vim and `claude` render as tmux renders them
  (all verified). Rejected: a host helper PTY (`script` is not installed, `ctr -t` needs a
  console, and resizing would mean `stty` on a hidden PTY), and one exec per keystroke or capture
  (about 100 ms each on Kata).
- *Kata hazard:* without `-t`, `ctr` never passes stdin EOF to the guest, and EOF from a pipe or
  file while the guest process lives wedges the shim until that `ctr` is SIGKILLed (measured; it
  caused an earlier a1 freeze). Every long-lived exec is an `ac_ctr::Exec`, whose only way to end
  is quit command, 3 s, SIGTERM, 3 s, SIGKILL, reap, then drop stdin (also on panic). systemd's
  default `KillMode=control-group` sends SIGTERM to the gateway and its `ctr` children together,
  and `ctr` forwards it, so a restart is clean. At startup the gateway SIGKILLs leftover `ctr`
  clients and detaches stale control clients. A watchdog probes running agents every 30 s
  (`AC_PROBE_INTERVAL`) and acts only on sustained failures on a host that is not busy, through a
  logged ladder (ADR-012; the first version used a 2 s probe and killed every client on one
  failure, which mistook slow agents for wedged ones under load).
- *Transport:* rusty_serve has no WebSocket upgrade, so a `fetch()`-streamed chunked GET carries
  NDJSON (a full snapshot with scrollback on connect, then changed lines and the cursor), and
  POSTs carry input, resize and take-over. `fetch` sends `Authorization`, so no ticket is
  needed. Origin is checked on every request; caps of 4 viewers per agent and 16 in all leave
  rusty_serve's 64 connections for the API; viewer routes never take the lifecycle lock.
- *Rendering:* the gateway turns snapshots into styled runs (SGR only) and drops every other
  escape (OSC 8/52, titles, DCS, APC, C0/C1, bidi overrides). The page (about 330 lines of
  in-repo JS, no third-party code, CSP `'self'` only) sets text with `textContent`.
- *View-only and take-over:* view-only is the default. The guarantee rests on the gateway, not
  tmux: input from a viewer without the lock gets 403 before anything is built; the view
  client's whole vocabulary is a closed enum (`ViewCommand`: client name, list windows, cursor
  info and snapshot of `main` or of one window id); the take-over client exists only while a
  viewer holds the lock and takes only `Input`, whose bytes are sent as `send-keys -H` hex (key
  names are mapped to terminal bytes), so no viewer byte can be read as a tmux command. tmux's `read-only` flag was tried and dropped:
  while such a client is attached, tmux refuses the agent's own tmux commands, even from inside
  its pane. One viewer holds the lock; a second take-over is refused unless forced; release or
  disconnect returns to view-only.
- *Windows (2026-10-22):* `Ctrl-b` does not reach tmux from a browser (it is a browser shortcut),
  so the page lists `main`'s windows (`list-windows`, refreshed on tmux's window notifications),
  marking the agent's current one and `job-*` windows, and a click picks one for that viewer only
  (`POST .../term/{viewer}/window`, body `@<n>` or `active`; `viewer:read`). The gateway never runs
  `select-window`, which would move the agent and every viewer: it snapshots each watched window by
  id (`display`/`capture-pane -t @<n>`), the first time by one exec for the picking viewer, and a
  taker's input goes to that window's active pane by the pane id from the list (`send-keys -t
  %<n>`). The only viewer text that reaches a command is a window id of exactly `@<digits>` (no
  leading zero) that is in the latest list, parsed into a number and rendered back from it;
  anything else is 400. A watched window that closes (a notification, or a failed capture) sends
  its viewers back to the agent's window. Picks are capped at 4 execs at once (429 beyond).
- *Recording:* while viewed, output chunks and take-over events go to the per-agent `ac-history`
  log (`<data>/history/<id>`, 0700, 1 MiB segments, 16 MiB kept). Keystrokes are not recorded.
  `GET .../history` returns plain text with every escape stripped.
**Consequences:** a viewer with the lock has the agent's shell, and so its login. Output while
nobody watches is not recorded (it stays in tmux scrollback). A vanished browser is noticed at
the next keep-alive write (5 s); a half-open connection holds its seat until TCP gives up.
**Open (desktop):** Dots' viewer protocol is unknown. Options: KasmVNC (bundles X server, VNC
and web client), Xvfb/x11vnc plus noVNC, a WebRTC streamer (Neko, Selkies), or ffmpeg
`x11grab` into the gateway. Leaning: KasmVNC behind the gateway, on the same transport and lock.

## ADR-007: Egress control
**Status:** accepted (2026-10-19; verified on BaileyServ; agent isolation and the firewalld zone
added 2026-10-20).
**Decision:** Agents have no direct route anywhere. The only way out is `ac-egress`, an HTTP
forward proxy on the agent bridge (`10.89.0.1:3128`, never `0.0.0.0`). It serves `CONNECT` and
plain absolute-URI requests to allowlisted hostnames on ports 443 and 80. nftables
(`deploy/agentfw.nft`) makes it the only path: from `acbr0` the host accepts only TCP to the
proxy, and nothing is forwarded to or from the bridge.
- Agents cannot reach each other. Frames between two bridge ports never reach the `inet` hooks
  (no `br_netfilter`), so a `bridge`-family rule drops every frame forwarded within `acbr0`. Host
  traffic is delivered locally, not forwarded, so the proxy is unaffected. (The CNI bridge
  plugin's `portIsolation` would also work, but `cni-agents.json` is written once per host.)
- firewalld evaluates the same packets after `agentfw`, so an `accept` there is not enough. The
  `agents` zone (`deploy/firewalld-agents.xml`) binds `acbr0` by name and accepts only TCP 3128.
  Before it, the proxy was reachable only because the default zone allowed TCP 1025-3388.
- The proxy checks the allowlist first and only then resolves the name, on the host. Agents
  get no DNS. Every address in the answer must be public (not private, loopback, link-local,
  CGNAT/tailnet, multicast, documentation or reserved), and the proxy connects only to those
  vetted addresses, with no second lookup. IP literals are refused, including disguised forms
  such as `127.1`.
- A plain request is forwarded with a `Host` header naming the vetted target, not the client's.
- The allowlist is a root-owned 0600 file (`/etc/agent-cloud/egress-allow.txt`), re-read on
  SIGHUP or when it changes; a bad version keeps the previous list. The proxy logs one line per
  decision (agent IP, host:port, allow or deny and why), never headers or bodies.
- The bridge has no NAT (`ipMasq: false`), so even without the firewall agents have no working
  route out.
- The agent image sets `HTTPS_PROXY` and friends as a convenience. It also turns off Claude
  Code's telemetry and its claude.ai connectors, which through the agent's login would reach
  the owner's connected accounts (Gmail, Drive and others). The proxy denies them regardless.
**Consequences:** Adding a host means editing the allowlist (deny lines in
`journalctl -u ac-egress` show what is missing). The proxy does not stop:
- exfiltration to an allowed host: a push to any repo or gist the agent's credentials reach, or
  data inside model API requests. The allowlist limits where data can go, not what goes there;
- domain fronting: TLS is not inspected, so inside a tunnel to an allowed name a client can name
  another site served from the same CDN address. Many CDNs refuse a mismatched SNI and Host,
  but nothing here enforces it;
- broad wildcards: `*.githubusercontent.com` admits any public repo's raw files.
DNS tunnelling is closed: agents have no resolver, and the proxy resolves only allowlisted names.
**Revisit when:** the gaps above matter enough for SNI checks or TLS interception. Per-agent lists:
ADR-016 (profiles on top of this base list).

## ADR-008: Reuse rusty_mill crates via a pinned git dependency
**Status:** accepted.
**Decision:** Depend on `Rusty-Mill/rusty_mill` crates by `git` + `rev` in `[workspace.dependencies]`
(all are `publish = false`). Used today: `platform`, `platform-mock` (process spawning and its test
double), `rusty_multimodal_db_engine`. Planned: `rusty_serve`, `rusty_agui`, `rusty_sandbox`.
**Consequences:** No vendoring; bumping the rev is a deliberate commit. Cargo fetches the monorepo.

## ADR-009: Embedded storage instead of Postgres
**Status:** accepted (supersedes the Postgres lines in the original design).
**Context:** One trusted owner, one host. The engine keeps whole records in RAM, so it suits a small
registry but not unbounded event history ([issue 774](https://github.com/Rusty-Mill/rusty_mill/issues/774)).
**Decision:** The registry (agents, lifecycle state) lives in `rusty_multimodal_db_engine` behind
`SessionStore` (`ac-store`). Events go to an append-only, size-rotated segment log per agent
(`ac-history`) with retention by pruning old segments.
**Consequences:** No database service to install, secure or back up; no ad hoc SQL (an `ac-ctl`
inspect command is needed); record layout changes need a schema-tag bump and migration.
**Revisit when:** a second consumer needs the same segment log (hoist it as its own crate) or the
registry outgrows RAM.

## ADR-010: Task API (jobs)
**Status:** accepted (2026-10-21; verified on BaileyServ with a1).
**Context:** A script or another agent should be able to say "run this prompt in agent X" and get a job
id, live status and the result, without exec-ing into the VM. Everything inside the VM and its workspace
is writable by the agent, so a prompt-injected agent can tamper with any file it writes there.
**Decision:** Jobs are part of `ac-gateway` (`jobs.rs`), under `/api/v1/agents/{id}/jobs`, with the same
Origin and bind rules as the rest of the API; since ADR-011 a caller needs a token with `jobs:read` or
`jobs:write` for the agent, not the owner's.
- *Routes:* `POST .../jobs` `{"prompt", "timeout_secs"?, "idempotency_key"?}` answers 202 with the job;
  `GET .../jobs` (the agent's retained jobs, newest first); `GET .../jobs/{job}[?wait=N]` (N <= 30 s:
  long-poll until the state changes; 8 waiters at most, then 429); `GET .../jobs/{job}/output` (the final
  assistant message; 409 while running) and `.../output?part=stream` (the stream-json log, live while
  running), plain text; `POST .../jobs/{job}/cancel`. Bodies are parsed with `rusty_json` (first-party,
  ADR-008); unknown members are refused.
- *States:* `queued`, `running`, `succeeded`, `failed`, `cancelled`, `timed_out`, `interrupted`, as a pure
  state machine in `ac-domain` (`job.rs`). One running job per agent; up to 8 queue behind it (429 beyond),
  in submission order. A job is accepted only while the agent is `running` (409 otherwise; resume is an
  explicit call); the check and the queueing happen under the lifecycle lock, so a pause cannot slip
  between them.
- *Trust:* state, timestamps, timeout and exit code live in the host registry (`ac-store`, `jobs.mmap`
  beside `agents.mmap`, same lock). What the agent writes (the result and the stream log) is untrusted
  data: capped, served as `text/plain` with `nosniff` and `Content-Security-Policy: default-src 'none';
  sandbox`, never parsed by the gateway.
- *Prompt delivery without stdin:* the prompt is kept host-side (`<data>/jobs/<agent>/<job>/prompt.txt`,
  root, 0700 directories). At start the gateway creates `.job-<job>.prompt` (0600, owned by `AC_UID`) in the
  root of the agent's workspace with `O_CREAT|O_EXCL`, which never follows a symlink. Below the workspace
  root everything is agent-controlled, so the gateway (root) never writes, chowns or reads a path there:
  a symlinked `.jobs` would turn a root write into a write anywhere on the host, and a symlinked result
  would turn the output API into a reader of host files (for another agent calling the API, too). A start
  exec, as the agent user in the VM, makes `.jobs/<job>/` (0700), moves the prompt to `prompt.txt` and
  opens tmux window `job-<job>` in session `main`. Every exec is an argv array: a constant script plus
  the job id (gateway-generated, 16 hex digits) and operator settings as positional parameters. The
  prompt is in no argv, host or guest (checked live with `/proc/*/cmdline`); `claude` reads it on stdin
  from the file.
- *Running:* the window runs `claude -p --output-format stream-json --verbose <permission flags>
  < prompt.txt` through `head -c 16 MiB` (a job whose log passes that is stopped) and `tee stream.jsonl`,
  with a readable digest on the terminal, so a human can watch it in the viewer and take over. When
  `claude` exits the runner writes `result.txt` (the `result` event, at most 256 KiB), then `exit_code`
  (via rename), then the marker `.job-<job>.done` in the workspace root. Verified: `claude -p` works
  non-interactively with the image's settings, streams, and contacts only `api.anthropic.com`.
- *Supervision:* one gateway thread ticks every second. For a running job it `lstat`s the marker on the
  host (a name directly in the trusted workspace root: nothing followed, nothing opened). It execs a
  status script only when the marker appears (at most every 3 s, so a fake marker is cheap) or every
  30 s, to notice a runner that vanished; the script finds the window by exact name in `list-windows`
  (`display -t` falls back to the current pane when the target is missing, which made the agent's own
  shell look like the job). On exit two execs copy the capped result and the last 1 MiB of the log to the
  host job directory, so output stays readable after the agent is paused.
- *Execs never see stdin EOF:* `CtrRuntime::exec_once` (start, status, kill and read, and also the
  watchdog probe and the viewer's short execs) runs `timeout -s TERM -k 3 <limit> ctr ... task exec` as an
  `Exec` with piped stdin that is dropped only after the process is reaped. Measured on a1: 125 execs of
  `sh -c 'sleep 0.2; echo x' </dev/null` and the 126th wedged the shim (a1 left `ctr task ls`), while 200
  with stdin held open did not, and 300 job-output reads through the new `exec_once` did not. The first
  version of this API polled with `/dev/null`-stdin execs and wedged a1 twice.
- *Timeouts and cancel:* default 30 min, 1 s to 4 h, enforced by the gateway from the registry's start
  time. Cancel and timeout run a kill exec: it finds the window by name, walks its process tree (children
  by parent pid, plus the pane's session), stops every process, kills it, closes the window by id, and
  fails unless all of them are gone (zombies count as gone). The runner also wraps `claude` in `timeout`
  at the deadline plus 5 min, a backstop for a gateway that is down.
- *Lifecycle:* pause or destroy of an agent with a queued or running job is 409 unless `?force=true`;
  then, before the VM stops, the running job is killed and marked `interrupted` and queued ones
  `cancelled`. Startup reconcile (and every tick) does the same for agents that are not running, without
  the kill. Queued jobs are cancelled, not kept: resuming is an explicit, possibly much later act, and
  running stale work then would surprise the caller. A gateway restart re-attaches: a job recorded
  `running` is probed, never started again (and the start exec refuses an existing `.jobs/<job>`). The
  job is marked `running` before its start exec, so a crash between the two shows up as `failed`
  ("runner ended without an exit code"), never as a duplicate. Purging an agent forgets its jobs.
- *Permissions:* a headless `claude` cannot ask. `AC_JOB_PERMISSION_MODE` (default `acceptEdits`; also
  `auto`, `dontAsk`, `manual`, `plan`) and `AC_JOB_ALLOWED_TOOLS` (comma-separated; default `Read`, `Glob`,
  `Grep`, `Edit`, `Write` and `Bash` patterns for `git status|diff|log|add|commit`, `ls`, `cat`,
  `python3`, `cargo build|test|check|fmt`, `sleep`) become flags. `bypassPermissions` /
  `--dangerously-skip-permissions` only with `AC_JOB_UNSAFE=1`. See security.md for what that allows.
- *Limits and retention:* prompt <= 64 KiB (413 beyond); per agent the last 20 finished jobs are kept
  (registry records and host copies), and the workspace keeps the last 5 `.jobs/<job>` directories, each
  log at most 16 MiB, so job logs take at most about 85 MiB of a workspace.
- *Idempotency:* the same `idempotency_key` for the same agent returns the retained job with that key
  (200) instead of creating one, whatever its prompt or state. Keys expire with their job's record.
- *Events:* queued, started, the outcome (with the exit code), cancelled and interrupted go to the
  agent's `ac-history` log as `job <id> <state>`: ids and states only, timestamps from the log, no prompt
  or output. `GET .../history` shows them.
**Consequences:** jobs run with the agent's own login and permissions; the permission list is a second
layer and the VM, egress allowlist and isolation are the boundary. A job's outcome is what the agent's
VM reports: a compromised agent can fake its own job's exit code and result, never another agent's, and
never the registry's state or times. Output is read through execs, so a wedged agent delays collection.
The image's PID 1 is `tini` (running `sleep infinity`), which reaps orphans, so a killed job leaves no
zombies (verified 2026-10-22: none after a cancel through the API, a `pkill` of `claude` and killed
orphans; before, with `sleep infinity` as PID 1, they stayed until the agent was paused).
**Not now:** `rusty_agui` (AG-UI events over SSE) fits a later per-job event stream that turns the
stream-json log into `RUN_STARTED`/`TEXT_MESSAGE_*`/`TOOL_CALL_*`/`RUN_FINISHED`; it does not fit the
control plane (AG-UI has no queue, job id or re-attach; a run is one request). Long-polling the status
is enough for now.

## ADR-011: Scoped API tokens
**Status:** accepted (2026-10-21).
**Context:** One bearer token (`AC_TOKEN_FILE`) could do everything: lifecycle, jobs, viewer take-over.
Handing a script or another agent a token to submit jobs also let it destroy agents and type into their shells.
**Decision:** Many named tokens, each with scopes and a set of agents, checked by the gateway before any handler.
- *File:* `/etc/agent-cloud/tokens` (`AC_TOKENS`), owned by the gateway's user (root), mode 0600, one token per
  line: `name scopes agents sha256 [created]`; `#` comments. `scopes` is a comma list of `agents:read`,
  `agents:write`, `jobs:read`, `jobs:write`, `viewer:read`, `viewer:control`, `egress:write` (ADR-016), `admin`
  (implies all); `agents` is
  `*` or a comma list of agent ids; `sha256` is the hex digest of the token; `created` is Unix seconds (a fifth,
  optional field, added for `token list`). Only digests are stored: a token is 32 bytes from `/dev/urandom` as
  hex, printed once by `ac-gateway token create <name> --scopes ... [--agents ...]`. `token list` shows names,
  scopes, agents and creation times; `token revoke <name>` removes a line. The CLI edits only this file (under a
  `flock` on its directory, then an atomic 0600 rewrite with `rusty_atomic_file`), never the registry, so it
  works while the gateway runs.
- *Reload:* as ac-egress's allowlist: re-read on SIGHUP (`systemctl reload ac-gateway`) and when its mtime,
  length or inode changes (checked each second). At startup a file that is unreadable, not a regular file,
  owned by another user, group/world accessible, over 64 KiB or malformed (unknown scope, bad agent id, bad
  digest, duplicate name or digest) stops the gateway; so does having no token at all. A bad later version is
  logged and the current tokens stay; a deleted file leaves only the legacy token. A revoked token's open
  viewer streams end at their next message or keep-alive (within about 5 s).
- *Legacy:* `AC_TOKEN_FILE` still works and is the `admin` token for every agent named `legacy` (a reserved
  name). Deprecated: the gateway says so at startup; migrate as in install.md and drop it later.
- *Matching:* the presented `Bearer` token is hashed with SHA-256 (`rusty_sha2`) and compared with every
  digest using `rusty_crypto_key::constant_time_eq` (both first-party, ADR-008).
- *Routes:* every route under `/api/` has exactly one entry in `access::ROUTES`, checked before dispatch. A path
  with no entry is 404 for every token, so a route added to a handler without an entry is unreachable; a test
  enumerates every path built from the table's words and checks that the handlers accept exactly the paths the
  table lists.

  | Route | Scope |
  |---|---|
  | `GET /api/v1/whoami` (name, scopes, agents; no secrets) | any valid token |
  | `POST /api/v1/mcp` (MCP; each tool call is checked as the route it names, ADR-017) | any valid token |
  | `GET /api/v1/agents` (lists only the token's agents) | `agents:read` |
  | `GET /api/v1/capacity` (admission figures, ADR-012; counts only, no ids) | `agents:read` |
  | `GET /api/v1/agents/{id}` | `agents:read` |
  | `POST /api/v1/agents/{id}` (create) | `agents:write` |
  | `POST /api/v1/agents/{id}/pause`, `.../resume` | `agents:write` |
  | `DELETE /api/v1/agents/{id}` (destroy; `?purge=true`, `?force=true`) | `agents:write` |
  | `GET .../{id}/jobs`, `.../jobs/{job}`, `.../jobs/{job}/output` | `jobs:read` |
  | `POST .../{id}/jobs`, `.../jobs/{job}/cancel` | `jobs:write` |
  | `GET .../{id}/term` (stream), `.../history`; `POST .../{id}/term/{viewer}/window` (pick a window to watch) | `viewer:read` |
  | `POST .../{id}/term/{viewer}/takeover` (`?force=true`), `release`, `keys`, `text`, `paste`, `resize` | `viewer:control` |
  | `GET /api/v1/egress/profiles`, `GET .../{id}/egress` (ADR-016) | `agents:read` |
  | `PUT .../{id}/egress` (set its profiles; also `egress_profiles` on create) | `egress:write` |
  | `GET .../{id}/checkpoints` (ADR-018) | `agents:read` |
  | `POST .../{id}/checkpoints`, `DELETE .../checkpoints/{cp}`, `POST .../{id}/restore`, `.../fork` (the token must also be for the fork's new agent) | `agents:write` |

- *Agents:* a token restricted to some agents gets 404 for any other agent id, existing or not, before its scope
  is looked at, so it cannot probe for ids. (A wrong method is still 405; that depends on the path's shape only.)
- *Viewer ids* belong to the token that opened the stream: another token, even `admin`, gets 404 for them, so
  input and take-overs are attributable to one token.
- *Failures:* 401 for a missing or unknown token, logged as `auth: unknown token sha256:<first 4 bytes of its
  digest>`, never the token or the header; 403 and 404 for a known token, logged with its name. After 20
  failures in 60 s, further ones get 429 and are not logged until the window ends; valid tokens are never
  refused for that. rusty_serve does not give handlers the peer address, so the count is per gateway, not per
  source address; the limiter keys by source (bounded to 1024) and gets the address once rusty_serve passes it.
- *Audit:* every state-changing call that passed the checks (create, pause, resume, destroy, job submit and
  cancel, take-over, release, egress, checkpoint, restore, fork; ADR-018) writes `audit token=<name> action=<action>
  [force=true] [purge=true] [job=<id>] status=<code>` to the agent's `ac-history` log (shown by `GET .../history`) and the same line with
  `agent=<id>` to the gateway's journal. Viewer input is not audited per call: it comes from the viewer whose
  take-over was. No prompt, body or terminal content.
- *Viewer page:* asks `whoami` after the token is entered, shows the token's name and scopes, and hides
  **Take over** without `viewer:control` (the gateway refuses it anyway).
**Consequences:** a stolen jobs token runs prompts in its agents (which is code execution as the agent user in
those VMs) but cannot pause, destroy, watch or type into them, nor see other agents. `admin` remains as
powerful as the old token. Tokens do not expire; revoke them. There is still no TLS of the gateway's own.
**Not now:** expiry and per-token rate limits; per-address failure limits (needs the peer address from
rusty_serve); scopes narrower than these.

## ADR-012: Admission against /dev/shm; a watchdog that tells slow from wedged
**Status:** accepted (2026-10-22).
**Context:** The capacity benchmark (sizing.md) found two failures that were not the agents' fault.
- *Memory.* Kata starts QEMU with `memory-backend-file,mem-path=/dev/shm,share=on` (verified on a running
  agent: fd `/dev/shm/qemu_back_mem.dimm1.* (deleted)`, a shared 4 GiB mapping). Guest RAM is a file in
  `/dev/shm`, half of RAM by default, and touched guest pages are never given back. At N=6 `/dev/shm` filled and
  four VMs died with `kvm run failed Bad address` while 9.6 GiB of RAM was still free.
- *Watchdog.* Under load the first watchdog's 2 s probe failed for slow agents. On one failure it SIGKILLed
  every exec client of the agent, a running job's included, and marked it unhealthy.

**Decision, admission.** A create or resume is admitted only if both hold:
- *Size:* (configured memory of every agent `Running` or `Creating`) + (the new guest's) + margin ≤ `/dev/shm`
  size.
- *Free space:* (the new guest's configured memory) + margin ≤ `/dev/shm` free now.

How it works:
- *Configured, not measured, memory.* Pages a guest touched never come back, so a long-running agent reaches
  its full size. Paused and destroyed agents do not count.
- *Code.* The rule is `ac_domain::capacity::admit`, pure, with saturating arithmetic. The host is read through
  `ac_ports::Capacity`: statfs(2) of `/dev/shm` (first-party `rusty_libc`, no dependencies of its own), and `default_memory` from the
  first `[hypervisor.*]` table of `/etc/kata-containers/configuration.toml`. If that table does not set it,
  Kata's own default of 2048 MiB is used and the startup log says so.
- *Atomic.* `ac_fleet::Fleet` checks before any side effect. The gateway serves every lifecycle call under one
  lock, so a check and its create cannot interleave with another's (tested).
- *Refusal.* **507** with `{"error","rule":"total"|"free","needed_mib","available_mib","more_agents"}`. `ac-ctl`
  prints the same message. The owner token gets no exception.
- *Visibility.* `GET /api/v1/capacity` (`agents:read`) returns `shm_total_mib`, `shm_free_mib`, `guest_mib`,
  `running_agents`, `committed_mib`, `margin_mib` and `max_more_agents`: counts only, never ids. Both
  `scripts/capacity.sh` and `scripts/post-reboot-check.sh` print it. The check script fails when committed
  exceeds the size.
- *Settings.* `AC_SHM_MARGIN_MIB` (default 512) and `AC_VM_MEMORY_MIB` (overrides the configured size).
  `AC_SHM_LIMIT_MIB` is for testing only: admission treats `/dev/shm` as that big, and the gateway logs a
  WARNING at startup.
- *Host.* `/dev/shm` is sized MemTotal − 8 GiB through `/etc/fstab` (`deploy/dev-shm.fstab`, install.md note 9).

**Decision, watchdog.** What tells slow and wedged apart:
- *A wedged shim* (stdin EOF reached a live exec, ADR-006) never answers again however idle the host is.
  Probes fail fast (measured: 2.0 s, `context deadline exceeded`), `ctr task ls` drops the agent, and only
  SIGKILL of the offending `ctr` client frees it.
- *A slow agent* answers within a generous budget, or fails only while the host is busy (measured: 4.8–8.6 s
  answers with 3 agents under load, load 5.6, io PSI some 28).

The watchdog therefore works as follows (`ac_domain::health`, pure; driver in `ac_gateway::watchdog`, with the
probe, process list, kill, pressure and clock as ports faked in tests):
- *Healthy.* A probe answering within `AC_PROBE_BUDGET` (10 s) is healthy, and a slow answer is logged with the
  pressure.
- *Busy host.* A failure while the host is busy is logged "slow, host busy, not acting" and restarts the
  streak. Busy means 1-minute load > `AC_BUSY_LOAD_PER_CORE` (1.0) × cores, or `/proc/pressure/io` some avg10
  > `AC_BUSY_IO_SOME` (20), or cpu some avg10 > `AC_BUSY_CPU_SOME` (50).
- *Escalation.* It acts only after `AC_PROBE_FAILURES` (3) consecutive failures spanning `AC_PROBE_WINDOW`
  (90 s) on an idle host. Then, re-probing after each rung and stopping as soon as the agent answers:
  1. SIGKILL the gateway's own exec clients of the agent. "Own" means an exec-id prefix in
     `ac_domain::health::OWN_EXEC_PREFIXES` (`acprobe` probes, `acview` viewer bridge, `acexec` one-shots); that
     list is the one definition.
  2. SIGKILL foreign clients of the agent that were already running when it stopped answering; a younger one
     cannot be the cause and is left. Every kill and every client left alone gets its own line: pid, exec-id,
     age, command line, why.
  3. Mark it unhealthy: shown as `"health":"unhealthy"` in its status, cleared by the next answer or when it
     stops running. It never blocks a lifecycle call and does not re-run the ladder while marked.
- *Logging.* Every line goes to the journal and to the agent's history. Live result: a deliberate wedge
  (`printf x | ctr task exec ... cat`) was found 115 s after it started, rung 1 left the foreign client, and
  rung 2 killed it. The agent answered again 117.5 s after the wedge.

**Consequences:**
- Fewer guests run than RAM alone would allow: 3 × 4096 MiB here, or 7 at 2048. That is the honest number,
  because the alternative is VMs dying.
- A wedge is recovered in about 2 minutes instead of 30 s. A wedge during sustained load waits until the load
  drops.
- An operator's own `ctr task exec` on an agent that has been failing for 90 s may be killed at rung 2, with a
  log line naming it.

**Not now:**
- Giving guest memory back (free page reporting, virtio-mem).
- Per-agent memory sizes; admission assumes every guest has the current `default_memory`.
- A probe that bypasses `ctr`.

## ADR-013: Starting jobs: a host-wide cap, CPU pressure and spacing
**Status:** accepted (2026-10-23).
**Context:** A queued job started as soon as its agent had no running job (ADR-010). For builds, CPU is the
host's limit ([sizing.md](sizing.md#builds-at-once-2048-mib)): with 2 vCPUs per guest on 4 cores, 2 builds at
once ran +27% cold and +49% warm, and 3 at once +86% and +166%, with CPU PSI some up to 60 and load 7. Most
Claude Code jobs wait on the model API and use almost no CPU, so a flat "N jobs at once" cap sized for builds
would throttle them for nothing.

**Decision.** A queued job starts only when all of these hold:
- its agent has no running job (unchanged);
- fewer than `AC_MAX_RUNNING_JOBS` jobs run host-wide (default: the host's core count);
- the CPU is not saturated: `/proc/pressure/cpu` some avg10 < `AC_JOB_CPU_PSI_MAX` (40) and the 1-minute load
  < the core count;
- the newest running job started at least `AC_JOB_START_SPACING` seconds ago (15), so it shows in the pressure
  figures before the next start is decided.

The oldest eligible job goes first, across agents (by accept time). There is no other starvation rule: a job
that waited long starts as soon as the gate opens.

How it works:
- *Code.* `ac_domain::schedule::plan` is pure: the active jobs, a pressure snapshot, the time and the limits go
  in; the job to start and why each other queued job waits come out. The supervisor (`ac_gateway::jobs`, every
  second) reads pressure through `ac_ports::HostLoad`, the watchdog's port and `/proc` adapter, faked in tests.
  It decides again after each start, with that job counted as running.
- *Restart.* Running jobs and their start times are in the registry, so after a gateway restart the
  re-attached jobs fill their slots and count for spacing; none is lost or started twice (tested).
- *Timeout.* Counted from the start, not from the submit (tested: queued 10 minutes, then killed 60 s after
  its start).
- *Cancel.* A job cancelled while queued is terminal, and its start is refused, so it never starts (tested).
  A paused or destroyed agent cancels its queued jobs as before.
- *Unreadable pressure.* The pressure gate is skipped and the journal says so once; the cap and the spacing
  still hold. Failing closed would stop every job on a host without PSI, with no error a caller sees.
- *Visibility.* A queued job's JSON has `queued_reason`: `agent busy` (its agent runs a job or has an older
  one queued), `job slots full`, `host busy` or `start spacing`, checked in that order. It is `null` when the
  job is not queued, and for up to a second after the submit, until the supervisor's first look.
  `GET /api/v1/capacity` adds `running_jobs`, `queued_jobs` and `max_running_jobs`. Each change of a job's
  reason goes to the journal, with the figures for `host busy`.
- *Live (2026-10-23, a1 and a2, five short jobs).* With `AC_JOB_CPU_PSI_MAX=0` a job on a1 stayed queued 27 s
  with `host busy` (journal: `waits: host busy (cpu PSI some 1.00 >= 0.00)`); it survived a gateway restart
  without the drop-in, started 0.7 s after the port came up, and succeeded. With `AC_MAX_RUNNING_JOBS=1`, two
  jobs running `sleep 20`: a2's waited with `job slots full` while a1's ran (25.6 s), then started 1.2 s after it
  ended. With the defaults, the same two jobs: a2's waited with `start spacing`, started 15.3 s after a1's, and
  both ran at once (`running_jobs` 2) and succeeded.
- *Real builds (2026-10-24, a1, a2 and a3, 21 jobs, `scripts/burst-test.sh`;
  [sizing.md](sizing.md#job-start-gate-under-real-builds)).* With `AC_JOB_CPU_PSI_MAX=20`, two build jobs were
  submitted together and a third once both ran. In 3 of 3 runs the third started after the spacing alone
  (15.2–15.6 s, `start spacing`) and three builds ran at once, with CPU PSI peaking at 56–59. At the third start
  PSI was 2.8–16.8 and load 1.7–2.4; `host busy` never appeared.
  - Cause: a build starts 4–7 s after its job (24–42 s cold, Claude's own start), and PSI avg10 shows it 10–13 s
    later still. Two builds then hold PSI at 12.6–29.8, so 20 is inside their band.
  - Two builds cost +30% cold and +34% to +38% warm against one alone; three at once (the gate off) +143% to
    +148% warm.
  - A tiny job waited the spacing, 15 s. The watchdog took no action.
  - Decided (2026-10-24): no lower limit. A PSI gate cannot see a build before it starts, the time to finish all
    three builds was the same with and without the gate (B 151–156 s, D 153 s), and a gate strict enough to
    hold builds at 2 would hold a tiny job about 75 s (an estimate). `AC_JOB_CPU_PSI_MAX` stays at the default
    40 on every host: a safety valve against saturation (3 builds reach 56–59), not a burst limiter. The cap
    and `/dev/shm` admission (ADR-012) are what bound the load.

**Consequences:**
- On this host (4 cores, 2 vCPUs per guest) up to 4 jobs run at once, builds included. With real build jobs, 2
  builds held load at 3.8–4.1, under the core count, and CPU PSI at 12.6–29.8, under 40, so a third build
  starts after the spacing. Each of 3 builds at once takes +143% to +148% (warm, 1 run), but all three finish in
  about the same time as when staggered.
- The gate acts only at start. A job that starts idle and then builds is not slowed down or stopped.
- The gate decides when a job starts, but its CPU arrives later: first the model's turn, then the build, then
  PSI's 10 s average. So no PSI limit holds a burst of build jobs at 2 unless the spacing covers that delay
  (about 45–60 s cold), which every job, tiny ones included, would pay. The roadmap keeps a design note
  (counting builds) and when to revisit it.
- A token for several agents can fill several slots, delaying other agents' jobs. `queued_reason` tells a
  `jobs:read` caller that the host is full or busy, not with whose jobs.
- Starts are at least 15 s apart: a burst of N jobs takes (N − 1) × 15 s to start.

**Not now:** priorities or per-agent fairness beyond FIFO; limiting the CPU of a running job; io pressure in
the gate.

## ADR-014: Settings from host facts: `ac-ctl doctor` and `ac-ctl tune`
**Status:** accepted (2026-10-23).
**Context:** BaileyServ's settings were tuned by hand, each from a measurement or a failure: `/dev/shm` =
MemTotal − 8 GiB (ADR-012), Kata `default_memory = 2048` and `default_vcpus = 2` (real builds),
`default_maxmemory = 8192` (QEMU's "phys-bits too low (36)"), `AC_JOB_CPU_PSI_MAX` (ADR-013). The reasons live in
[sizing.md](sizing.md) and [install.md](install.md), but nothing checked a host against them, said what they
would be on other hardware, or said which figures were measured and which were carried over.

**Decision.**
- *Host settings come from host facts.* `/dev/shm`, `default_maxmemory`, `default_vcpus`, `AC_MAX_RUNNING_JOBS`
  and `AC_JOB_CPU_PSI_MAX` follow from MemTotal, cores, physical address bits and the measured shape.
- *Workload settings come from a profile backed by measurements.* `idle`, `build` (the default) or `heavy` sets
  `default_memory`; `AC_PROFILE` in `gateway.env` records the host's profile.
- *Every recommendation is labelled.* It has a stable rule id, a one-line reason citing sizing.md or install.md,
  and a confidence: `measured` (reproduces what sizing.md measured on the i5-3550, on a host of that shape),
  `extrapolated` (derived from a rule; not measured on this shape of hardware), or `measured: 20 did not hold a
  burst` (the PSI limit: the lower limit was run on this shape and failed, so the default stands).
- *Nothing is applied silently.* `ac-ctl doctor` only reads. `ac-ctl tune` prints diffs; only `--apply` (root)
  writes, and the gateway only logs one line at startup when its settings differ.
- *Neither takes the registry lock.* Both read files and `/proc`, so they work while `ac-gateway` runs.

How it works:
- *Code.* `ac_domain::tune` is pure: `facts` (`HostFacts` and the parsers for meminfo, cpuinfo, CPU lists, mount
  tables, `EnvironmentFile`s and Kata's hypervisor table), `rules` (`recommend(&HostFacts, Profile)` →
  `Vec<Finding>`, `forecast`, the rule list `--explain` prints) and `edit` (line edits and `-U0` diffs). The
  collector, `ac_ctr::facts::collect`, reads through the `HostRead` trait, which tests fake.
- *Facts.* MemTotal, online CPUs, physical address bits and the vmx/svm flag; the disk class of the data
  directory (for btrfs, by the mount that holds it: a subvolume's `st_dev` names no block device); `/dev/shm` as
  mounted, as used and in fstab; Kata's `default_memory`, `default_vcpus`, `default_maxmemory`, `default_maxvcpus`
  and initrd; `gateway.env`; `/dev/kvm` opened read-write; whether the initrd has `fix-kata-initrd.sh`'s NIC
  drop-in (the initrd is streamed through `gzip -dc` into a cpio reader, nothing is unpacked); the nft rule
  counts and firewalld zone that `post-reboot-check.sh` checks (a test keeps the two lists equal); the modes of
  `tokens` and `egress-allow.txt`. What only root may read is skipped and says so.
- *Facts files.* `doctor --json` includes the facts; `--facts-file` reads them back, or a hand-written subset,
  to plan a host that is not this one (`deploy/hosts/`). A fact left out is skipped; the recommendations that
  need only size and cores still show.
- *Tune.* It edits only the keys it owns: Kata's `default_memory`, `default_vcpus` and `default_maxmemory` in the
  first `[hypervisor.*]` table (a line edit that keeps comments, layout and other keys; a missing key goes after
  its commented template), the `gateway.env` keys of the rules and `AC_PROFILE`, and `size=` of the `/dev/shm`
  fstab line (added in the style of `deploy/dev-shm.fstab` if missing). Each changed file is copied to
  `<file>.ac-tune-<UTC stamp>` first and replaced atomically with its mode and owner kept. A second run changes
  nothing. It refuses while any Kata VM runs (unless `--force`; it names those that need a pause and resume)
  and refuses a `/dev/shm` smaller than what is in use. It never reboots, remounts (it prints the remount),
  rebuilds the initrd or touches the firewall or units: those are printed as manual steps with the command
  from install.md. `--restart-gateway` restarts the gateway and waits for its port (since ADR-015 the restart
  itself waits for readiness; the port wait stays as a second check and for older units). `--root DIR` edits copies under DIR, to try it without touching the host.

| Rule | Recommendation | Measured when |
|---|---|---|
| `shm.size` | MemTotal − 8 GiB (MemTotal ≥ 16 GiB), else MemTotal/2; error below 8 GiB | MemTotal within 2 GiB of the measured 23909 MiB |
| `kata.default_maxmemory` | 8192 at ≤ 36 physical address bits, else Kata's default | 36 bits |
| `kata.default_vcpus` | 2 at ≥ 4 cores, else 1; never above 2 | 4 cores |
| `kata.default_memory` | idle 2048, build 2048, heavy 4096 | idle and build (any host); heavy never |
| `gateway.max_running_jobs` | the core count (the default) | 4 cores |
| `gateway.cpu_psi_max` | the default 40 on every shape: a safety valve, not a burst limiter | 4 cores, 2 vCPUs: `measured: 20 did not hold a burst` (see below) |
| `disk.class` | information: the measured cold-start cost of a rotational disk | rotational |
| `prereq.*` | `/dev/kvm`, vmx/svm, Kata config, initrd drop-in, fstab line = running mount, firewall, modes | (checks) |

`gateway.cpu_psi_max` recommended 20 on 4 cores with 2-vCPU guests until 2026-10-24, from the measured PSI of two
builds (23.7), tagged `measured input, gate unvalidated`. Run against bursts of real build jobs (ADR-013) it did
not hold builds at 2: three builds started at CPU PSI 2.8, 16.8 and 16.1. The rule now recommends the default 40
on every shape, tagged `measured: 20 did not hold a burst` on this shape. 3 builds peak at 56–59, so 40 is a
safety valve against saturation, not a burst limiter; the job cap and `/dev/shm` admission bound the load. On
this shape a different value set by hand is a warning (tune sets 40); elsewhere it is information.

*Live (2026-10-23, BaileyServ, gateway running).* `sudo ac-ctl doctor --profile build`: 17 ok, 1 info (disk
class), every finding measured, exit 0 (since relabelled: since 2026-10-24 the PSI rule recommends 40, tagged `measured: 20 did not hold a burst`; as a user: gateway.env and the firewall skipped, still exit 0). `tune`
showed no changes. `--json` parsed with `python3 -m json.tool`. On copies of the live files with
`default_memory = 4096`, `default_vcpus = 1`, `default_maxmemory` commented out, no PSI line and no fstab line
(`--root`): doctor exit 2 (2 errors, 3 warnings; errors exit 4 since); `tune --apply` wrote exactly those five edits, three backups
equal to the copies, gateway.env still 0600, the rest of each file byte-identical; a second run changed nothing.
With a1 running, `--apply` was refused naming a1 and wrote nothing. The gateway started on the untuned copy
logged `settings differ ... default_maxmemory unset (0: all host RAM) -> 8192, default_vcpus 1 -> 2,
default_memory 4096 -> 2048, AC_JOB_CPU_PSI_MAX unset (40) -> 20; run ac-ctl doctor` (the rule then recommended 20), and nothing on the live
settings. `post-reboot-check.sh` passed with its new line. The facts file of the hypothetical 32 GiB / 8 core /
NVMe host forecasts 11 agents at 2048 MiB and 4 builds at once (extrapolated).

**Consequences:**
- A new host gets its numbers, with their provenance, before anything is installed: `doctor --facts-file`.
- On another shape most figures are `extrapolated`, and the doctor says so; the PSI limit is the default 40 on
  every shape.
- A setting a person chose deliberately below the recommendation (fewer job slots, a calibrated PSI limit on
  other hardware) is `info`, not a warning, and tune leaves it.
- Exit codes: doctor 0 ok, 1 warnings, 4 errors (ac-ctl's 2 is bad usage, or doctor could not run, and 3 is
  "the gateway holds the data dir"); `post-reboot-check.sh` reads the JSON `errors` count, not the exit code,
  and FAILs on errors only.
- MemTotal moves by a few MiB across kernels, so `/dev/shm` within 1% of the target is ok.

**Not now:** calibrating the PSI limit automatically; editing the initrd, firewall or units; per-agent memory
classes; telling nested virtualization from bare metal.

## ADR-015: The gateway tells systemd when it is ready (`Type=notify`)
**Status:** accepted (2026-10-24; verified on BaileyServ, a reboot still to test).
**Context:** `ac-gateway.service` was `Type=simple`, so systemd reported it active as soon as the process
started, before the startup reconcile ran and before it listened. `systemctl restart ac-gateway && curl`
failed with connection refused, and every script and `ac-ctl tune --restart-gateway` had to poll the port.

**Decision.**
- `Type=notify`, `NotifyAccess=main`. The gateway sends `READY=1` with a `STATUS=` line
  (`listening on <addr>; N agents (M running); reconcile done`) once the tokens are loaded (a bad tokens file
  refuses the start), the startup reconcile has run, admission is set up and the listener is bound, just before
  it accepts. Connections queue in the listen backlog from the bind, so READY means a connect succeeds. A start
  that fails exits non-zero before READY, and `systemctl restart` fails at once instead of timing out.
- A failed reconcile is still logged and served through (as before); the status then says `reconcile
  failed (see the log)` and `post-reboot-check.sh` FAILs on it. Making it fatal would turn a containerd hiccup
  at boot into a restart loop with the API down.
- SIGTERM or SIGINT sends `STOPPING=1`, stops the server and exits 0 within a second (the signal thread ticks
  each second); before, the process died on the signal. SIGHUP still re-reads the tokens.
- Std only (`crates/ac-gateway/src/notify.rs`): one `UnixDatagram` send per message, abstract names through
  `SocketAddrExt`, no unsafe. rusty_mill has no sd_notify crate, and the protocol is a single datagram, so
  libsystemd or a crate would add more than it saves. Without `NOTIFY_SOCKET` (run by hand, or under a
  `Type=simple` unit, which gets none) nothing is sent. A malformed `NOTIFY_SOCKET` or a failed send is logged
  as a warning and the gateway carries on.
- `TimeoutStartSec=180`. Measured startup is 0.1–1.1 s. A gateway SIGKILLed with a1 running took 0.14 and
  0.18 s. The slower case is a1's VM killed too, so the reconcile pauses it and cleans up: 0.51, 0.66, 0.77 and
  1.11 s. 180 s outlasts one external command (`ctr`, `ip`, CNI) at its 120 s `command_timeout`, which the
  reconcile logs and serves through.

**Evidence (2026-10-24, BaileyServ).** Before: `time systemctl restart ac-gateway && curl .../capacity` 0.04 s,
then `curl: (7) ... Could not connect`. After: 0.88 s, then the capacity JSON. With systemd at debug level:
`Got notification message from PID ...: READY=1, STATUS=listening on 127.0.0.1:8780; 2 agents (0 running);
reconcile done`, `Changed start -> running`; on stop `STOPPING=1`, `Changed stop-sigterm -> dead`, exit 0. After
a gateway SIGKILL the unit went `activating/auto-restart` → `activating/start` → `active` with the port closed
until `active`. A bad tokens file under a transient `Type=notify` unit failed the start in 0.06 s. The
integration tests (`crates/ac-gateway/tests/notify.rs`) run the binary against a fake `NOTIFY_SOCKET` (a path
and an abstract name).

**Consequences:**
- Install order matters: the binary before the unit. A binary from before this ADR never sends READY, so under
  the new unit systemd kills it after 180 s and restarts it, over and over. The new binary under the old unit
  works as before (install.md, *Readiness*).
- `ac-ctl tune --restart-gateway` and the scripts keep their port waits: a cheap second check, and the only
  one when they run against an old unit.
- A start that hangs (for example a wedged containerd) now fails after 180 s, visibly, instead of reporting
  active and refusing connections.

**Not now:** `WatchdogSec` (the gateway has its own agent watchdog; a systemd watchdog would need a heartbeat
from a thread that proves the server is healthy); `Type=notify-reload` (SIGHUP's `ExecReload` stays);
`ac-egress` and `ac-agentfw`, whose readiness means something else.

## ADR-016: Per-agent egress profiles, keyed by a proxy credential
**Status:** accepted (2026-10-25; verified on BaileyServ, a reboot still to test).
**Context:** ADR-007 gives every agent the same allowlist. A Rust agent needs crates.io, which the others should
not reach. Adding it to the base list would open a code-delivery channel to every agent.

**Decision.**
- *Profiles:* named host lists in `/etc/agent-cloud/egress/profiles/<name>.txt` (root, 0600, the allowlist's
  format; the directory 0700). Names follow the agent-id rule, so `-` can never be one. Shipped:
  `deploy/egress/profiles/` `rust`, `python`, `node`. The API can only attach profiles that exist on the host. It
  never defines hosts, so a token cannot widen egress beyond what root installed.
- *Identity is a credential, not the source IP.* The bridge carries no MAC or IP binding, and another agent's
  address is easy to claim. Every create and resume makes a new secret: 32 bytes of `/dev/urandom` as hex. The
  VM gets `HTTPS_PROXY`, `HTTP_PROXY` and the lowercase forms set to `http://<id>:<secret>@10.89.0.1:3128`
  through `ctr run --env-file`, a 0600 file in a 0700 directory (`<data>/boot`). The file is created with
  `O_EXCL` and removed when `ctr` returns. The secret is never in a host argv: an `ac-ctr` test checks every
  argv and env of create, resume and a failed run. These override the image's credential-less variables.
- *Agents file:* `/etc/agent-cloud/egress/agents` (root, 0600), one line per agent:
  `<agent> <sha256(secret)|-> <profile,...|->`. The gateway writes it atomically (`rusty_atomic_file`, under a
  lock, only on change). Only the digest is stored. It writes the digest before `ctr run`. It clears the digest
  before a pause or destroy stops the VM, and again when a start fails, so a credential never outlives its VM.
  The startup reconcile clears the digests of agents with no VM, and purge drops the line. A destroy without
  purge keeps the profiles, so a re-created agent gets them back.
- *Proxy:* `ac-egress` never opens the registry. Before every decision it re-stats the agents file and
  each profile, and re-reads any that changed. A file that is not a regular file, not root-owned,
  group/world-accessible, over 64 KiB or malformed keeps its last good version. A profile that never parsed
  adds nothing. This is per file, so one bad profile cannot freeze a detach. More than 256 profiles is an
  error. The rules:
  - no `Proxy-Authorization`: the base list;
  - exactly one well-formed `Basic` credential whose digest matches that agent's line (SHA-256,
    `constant_time_eq`): the base list plus the agent's profiles;
  - anything else gets `407` with `Proxy-Authenticate: Basic realm="agent-cloud"`, never the base list. That
    covers an unknown agent, no digest, a wrong secret, two headers, another scheme, bad base64 or a value over
    256 bytes.
  The header is never forwarded. Log lines carry `agent=<id>`, or `agent=-` without a credential and `agent=<id>?`
  for a refused one. A deny names a profile that would allow it (`(profile rust would allow it)`).
- *API:* a new scope `egress:write` (not implied by `agents:write`; `admin` has it).
  - `GET /api/v1/egress/profiles` (`agents:read`): names and host counts.
  - `GET /api/v1/agents/{id}/egress` (`agents:read`): `{"id","profiles","effective_hosts"}`, plus `missing` for
    an attached profile that is no longer installed.
  - `PUT /api/v1/agents/{id}/egress` (`egress:write`) sets the list: 400 for an unknown or unusable name, 404
    for an unknown agent, 409 for a destroyed one.
  - `egress_profiles` on create also needs `egress:write` (403 without).
  Every PUT and create is audited with `profiles=<names|->`. A change applies to the agent's next request, with
  no restart.
- *ac-ctl:* `create` and `resume` boot without a credential (base list only), and `pause` and `purge` leave the
  agents file alone. The gateway's next startup sweep clears what they left. `doctor` checks the agents file's
  mode and owner (`prereq.egress_agents_mode`, absent is fine) and that every attached profile is installed and
  readable (`prereq.egress_profiles`).
- New first-party dependency: `rusty_base64` (strict canonical decode, ADR-008).

**Evidence (2026-10-25, BaileyServ, a1 and a2).**
- a1 without profiles: `index.crates.io` CONNECT 403, `api.anthropic.com` 200, and `claude -p "say ok"`
  answered `ok` with the proxy logging `allow ... agent=a1 api.anthropic.com:443`.
- With `rust` attached through a throwaway `egress:write` token, a1 got 200 within 0.5 s of the PUT while a2
  got 403. Detached, a1 got 403 again.
- With a toolchain image, `cargo fetch` (8 crates) worked with `rust` and failed with CONNECT 403 without it.
- Two resumes of a1 each rotated the digest, and the pre-resume URL then got 407. A wrong secret and a2's name
  with a1's secret got 407 with the realm header.
- A root scan of every host `/proc/*/cmdline` (469 passes) during two pause/resume cycles found no `a1:`. A
  positive control was caught.

**Consequences:**
- A stolen credential works from any agent VM on the bridge until that agent's next pause or resume. It is
  readable inside its own VM (environment, `/proc`), and on the host by root: containerd's container record
  holds the env. An agent that leaks its credential leaks only its own profiles.
- A profile is a code-delivery channel: `rust` lets the agent fetch and build any published crate.
- Agents booted before this ADR, or by `ac-ctl`, have no credential and get the base list. Pause and resume
  them to give them their profiles.
- *Install order:* either order is safe; install the proxy first. A new proxy with an old gateway sees no agents
  file, so everyone gets the base list. A new gateway with an old proxy hands out credentials the old proxy
  drops with the other `Proxy-*` headers, so everyone gets the base list and attachments do nothing until the
  proxy is upgraded.
- *Rollback:* reinstall the old gateway, then remove `/etc/agent-cloud/egress/agents`, then reinstall the old
  proxy if wanted. An old gateway never clears digests, so until the file is gone, running agents keep their
  profiles.

**Not now:** per-profile ports (443 and 80 as the base list); per-agent base lists; binding a credential to a VM's
MAC or IP; profiles editable through the API.

## ADR-017: An MCP interface on the gateway, split by token scope
**Status:** accepted (2026-10-28; verified on BaileyServ the same day with Claude Code 2.1.312 added by
`claude mcp add`: 21 of 22 tools listed for a token without `egress:write`, checkpoint tools and a 409 passed through).
**Context:** An MCP client (Claude Code, claude.ai connectors, another agent) could only drive the gateway through
hand-written REST calls. The Ephemeral Sandbox comparison (2026-10-27) found its per-capability MCP tool sets the
idea worth borrowing; ADR-011's scopes already split what a caller may do.
**Decision:** `POST /api/v1/mcp` speaks MCP's Streamable HTTP transport over the existing listener, token and
Origin check.
- *Stateless, JSON only:* no `Mcp-Session-Id`, no SSE (`GET` is 405), single messages (batches are refused).
  Versions `2025-11-25`, `2025-06-18`, `2025-03-26`: `initialize` echoes the client's when served, else the
  newest; an unserved `MCP-Protocol-Version` header is 400. Notifications get 202.
- *One tool per REST route* (declared on the route in `access::ROUTES` since ADR-020): `whoami`; `list_agents`, `get_agent`, `get_capacity`,
  `list_egress_profiles`, `get_agent_egress` (`agents:read`); `create_agent`, `pause_agent`, `resume_agent`,
  `destroy_agent` (`agents:write`); `set_agent_egress` (`egress:write`); `list_jobs`, `get_job`,
  `get_job_output` (`jobs:read`); `submit_job`, `cancel_job` (`jobs:write`); `get_history` (`viewer:read`);
  `list_checkpoints` (`agents:read`); `create_checkpoint`, `delete_checkpoint`, `restore_checkpoint`,
  `fork_agent` (`agents:write`, ADR-018). A tool takes its method, path and scope from the route that offers it.
- *Same path as REST:* a call becomes that route's method, target and body, then goes through `access::check`
  (agents first, then scope), the handler and the audit line, exactly as an HTTP request. Arguments are checked
  by kind first (ids parsed as `AgentId`/`JobId`, query values only booleans, counts or lowercase words, unknown
  arguments refused), so no argument can change the path or add a query parameter.
- *Scopes decide what is listed:* `tools/list` shows only the tools the token's scopes allow. Its agents are
  not looked at there; a call on another agent gets the route's 404.
- *Results:* the route's body as text; a status of 300 or more is a tool error (`isError`) with `HTTP <status>:`
  in front, so the model sees why. An unknown tool is JSON-RPC `-32602`.
- *No viewer tools beyond history:* the terminal stream and input (`viewer:control`) need a live viewer seat,
  which a request/response tool cannot hold.
**Consequences:** any MCP client with a token gets the gateway's tools with no new credential, attack surface or
dependency (`rusty_json`, already used). Job output and history reach the model as text the agent wrote: the
server's `instructions` and the tool descriptions say it is untrusted. A `get_job` with `wait` holds a connection
up to 30 s, under the same waiter cap as REST.
**Not now:** structured output (`structuredContent`, `outputSchema`); resources or prompts; SSE and server-sent
notifications; OAuth (claude.ai custom connectors can send the bearer token as a request header).

## ADR-018: Workspace checkpoints and forks on btrfs subvolumes
**Status:** accepted (2026-10-28; verified on BaileyServ the same day with live VMs: a1 migrated, a checkpoint taken
while a job wrote a file a second, a fork booted from it, restore and its undo, each read back inside the VM).
**Context:** An agent that breaks its workspace (a bad refactor, a deleted directory) can only be fixed by hand,
and trying two approaches from one starting point means copying a workspace by hand. LayerFS-style branching
filesystems were considered and rejected: FUSE on top of virtiofs, a non-durable store and a very large
dependency, for a single host with few long-lived workspaces. BaileyServ's data disk is already btrfs.

**Decision.**
- *Volumes:* on a btrfs workspace root, every new workspace is a subvolume (`btrfs subvolume create`). The path
  `<data>/workspaces/<id>` and the bind mount do not change. Elsewhere workspaces stay plain directories and the
  routes below answer 409. A subvolume is recognised by statfs(2) (`BTRFS_SUPER_MAGIC`) and its root's inode
  number (256); nothing is spawned to tell.
- *Checkpoint:* `btrfs subvolume snapshot -r` of the workspace to `<data>/checkpoints/<id>/<checkpoint>` (both
  directories 0700). The id is the UTC time, `YYYYMMDD-HHMMSS`, with `-2`, `-3` for the same second, so ids sort
  by age. An optional label (1 to 80 characters, no control characters) is kept beside the snapshot as
  `<checkpoint>.label`, never inside it. The filesystem is the record: no registry rows, so nothing can drift.
  Running or paused agents; at most `AC_CHECKPOINT_MAX` (default 8) per agent, then 409 until one is deleted.
- *Restore:* paused agents only (a running VM's bind mount would keep the old subvolume), and no job. It first
  checkpoints the current workspace (labelled `before restore of <checkpoint>`, so it needs room under the cap),
  then snapshots the checkpoint to `<id>.restore`, renames the workspace to `<id>.old`, renames `<id>.restore`
  into place and deletes `<id>.old`. Every restore can be undone by restoring what it kept. A swap cut short by
  a crash is finished or undone by the next restore or migrate of that agent.
- *Fork:* a writable snapshot of a running, paused or destroyed agent's workspace, or of one of its
  checkpoints, becomes the workspace of a new agent, which then boots through the create path (admission, a new
  proxy credential, its own network). Nothing else is inherited: egress profiles are attached explicitly
  (`egress_profiles`, needing `egress:write`), and no jobs come along. The new id must have no workspace and no
  running or paused VM.
- *Migrate:* `ac-ctl workspace migrate <id>` (paused or destroyed agents) turns a plain-directory workspace
  into a subvolume: `btrfs subvolume create <id>.migrate`, `cp -a --reflink=auto <id>/. <id>.migrate/`
  (reflinked on btrfs, so it costs metadata only), then the same rename swap. It is the one step that walks the
  agent-controlled tree as root, once, with the VM stopped; `cp -a` copies symlinks as links. The gateway logs
  at startup which workspaces are still plain directories.
- *Purge* deletes the agent's checkpoints and its workspace with `btrfs subvolume delete` (read-only snapshots
  cannot be removed with `rm`).
- *API:*
  - `GET .../{id}/checkpoints` (`agents:read`): `{"id","max","checkpoints":[{"id","label"?}]}`, oldest first.
  - `POST .../{id}/checkpoints` (`agents:write`), body `{"label"?}`: 201 `{"id","label"?}`.
  - `DELETE .../{id}/checkpoints/{cp}` (`agents:write`).
  - `POST .../{id}/restore` (`agents:write`), body `{"checkpoint"}`: `{"id","restored","kept"}`.
  - `POST .../{id}/fork` (`agents:write`), body `{"id","checkpoint"?,"egress_profiles"?}`: 201 and the new
    agent. The token must be for the new agent too (404 otherwise, as for any other agent).
  Refusals (wrong state, plain directory, cap, taken id) are 409 and change nothing; an unknown checkpoint is
  404. Audited as `checkpoint`, `checkpoint-delete` (`checkpoint=<id>`), `restore` and `fork` (`to=<id>`,
  `checkpoint=<id>`, `profiles=`).
- *ac-ctl:* `checkpoints`, `checkpoint [--label TEXT]`, `delete-checkpoint`, `restore`, `fork [--from CP]` and
  `workspace migrate`, with the same rules. A fork from `ac-ctl` attaches no profiles.
- *Code:* the rules are pure (`ac_domain::checkpoint`: ids, labels, which states and volumes allow which
  operation, the cap). The `Snapshots` port is separate from `Runtime` (volumes, not VMs); `ac_ctr::snap`
  implements it with `btrfs` through the same spawner and timeout as `ctr` (`AC_BTRFS` names the binary).

**Evidence (2026-10-28).** `scripts/btrfs-checkpoint-test.sh` on a 256 MiB loopback btrfs (kernel 6.18,
btrfs-progs 6.6.3, as root): workspace created as a subvolume, a checkpoint refused a write (read-only),
restore brought the old file back and kept the replaced one, a fork from that checkpoint held its contents, a
plain directory migrated with its contents, and purge left nothing behind. The unit tests run the same steps
against a stand-in `btrfs`, including crash recovery of a half-done swap.

**Consequences:**
- A checkpoint of a running agent is crash-consistent: what the guest had written through virtiofs at that
  instant, as if the power went. Kata's virtiofs does not enable writeback caching by default, so writes reach
  the host on `write()` **[unverified on BaileyServ]**. Pause first for a clean point.
- A fork copies the source's Claude Code login and Git credentials, since they live in the workspace.
- Checkpoints keep deleted and rewritten data on disk; the cap bounds how much. btrfs qgroups stay off (they
  slow down with many snapshots), so a per-agent disk quota is still open.
- *Rollback:* an older gateway treats subvolumes as directories, so create, pause, resume and destroy keep
  working. Its purge of a workspace still works as root (`rmdir` of an emptied subvolume), but it does not
  delete checkpoints: remove `<data>/checkpoints/<id>/*` with `btrfs subvolume delete`.

**Not now:** checkpoints before each job (an opt-in on the job body, with its own rotating cap); sizes in the
list (`btrfs filesystem du` is slow on a rotational disk); an XFS reflink backend (no host needs one); memory
snapshots (the roadmap's separate item).

## ADR-019: Each job's changed paths in the agent's history
**Status:** accepted (2026-10-28; the adapter test ran against real btrfs, btrfs-progs 6.6.3 on kernel 6.18).
**Context:** After a job, the only record of what it did to the workspace is its own final message, which the
agent wrote. Workspaces are btrfs subvolumes (ADR-018), so the filesystem can say what changed, cheaply and
without trusting the agent.

**Decision.**
- *Snapshots:* as a job starts, before its window opens, a read-only snapshot of the workspace goes to
  `<data>/checkpoints/<id>/<job>.start`. When the job ends from `running` (any of succeeded, failed, cancelled,
  timed out, interrupted), a second goes to `<job>.end`. Names with a `.` are never checkpoint ids, so neither
  is listed nor counted under `AC_CHECKPOINT_MAX`. A job that never started gets none.
- *Diff:* `btrfs send -q --no-data -p <job>.start -f <job>.send <job>.end`, then `btrfs receive --dump -f
  <job>.send`. Send compares the two trees in the kernel and reads no file data; root never opens a path inside
  the workspace. Both snapshots and the stream are deleted afterwards, on failure too; a purge deletes any a
  crash left behind.
- *Paths:* `ac_domain::changes::paths` reads the listing: every path added, removed, renamed (both names),
  written, truncated, or given a new owner, mode or xattr. Times-only changes (`utimes`), the root, and send's
  temporary `o<ino>-<gen>-<seq>` names are left out. Paths stay escaped as btrfs prints them (`\ `, `\\`,
  `\ooo`), so none holds whitespace or a control character.
- *Note:* one history line after the job's end state, `job <id> changed <n> paths: <path> <path> ...`, the
  runner's own files (`.jobs/`, the root's `.job-*`) left out, at most `CHANGE_NOTE_CAP` (2 KiB) of paths and
  then `(and <k> more)`. It is shown by `GET .../history` and the `get_history` tool, with no new route.
- *Where it cannot:* a plain-directory workspace takes no snapshot and gets no note; a failed snapshot or diff
  is logged to the journal and the job is unaffected.
- *Code:* `jobs::Changes` (begin, end) is a seam beside `JobRunner`, implemented by `CtrRuntime` over
  `ac_ctr::snap` and faked in the job tests.

**Consequences:**
- What a job did is known from the filesystem, not from the agent's account of it. The names are still
  agent-chosen text.
- A start snapshot keeps what the job deletes or rewrites on disk until it ends; the end snapshot lives only for
  the diff.
- Each started job costs two snapshot creations and deletions, and a metadata send proportional to what changed.
  A job interrupted by pause or destroy is diffed under the lifecycle lock, before the VM stops.
- A change undone within the job (a file written then removed) can still be listed; a `touch` is not.

**Not now:** a structured route for the summary (it would be one more `access::ROUTES` entry, ADR-020); keeping the start snapshot as a restorable checkpoint (ADR-018's "checkpoints before each job").

## ADR-020: One operation catalog for REST and MCP; `ac-ctl` stays local
**Status:** accepted (2026-10-28).
**Context:** The Ephemeral Sandbox comparison (2026-10-27) suggested generating the CLI and the MCP tools from one
declaration, once there were two real consumers. There are now two tables that describe the same operations:
`access::ROUTES` (method, path, scope, audit name) and `mcp::TOOLS` (name, description, method, path, arguments),
kept in step by a test. `ac-ctl` is not a third copy of them: it drives `Fleet` directly on the data directory and
refuses while the gateway holds it (exit 3), so it works with the gateway stopped (`reconcile` after a reboot,
`workspace migrate`, recovery), needs no token, and has commands with no route (`reconcile`, `migrate`, `doctor`,
`tune`). Of the gateway's 22 tools, 10 have an `ac-ctl` twin; jobs, egress, history, capacity and whoami do not.
**Decision:**
- *One table for the gateway.* A route of `access::ROUTES` carries its MCP tool, if it has one (name, description,
  where arguments go, the arguments). `mcp::TOOLS` goes; `tools/list`, `tools/call` and the scope a tool gets all
  read the route. Method and path are written once, so a tool cannot drift from its route; the cross-check
  test becomes a pinned list of tool names plus one id argument per path placeholder. On the wire only the
  `tools/list` order changes (it now follows the route table); names, descriptions and schemas are the same.
- *`ac-ctl` keeps its own parser.* Generating its commands from the route table would mean turning it into a
  REST client (a token on the host, an HTTP client, and no way to work with the gateway stopped) or giving the
  table a second, local dispatch path. Neither has a forcing function yet.
**Revisit when:** `ac-ctl` needs to act while the gateway runs (then it becomes a client of the same table, and
its fleet commands are generated from it), or a third front end (a web UI form, another protocol) appears.
**Consequences:** one place to add an operation to REST and MCP, a little less code; `ac-ctl` and the tools
can still differ in naming (`create` vs `create_agent`) and in what they offer.

## ADR-021: Workspace backups as btrfs send streams in files
**Status:** proposed (2026-10-28; tested on loopback filesystems by `scripts/btrfs-backup-test.sh`, not yet on
BaileyServ). Amended the same day, before any host ran it: the first version received each backup into a btrfs
filesystem on a second disk (#15). BaileyServ has no second disk, so backups are now stream files in any
directory, such as a share on another machine (Beast).
**Context:** checkpoints (ADR-018) undo an agent's mistakes, but they sit on the same disk as the workspace: a
dead disk or a lost host loses both. M4 asks for backups with a tested restore. Workspaces are already
subvolumes, and a read-only snapshot is what `btrfs send` needs, so an incremental send costs only what changed.
`btrfs send -f` writes that stream to a file, which any filesystem holds; only a restore needs btrfs.
**Decision.**
- *Where:* any directory (`<dest>`): a local disk, or a mount of an NFS or SMB share on another machine, or of
  object storage. Off this host is the point; the directory is the operator's choice. `<dest>/<agent>` is made
  0700 (workspaces hold logins).
- *Streams:* `<dest>/<agent>/<id>.full.btrfs`, or `<id>.after-<parent>.btrfs` for one sent against the backup
  `<parent>`. Ids are the checkpoint stamp (`YYYYMMDD-HHMMSS`), always after the newest one taken, so they sort
  by age even after retention frees one or the clock steps back.
- *Send:* a read-only snapshot of the workspace to `<data>/backups/<agent>/<id>`, then
  `btrfs send -q [-p <parent>] -f <stream>.part <snapshot>`, `fsync`, rename to `<stream>`, `fsync` of the
  directory. The parent is the newest backup kept here whose chain at `<dest>` is whole, unless that chain is
  already `--full-every` long (default 7, at least 1): then the send is full, so a restore never replays more
  than that. After a good send only the new snapshot stays here, as the next parent.
- *Chains:* a backup can be restored if its chain is whole: every stream from the last full one up to it.
- *Retention:* the newest `--keep` (default 7, at least 1) restorable backups per agent, and every stream their
  chains need; the rest go, broken streams too. Up to `keep + full-every − 1` streams stay at once.
- *Cut short:* a stream is listed only once renamed from `.part`, after it was flushed, so a stream cut short
  (a crash, a full disk, a lost share) is never listed, and the next backup of that agent deletes it. A failed
  send deletes its `.part` and its snapshot here, keeping the old parent.
- *Restore:* `ac-ctl restore-backup <dest> <agent> [<backup>]` (default the newest). Refused while the agent's
  Kata VM runs (read from the process list) and for a plain-directory workspace. It replays the chain with
  `btrfs receive -f <stream>` into `<workspaces>/<agent>.receive/` (the full one, then each increment, found by
  its parent's received uuid), takes a writable snapshot of the backup as `<agent>.restore`, deletes the
  received copies, then swaps it in as ADR-018's restore does: a workspace that is there is kept first as a
  checkpoint (`before restore of backup <id>`, so it needs room under `AC_CHECKPOINT_MAX`); one that is gone
  (purged, or a new host) is simply created, and `create` boots it. Leftovers of a restore cut short are
  removed by the next.
- *No registry:* `backup`, `backups` and `restore-backup` read only the filesystem and the process list, so
  they run while `ac-gateway` holds the data directory. They touch no fleet state, so they are not the
  `ac-ctl` acting beside the gateway that ADR-020 would revisit for. A backup and a restore hold
  `<data>/backup.lock` against each other. A checkpoint restore or migrate racing a backup of the same agent
  makes that agent's backup fail (its snapshot finds no workspace); the next run takes it.
- *Schedule:* `deploy/ac-backup.service` (oneshot, `nice` 19, idle I/O class) run daily by `ac-backup.timer`
  (`Persistent=`, so a run missed while the host was off happens at boot), after its destination is mounted.
  A run that leaves any agent without a backup (a plain-directory workspace, a full or missing share) exits 1,
  and the unit shows failed.
- *Purge* deletes the parent kept here; the backups at `<dest>` stay (that is what they are for).
- *Code:* the rules are pure (`ac_domain::backup`: stream names, ids, chains, parent, retention, restore
  refusals and which backup). `ac_ctr::backup` runs `btrfs` through the same spawner as ADR-018, with no time
  limit for send and receive; the CLI (`ac-ctl`'s `backup` module) checks the VM and the checkpoint cap before
  a restore.

**Evidence (2026-10-28).** `scripts/btrfs-backup-test.sh` (kernel 6.18, btrfs-progs 6.6.3, as root) through the
`ac-ctl` binary, with the data directory on a 256 MiB loopback btrfs and the streams on a 256 MiB loopback ext4,
as on a mounted share: a full send of a 16 MiB workspace (16.0 MiB) and an incremental one after a one-line
change (224 B); five more with `--keep 3 --full-every 3` leaving 4 streams, a full one among them; a `.part`
neither listed nor kept; a restore of the newest over a broken workspace (chain replayed, owners and the 16 MiB
file intact, the broken workspace kept as a checkpoint) and of the oldest into a purged one; a lost full stream
hiding its chain from the list and from restore; and the refusals (plain directory, unknown backup, a missing
destination). By hand, a 200 MiB workspace sent to a 64 MiB ext4 failed (`No space left on device`) and left
nothing at the destination or here. In the first version, the script found that a freed id was reused and sorted
wrong, now fixed by `next_id`. The unit tests run the same steps against a stand-in `btrfs` whose stream is a tar
of the snapshot, including a send that fails half way and a broken chain.

**Consequences:**
- A backup of a running agent is crash-consistent, like a checkpoint: pause first for a clean point.
- A stream cannot be browsed: to read one file from a backup, restore it (or `btrfs receive` it on any btrfs).
  A stream is only as good as its chain, so a lost full stream loses up to `--full-every` backups; `--keep` is
  counted in restorable backups, so retention never leaves fewer than asked.
- The first backup of each workspace, and every `--full-every`th, is a full copy; the others carry only what
  changed. `btrfs send` reads the snapshot, so a full one loads the rotational disk: hence the idle I/O class.
- A backup holds the agent's Claude Code login and Git credentials, as the workspace does; the share needs the
  same care as the data directory (export it to this host only).
- The parent kept here pins the data it shares with the workspace until the next backup replaces it.
- *Rollback:* an older gateway or `ac-ctl` ignores `<data>/backups`; its purge leaves the parent there: remove
  `<data>/backups/<id>/*` with `btrfs subvolume delete`. Streams at `<dest>` need only `btrfs receive -f` on any
  btrfs, none of this code.

**Not now:** the registry, tokens and the egress agents file (the data directory outside workspaces; copying the
registry needs the gateway's cooperation, and agents and tokens can be recreated); checkpoints in backups; a
gateway route or MCP tool; uploads to object storage without a mount; compression (`btrfs send --compressed-data`
needs a newer kernel and protocol 2) and encryption (use an encrypted share or filesystem); a checksum beside each
stream (btrfs checks every command in a stream on receive).
