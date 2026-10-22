# Sizing

Measured on BaileyServ on 2026-10-22 with [`scripts/capacity-bench.sh`](../scripts/capacity-bench.sh).
Raw samples (2 s host CSV, per-VM CSV, events) are in `/var/tmp/ac-bench-<date>/` on the host. This replaces
the earlier estimates. What changed: an agent costs far less than estimated when fresh (0.5 GiB, not 2 GiB),
but the guest never returns memory, so a long-running agent drifts toward its full `default_memory`. On this
host the first limits are the single rotational disk and `/dev/shm`, not RAM. Since then `/dev/shm` has been
enlarged to MemTotal − 8 GiB (15.3 GiB here), and the gateway refuses a create or resume that would not fit it
(ADR-012). The rule: **N × `default_memory` + 512 MiB must fit `/dev/shm`.**

## How it was measured

- **Agents.** `bench-<n>` agents were created and destroyed through the gateway API, using a throwaway token
  with `agents:read,agents:write` for `bench-*` only. Each exec held its stdin open (ADR-010).
- **Load.** Each step of the staircase ran three synthetic jobs in every agent at once, for 90 s:
  - a python3 busy loop;
  - a 1 GiB allocation, touched every 5 s;
  - a disk job on `/workspace` (virtiofs on btrfs). It reads a 512 MiB file cold, after a host page-cache drop,
    then writes 512 MiB of random data with an fsync every 8 MiB.
