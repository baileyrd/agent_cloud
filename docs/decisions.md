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
  (`AC_PROBE_INTERVAL`) with a 2 s exec; on failure it SIGKILLs our `ctr` clients, then every
  `ctr` exec client of that agent, and if it still fails marks it `"health":"unhealthy"` without
  touching the VM.
- *Transport:* rusty_serve has no WebSocket upgrade, so a `fetch()`-streamed chunked GET carries
  NDJSON (a full snapshot with scrollback on connect, then changed lines and the cursor), and
  POSTs carry input, resize and take-over. `fetch` sends `Authorization`, so no ticket is
  needed. Origin is checked on every request; caps of 4 viewers per agent and 16 in all leave
  rusty_serve's 64 connections for the API; viewer routes never take the lifecycle lock.
- *Rendering:* the gateway turns snapshots into styled runs (SGR only) and drops every other
  escape (OSC 8/52, titles, DCS, APC, C0/C1, bidi overrides). The page (about 250 lines of
  in-repo JS, no third-party code, CSP `'self'` only) sets text with `textContent`.
- *View-only and take-over:* view-only is the default. The guarantee rests on the gateway, not
  tmux: input from a viewer without the lock gets 403 before anything is built; the view
  client's whole vocabulary is a closed enum (`ViewCommand`: client name, cursor info,
  snapshot); the take-over client exists only while a viewer holds the lock and takes only
  `Input`, whose bytes are sent as `send-keys -H` hex (key names are mapped to terminal bytes),
  so no viewer byte can be read as a tmux command. tmux's `read-only` flag was tried and dropped:
  while such a client is attached, tmux refuses the agent's own tmux commands, even from inside
  its pane. One viewer holds the lock; a second take-over is refused unless forced; release or
  disconnect returns to view-only.
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
**Status:** proposed (2026-10-21).
**Context:** A script or another agent should be able to say "run this prompt in agent X" and get a job
id, live status and the result, without exec-ing into the VM. Everything inside the VM and its workspace
is writable by the agent, so a prompt-injected agent can tamper with any file it writes there.
**Decision:** Jobs are part of `ac-gateway`, under `/api/v1/agents/{id}/jobs`, with the same token, Origin
and bind rules as the rest of the API.
- *Routes:* `POST .../jobs` `{"prompt", "timeout_secs"?, "idempotency_key"?}` answers 202 with the job;
  `GET .../jobs` (the agent's retained jobs, newest first); `GET .../jobs/{job}[?wait=N]` (N <= 30 s:
  long-poll until the state changes); `GET .../jobs/{job}/output` (the final assistant message) and
  `.../output?part=stream` (the stream-json log), plain text; `POST .../jobs/{job}/cancel`.
- *States:* `queued`, `running`, `succeeded`, `failed`, `cancelled`, `timed_out`, `interrupted`, as a pure
  state machine in `ac-domain` (`job.rs`). One running job per agent; up to 8 queue behind it (429 beyond).
  A job is accepted only while the agent is `running` (409 otherwise; resume is an explicit call).
- *Trust:* state, timestamps, timeout and exit code live in the host registry (`ac-store`, `jobs.mmap`
  beside `agents.mmap`, same lock). What the agent writes (the result and the stream log) is untrusted
  data: capped, served as `text/plain` with `nosniff`, never parsed by the gateway.
- *Prompt delivery without stdin:* the prompt is kept host-side (`<data>/jobs/<agent>/<job>/prompt.txt`,
  root, 0700 directories). At start the gateway creates `.job-<job>.prompt` (0600, owned by `AC_UID`) in the
  root of the agent's workspace with `O_CREAT|O_EXCL`, which never follows a symlink. Everything below the
  workspace root is agent-controlled, so the gateway (root) never writes, chowns or reads a path inside
  it; a symlinked `.jobs` would otherwise turn a root write into a write anywhere on the host. A short
  start exec, running as the agent user in the VM, makes `.jobs/<job>/` (0700), moves the prompt to
  `prompt.txt`, and opens tmux window `job-<job>` in session `main`. Every exec is an argv array: a
  constant script plus the job id (gateway-generated, `[a-z0-9]`) and operator settings as positional
  parameters. The prompt never appears in any argv, host or guest; `claude` reads it on stdin from the file.
