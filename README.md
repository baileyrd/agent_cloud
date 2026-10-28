# agent-cloud

Self-hosted "cloud computers" for AI agents on a single Fedora home server.
Each agent gets its own isolated machine (a Kata Containers microVM on KVM) with a
shell, a desktop and a browser. You can watch it work and take over, from any
device on your tailnet. Nothing runs in a third-party cloud except the model
calls the agents themselves make.

> Status: **early build.** The Rust domain, fleet, `ctr`/Kata adapter, registry and
> event-log crates have unit tests; nothing has run on real KVM yet. Claims that
> are unverified are marked **[unverified]**. See [docs/research-notes.md](docs/research-notes.md)
> for what the design is based on and what is still unknown.

## What you get

- One isolated microVM per agent, with its own guest kernel (hardware isolation, not just namespaces).
- A shell environment for the agent harness (Codex, Claude Code, Pi or similar).
- A virtual desktop (Xorg dummy display, Xfce, policy-locked Chromium) the agent can drive.
- An MCP endpoint (`POST /api/v1/mcp`) whose tools follow the caller's token scopes (ADR-017).
- A single authenticated WebSocket for watching and taking over the desktop.
- Persistent workspace volumes with checkpoints, restore and forks on btrfs, an embedded agent registry and append-only per-agent event history (no database server). Workspaces are backed up incrementally to a second btrfs disk (`ac-ctl backup`, ADR-019).
- Allowlisted egress: agents reach model APIs and Git hosts only, through `ac-egress` (ADR-007).

## What this is not

- Not multi-tenant hosting. It assumes one trusted owner.
- Not offline. Agents call hosted model APIs unless you configure a local model.
- Not a fork of Cloudroom core. Cloudroom informed the design but needs a full Ubuntu VM
  per agent (systemd, ext4 quotas). See [ADR-003](docs/decisions.md).

## Architecture at a glance

![architecture](docs/architecture.svg)

| Layer | Component |
|---|---|
| Host | Fedora Server, KVM, containerd + Kata runtime, SELinux enforcing, cgroup v2, Tailscale |
| Control plane | One Rust binary: gateway, fleet/spawner, embedded registry + event log, workspace volumes, secrets, egress allowlist proxy |
| Per-agent microVM | Shell container (agent harness) + desktop container (Xorg dummy, Xfce, Chromium, streamer) |
| External | Model APIs, Git remotes (allowlisted) |

Details: [docs/architecture.md](docs/architecture.md).

## Quick start

1. Confirm KVM works on the host: [docs/install.md, Phase 0](docs/install.md).
2. Install containerd and Kata, then run the isolation smoke test (Phases 1-3).
3. Build the agent image (Phase 4), start one agent by hand, open its desktop.
4. Drive it with the CLI (needs root for containerd):
   `cargo build --release -p ac-ctl`, then
   `sudo target/release/ac-ctl --image agent:latest create a1` (also `pause`, `resume`, `destroy`, `status`).
   `destroy` keeps the workspace (a later `create` of the same id reuses it); `destroy --purge` removes it.
   `pause` stops the VM and `resume` boots a fresh one on the same workspace. `reconcile` (no agent id) marks
   agents whose VM vanished, for example after a reboot, as paused; the gateway runs it at startup.
   Defaults: data in `/var/lib/agent-cloud`, containerd namespace `agents`. Verified on one host (Fedora 44, i5-3550):
   create, pause, resume, destroy all worked. A fresh Fedora host needs the fixes in [docs/install.md](docs/install.md#fedora-44-notes-verified-on-baileyserv-i5-3550-2026-10-18).
5. Check the host's settings against its facts: `sudo ac-ctl doctor`, and `sudo ac-ctl tune` for the diffs it
   would apply (ADR-014; [install.md](docs/install.md#host-settings-ac-ctl-doctor-and-ac-ctl-tune-adr-014)).
6. Run `sudo scripts/m0-smoke.sh` first on a new host: it checks KVM, containerd, Kata and the
   kernel-differs isolation test (runbook Phases 0-3).
7. Gateway: `ac-gateway` serves `GET /api/v1/agents` (every agent and its state),
   `GET|POST|DELETE /api/v1/agents/{id}` and `POST .../pause|resume`
   with a scoped bearer token (`ac-gateway token create|list|revoke`, ADR-011). On a btrfs data disk,
   `.../checkpoints`, `.../restore` and `.../fork` checkpoint, restore and fork workspaces (ADR-018;
   [install.md](docs/install.md#host-services)). `DELETE` keeps the workspace;
   `DELETE ...?purge=true` removes it. Env: `AC_TOKENS` (default `/etc/agent-cloud/tokens`, 0600), `AC_IMAGE`, `AC_BIND` (private addresses
   only; default `127.0.0.1:8780`), `AC_DATA`. The terminal viewer is at `/ui/<agent-id>` (ADR-006; how to open it:
   [docs/install.md, Terminal viewer](docs/install.md#host-services)). Not yet: the agent API (AG-UI) and desktop viewer.
   systemd units for the gateway, the agent firewall and the egress proxy (`ac-egress`, ADR-007): [docs/install.md, Host services](docs/install.md#host-services).

## Documentation

| Doc | Purpose |
|---|---|
| [docs/architecture.md](docs/architecture.md) | Components, data flows, trust boundaries |
| [docs/install.md](docs/install.md) | Phased install runbook with checkpoints |
| [docs/security.md](docs/security.md) | Threat model and hardening checklist |
| [docs/sizing.md](docs/sizing.md) | Per-agent footprint and capacity, measured on BaileyServ; hardware per 4/8/16 agents |
| [docs/decisions.md](docs/decisions.md) | Architecture decision records |
| [docs/research-notes.md](docs/research-notes.md) | Evidence behind the design and open unknowns |
| [docs/roadmap.md](docs/roadmap.md) | Milestones and open questions |

## Engineering conventions

Modular, typed and tested: explicit over implicit, type hints on all Python, flat
control flow, small single-purpose functions, domain logic free of I/O, minimal
dependencies, no secrets in repo or logs, external input validated at the boundary.
Tests cover happy path and failure cases for all non-trivial logic.
