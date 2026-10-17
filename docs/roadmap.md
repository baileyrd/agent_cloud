# Roadmap

## M0: Host ready
- [ ] KVM verified on the Fedora server (`/dev/kvm`, `virt-host-validate`)
- [ ] containerd and Kata installed; kernel-differs smoke test passes
- [ ] SELinux denials, if any, resolved without disabling it

## M1: One agent, by hand
- [ ] Desktop image: Xorg dummy (or chosen streamer), Xfce, policy-locked Chromium
- [ ] Shell image: unprivileged user, harness, toolchains
- [ ] Start one agent under Kata, open its desktop, run a task end to end

## M2: Viewer
- [ ] Decide streaming layer (ADR-006)
- [ ] Watch and take over via one authenticated WSS

## M3: Control plane
- [ ] Spawner: create, pause, destroy, workspace volume lifecycle
- [ ] Gateway with token auth over Tailscale HTTPS
- [x] Session registry on the multimodal engine (`ac-store`) and event log (`ac-history`)
- [x] Fleet lifecycle and `ctr`/Kata adapter against fakes (`ac-fleet`, `ac-ctr`); real-host run pending M0

## M4: Safe to leave running
- [ ] Egress allowlist proxy and nftables rules
- [ ] Per-agent CPU, memory and disk limits
- [ ] Backups with a tested restore
- [ ] Five-agent load test; update [sizing.md](sizing.md) with real numbers

## Later
- Local model option for an offline posture
- Snapshot and resume agents
- Capture the Dots viewer protocol on an unfiltered network

## Open questions
1. Pod shape (ADR-004) and control-plane shape (ADR-005).
2. Which harnesses to support first.
3. Does Cloudroom core earn a place inside a full VM for any agent?
4. Hardware: what does the Fedora server actually have (CPU, RAM, KVM)?
