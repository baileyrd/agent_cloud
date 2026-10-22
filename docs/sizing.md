# Sizing

All numbers are **estimates [unverified]**. Nobody has published sizing for this stack.
ChatGPT Dots reported about 9 vCPUs, 10 GiB RAM and a 32 GB disk inside its sandbox, but
those are visible limits, not dedicated hardware. Replace these with your own
measurements (install.md, Phase 8).

## Per-agent footprint

| Resource | Idle | Active |
|---|---|---|
| RAM | about 2 GiB | 4-6 GiB (Chromium with tabs, harness, desktop) |
| CPU | under 0.2 core | 1-2 cores, more during builds |
| Disk | 10-20 GB | grows with repos and build artifacts |

Kata overhead is roughly 100-200 MiB per microVM for the guest kernel and runtime.
Builds (Rust in particular) are the spike.

## Capacity

Reserve about 2 cores and 8 GiB for the host and the control plane. RAM caps you
first; CPU can be overcommitted 2-4x because most agents wait on model calls.

| Server | Specs | Active at once | Provisioned, mostly idle |
|---|---|---|---|
| Mini PC | 8 cores, 32 GB RAM, 1 TB NVMe | 4-6 | 8-10 |
| Workstation | 16 cores, 64 GB RAM, 2 TB NVMe | 10-12 | 20+ |
| Server class | 32+ cores, 128-256 GB ECC, NVMe mirror | 25-50 | 80+ |

## Requirements that matter more than size

- KVM on bare metal (nested works but costs performance and adds complexity).
- NVMe storage; builds saturate SATA first. Keep the data directory on fast storage.
- No GPU unless you add local models.
- ECC RAM is nice for an always-on box, not required.

## Limits you will hit before hardware

- Model API rate limits and subscription caps.
- Memory spikes from builds and browsers: set per-agent limits and a build concurrency cap.
- Sustained simultaneous load moves you toward the "active at once" column.

## Recommendation

Start on 16 cores and 64 GB. Measure RAM and CPU for a week on a few agents, then resize.

## Reference host: BaileyServ (measured with `scripts/capacity.sh`)

| Item | Value |
|---|---|
| CPU | Intel Core i5-3550 (Ivy Bridge, 4 cores / 4 threads, 3.3-3.7 GHz); VT-x and EPT supported per Intel ARK |
| RAM | 24 GiB DDR3-1600 (2x4 + 2x8), all four slots used; CPU maximum is 32 GiB, so upgrade means 4x8 |
| Disk | One 1 TB 2.5" rotational drive (`ST1000LM024`), ~910 GB free; no SSD |
| OS | Fedora 44 Workstation, SELinux enforcing, bare metal (`systemd-detect-virt` = none) |
| Blocker | `/dev/kvm` absent: VT-x disabled in firmware (`vmx` flag count 0) |

Estimate from the per-agent numbers above (host reserve 3 GiB and 1 core): **4 active / 9 provisioned**, limited by RAM.
Practical plan, adjusted for 4 non-hyperthreaded cores and the spinning disk: **2-3 active, 6-9 provisioned**.
The disk, not RAM, is the first thing to fix: put `/var/lib/containerd` and the data directory on an SSD.
Treat this box as a proof-of-concept host. **[unverified]**: nothing has been run on it under Kata yet.

## Reference host: Proxmox (MS-01 class, measured)

| Item | Value |
|---|---|
| CPU | Intel Core i9-12900H (6P + 8E cores, 20 threads); VT-x; nested KVM enabled (`kvm_intel nested = Y`) |
| RAM | 31 GiB total; about 14 GiB used and 16 GiB available with the current guests |
| Disk | 1 TB NVMe (Kingston); a 23.6 TB external HDD (bulk storage only) |
| Guests | VMs: opnsense (8 GiB), homeassistant (4 GiB). LXC: samba, plex, arrstack, traefik, headscale, remindme, agentos |

Option: a Fedora VM here with CPU type `host` can run Kata (nested). RAM is the limit: a 12 GiB VM gives roughly
1-2 active / 4 provisioned agents with these per-agent numbers, each noticeably faster than on BaileyServ
(NVMe, modern cores), minus the nested-virtualization overhead **[unverified, not measured]**.
Do not run Kata in an LXC container (shared host kernel, no supported path).
