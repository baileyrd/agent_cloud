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
