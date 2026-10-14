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

Reserve about 2 cores and 8 GiB for the host, Postgres and the control plane. RAM caps you
first; CPU can be overcommitted 2-4x because most agents wait on model calls.

| Server | Specs | Active at once | Provisioned, mostly idle |
|---|---|---|---|
| Mini PC | 8 cores, 32 GB RAM, 1 TB NVMe | 4-6 | 8-10 |
| Workstation | 16 cores, 64 GB RAM, 2 TB NVMe | 10-12 | 20+ |
| Server class | 32+ cores, 128-256 GB ECC, NVMe mirror | 25-50 | 80+ |

## Requirements that matter more than size

- KVM on bare metal (nested works but costs performance and adds complexity).
- NVMe storage; builds saturate SATA first. Keep Postgres on fast storage.
- No GPU unless you add local models.
- ECC RAM is nice for an always-on box, not required.

## Limits you will hit before hardware

- Model API rate limits and subscription caps.
- Memory spikes from builds and browsers: set per-agent limits and a build concurrency cap.
- Sustained simultaneous load moves you toward the "active at once" column.

## Recommendation

Start on 16 cores and 64 GB. Measure RAM and CPU for a week on a few agents, then resize.
