# Roadmap

## M0: Host ready
- [x] KVM verified on the Fedora server (BaileyServ: VT-x enabled in BIOS, `/dev/kvm` present)
- [x] containerd and Kata installed; VM smoke test passes (`scripts/m0-smoke.sh`)
- [x] SELinux stays enforcing; no Kata-related denials seen

## M1: One agent, by hand
- [ ] Desktop image: Xorg dummy (or chosen streamer), Xfce, policy-locked Chromium
- [x] Shell image (`images/agent-shell`): unprivileged user, Claude Code, toolchains; its login is kept on
  `/workspace` and survives pause/resume
- [x] Start one container under Kata via `ac-ctl` (create/pause/resume/destroy verified with `nginx:alpine`)
- [ ] Start one real agent image (shell + desktop), open its desktop, run a task end to end

## M2: Viewer
- [x] Terminal viewer: watch an agent's tmux session and take over its input from a browser, view-only by
  default, recorded while viewed (ADR-006; verified on BaileyServ 2026-10-20)
- [x] Window list in the viewer: watch any of the session's windows (job windows marked) without moving the
  agent, and type into the one shown (ADR-006; verified on BaileyServ 2026-10-22; Ctrl-b never reached tmux
  from a browser)
- [ ] Desktop streaming layer (ADR-006, open part) on the same transport and take-over lock

## M3: Control plane
- [x] Spawner: create, pause (stops the VM), resume, destroy (keeps the workspace) and purge, verified through
  the gateway on BaileyServ (2026-10-19)
- [x] Startup reconcile: agents recorded running with no VM become paused and are cleaned up (`ac-ctl reconcile`,
  run by `ac-gateway` at start); verified after a simulated VM crash and across a real reboot (2026-10-20)
- [x] Gateway lifecycle API with bearer auth, private-bind check (`ac-gateway`); TLS via Tailscale still pending
- [x] Task API: jobs with ids, long-poll status, capped output, cancel, timeouts, re-attach after a gateway restart
  (ADR-010; verified on BaileyServ 2026-10-21)
- [x] Scoped API tokens: one per caller, with scopes and agents, digests only, audit trail, revocable while the
  gateway runs (ADR-011; verified on BaileyServ 2026-10-22)
- [ ] Token expiry; failure limits per source address (needs the peer address from rusty_serve); drop the
  deprecated `AC_TOKEN_FILE` once hosts have migrated
- [ ] Per-job event stream (AG-UI via `rusty_agui`)
- [x] Session registry on the multimodal engine (`ac-store`) and event log (`ac-history`)
- [x] Fleet lifecycle and `ctr`/Kata adapter (`ac-fleet`, `ac-ctr`), tested against fakes and on the host

## M4: Safe to leave running
- [x] Egress allowlist proxy and nftables rules (`ac-egress`, `deploy/agentfw.nft`; ADR-007), verified from an agent
  2026-10-19
- [x] `tini` as the agent image's PID 1, so killed jobs leave no zombies; no Claude Code self-update
  (`DISABLE_AUTOUPDATER=1`) (verified on BaileyServ 2026-10-22)
- [ ] Per-agent CPU, memory and disk limits
- [ ] Backups with a tested restore
- [x] Load test to N=6 with `scripts/capacity-bench.sh`; [sizing.md](sizing.md) now holds measured numbers
  (BaileyServ, 2026-10-22)
- [ ] Watchdog false positives under load: at N=3 and N=6 a busy agent missed the 2 s probe and the
  watchdog SIGKILLed exec clients, a running job's included (sizing.md). Probe with a longer timeout or
  only when the host is not saturated
- [ ] Guests never return memory and their RAM lives in `/dev/shm` (11.7 GiB here, filled at N=6): set
  `default_memory` to 2048 (measured enough for Claude Code), size `/dev/shm` to the running agents, and
  try free page reporting or virtio-mem
- [ ] Creates are serialized behind the gateway's API lock (a 4-agent storm takes 4 + 4 + 4 + 4 s)

## Later
- Local model option for an offline posture
- Memory snapshots, so pause/resume keeps running processes (today pause stops the VM)
- Capture the Dots viewer protocol on an unfiltered network

## Open questions
1. Pod shape (ADR-004) and control-plane shape (ADR-005).
2. Which harnesses to support first.
3. Does Cloudroom core earn a place inside a full VM for any agent?
4. Hardware: what does the Fedora server actually have (CPU, RAM, KVM)?