- **No Claude calls.** The staircase used no Claude Code calls. Claude's own cost was measured separately on
  a1, which already has a login (see [Claude Code](#claude-code-measured-on-a1)).
- **Watchdog.** A 2 s watchdog stopped the run on the first threshold crossed: MemAvailable < 1.5 GiB, swap
  traffic, memory PSI, host OOM, ac-gateway or ac-egress not answering, or load > 8 for 60 s. The io PSI
  `full` limit was 80 during load phases and 40 elsewhere.
- **Runs.** There were four runs. A run is labelled by the time it started, which is also its directory name.

  | Run | Scope | How it ended |
  |---|---|---|
  | 10:24 | N=1 | io PSI full 41.8 > 40; the limit was still 40 in load phases then |
  | 11:21 | N=1 plus the storm | complete |
  | 11:31 | Staircase N = 1, 2, 3, 4, 6 | load average > 8 for 60 s (see below) |
  | 11:56 | N=1, the storm and a `default_memory = 2048` agent | complete |

  Figures give `n` runs and min / median / max. The staircase steps N ≥ 2 come from **one run** (11:31)
  and are flagged *(1 run)*.

## Host

| Item | Value |
|---|---|
| CPU | Intel Core i5-3550 (Ivy Bridge, 4 cores, no SMT, 3.3–3.7 GHz) |
| RAM | 23.3 GiB usable; 21.9 GiB available with a1 and a2 paused; 8 GiB zram swap (compressed RAM, not disk) |
| Disk | One rotational drive, `ST1000LM024`, 1 TB; btrfs with `compress=zstd:1` |
| `/dev/shm` | tmpfs, 11.7 GiB during these runs (the default: half of RAM); 15.3 GiB since (MemTotal − 8 GiB, ADR-012) |
| Kernel | 7.2.11-200.fc44 |
| Kata | `default_vcpus = 1`, `default_memory = 4096`, `default_maxmemory = 8192`, `memory_slots = 10`, `enable_virtio_mem = false`, virtio-fs with `cache=auto` |
| Idle load | 0.0–0.2 |

## Per-agent footprint

Host memory is the sum over the agent's QEMU, both virtiofsd processes and `containerd-shim-kata-v2`, from
`/proc/<pid>/smaps_rollup`. RSS double-counts guest RAM, because QEMU and virtiofsd both map it. **PSS is the
figure to use.**

| Measure | Result | Runs |
|---|---|---|
| Fresh agent, idle 60 s: RSS / PSS | 513–526 / 490–503 MiB | 3 |
| Of which in `/dev/shm` (guest RAM) | ~380 MiB; the rest is VMM overhead, ~140 MiB | 3 |
| Guest `free -m` when fresh | 3901 MiB total, 264 MiB used | 3 |
| After one 512 MiB file write in the guest: PSS | 0.98–1.0 GiB | 3 |
| After one load phase: PSS | 2.44 / 2.45 / 2.54 GiB | 3 |
| Same agent idle again, after loads: PSS | 1.7–2.0 GiB; it never drops | 1 run |
| Idle CPU | 4.1–6.1% of one core | 4 |
| Disk: image | ~1 GiB in total, stored once and shared by every agent | 1 |
| Disk: new agent | 0 KiB writable layer and empty workspace | 3 |
| Disk: a1 / a2 after a few days of use | 38 / 28 MiB | 1 |

**Guest memory is never returned.** Memory a guest has touched stays allocated on the host until the VM stops,
because there is no balloon, free page reporting or virtio-mem (`enable_virtio_mem = false`). The guest's page
cache counts as touched. A long-running agent therefore tends toward `default_memory` plus ~150 MiB. Pause
(which stops the VM) and resume reset it.

**Guest RAM lives in `/dev/shm`.** Kata starts QEMU with `memory-backend-file,mem-path=/dev/shm,share=on`, which
virtio-fs needs. The total memory that all running guests have touched cannot exceed `/dev/shm`, 11.7 GiB during
these runs.
At N=6 it filled, and four VMs stopped with `kvm run failed Bad address` (see below). This ceiling sits below
host RAM: MemAvailable was still 9.6 GiB at that moment.

| Timing | Seconds (min / median / max) | Runs |
|---|---|---|
| Create, host page cache warm (API call to first exec) | 3.76 / 3.96 / 4.06 | n=4 |
| Create, other agents idle, after a cache drop (N = 2 to 5) | 9.64 / 9.88 / 10.64 | 1 run, n=4 |
| Pause (stops the VM) | 1.40 / 2.00 / 2.64 | n=12 |
| Resume (API call to first exec) | 3.63 / 3.69 / 3.84 | n=12 |
| Create while 1 agent runs the load | 16.71 / 16.74 | n=2 |
| Create while 6 agents run the load | **failed**: vsock timeout | 1 run |
| Storm of 4 creates at once | 4.44 / 8.15 / 17.02 | 2 runs, n=8 |

The storm mostly measures the gateway, not the disk. The gateway holds one lock across a create, so the four
creates run one after another, and each create call lasts as long as its boot.

## Claude Code (measured on a1)

Measured on a1, which already had a login. a1 was resumed through the API and paused again afterwards.
`claude -p` ran in the guest with stdin `/dev/null`. Peak RSS is `getrusage(RUSAGE_CHILDREN).ru_maxrss`, the
figure `time -v` reports; the image has no `/usr/bin/time`. Guest memory was sampled every 0.1 s. Each job
ran 3 times.

| Job | Peak RSS (MiB) | Guest used peak (MiB) | Duration (s) |
|---|---|---|---|
| `claude -p "say ok"` | 174 / 187 / 187 | 425 / 451 / 453 | 3.2 / 4.8 / 22.8 (the first was cold) |
| Prompt reading ~10 small files under `/workspace` | 182 / 189 / 189 | 474 / 474 / 475 | 12.5 / 12.8 / 18.6 |

- `free -m` used memory in the guest was 264 MiB before the first job and 332–433 MiB between jobs.
- a1's host PSS went from 487 MiB to 710 MiB over the six jobs.
- The model below adds **~190 MiB per concurrent Claude job** to each synthetic-loaded agent. Through the
  never-returned rule, the guest's page cache from reading files adds a little more over time.

## Concurrency staircase (run 11:31, 1 run per step)

All agents ran the load at once. "Per agent" figures give min–max across the agents.

| N | Boot of new agent (s) | MemAvailable idle / load (GiB) | `/dev/shm` idle / load (GiB) | Host CPU busy in load | Burner speed vs N=1 | Write per agent (MiB/s) | Cold read per agent (MiB/s) | Worst fsync (s) | io PSI full max | Load max |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 3.96 | 21.0 / 19.5 | 0.9 / 2.3 | 24% | 100% | 42.8 | 54.9 | 0.21 | 33 | 1.7 |
| 2 | 9.64 | 18.5 / 17.1 | 3.2 / 4.7 | 48% | 97–99% | 24.4–24.7 | 13.8–14.7 | 0.89 | 37 | 2.8 |
| 3 | 10.49 | 16.1 / 14.8 | 5.5 / 6.8 | 72% | 94–96% | 14.5–15.1 | 7.4–7.9 | 1.50 | 43 | 4.9 |
| 4 | 10.64 | 13.9 / 12.6 | 7.7 / 8.9 | 92% | 89–91% | 11.5–11.9 | 5.7–6.0 | 1.81 | 43 | 6.9 |
| 6 | 9.88, 3.64 | 10.8 / 9.6 | 10.7 / **11.7 (full)** | 88% | 21–50% (2 survivors) | 6.4–10.9 | 0.1–4.1 | 2.03 | 46 | 11.3 |

- **Per-VM PSS.** About 2.35–2.5 GiB under load at N = 1–4. Idle between loads it was 1.0, 1.7, 2.0, 2.0
  and 1.9 GiB at N = 1, 2, 3, 4, 6, growing as agents kept memory from earlier loads.
- **CPU per agent under load.** 100, 100, 99 and 95% of one core at N = 1–4: each agent has one vCPU.
- **Totals.** Swap use was 0 at every step. Memory PSI peaked at 2.6. No host OOM. ac-egress stayed at
  3.4–3.7 MiB RSS and ≤ 0.5% CPU.
- **Network.** Six agents ran `curl -I https://api.anthropic.com` at once through the proxy: connect
  1.9–2.9 ms, TLS 106–112 ms, total 187–196 ms. A single agent took 0.20–0.25 s (3 runs).

**What ended the run.** No memory, swap or OOM threshold tripped. At N=6, `/dev/shm` filled during the load,
and three VMs logged `kvm run failed Bad address` and stopped: bench-1, bench-2 and bench-4. The fourth
`Bad address` was bench-7, the agent created during the load, which failed to boot with a vsock timeout.
bench-6 also returned no output: the gateway watchdog marked it unhealthy and SIGKILLed its exec clients. Only
bench-3 and bench-5 finished. The run then moved on to the storm, where the 1-minute load average, still high
from the N=6 load, crossed the threshold: **`load average 9.88 > 8 for 60 s`**. The storm and the
`default_memory = 2048` agent were run again in a separate run (11:56), which completed.

**The gateway watchdog misfires under load.** Its 2 s probe went unanswered:
- for bench-1 under the N=3 load, once: it killed the gateway's own `ctr` clients;
- for bench-1 and bench-6 under the N=6 load: it killed every `ctr` exec client and marked bench-6 unhealthy.

A real job's exec is one of the gateway's own clients, so a busy host can kill a running job. This is
tracked in the [roadmap](roadmap.md).

## Bottleneck at each N

| N | Bottleneck | Evidence |
|---|---|---|
| 1 | Disk | One agent's disk job alone held io PSI full at 33–42%. Writes ran at 22–43 MiB/s with fsync (3 runs); cold reads at 49–72 MiB/s. |
| 2–3 | Disk | The disk's total stays ~45 MiB/s of writes, so each agent's share falls to 24 and then 15 MiB/s. The worst fsync goes from 0.2 to 0.9 to 1.5 s, and cold reads drop to 14 and then 8 MiB/s. CPU (48–72%) and RAM are fine. |
| 4 | CPU and disk | The host is 92% busy, burners slow to 89–91%, and CPU PSI reaches 9. Writes are 11.5 MiB/s per agent, with fsyncs up to 1.8 s. `/dev/shm` is at 8.9 of 11.7 GiB. |
| 6 | `/dev/shm`, then CPU | `/dev/shm` full: 4 VMs died. CPU PSI some reached 65 and load 11. Host RAM still had 9.6 GiB available. |

The proxy was never a bottleneck in what was measured.

## Recommended settings for this host

**Memory.** The most effective lever is `default_memory`, because it caps what each agent eventually costs
the host. Claude Code itself peaks at ~190 MiB and the guest's own use at ~475 MiB. A guest with
`default_memory = 2048` (1888 MiB visible) booted in 3.57 s, and the 1 GiB toucher passed in 2.1 s (1 run).
So 2048 MiB is enough for Claude Code plus moderate tools, and 3072 MiB leaves room for builds.

**4096 MiB is not needed for Claude Code.** It is only worth keeping for agents that run large builds; those
weren't measured. Claude Code's own install documentation still asks for 4 GB.

**vCPUs.** Keep `default_vcpus = 1` on this 4-core host. Every loaded agent kept its one core busy, and two
vCPUs per agent would only make them compete for cores sooner. Two vCPUs weren't measured.

| Workload | Running at once | Why |
|---|---|---|
| (a) Idle-heavy (mostly waiting on the model API) | **4**, with `default_memory = 2048`; admission allows up to 7. At 4096: **3**, the most admission allows since `/dev/shm` is 15.3 GiB | Worst case is total memory touched ≤ `/dev/shm`. Idle CPU is ~5% of a core each, so CPU is not the limit. |
| (b) CPU or disk heavy (builds, tests) | **2**, at most 3 | The disk's total is split evenly, and fsyncs pass 1 s from N=3. The watchdog first misfired at N=3. CPU saturates at N=4. |

**Paused agents** cost no RAM or CPU: pause stops the VM, and resume takes ~3.7 s. Provision as many as the disk
holds, and keep the running count at the figures above.

**Since these runs (ADR-012).**
- *`/dev/shm` resized.* It is now MemTotal − 8 GiB: 15717 MiB here, through `/etc/fstab`, see install.md.
- *Admission control.* The gateway counts every running or creating guest at its configured `default_memory`,
  and refuses (507) a create or resume unless the total plus the new guest plus 512 MiB fits `/dev/shm`. The new
  guest plus 512 MiB must also fit what is free. Admission counts the configured size, not what a guest uses,
  because touched pages never come back.
- *Running agents now.* At `default_memory = 4096`, 3 agents run (3 × 4096 + 512 = 12800 ≤ 15717) and a fourth
  is refused instead of crashing a VM. At 2048 it would be 7.
- *Checking.* `GET /api/v1/capacity` and `scripts/capacity.sh` show the figures.

Still open: a way for guests to return memory, such as free page reporting or virtio-mem.

## What this implies for hardware

Planning numbers, from the measurements above:
- **RAM per running agent.** `default_memory` + 0.15 GiB at worst; that is where a long-running agent ends up.
- **Host reserve.** ~4 GiB: this host used ~1.5 GiB idle, plus page cache for the disk.
- **`/dev/shm`.** At least N × `default_memory`; the default is half of RAM.
- **Cores.** One core per CPU-busy agent, plus one for the host. An idle agent needs ~0.05 core.

| Running agents | RAM at 2 GiB per guest | RAM at 4 GiB per guest | Cores, mostly idle | Cores, all CPU-busy | Disk |
|---|---|---|---|---|---|
| 4 | 13 GiB, so 16 GiB | 21 GiB, so 24–32 GiB | 4 | 5–6 | ~1 GiB image + workspaces |
| 8 | 22 GiB, so 32 GiB | 38 GiB, so 48 GiB | 4 | 9–10 | ditto |
| 16 | 39 GiB, so 48 GiB | 71 GiB, so 96 GiB | 4–6 | 17–18 | ditto |

The i5-3550 platform tops out at 32 GiB. Ivy Bridge also limits guests to 36-bit addresses (64 GiB).

**Disk class.** The rotational disk is the first limit for disk-heavy agents. Its total throughput is fixed,
so per-agent throughput falls as 1/N and fsync latency rises past 1 s from N=3. Boots after a cache drop
took ~10 s against ~4 s warm. A boot while six agents were busy failed.

**Would an SSD alone change the answer?**
- For disk-heavy agents, yes. It removes the limit that bites at N = 1–3, which leaves CPU, 3–4 heavy agents
  on these 4 cores, as the next limit.
- For idle-heavy agents, no. Their limit is `/dev/shm` and RAM; the N=6 failure was memory backing, not
  disk.
- Cold boots should approach the ~4 s warm figure. That is an inference: no SSD was measured.

## Not measured

- Any SSD or NVMe disk; everything here is one rotational disk.
- More than one vCPU per agent; real builds (cargo, npm), their memory peak and their disk pattern.
- Claude Code jobs in several agents at once. The model adds a1's single-job peak to each agent.
- Long runs: hours of drift, many jobs in one agent, Chromium or desktop agents.
- N=8. The staircase stopped at N=6, as `--max-n 6` intended, and the N=6 load already failed.
- The proxy under real model traffic. Only 6 concurrent HEAD requests went through it, and its CPU was
  sampled at 2 s granularity.
- Free page reporting, virtio-mem. The `/dev/shm` resize and admission were checked live (ADR-012), but the
  staircase was not re-run with them.
- A create while the load runs at N = 2–4; it was measured only at N=1 and N=6.
- A truly parallel create storm. The gateway serializes creates.

## Requirements that still hold

- KVM on bare metal. Nested KVM works but costs performance; not measured here.
- Fast storage for `/var/lib/containerd` and the data directory: this is the first limit measured here.
- No GPU unless you add local models. ECC is nice for an always-on box, not required.
- Model API rate limits and subscription caps will bound concurrent Claude jobs independently of hardware.

## Reference host: Proxmox (MS-01 class)

| Item | Value |
|---|---|
| CPU | Intel Core i9-12900H (6P + 8E cores, 20 threads); VT-x; nested KVM enabled (`kvm_intel nested = Y`) |
| RAM | 31 GiB total; about 14 GiB used and 16 GiB available with the current guests |
| Disk | 1 TB NVMe (Kingston); a 23.6 TB external HDD (bulk storage only) |
| Guests | VMs: opnsense (8 GiB), homeassistant (4 GiB). LXC: samba, plex, arrstack, traefik, headscale, remindme, agentos |

Option: a Fedora VM here with CPU type `host` can run Kata (nested). Applying the measured numbers to a
12 GiB VM gives about 4 running agents at `default_memory = 2048`, with `/dev/shm` sized to match, and NVMe
for disk-heavy work **[not measured: nested overhead unknown]**. Do not run Kata in an LXC container: it
shares the host kernel and has no supported path.
