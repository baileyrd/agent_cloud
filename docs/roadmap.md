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
- [x] Token expiry: opt-in `--expires`, refused from then on, streams ended (ADR-022)
- [ ] Failure limits per source address (needs the peer address from rusty_serve); drop the
  deprecated `AC_TOKEN_FILE` once hosts have migrated
- [ ] Per-job event stream (AG-UI via `rusty_agui`)
- [x] Session registry on the multimodal engine (`ac-store`) and event log (`ac-history`)
- [x] Fleet lifecycle and `ctr`/Kata adapter (`ac-fleet`, `ac-ctr`), tested against fakes and on the host

## M4: Safe to leave running
- [x] Egress allowlist proxy and nftables rules (`ac-egress`, `deploy/agentfw.nft`; ADR-007), verified from an agent
  2026-10-19
- [x] Per-agent egress profiles (`rust`, `python`, `node`) keyed by a per-boot proxy credential, attached with an
  `egress:write` token, no restart (ADR-016; verified on BaileyServ 2026-10-25, a reboot still to test)
- [ ] Verify the `python` and `node` profiles with a real `pip install` and `npm install` (only `curl -I` so far)
- [x] `tini` as the agent image's PID 1, so killed jobs leave no zombies; no Claude Code self-update
  (`DISABLE_AUTOUPDATER=1`) (verified on BaileyServ 2026-10-22)
- [x] Per-agent CPU, memory and disk limits: `cpu.max` and `memory.max` on each sandbox's cgroup and a btrfs
  simple quota on its workspace, applied on every boot (ADR-023; verified on BaileyServ 2026-10-29 with
  `scripts/limits-test.sh`: held at 2.00 cores under load, a write past the quota refused)
- [ ] Backups with a tested restore: workspaces as incremental `btrfs send` stream files in any directory (a share
  on another machine), daily by `ac-backup.timer`, with `ac-ctl restore-backup` replaying a chain (ADR-021; verified
  on BaileyServ 2026-10-29 against an SMB share on Beast: full and incremental backups of a1 and a2, a restore of
  a2). To do: boot a restored workspace and read it back inside the VM; back up the registry, tokens and egress
  agents file too
- [x] Load test to N=6 with `scripts/capacity-bench.sh`; [sizing.md](sizing.md) now holds measured numbers
  (BaileyServ, 2026-10-22)
- [x] The watchdog tells slow from wedged: it acts only on sustained failures on an idle host, through a logged
  ladder (ADR-012; verified 2026-10-22)
- [x] Admission control: no create or resume beyond what `/dev/shm` holds; `/dev/shm` sized MemTotal − 8 GiB
  (ADR-012; verified 2026-10-22)