- *Running:* the window runs `claude -p --output-format stream-json --verbose <permission flags>
  < prompt.txt`, through `head -c 16 MiB` (a job whose log passes that is stopped) and `tee stream.jsonl`,
  with a readable digest on the terminal, so a human can watch it in the viewer and take over. When
  `claude` exits, the runner writes `result.txt` (the `result` event, at most 256 KiB) and then
  `exit_code` (via rename). The execs that start, poll and kill a job exit by themselves and run with
  stdin `/dev/null`, the probe's path (`exec_once`), so no live exec ever sees stdin EOF.
- *Supervision:* one gateway thread ticks every second. For a running job it polls a 2 s status exec
  (window alive, else `exit_code`, else gone) every 2 s; on exit it copies the capped result and the last
  1 MiB of the log to the host job directory with two more execs, then records the outcome. Output stays
  readable after the agent is paused.
- *Timeouts and cancel:* default 30 min, 1 s to 4 h, enforced by the gateway from the registry's start
  time. Cancel and timeout run a kill exec: it walks the window's process tree (children by parent pid,
  plus the pane's session), stops every process, kills it, closes the window, and fails unless all of
  them are gone. The runner also wraps `claude` in `timeout` at the deadline plus 5 min, a backstop for
  a gateway that is down.
- *Lifecycle:* pause or destroy of an agent with a queued or running job is 409 unless `?force=true`;
  then the running job becomes `interrupted` and queued ones `cancelled`. Startup reconcile does the same
  for agents without a VM. Queued jobs are cancelled, not kept: resuming is an explicit, possibly much
  later act, and running stale work then would surprise the caller. A gateway restart re-attaches: a job
  recorded `running` is polled, never started again (and the start exec refuses an existing
  `.jobs/<job>`). The job is marked `running` before its start exec, so a crash between the two shows up
  as `failed` ("runner ended without an exit code"), never as a duplicate.
- *Permissions:* a headless `claude` cannot ask. `AC_JOB_PERMISSION_MODE` (default `acceptEdits`) and
  `AC_JOB_ALLOWED_TOOLS` (default `Read,Glob,Grep,Edit,Write` and `Bash` patterns for `git
  status|diff|log|add|commit`, `ls`, `cat`, `python3`, `cargo build|test|check|fmt`, `sleep`) become
  flags. `bypassPermissions` / `--dangerously-skip-permissions` only with `AC_JOB_UNSAFE=1`.
- *Limits and retention:* prompt <= 64 KiB; per agent the last 20 finished jobs are kept (registry
  records and host copies), and the workspace keeps the last 5 `.jobs/<job>` directories, each log at
  most 16 MiB, so job logs take at most about 85 MiB of a workspace.
- *Idempotency:* the same `idempotency_key` for the same agent returns the retained job with that key
  (200) instead of creating one, whatever its prompt or state.
- *Events:* queued, started, the outcome, cancelled and interrupted go to the agent's `ac-history` log
  as `job <id> <state>`: ids and states only, timestamps from the log, no prompt or output.
**Consequences:** jobs run with the agent's own login and permissions; the permission list is a second
layer and the VM, egress allowlist and isolation are the boundary. A job's outcome is what the agent's
VM reports: a compromised agent can fake its own job's exit code and result, never another agent's, and
never the registry's state or times. Output is read through execs, so a wedged agent delays collection
(the watchdog still recovers it).
**Not now:** `rusty_agui` (AG-UI events over SSE) fits a later per-job event stream that turns the
stream-json log into `RUN_STARTED`/`TEXT_MESSAGE_*`/`TOOL_CALL_*`/`RUN_FINISHED`; it does not fit the
control plane (AG-UI has no queue, job id or re-attach; a run is one request). Long-polling the status
is enough for now.
