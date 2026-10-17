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
**Consequences:** Sync ports first; async (tokio) enters only with the gateway's WebSocket proxy.

## ADR-006: Desktop streaming layer
**Status:** open.
**Context:** Dots' viewer protocol is unknown (the WebSocket was blocked by a network proxy
during inspection).
**Options:** KasmVNC (bundles X server, VNC and web client; simplest), Xvfb/x11vnc plus
noVNC, a WebRTC streamer such as Neko or Selkies (lower latency), ffmpeg `x11grab` into the
gateway (most control, most work).
**Leaning:** KasmVNC to get a working viewer, behind the gateway so clients use one
authenticated WSS. Note it brings its own X server; the dummy-driver Xorg design may be replaced.

## ADR-007: Egress control
**Status:** proposed.
**Decision:** Agents have no direct internet route; all traffic goes through an allowlist
proxy plus nftables rules on the host.

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