- [ ] Guests never return memory: try free page reporting or virtio-mem
- [x] Real-build measurement (`scripts/real-build-bench.sh`, 2026-10-22). This repo's release and test builds
  peak at 314 MiB used at 1 vCPU (468 MiB at 2), with ≥ 65% of a 2048 MiB guest free, so per-agent memory
  classes are not needed for builds of this size. With 2 or 3 builds at once, CPU is the limit
  ([sizing.md](sizing.md#real-builds)).
- [x] `default_memory` and `default_vcpus` set to 2048 and 2, as the measurements proposed (admits 7 agents
  here instead of 3; builds 40% faster; 7% slower at 3 builds at once).
- [ ] Measure a bigger build (a larger Rust workspace, C++ or npm) before trusting 2048 for every build
- [ ] Measure on an SSD (every figure in sizing.md is from one rotational disk)
- [ ] Creates are serialized behind the gateway's API lock (a 4-agent storm takes 4 + 4 + 4 + 4 s)
- [x] Job start gate: a host-wide cap (cores), CPU PSI and load, and 15 s between starts; FIFO across agents;
  `queued_reason` and job counts in `GET /api/v1/capacity` (ADR-013)
- [x] Measure the gate with real build jobs submitted together (`scripts/burst-test.sh`, 2026-10-24,
  [sizing.md](sizing.md#job-start-gate-under-real-builds)). `AC_JOB_CPU_PSI_MAX=20` with 15 s spacing does
  **not** keep a burst at 2: in 3 of 3 runs a third build started after the spacing, before PSI showed the
  second build. `ac-ctl doctor` now recommends the default 40 on every host, tagged `measured: 20 did not hold
  a burst`: a safety valve against saturation, not a burst limiter.
- Dropped (2026-10-24): running B again at `AC_JOB_CPU_PSI_MAX=10`. A PSI gate cannot see a build before it
  starts. The makespan was the same with and without the gate (B warm 153–159 s, D 158 s). A gate strict enough
  to hold builds at 2 would cost a tiny job about 75 s (an estimate from the replay).
- [ ] Light jobs and the gate (design note, not implemented). A gate strict enough to hold builds at 2 holds
  every job. At PSI 10 the replay estimates a tiny job about 75 s behind two warm builds, ten times its own 6–8 s
  run (measured at 20 the cost was only the 15 s spacing). And no start-time gate sees a build until after the
  job's first model turn (4–7 s warm, 24–42 s cold) plus its signal's lag (PSI avg10: 10–13 s). Options:
  - *Count builds instead of pressure.* A running job whose VM used more than one core over the last few
    seconds counts as a build, from the VM's own CPU (the gateway knows each agent's QEMU and virtiofsd). A new
    job waits only while `floor(cores / vCPUs)` jobs count as builds. Light jobs then run beside two builds, and
    the signal follows a build within a sample, not avg10's 10 s. A burst still needs a spacing about as long
    as a job's time to its first command.
  - *A job declares itself light* (`"light": true` on submit, or a per-token default) and skips the pressure
    gate and the spacing, keeping the cap. It is simpler, but a mislabelled build runs unchecked.
  - Preferred: counting builds. It also fixes the lag, which a light flag does not.
  - Revisit when jobs time out under concurrent builds, when tiny-job waits become a real complaint, or when
    more than 2 concurrent builds become routine. It is moot at 8+ cores, where three 2-vCPU builds no longer
    oversubscribe the CPU.
- [x] Settings from host facts: `ac-ctl doctor` (read-only; each figure `measured` or `extrapolated`, with its
  source) and `ac-ctl tune` (diffs; `--apply` with backups); `--facts-file` plans other hardware; the gateway
  logs one line when its settings differ (ADR-014)
- [ ] Calibrate `default_vcpus` and builds at once on another core count with
  `scripts/real-build-bench.sh`, so doctor can mark them measured there
- [x] `ac-gateway.service` is `Type=notify` (ADR-015): a restart returns once the gateway listens, with its
  status (`reconcile done` or `failed`); verified 2026-10-24 (restart then curl with no sleep, gateway SIGKILL
  with a1 running and with a1's VM killed). A reboot is still to test.
- [x] MCP interface on the gateway (`POST /api/v1/mcp`), one tool per route, listed by the token's scopes
  (ADR-017); verified 2026-10-28 with Claude Code as the client on BaileyServ
- [x] Workspace checkpoints, restore and forks on btrfs subvolumes, through the gateway and `ac-ctl` (ADR-018;
  verified on BaileyServ with live VMs 2026-10-28: checkpoint mid-write, fork, restore and undo)
- [x] Each job's changed paths in the agent's history, from btrfs snapshots at its start and end (ADR-019;
  tested on a loopback btrfs; not yet on BaileyServ)

## Later
- Local model option for an offline posture
- Memory snapshots, so pause/resume keeps running processes (today pause stops the VM)
- Capture the Dots viewer protocol on an unfiltered network

## Open questions
1. Pod shape (ADR-004) and control-plane shape (ADR-005).
2. Which harnesses to support first.
3. Does Cloudroom core earn a place inside a full VM for any agent?
4. Hardware: what does the Fedora server actually have (CPU, RAM, KVM)?
