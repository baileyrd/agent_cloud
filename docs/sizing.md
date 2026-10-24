# Sizing

Measured on BaileyServ on 2026-10-22 with [`scripts/capacity-bench.sh`](../scripts/capacity-bench.sh).
Raw samples (2 s host CSV, per-VM CSV, events) are in `/var/tmp/ac-bench-<date>/` on the host. This replaces
the earlier estimates. What changed: an agent costs far less than estimated when fresh (0.5 GiB, not 2 GiB),
but the guest never returns memory, so a long-running agent drifts toward its full `default_memory`. On this
host the first limits are the single rotational disk and `/dev/shm`, not RAM. Since then `/dev/shm` has been
enlarged to MemTotal − 8 GiB (15.3 GiB here), and the gateway refuses a create or resume that would not fit it
(ADR-012). The rule: **N × `default_memory` + 512 MiB must fit `/dev/shm`.** A real build (a release
and a test build of this repo, offline, in an agent) was measured since: it needs far less than 2048 MiB of
guest memory, and with more than one at once CPU, not the disk, is the limit ([Real builds](#real-builds)).

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
  `full` limit was 80 during load phases and 40 elsewhere. (Since 2026-10-23 the settle right after a
  workspace purge also has 80: a purge's own disk I/O tripped 40.10 > 40 in a `--max-n 2` run.)
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

## Real builds

Measured on 2026-10-22 with [`scripts/real-build-bench.sh`](../scripts/real-build-bench.sh); raw data in
`/var/tmp/ac-rb-<date>/` (`runs.csv` per build, `host.csv`, `pss.csv`, and the guest's 1 s samples).

**Workload.** This repo, built offline from a clean target dir in a fresh agent. There are 10 workspace
crates and 44 vendored dependencies, including the rusty_mill git dependency.
- *Release:* `cargo build --release --workspace --offline --locked`. This includes the link step.
- *Test:* `cargo test --workspace --no-run --offline --locked`. Debug profile, an 840 MiB target dir.

**Image.** A measurement-only image: the shipped one plus Fedora's gcc, rust and cargo 1.99.0. The sources
and `cargo vendor` output were copied into `/workspace`. The egress allowlist was unchanged (same sha256),
and `ac-egress` logged no allow or deny line during any run.

**Runs.**
- One agent per memory size, at 1 vCPU. Each size ran the release build 3 times back to back, then the test
  build 3 times.
- *Cold* is release run 1: the host page cache was dropped before the boot and again before the build. The
  first test run follows three release builds, so its sources were already warm.
- **Every cell is one run of the script**: the cold figure is 1 build and each warm figure is 2.

**What was measured.**
- *Guest memory*, sampled every second. "Used" is MemTotal − MemFree − Buffers − Cached − SReclaimable, as
  `free` counts it.
- *Biggest process:* `getrusage(RUSAGE_CHILDREN).ru_maxrss`, the figure `time -v` reports.
- *OOM:* the guest's `/proc/vmstat` `oom_kill` count. Guest `dmesg` cannot be read: `dmesg_restrict = 1`,
  and container root lacks CAP_SYSLOG. The container cgroup has no `memory.events`.
- *Host:* the VM's PSS (QEMU, virtiofsd, shim) every 2 s, plus `/dev/shm` used after each build.
- *Guest swap:* the guest has none.

| Guest memory | Build | Run | Wall (s) | Guest used peak (MiB) | Min MemAvailable (MiB, % of guest) | OOM | Exit | VM PSS peak / end (MiB) | `/dev/shm` after (MiB) |
|---|---|---|---|---|---|---|---|---|---|
| 2048 (1888 visible) | release | cold | 118.1 | 257 | 1439 (76%) | 0 | 0 | 869 / 869 | 751 |
| | release | warm ×2 | 85.7, 85.7 | 242, 267 | 1429–1454 (76–77%) | 0 | 0 | 870–871 / 870 | 752 |
| | test | first | 126.1 | 264 | 1430 (76%) | 0 | 0 | 1076 / 1076 | 956 |
| | test | warm ×2 | 124.0, 124.1 | 267, 286 | 1409–1427 (75–76%) | 0 | 0 | 1079 / 1079 | 958–959 |
| 3072 (2893) | release | cold | 118.0 | 256 | 2377 (82%) | 0 | 0 | 945 / 945 | 827 |
| | release | warm ×2 | 85.4, 85.4 | 250, 256 | 2377–2383 (82%) | 0 | 0 | 946–947 / 947 | 828–829 |
| | test | first | 126.2 | 291 | 2341 (81%) | 0 | 0 | 1135 / 1135 | 1015 |
| | test | warm ×2 | 124.0, 123.7 | 299, 303 | 2328–2332 (80–81%) | 0 | 0 | 1151–1152 / 1152 | 1031–1032 |
| 4096 (3901) | release | cold | 118.1 | 260 | 3374 (86%) | 0 | 0 | 959 / 959 | 841 |
| | release | warm ×2 | 85.9, 86.0 | 295, 295 | 3339 (86%) | 0 | 0 | 960 / 960 | 842 |
| | test | first | 127.0 | 310 | 3323 (85%) | 0 | 0 | 1157 / 1157 | 1037 |
| | test | warm ×2 | 125.1, 125.0 | 292, 314 | 3318–3340 (85–86%) | 0 | 0 | 1173 / 1173 | 1053–1054 |
| 2048, **2 vCPUs** | release | cold | 84.2 | 344 | 1351 (72%) | 0 | 0 | 954 / 954 | 836 |
| | release | warm ×2 | 52.6, 50.9 | 309, 395 | 1300–1386 (69–73%) | 0 | 0 | 955–978 / 978 | 837–860 |
| | test | first | 77.6 | 467 | 1227 (65%) | 0 | 0 | 1249 / 1249 | 1129 |
| | test | warm ×2 | 75.0, 75.1 | 460, 468 | 1226–1234 (65%) | 0 | 0 | 1256–1262 / 1262 | 1136–1143 |

- **The build barely touches guest memory.** Guest used never passed 314 MiB at 1 vCPU, or 468 MiB at 2
  (cargo then runs 2 rustc jobs). The biggest process was always rustc, at 243–289 MiB maxrss; the linker never
  exceeded it. Guest page cache peaked at ~360 MiB. More guest memory changes nothing in time or use.
- **`/dev/shm` plateaus.** Build after build of the same kind, it grows by 0–2 MiB, or 23 MiB once at
  2 vCPUs. It steps up once, by ~200 MiB, to the test build's larger working set, then stays. A build agent's
  host cost here ends at 0.85–1.23 GiB of PSS, not at `default_memory`. A bigger build would push it further,
  since touched pages never come back.
- **Cold costs ~38% at 1 vCPU.** A cold release build took 118 s against 86 s warm. The cold boot took 18–19 s
  against 3.5 s warm. During cold builds the disk reached 99% busy with io PSI full 38–47; during warm release
  builds the disk was 2–3% busy on average. After every build the host spent another 10–32 s writing the output
  back at ~99% disk busy.
- **2 vCPUs cut the time by ~40%**: release 51.8 s, test 75.1 s warm. Guest used rose by ~150 MiB, and
  MemAvailable stayed at ≥ 65%.
- **Host during one build:** MemAvailable ≥ 20.7 GiB, memory PSI 0, load ≤ 2.4, no swap, no host OOM. The
  gateway watchdog logged nothing about the bench agents.
- **For comparison:** natively on the host (not in a VM), with an empty `CARGO_HOME`, `-j1` builds the release
  in 70 s (maxrss 305 MiB) and the test in 72 s. At `-j4` the release takes 19 s. A 1-vCPU agent is ~23%
  slower than native `-j1` on warm runs.

**Rule.** A size is clean when every run exits 0 with no OOM kill and the minimum MemAvailable is at least 15%
of guest RAM. **2048 MiB is the smallest clean size measured.** Its lowest figure was 65%, at 2 vCPUs. Nothing
below 2048 was tried.

### Builds at once (2048 MiB)

There were 2 agents, then 3, each from a fresh boot. All ran the release build at once, twice: cold, then
warm. The baseline is the single-build figure at the same vCPU count. These are 1 run per step. The N=1 host
columns cover the whole single-agent run (release and test builds); "—" was not computed.

| vCPUs | N | Cold (s) | vs alone | Warm (s) | vs alone | Builds per hour, warm | Host CPU busy (mean) | CPU PSI some (max) | Disk busy (mean) | io PSI full (max) | Load (max) |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 1 | 118.1 | — | 85.7 | — | 42 | 26% | 0.7 | 11% | 47 | 2.0 |
| 1 | 2 | 130.8 | +11% | 97.6–97.7 | +14% | 74 | 48% | 2.2 | 15% | 42 | 2.7 |
| 1 | 3 | 151.3–151.4 | +28% | 127.8–128.9 | +50% | 84 | 67% | 15.4 | 20% | 50 | 4.2 |
| 2 | 1 | 84.2 | — | 51.8 | — | 70 | — | — | — | 41 | 2.4 |
| 2 | 2 | 106.8–107.2 | +27% | 77.0–77.6 | +49% | 93 | 72% | 23.7 | 20% | 43 | 5.8 |
| 2 | 3 | 156.8–157.0 | +86% | 136.6–138.1 | +166% | 78 | 82% | 59.9 | 15% | 47 | 7.0 |

- **CPU is the limit, not the disk.** In warm runs the disk averaged 15–20% busy while CPU PSI rose with every
  agent added. Each 1-vCPU build costs about one host core, the vCPU plus its virtiofsd. The disk only
  saturates in the cold phase, where io PSI full reached 42–50 whatever the N.
- **2 vCPUs win up to 2 builds at once and lose at 3.** At 3 builds, 6 vCPUs share 4 cores: it is 7% slower
  than 1 vCPU, CPU PSI reaches 60 and load 7. That is close to the bench's abort (load > 8 for 60 s).
- **Memory was never close.** Each agent's guest used peaked at 224–380 MiB, and its PSS ended at 0.81–0.94
  GiB. Host MemAvailable was ≥ 19.1 GiB with 3 builds running.
- No threshold tripped in the final runs. Three earlier attempts were stopped by the io PSI limit of 40, each
  outside a build: twice a `journalctl --since` reading the 1 GiB journal from a cold cache, and once a
  journal follower's start-up scan before the bench. The script now follows the journal from the start
  instead.

### Admission capacity

Admission counts every guest at `default_memory`: N × size + 512 MiB must fit in `/dev/shm`. The VMM's own
~0.15 GiB per VM is not in `/dev/shm`; it comes out of the 8 GiB host reserve. So N = floor((`/dev/shm` −
512) / size). The 15717 MiB column matches what `GET /api/v1/capacity` reported (`max_more_agents` 7, 4
and 3).

| `default_memory` | `/dev/shm` 15717 MiB (this host, 24 GiB RAM) | 24 GiB (32 GiB RAM) | 40 GiB (48 GiB RAM) |
|---|---|---|---|
| 2048 | 7 | 11 | 19 |
| 3072 | 4 | 7 | 13 |
| 4096 | 3 | 5 | 9 |

The RAM column assumes `/dev/shm` = MemTotal − 8 GiB (ADR-012).

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
the host.
- Claude Code itself peaks at ~190 MiB, and the guest's own use at ~475 MiB.
- A release or test build of this repo peaks at 314 MiB used at 1 vCPU and 468 MiB at 2, with at least 65%
  of a 2048 MiB guest still available ([Real builds](#real-builds)).
- **Proposal: `default_memory = 2048`.** That admits 7 agents here instead of 3. 3072 and 4096 changed neither
  the build time nor its memory use.
- Claude Code's own install documentation still asks for 4 GB. Builds much bigger than this repo were not
  measured: those, and not Claude Code, are the reason to keep more.

**vCPUs. Proposal: `default_vcpus = 2`.**
- A build is ~40% faster with 2 vCPUs (release 52 s against 86 s warm), and 2 builds at once still finish
  sooner than with 1 vCPU (77 s against 98 s).
- At 3 builds at once, 6 vCPUs on 4 cores is 7% slower than 1 vCPU, and CPU PSI reaches 60.
- Idle vCPUs cost nothing measurable. If 3 or more builds at once is the normal case on this host, keep 1.

| Workload | Running at once | Why |
|---|---|---|
| (a) Idle-heavy (mostly waiting on the model API) | **4**, with `default_memory = 2048`; admission allows up to 7. At 4096: **3**, the most admission allows since `/dev/shm` is 15.3 GiB | Worst case is total memory touched ≤ `/dev/shm`. Idle CPU is ~5% of a core each, so CPU is not the limit. |
| (b) Builds and tests | **2**; 3 runs, 1.5× slower | Real builds: +14% (2 at once) and +50% (3 at once) at 1 vCPU, limited by CPU, not disk or memory. A disk-heavy synthetic job splits the disk evenly, with fsyncs over 1 s from N=3. |

**Paused agents** cost no RAM or CPU: pause stops the VM, and resume takes ~3.7 s. Provision as many as the disk
holds, and keep the running count at the figures above.

**Since these runs (ADR-012).**
- *`/dev/shm` resized.* It is now MemTotal − 8 GiB: 15717 MiB here, through `/etc/fstab`, see install.md.
- *Admission control.* The gateway counts every running or creating guest at its configured `default_memory`,
  and refuses (507) a create or resume unless the total plus the new guest plus 512 MiB fits `/dev/shm`. The new
  guest plus 512 MiB must also fit what is free. Admission counts the configured size, not what a guest uses,
  because touched pages never come back.
- *Running agents now.* At `default_memory = 2048`, the setting now, 7 agents run (7 × 2048 + 512 = 14848 ≤
  15717) and an eighth is refused instead of crashing a VM. At 4096 it was 3.
- *Checking.* `GET /api/v1/capacity` and `scripts/capacity.sh` show the figures.

Still open: a way for guests to return memory, such as free page reporting or virtio-mem.

**Jobs at once (ADR-013).** Admission bounds running *agents*; the job gate bounds running *jobs* and when
the next one starts. The two layers relate to the build measurements above like this:
- *The cap*, `AC_MAX_RUNNING_JOBS` = cores (4), is the most API-bound jobs that run at once. Those use ~5% of a
  core each, so the cap, not the CPU, is their limit. It is also never more than the running agents, one job
  each.
- *The pressure gate* was meant to limit builds: two builds drove PSI to 23.7 and three to 59.9, so a limit of
  20 was to hold a burst at 2. **Measured with real build jobs it does not**
  ([Job start gate under real builds](#job-start-gate-under-real-builds)): a third build started in 3 of 3 runs,
  15 s after the second, before PSI reached 20. Two builds held PSI at 12.6–29.8, so 20 sits inside their band;
  one build stays at ≤ 1.3. The load gate never shut: two builds peaked at load 3.8–4.1 there (5.8 in the
  bench above).
- *The spacing* (15 s) is what actually spaced the starts: every wait seen was `start spacing`. A job's build
  starts 4–7 s after the job (warm) or 24–42 s (cold: Claude's own start), and PSI avg10 takes another 10–13 s
  to show it, so 15 s is too short for PSI 20 to see the second build.
- The gate only decides starts. A job that starts idle and then builds is not slowed down.

## Job start gate under real builds

Measured on 2026-10-24 with [`scripts/burst-test.sh`](../scripts/burst-test.sh), live gate settings: CPU PSI
some < 20, load < 4, 15 s spacing, at most 4 jobs (the gateway's startup line). Raw data in
`/var/tmp/ac-burst-20261024-133504/` (baseline) and `/var/tmp/ac-burst-20261024-134346/` (B, C, D): `burst.csv`
(1 s samples: CPU PSI, load, capacity, each job's state and `queued_reason`), `jobs.csv`, `checks.csv`, the
journal.

**Setup.**
- Agents a1, a2 and a3 (2048 MiB, 2 vCPUs, each with its own Claude login), resumed on the measurement image
  of [Real builds](#real-builds) (cargo 1.99.0).
- Workload: this repo at HEAD with vendored dependencies in `/workspace/burst`. Every build is
  `cargo build --release --workspace --offline --locked` from a clean target dir.
- *Build job:* a job through the API (`timeout_secs` 900) asking Claude to run exactly that build and reply
  `built`. *Tiny job:* "reply with exactly: pong".
- *Build time* is read on the host side of the workspace: `target/`'s birth time to the newest mtime under
  `target/release`. Model latency is not in it; the job's own start and finish are reported separately.
- *Cold* as in Real builds: the agents boot fresh after a host page-cache drop, then the cache is dropped again
  before the run. Run 1 of base, B and C is cold; the rest are warm; D ran warm.
- Before each run the host was quiet: load1 < 1, CPU PSI < 2.
- 21 jobs, all succeeded (`built` or `pong`). Every cold figure and all of D are **single runs**.

**Experiments.**
- *base:* the build alone in a1 by direct exec (no Claude call), 3 times.
- *A, derived from B/C starts:* the first two jobs of every B and C run (a build job on a1, then at once one on
  a2). Queueing figures come from all 6 runs. Wall times come from C only, since in B a third build joined.
- *B:* A, then a build job on a3 once both jobs run. 3 runs.
- *C:* as B, with the tiny job on a3. 3 runs.
- *D:* B with `AC_JOB_CPU_PSI_MAX=1000` and `AC_JOB_START_SPACING=0` (a temporary drop-in), once. The load gate
  has no setting and stayed.

**Baseline.** Cold 74.0 s; warm 58.2 and 57.9 s. Peak CPU PSI some 1.0–1.2, load ≤ 2.1. The warm figure is
12% above Real builds' 51.8 s: here three VMs are up and the time is taken on the host side. Slowdowns below
are against this baseline.

**Timeline** (seconds from a1's submit; "build" is start..end; reasons from the 1 s samples, `null` is the
supervisor's first second):

| Run | Job | Submit | Start | Build | Finish | Queued (reason) | CPU PSI / load at start |
|---|---|---|---|---|---|---|---|
| B #1 cold | a1 build | 0.0 | 0.3 | 41.8..193.2 | 194.6 | 0.3 | 0.27 / 1.01 |
| | a2 build | 0.6 | 15.9 | 41.8..193.7 | 197.3 | 15.3 (start spacing) | 0.13 / 1.01 |
| | a3 build | 16.2 | 31.4 | 42.1..193.4 | 197.0 | 15.2 (start spacing) | 2.79 / 1.68 |
| B #2 | a1 build | 0.0 | 0.3 | 4.5..103.8 | 106.1 | 0.3 | 0.01 / 0.92 |
| | a2 build | 0.6 | 15.8 | 21.7..132.5 | 133.9 | 15.3 (start spacing) | 1.40 / 1.46 |
| | a3 build | 16.4 | 31.9 | 42.8..151.1 | 153.2 | 15.5 (start spacing) | 16.84 / 2.35 |
| B #3 | a1 build | 0.0 | 0.5 | 6.1..107.9 | 109.9 | 0.5 | 0.33 / 0.93 |
| | a2 build | 0.4 | 16.1 | 22.4..139.9 | 144.0 | 15.7 (start spacing) | 1.33 / 1.18 |
| | a3 build | 16.6 | 32.2 | 41.1..156.1 | 159.4 | 15.6 (start spacing) | 16.10 / 2.03 |
| C #1 cold | a1 build | 0.0 | 0.3 | 40.0..135.9 | 137.9 | 0.3 | 0.03 / 1.11 |
| | a2 build | 0.5 | 15.9 | 40.0..135.9 | 139.3 | 15.3 (start spacing) | 0.25 / 1.08 |
| | a3 pong | 16.1 | 30.9 | — | 39.2 | 14.8 (start spacing) | 2.02 / 1.37 |
| C #2 | a1 build | 0.0 | 0.6 | 6.4..84.2 | 87.8 | 0.6 | 0.19 / 1.05 |
| | a2 build | 0.9 | 16.2 | 23.6..103.2 | 105.1 | 15.3 (start spacing) | 1.24 / 1.13 |
| | a3 pong | 16.5 | 31.6 | — | 38.7 | 15.1 (start spacing) | 13.77 / 2.01 |
| C #3 | a1 build | 0.0 | 0.6 | 5.8..83.3 | 85.7 | 0.6 | 0.01 / 0.93 |
| | a2 build | 0.3 | 16.1 | 23.2..103.4 | 106.0 | 15.8 (start spacing) | 1.25 / 1.03 |
| | a3 pong | 16.7 | 32.3 | — | 38.5 | 15.6 (start spacing) | 15.56 / 1.62 |
| D (gate off) | a1 build | 0.0 | 0.9 | 7.3..148.5 | 150.9 | 0.9 | 0.02 / 0.94 |
| | a2 build | 0.6 | 1.1 | 10.0..151.6 | 154.2 | 0.5 | 0.02 / 0.94 |
| | a3 build | 1.4 | 2.6 | 9.0..152.8 | 157.5 | 1.2 | 0.38 / 0.94 |

| Run | Third job started, against both builds running | CPU PSI first ≥ 20 | Peak CPU PSI | Builds at once: peak (s at the peak) | Peak `running_jobs` | Peak load |
|---|---|---|---|---|---|---|
| B #1 cold | 10.4 s **before** | 66.5 | 59.2 | 3 (151) | 3 | 7.26 |
| B #2 | 10.2 s after | 33.2 | 55.9 | 3 (61) | 3 | 5.89 |
| B #3 | 9.8 s after | 35.1 | 57.0 | 3 (67) | 3 | 6.34 |
| C #1 cold | 9.1 s **before** | 70.4 | 23.6 | 2 (96) | 3 | 4.06 |
| C #2 | 8.0 s after | 35.4 | 29.8 | 2 (61) | 3 | 3.76 |
| C #3 | 9.1 s after | 35.4 | 28.1 | 2 (60) | 3 | 3.76 |
| D | 7.4 s **before** | 13.8 | 59.0 | 3 (138) | 3 | 6.74 |

`queued_reason` was never `host busy`: the journal logged 12 waits, all `start spacing`.

**Build wall times** (s; min / median / max; slowdown against the baseline above, 74.0 cold and 58.05 warm):

| Builds | Cold | Warm |
|---|---|---|
| 1 (baseline) | 74.0 (1 run) | 57.9 / 58.05 / 58.2 (2 runs) |
| 2: A (C's a1 and a2) | 95.9, 95.9: **+30%** (1 run) | 77.5 / 78.7 / 80.2: **+34% to +38%** (2 runs, 4 builds) |
| 3, staggered 15 s: B | 151.3 / 151.4 / 151.9: +104% to +105% (1 run) | 99.3 / 109.6 / 117.6: +71% to +103% (2 runs, 6 builds) |
| 3 at once: D | — | 141.2 / 141.6 / 143.8: **+143% to +148%** (1 run) |

The time to finish all three (submit to the last build's end) was about the same with and without the gate:
B warm 151.1 and 156.1 s, D 152.8 s. The 15 s stagger only shortened the first builds.

**Verdict on each claim.**
1. *At most 2 builds at once with PSI 20:* **not held.** In all 3 B runs three builds ran at once, for 61–151 s.
   a3 waited only for the spacing (15.2–15.6 s), and at its start CPU PSI was 2.79, 16.84 and 16.10, load
   1.68–2.35.
2. *Overlapping builds within the measured +27% / +49%:* two builds cost +30% cold (1 run) and +34% to +38%
   warm, so they are in range. In absolute terms, two warm builds (77.5–80.2 s) match the earlier 77.0–77.6 s.
   Three at once cost +143% to +148% warm (D, 1 run), against +166% before.
3. *Cost to a tiny job:* it waited 14.8–15.6 s, all `start spacing`, then ran 6.2–8.3 s. That cost is
   acceptable, but it is only the spacing: the pressure gate never shut. A gate that held builds at 2 would hold
   the tiny job too. With the limit proposed below, the replay puts that wait at about 75 s warm (until a build
   ends), roughly ten times the job's own run time. See the roadmap.
4. *PSI avg10's lag:* **yes, it lets a third build in.** The gate decides when a job starts. The build starts
   4.3–7.4 s later warm, or 24–42 s later cold (Claude's own start), and PSI avg10 then needs 10–13 s to show it.
   - Warm: PSI passed 20 17.3–19.3 s after a2's job start. a3 had started 15.4–16.2 s after it, 1.3–3.8 s
     earlier.
   - Cold: it passed 20 after 50.6–54.6 s, and a3 started before a2's build had begun.
   - After the ramp, two builds held PSI at 12.6–29.8, below 20 in 31–68% of the samples (C runs). Even a longer
     spacing would let a third in whenever PSI dips.
   - The load gate never shut: two builds peaked at load 3.76–4.06.
5. *Watchdog:* no action. No `rung` or `unhealthy` line and no `not acting` line in the gateway's journal.

**Host.** No abort threshold tripped:
- io PSI full ≤ 45.7, memory PSI some ≤ 0.72, no swap, host MemAvailable ≥ 18.9 GiB.
- Between runs: no job running or queued and no cargo, rustc or claude left in any guest.
- `/dev/shm` was 1.44 GiB after fresh boots and 2.65–3.28 GiB after builds (touched guest memory, plateauing
  as in Real builds).
- The egress allowlist was unchanged (sha256). ac-egress logged no deny; its only allows were
  api.anthropic.com:443 (25, the jobs' model calls and the login checks) and platform.claude.com:443 (1).

**What would hold a burst at 2.** This is a replay of the measured curves against other limits, not run:

| CPU PSI some reached, after a2's job start | Warm (4 runs) | Cold (2 runs) |
|---|---|---|
| ≥ 20 | 17.3–19.3 s | 50.6–54.6 s |
| ≥ 15 | 14.6–15.9 s | 48.4–49.0 s |
| ≥ 10 | 9.7–11.2 s | 30.2–42.5 s |

- One build alone peaked at 1.3 and idle agents at ≤ 3.6, while two builds never went below 12.6 after their
  ramp. After the first of two builds ended, PSI fell below 10 within 7.1–7.6 s.
- So **`AC_JOB_CPU_PSI_MAX=10` with the 15 s spacing** holds warm bursts at 2, with a 3.8–5.3 s margin, and
  reopens about 7 s after a build ends.
- Cold starts need a spacing of at least 45 s (60 for a margin), which every burst would pay, tiny jobs
  included.
- Proposed: 10 and 15 s, to be validated by running B again. Defaults and rules are unchanged.

## Settings from host facts (ADR-014)

`ac-ctl doctor` turns the figures above into rules over a host's facts; `ac-ctl tune` applies them
([install.md](install.md#host-settings-ac-ctl-doctor-and-ac-ctl-tune-adr-014)). A recommendation is `measured` only
where it reproduces a measurement on this shape of host; elsewhere it is `extrapolated`. The PSI limit is a third
case, `measured input, gate unvalidated`: its input was measured here. Its effect as a gate was measured on
2026-10-24 and did not hold ([Job start gate under real builds](#job-start-gate-under-real-builds)).

| Rule | From | Measured | Carried over |
|---|---|---|---|
| `/dev/shm` = MemTotal − 8 GiB (MemTotal/2 below 16 GiB; error below 8 GiB) | the staircase's N=6 failure with RAM free; ADR-012's 15717 MiB | MemTotal 23909 MiB (± 2 GiB) | the 8 GiB reserve on bigger hosts: this host used ~1.5 GiB idle plus page cache |
| `default_maxmemory` = 8192 at ≤ 36 address bits | install.md note 3: QEMU's "phys-bits too low (36)" | 36 bits | fewer than 36 bits; above 36 Kata's default is left alone, untested here |
| `default_vcpus` = 2 at ≥ 4 cores, else 1, never above 2 | Builds at once: 2 vCPUs ~40% faster, 3 builds at 2 vCPUs 7% slower | 4 cores | any other core count; more than 2 vCPUs per guest was never run |
| `default_memory` idle 2048, build 2048, heavy 4096 | Claude Code ~475 MiB used; this repo's builds 468 MiB used, ≥ 65% free | idle, build | heavy: bigger builds were not measured |
| `AC_MAX_RUNNING_JOBS` = cores | ADR-013, Jobs at once | 4 cores | other core counts |
| `AC_JOB_CPU_PSI_MAX` = 20 on 4 cores with 2-vCPU guests, else 40 | Builds at once: 2 builds peaked at CPU PSI 23.7, 3 at 59.9 | the input only: on 4 cores, 2 vCPUs it is `measured input, gate unvalidated`. The gate at 20 ran against real build jobs on 2026-10-24 and did not hold a burst at 2 ([Job start gate under real builds](#job-start-gate-under-real-builds); 10 proposed) | every other shape: the default 40, "uncalibrated" |
| Forecast: agents = floor((`/dev/shm` − 512) / guest), builds = floor(cores / vCPUs) | Admission capacity; Builds at once | 7 agents and 2 builds here | the builds figure on other core counts |
| Boot class | Real builds: cold boot 18–19 s against 3.5 s warm | rotational | SSD and NVMe: no SSD was measured |

**Uncalibrated on other hardware.** On any host but this shape: how many builds at once a core count carries
(the CPU-bound limit was measured on 4 cores only), the PSI limit that holds them there, more than 2 vCPUs per
guest, the host reserve above 24 GiB, and every SSD figure. Run `scripts/real-build-bench.sh` (`matrix`, then
`concurrent`) on such a host before trusting `extrapolated` figures for builds. For planning,
`ac-ctl doctor --facts-file deploy/hosts/32g-8core-nvme.json --profile build` forecasts 11 agents at 2048 MiB
(admission, measured arithmetic) and 4 builds at once (extrapolated).

## What this implies for hardware

Planning numbers, from the measurements above:
- **RAM per running agent.** `default_memory` + 0.15 GiB at worst; that is where a long-running agent ends up.
  An agent that has built this repo (release and test) costs ~1.2 GiB. Budget `default_memory` + 0.15 GiB per
  concurrent build agent anyway, because pages are never returned.
- **Host reserve.** ~4 GiB: this host used ~1.5 GiB idle, plus page cache for the disk.
- **`/dev/shm`.** At least N × `default_memory`; the default is half of RAM.
- **Cores.** One core per CPU-busy agent, plus one for the host. An idle agent needs ~0.05 core. A 1-vCPU
  build uses about one host core, the vCPU plus its virtiofsd. At 3 builds on 4 cores each is 1.5× slower.
- **Build time on this CPU** (i5-3550, 1 vCPU): this repo's release build takes 86 s warm and 118 s cold, the
  test build 124 s. With 2 vCPUs: 52 s and 75 s.

| Running agents | RAM at 2 GiB per guest | RAM at 4 GiB per guest | Cores, mostly idle | Cores, all CPU-busy | Disk |
|---|---|---|---|---|---|
| 4 | 13 GiB, so 16 GiB | 21 GiB, so 24–32 GiB | 4 | 5–6 | ~1 GiB image + workspaces |
| 8 | 22 GiB, so 32 GiB | 38 GiB, so 48 GiB | 4 | 9–10 | ditto |
| 16 | 39 GiB, so 48 GiB | 71 GiB, so 96 GiB | 4–6 | 17–18 | ditto |

The i5-3550 platform tops out at 32 GiB. Ivy Bridge also limits guests to 36-bit addresses (64 GiB).

**Disk class.** For real builds of this size the disk is not the first limit: in warm builds it averaged
15–20% busy while CPU PSI climbed. It costs at the cold start: an 18–19 s boot instead of 3.5 s, a release
build 38% slower, and io PSI full at 40–50 in every cold build. **For build agents, cores come before an SSD.**
For disk-heavy synthetic jobs, the rotational disk is the first limit. Its total throughput is fixed,
so per-agent throughput falls as 1/N and fsync latency rises past 1 s from N=3. Boots after a cache drop
took ~10 s against ~4 s warm. A boot while six agents were busy failed.

**Would an SSD alone change the answer?**
- For disk-heavy agents, yes. It removes the limit that bites at N = 1–3, which leaves CPU, 3–4 heavy agents
  on these 4 cores, as the next limit.
- For idle-heavy agents, no. Their limit is `/dev/shm` and RAM; the N=6 failure was memory backing, not
  disk.
- For build agents like this repo's, only at the start. It should remove most of the 38% cold-build penalty
  and the 15 s cold-boot penalty, but not the CPU-bound slowdown of builds at once. That is an inference.
- Cold boots should approach the ~4 s warm figure. That is an inference: no SSD was measured.

## Not measured

- Any SSD or NVMe disk; everything here is one rotational disk.
- Builds other than this repo's Rust build: npm, C++ or bigger Rust projects with heavier links. The
  conclusion that 2048 MiB is enough holds only for builds of this size.
- Real builds on an SSD; hours-long growth in a build agent; many builds of different projects in one agent.
- Memory give-back: a build agent keeps its peak until it is paused.
- Claude Code's own memory in several agents at once. Up to 3 jobs ran together (2026-10-24), but per-agent
  memory was not sampled; the model adds a1's single-job peak to each agent.
- The job gate at the proposed `AC_JOB_CPU_PSI_MAX=10`. Its effect is a replay of the 20 runs' curves. Also
  unmeasured: a cold burst at any spacing above 15 s, more than 3 build jobs, a job that starts idle and builds
  later, and any other core count.
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
