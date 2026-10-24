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
**Revisit when:** agents need different allowlists, or the gaps above matter enough for SNI
checks or TLS interception.

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
  `agents:write`, `jobs:read`, `jobs:write`, `viewer:read`, `viewer:control`, `admin` (implies all); `agents` is
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
  cancel, take-over, release) writes `audit token=<name> action=<action> [force=true] [purge=true] [job=<id>]
  status=<code>` to the agent's `ac-history` log (shown by `GET .../history`) and the same line with
  `agent=<id>` to the gateway's journal. Viewer input is not audited per call: it comes from the viewer whose
  take-over was. No prompt, body or terminal content.
- *Viewer page:* asks `whoami` after the token is entered, shows the token's name and scopes, and hides
  **Take over** without `viewer:control` (the gateway refuses it anyway).
**Consequences:** a stolen jobs token runs prompts in its agents (which is code execution as the agent user in
those VMs) but cannot pause, destroy, watch or type into them, nor see other agents. `admin` remains as
powerful as the old token. Tokens do not expire; revoke them. There is still no TLS of the gateway's own.
**Not now:** expiry and per-token rate limits; per-address failure limits (needs the peer address from
rusty_serve); scopes narrower than these seven.

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
  - Replaying the curves, PSI 10 with 15 s spacing would hold warm bursts at 2, but not cold ones; not changed
    without a live run.

**Consequences:**
- On this host (4 cores, 2 vCPUs per guest) up to 4 API-bound jobs run at once. With real build jobs, 2
  builds held load at 3.8–4.1, under the core count, and CPU PSI at 12.6–29.8. At the limit of 20 a third build
  was not held back (above, Real builds).
- The gate acts only at start. A job that starts idle and then builds is not slowed down or stopped.
- The gate decides when a job starts, but its CPU arrives later: first the model's turn, then the build, then
  PSI's 10 s average, so a burst of build jobs can start a third build before PSI shows the second. At 20 it
  did, in every run (above). Replayed, 10 would hold warm bursts at 2; cold ones need a spacing of about 45–60 s.
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
  `extrapolated` (derived from a rule; not measured on this shape of hardware), or `measured input, gate
  unvalidated` (the figure it rests on was measured on this shape, the setting's own effect was not).
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
  from install.md. `--restart-gateway` restarts the gateway and waits for its port, since it reports active
  before it listens. `--root DIR` edits copies under DIR, to try it without touching the host.

| Rule | Recommendation | Measured when |
|---|---|---|
| `shm.size` | MemTotal − 8 GiB (MemTotal ≥ 16 GiB), else MemTotal/2; error below 8 GiB | MemTotal within 2 GiB of the measured 23909 MiB |
| `kata.default_maxmemory` | 8192 at ≤ 36 physical address bits, else Kata's default | 36 bits |
| `kata.default_vcpus` | 2 at ≥ 4 cores, else 1; never above 2 | 4 cores |
| `kata.default_memory` | idle 2048, build 2048, heavy 4096 | idle and build (any host); heavy never |
| `gateway.max_running_jobs` | the core count (the default) | 4 cores |
| `gateway.cpu_psi_max` | 20 on 4 cores with 2-vCPU guests, else the default 40, "uncalibrated" | never: on 4 cores with 2 vCPUs it is `measured input, gate unvalidated` (see below) |
| `disk.class` | information: the measured cold-start cost of a rotational disk | rotational |
| `prereq.*` | `/dev/kvm`, vmx/svm, Kata config, initrd drop-in, fstab line = running mount, firewall, modes | (checks) |

`gateway.cpu_psi_max = 20` is `measured input, gate unvalidated`: the PSI it rests on was measured (two 2-vCPU
builds peaked at 23.7). The gate set to 20 ran against bursts of real build jobs on 2026-10-24 (ADR-013) and did
not hold builds at 2, so the tag stays. A limit of 10 is proposed from that run's curves. Rule, reason text
and tag change only once a live run shows the new limit holding.

*Live (2026-10-23, BaileyServ, gateway running).* `sudo ac-ctl doctor --profile build`: 17 ok, 1 info (disk
class), every finding measured, exit 0 (since relabelled: the PSI limit is `measured input, gate unvalidated`; as a user: gateway.env and the firewall skipped, still exit 0). `tune`
showed no changes. `--json` parsed with `python3 -m json.tool`. On copies of the live files with
`default_memory = 4096`, `default_vcpus = 1`, `default_maxmemory` commented out, no PSI line and no fstab line
(`--root`): doctor exit 2 (2 errors, 3 warnings; errors exit 4 since); `tune --apply` wrote exactly those five edits, three backups
equal to the copies, gateway.env still 0600, the rest of each file byte-identical; a second run changed nothing.
With a1 running, `--apply` was refused naming a1 and wrote nothing. The gateway started on the untuned copy
logged `settings differ ... default_maxmemory unset (0: all host RAM) -> 8192, default_vcpus 1 -> 2,
default_memory 4096 -> 2048, AC_JOB_CPU_PSI_MAX unset (40) -> 20; run ac-ctl doctor`, and nothing on the live
settings. `post-reboot-check.sh` passed with its new line. The facts file of the hypothetical 32 GiB / 8 core /
NVMe host forecasts 11 agents at 2048 MiB and 4 builds at once (extrapolated).

**Consequences:**
- A new host gets its numbers, with their provenance, before anything is installed: `doctor --facts-file`.
- On another shape most figures are `extrapolated`, and the doctor says so; the PSI limit stays at the default
  until `scripts/real-build-bench.sh` calibrates it there.
- A setting a person chose deliberately below the recommendation (fewer job slots, a calibrated PSI limit on
  other hardware) is `info`, not a warning, and tune leaves it.
- Exit codes: doctor 0 ok, 1 warnings, 4 errors (ac-ctl's 2 is bad usage, or doctor could not run, and 3 is
  "the gateway holds the data dir"); `post-reboot-check.sh` reads the JSON `errors` count, not the exit code,
  and FAILs on errors only.
- MemTotal moves by a few MiB across kernels, so `/dev/shm` within 1% of the target is ok.

**Not now:** calibrating the PSI limit automatically; editing the initrd, firewall or units; per-agent memory
classes; telling nested virtualization from bare metal.
