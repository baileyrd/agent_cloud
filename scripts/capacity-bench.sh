#!/bin/bash
# Measure how many agents this host runs at once, what limits it, and what one agent costs.
#   sudo scripts/capacity-bench.sh [--max-n N] [--estimate] [--small-mem MB]
# --max-n caps the staircase N = 1 2 3 4 6 8 (default 6). --estimate prints the prediction and exits.
# No Claude Code calls: the load is synthetic (Claude's own footprint is measured separately, on a
# logged-in agent; docs/sizing.md).
# --small-mem: default_memory (MB) for one extra agent at the end (default 2048, 0 skips it); the Kata
# config is restored byte for byte afterwards, on every exit path.
# Agents are bench-<n> only (n <= 24), created and destroyed (purged) through the gateway API with a
# throwaway token scoped to agents:read,agents:write for bench-1..bench-24, revoked on exit; a1, a2
# and other workspaces are never touched. A watchdog stops the run on the first
# threshold (trip() in bench-guard.sh); the reason is in $OUT/abort. Raw samples: /var/tmp/ac-bench-<date>/*.csv.
# Exit: 0 done, 3 a threshold ended the run, 2 setup error, 1 cleanup left something behind.
set -u
MAX_N=6 ESTIMATE=0 SMALL_MEM=2048 ARGS="$*"
while [ $# -gt 0 ]; do
    case $1 in
        --max-n) MAX_N=$2; shift ;;
        --estimate) ESTIMATE=1 ;;
        --small-mem) SMALL_MEM=$2; shift ;;
        *) echo "usage: $0 [--max-n N] [--estimate] [--small-mem MB]" >&2; exit 2 ;;
    esac
    shift
done
case $MAX_N$SMALL_MEM in *[!0-9]*) echo "numbers only" >&2; exit 2 ;; esac

EXEC_TAG=bench
. "$(dirname "$0")/bench-guard.sh" || exit 2

# --- estimate ---------------------------------------------------------------------------------
VM_MIB=$(kata default_memory) VCPUS=$(kata default_vcpus) MAXMEM=$(kata default_maxmemory)
estimate() {
    awk -v avail="$(kib MemAvailable)" -v floor=$MEM_FLOOR_KIB -v vm="$VM_MIB" -v c="$CORES" \
        -v vcpu="$VCPUS" -v max="$MAX_N" -v small="$SMALL_MEM" -v shm="$(df -m --output=size /dev/shm | tail -n 1)" 'BEGIN {
        room = (avail - floor) / 1048576
        printf "MemAvailable %.1f GiB, abort floor 1.5 GiB: %.1f GiB for agents\n", avail / 1048576, room
        printf "Kata: default_memory %d MiB, default_vcpus %d; %d host cores\n", vm, vcpu, c
        printf "worst case (every guest touches all %d MiB + ~200 MiB VMM): %d agents\n", vm, int(room * 1024 / (vm + 200))
        printf "guest RAM is a file in /dev/shm (%d MiB): at most %d guests can touch all of theirs\n", shm, int(shm / vm)
        printf "load phase (~0.4 GiB idle + 1 GiB toucher + ~0.5 GiB guest page cache per VM): ~%d agents by RAM\n", int(room / 1.9)
        printf "CPU: one busy vCPU per agent saturates the %d cores at N=%d; above that each burner gets ~%d/N of a core\n", c, c / vcpu, c
        printf "load-average abort (>%d for 60 s) is likely from N=%d on (burners plus I/O wait on one rotational disk)\n", 2 * c, 2 * c - 2
        printf "staircase: N =" ; n = split("1 2 3 4 6 8", s, " ")
        for (i = 1; i <= n; i++) if (s[i] <= max) printf " %d", s[i]
        printf " (--max-n %d), then a 4-agent create storm", max
        if (small) printf " and one agent at default_memory %d", small
        printf "\n"
    }'
}
echo "== estimate"
estimate
[ "$ESTIMATE" = 1 ] && exit 0
[ "$(id -u)" = 0 ] || { echo "run as root (ctr, smaps_rollup, the tokens file)" >&2; exit 2; }
guard_token capacity-bench "$(seq -s, -f 'bench-%g' 1 24)"

OUT=/var/tmp/ac-bench-$(date +%Y%m%d-%H%M%S)
mkdir -p -m 700 "$OUT" || exit 2
exec > >(tee -a "$OUT/run.log") 2>&1
MAIN=$$
echo "output: $OUT"
echo "$ARGS" > "$OUT/args"

# --- preflight ---------------------------------------------------------------------------------
guard_services
r=$(API_SECS=10 api GET /agents)
case $r in 200\ *) ;; *) echo "list agents: $r" >&2; exit 2 ;; esac
# Purged agents stay in the registry as destroyed (their ids are reusable); any other state is in use.
live=$(echo "$r" | grep -oE '"id":"bench-[0-9]+","state":"[a-z]+"' | grep -v '"destroyed"')
[ -z "$live" ] || { echo "bench-* agents in use; clean up first: $live" >&2; exit 2; }
state > "$OUT/state.before"
cp -a "$KATA" "$OUT/configuration.toml.orig" || exit 2

{
    echo "== baseline $(date -Is)"
    lscpu | grep -E 'Model name|^CPU\(s\)|Thread|MHz'
    uname -r
    grep -E 'MemTotal|MemAvailable|SwapTotal|SwapFree' /proc/meminfo
    swapon --show
    lsblk -d -o NAME,ROTA,MODEL,SIZE -e 7,252
    df -h "$DATA" /var/lib/containerd | tail -n +2
    grep -E '^(default_vcpus|default_maxvcpus|default_memory|default_maxmemory|memory_slots|enable_virtio_mem|virtio_fs_cache|shared_fs) ' "$KATA"
    echo "loadavg $(cat /proc/loadavg)"
    ctr -n "$NS" images ls | awk 'NR == 1 || /agent-shell/'
} | tee "$OUT/baseline.txt"

echo "idle 0" > "$OUT/phase"

# --- agents -------------------------------------------------------------------------------------
guard_hold

AGENTS=()   # running bench agents, in creation order
NEXT=1
echo "ts,event,agent,n,value" > "$OUT/events.csv"
echo "ts,phase,n,agent,rss_kib,pss_kib,cpu_pct" > "$OUT/vm.csv"

# The load programs, copied into each bench workspace (owned like the workspace).
LOAD=$OUT/load.py
cat > "$LOAD" <<'PY'
import os, sys, time

def cpu(secs):
    end, n = time.monotonic() + secs, 0
    while time.monotonic() < end:
        n += 1
    print(f"cpu_iters={n}")

def mem(secs):
    t = time.monotonic()
    b = bytearray(1 << 30)
    for i in range(0, len(b), 4096):
        b[i] = 1
    touch = time.monotonic() - t
    while time.monotonic() < t + secs:
        time.sleep(5)
        for i in range(0, len(b), 4096):
            b[i] ^= 1
    print(f"mem_touch_s={touch:.2f}")

DAT = "/workspace/.bench/disk.dat"

def write(until, pace=0.0):
    """Up to 512 MiB of random 1 MiB blocks into DAT, fsync every 8 MiB, `pace` s apart; MiB written,
    secs, worst fsync."""
    blk, t0, n, fmax = os.urandom(1 << 20), time.monotonic(), 0, 0.0
    fd = os.open(DAT + ".new", os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    while n < 512 and time.monotonic() < until:
        os.write(fd, blk)
        n += 1
        time.sleep(pace)
        if n % 8 == 0:
            s = time.monotonic()
            os.fsync(fd)
            fmax = max(fmax, time.monotonic() - s)
    os.fsync(fd)
    w = time.monotonic() - t0
    os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
    os.close(fd)
    os.replace(DAT + ".new", DAT)
    return n, w, fmax

def prep(_):
    # Unmeasured setup, paced (~16 MiB/s) so it stays well inside the io PSI abort threshold.
    write(time.monotonic() + 600, 0.06)

def disk(secs):
    # Read the file the last phase (or prep) wrote, cold (the host drops its page cache first),
    # for up to 40% of secs; then write a new one in the rest.
    t0, r, rmax = time.monotonic(), 0, 0.0
    fd = os.open(DAT, os.O_RDONLY)
    while time.monotonic() < t0 + secs * 0.4:
        s = time.monotonic()
        k = len(os.read(fd, 1 << 20))
        rmax = max(rmax, time.monotonic() - s)
        if not k:
            break
        r += k
    rt = time.monotonic() - t0
    os.close(fd)
    n, w, fmax = write(t0 + secs)
    print(f"disk_r_mib={r >> 20} disk_r_mibs={r / 1048576 / rt:.1f} read_max_ms={rmax * 1000:.0f} "
          f"disk_w_mib={n} disk_w_mibs={n / w:.1f} fsync_max_ms={fmax * 1000:.0f}")

globals()[sys.argv[1]](int(sys.argv[2]) if len(sys.argv) > 2 else 0)
PY

# create <id>: through the API; records the API call time and boot (create call to first exec).
create() {
    local t0 r
    [ "${1#bench-}" -le 24 ] || { echo "$1: beyond the bench token's agents"; return 1; }
    t0=$(now)
    r=$(api POST "/agents/$1")
    case $r in 201\ *) ;; *) echo "create $1: $r"; return 1 ;; esac
    ev create_api_s "$1" $((${#AGENTS[@]} + 1)) "$(since "$t0")"
    local i=0
    until gx "$1" true >/dev/null; do
        i=$((i + 1)); [ "$i" -lt 600 ] || { echo "$1: no exec after create"; return 1; }
        sleep 0.2
    done
    BOOT=$(since "$t0")
    ev boot_s "$1" $((${#AGENTS[@]} + 1)) "$BOOT"
    local ws=$DATA/workspaces/$1
    install -d -o "$(stat -c %u "$ws")" -g "$(stat -c %g "$ws")" -m 700 "$ws/.bench"
    install -o "$(stat -c %u "$ws")" -g "$(stat -c %g "$ws")" -m 600 "$LOAD" "$ws/.bench/load.py"
}
# vm_sample <phase> <secs>: per-agent CPU over the next secs, then RSS and PSS.
vm_sample() {
    local -A t0 p
    local a
    for a in "${AGENTS[@]}"; do p[$a]=$(vm_pids "$a" | tr '\n' ' '); t0[$a]=$(ticks ${p[$a]}); done
    nap "$2"
    for a in "${AGENTS[@]}"; do
        # A VM whose processes went away (it died) is recorded as dead, not as a figure.
        [ "$(vm_pids "$a" | tr '\n' ' ')" = "${p[$a]}" ] || { echo "$(date +%s),$1,${#AGENTS[@]},$a,dead,dead,dead" >> "$OUT/vm.csv"; continue; }
        set -- "$1" "$2" $(mem ${p[$a]})
        echo "$(date +%s),$1,${#AGENTS[@]},$a,$3,$4,$(awk -v d=$(( $(ticks ${p[$a]}) - t0[$a] )) -v s="$2" -v c="$CLK" 'BEGIN { printf "%.1f", 100 * d / c / s }')" >> "$OUT/vm.csv"
    done
}

# guest <phase> <cmd> in every agent at once; each output line becomes key=value events.
guest_all() {
    local a pids=() f=$OUT/guest-$1-${#AGENTS[@]}
    for a in "${AGENTS[@]}"; do
        ( out=$(gx "$a" "$2"); printf '%s %s\n' "$a" "$(echo "$out" | tr '\n' ' ')" >> "$f" ) &
        pids+=($!)
    done
    wait "${pids[@]}"
    guest_events "$f"
}
guest_events() {
    local a rest kv
    while read -r a rest; do
        for kv in $rest; do case $kv in *=*) ev "${kv%%=*}" "$a" "${#AGENTS[@]}" "${kv#*=}" ;; esac; done
    done < "$1"
}

# load_phase <extra-id>: CPU burner, 1 GiB toucher and disk writer in every agent at once for 90 s;
# with an id, also boots that agent 15 s in (boot time under load).
load_phase() {
    local a k pids=() f=$OUT/guest-load-${#AGENTS[@]}
    sync; echo 3 > /proc/sys/vm/drop_caches  # the disk readers read cold
    echo "load ${#AGENTS[@]}" > "$OUT/phase"
    for a in "${AGENTS[@]}"; do
        for k in cpu mem disk; do
            ( out=$(gx "$a" "timeout -k 5 150 python3 /workspace/.bench/load.py $k 90")
              printf '%s %s\n' "$a" "$(echo "$out" | tr '\n' ' ')" >> "$f" ) &
            pids+=($!)
        done
    done
    if [ -n "${1:-}" ]; then
        ( nap 15; create "$1" && ev boot_under_load_s "$1" "${#AGENTS[@]}" "$BOOT" ) &
        pids+=($!)
    fi
    vm_sample load 90
    wait "${pids[@]}"
    guest_events "$f"
    [ -z "${1:-}" ] || destroy "$1"
}

# --- cleanup: every exit path ---------------------------------------------------------------------
cleanup() {
    trap '' USR1
    trap - EXIT INT TERM
    echo "== cleanup"
    local k fail=0
    # Background jobs (sampler, load and create subshells) first, the stdin holder last: an exec
    # whose agent is destroyed ends; one still running must never see its stdin close.
    kill $(jobs -p | grep -vx "$HOLDER") 2>/dev/null
    # Every id this run may have created, whether or not its create call returned.
    for ((k = 1; k < NEXT; k++)); do destroy "bench-$k" >/dev/null || destroy "bench-$k" || fail=1; done
    pkill -KILL -f "^([^ ]*/)?ctr .* task exec --exec-id $EXEC_TAG-" 2>/dev/null
    kill "$HOLDER" 2>/dev/null
    if ! cmp -s "$OUT/configuration.toml.orig" "$KATA"; then
        cat "$OUT/configuration.toml.orig" > "$KATA" && echo "kata config restored"
    fi
    echo "diff $KATA against the backup:"
    diff "$OUT/configuration.toml.orig" "$KATA" && echo "  (identical)" || fail=1
    nap 3
    state > "$OUT/state.after"
    left=$( { ctr -n "$NS" containers ls -q; ctr -n "$NS" tasks ls -q; ip netns; ls "$DATA/workspaces"
              ps -C qemu-system-x86_64,virtiofsd,containerd-shim-kata-v2,ctr -o args=; } 2>&1 |
              grep -E 'bench-[0-9]{1,3}([^0-9-]|$)' )
    if [ -n "$left" ]; then echo "LEFT BEHIND:"; echo "$left"; fail=1
    else echo "no bench-* container, task, netns, workspace or process left"; fi
    if diff "$OUT/state.before" "$OUT/state.after" > "$OUT/state.diff"; then
        echo "ac-egress, ac-gateway, nftables, firewalld zones, links and netns as before"
    else echo "HOST STATE CHANGED:"; cat "$OUT/state.diff"; fail=1; fi
    rm -f "$HOLD"
    "$GW" token revoke "$TNAME" >/dev/null && echo "token $TNAME revoked" || fail=1
    summary
    [ "$fail" = 0 ] || exit 1
    [ -s "$OUT/abort" ] && exit 3
    exit "${1:-0}"
}
trap 'STOP="interrupted by a signal"; cleanup 2' INT TERM
trap 'cleanup 3' USR1
trap 'cleanup $?' EXIT

# --- summary from the CSVs ------------------------------------------------------------------------
summary() {
    echo "== summary ($OUT)"
    [ -s "$OUT/abort" ] && echo "ended by: $(cat "$OUT/abort")" || echo "ended by: ${STOP:-staircase complete}"
    awk -F, 'NR > 1 { k = $2 " " $4; v[k] = v[k] " " $5 }
        END { for (k in v) { n = split(substr(v[k], 2), a, " "); asort(a)
            printf "%-24s n=%d min=%s med=%s max=%s\n", k, n, a[1], a[int((n + 1) / 2)], a[n] } }' \
        "$OUT/events.csv" | sort -t' ' -k1,1 -k2,2n
    echo "-- host per phase and N: min MemAvailable MiB, max swap MiB, max PSI mem some/full io some/full cpu some, max load1, mean cpu busy %, max egress RSS KiB, max /dev/shm MiB"
    awk -F, 'NR > 1 { k = $2 " " $3; c[k]++; if (!(k in ma) || $4 < ma[k]) ma[k] = $4
            if ($5 > sw[k]) sw[k] = $5; if ($7 > ms[k]) ms[k] = $7; if ($8 > mf[k]) mf[k] = $8
            if ($9 > is[k]) is[k] = $9; if ($10 > if_[k]) if_[k] = $10; if ($11 > cs[k]) cs[k] = $11
            if ($12 > l[k]) l[k] = $12; b[k] += $13; if ($14 > er[k]) er[k] = $14; if ($16 > sh[k]) sh[k] = $16 }
        END { for (k in c) printf "%-10s mem %6d swap %4d psi %5.1f/%5.1f %5.1f/%5.1f %5.1f load %5.2f busy %3.0f%% egress %d shm %d\n",
            k, ma[k], sw[k], ms[k], mf[k], is[k], if_[k], cs[k], l[k], b[k] / c[k], er[k], sh[k] }' \
        "$OUT/host.csv" | sort -k1,1 -k2,2n
    echo "-- per VM per phase and N: mean RSS / PSS MiB, mean CPU % of one core, sum CPU %, VMs that died"
    awk -F, 'NR > 1 { k = $2 " " $3; if ($5 == "dead") { d[k]++; c[k] += 0; next }
            c[k]++; r[k] += $5; p[k] += $6; u[k] += $7 }
        END { for (k in c) printf "%-10s rss %5.0f pss %5.0f cpu %5.1f%% sum %5.1f%% dead %d\n",
            k, c[k] ? r[k] / c[k] / 1024 : 0, c[k] ? p[k] / c[k] / 1024 : 0, c[k] ? u[k] / c[k] : 0, u[k], d[k] }' \
        "$OUT/vm.csv" | sort -k1,1 -k2,2n
}

# --- run ------------------------------------------------------------------------------------------
sampler &
SAMPLER=$!
STEPS=()
for n in 1 2 3 4 6 8; do [ "$n" -le "$MAX_N" ] && STEPS+=("$n"); done
LAST=${STEPS[-1]}

for n in "${STEPS[@]}"; do
    echo "== N=$n $(date +%T)"
    # Stop adding when the next agents would not fit: worst load-phase RSS seen so far per VM.
    peak=$(awk -F, 'NR > 1 && $2 == "load" && $5 != "dead" { if ($5 > m) m = $5 } END { print m + 0 }' "$OUT/vm.csv")
    need=$(( (n - ${#AGENTS[@]}) * peak ))
    if [ $(( $(kib MemAvailable) - need )) -lt $((MEM_FLOOR_KIB + 1048576)) ]; then
        STOP="headroom: N=$n needs ~$((need / 1024)) MiB more, $(( $(kib MemAvailable) / 1024 )) MiB available"
        echo "$STOP"; break
    fi
    [ "$n" = 1 ] || { echo "settle $n" > "$OUT/phase"; settle; }  # the others idle while one boots
    while [ "${#AGENTS[@]}" -lt "$n" ]; do
        id=bench-$NEXT; NEXT=$((NEXT + 1))
        echo "create" "$n" > "$OUT/phase"
        create "$id" || { STOP="create $id failed"; break 2; }
        AGENTS+=("$id")
        echo "  $id up in ${BOOT}s"
        [ "$n" = 1 ] || gx "$id" 'python3 /workspace/.bench/load.py prep' || { STOP="prep $id failed"; break 2; }
    done
    if [ "$n" = 1 ]; then
        a=${AGENTS[0]}
        echo "footprint 1" > "$OUT/phase"
        nap 60
        vm_sample footprint 60
        gx "$a" 'free -m' | tee "$OUT/guest-free-1.txt"
        shim=$(vm_pids "$a" | while read -r p; do [ "$(cat /proc/$p/comm)" = containerd-shim ] && echo "$p"; done)
        up=$(grep -F " /run/containerd/io.containerd.runtime.v2.task/$NS/$a/rootfs " "/proc/${shim:-0}/mountinfo" |
            tr ',' '\n' | sed -n 's/^upperdir=//p')
        ev disk_upper_kib "$a" 1 "$(du -sk "${up:-/nonexistent}" 2>/dev/null | cut -f1)"
        ev disk_workspace_kib "$a" 1 "$(du -sk --exclude=.bench "$DATA/workspaces/$a" | cut -f1)"
        for k in 1 2 3; do
            echo "pause 1" > "$OUT/phase"
            t0=$(now); r=$(api POST "/agents/$a/pause"); ev pause_s "$a" 1 "$(since "$t0")"
            case $r in 200\ *) ;; *) STOP="pause: $r"; break 2 ;; esac
            t0=$(now); r=$(api POST "/agents/$a/resume"); ev resume_api_s "$a" 1 "$(since "$t0")"
            case $r in 200\ *) ;; *) STOP="resume: $r"; break 2 ;; esac
            until gx "$a" true >/dev/null; do sleep 0.2; done
            ev resume_s "$a" 1 "$(since "$t0")"
        done
        gx "$a" 'python3 /workspace/.bench/load.py prep' || { STOP="prep $a failed"; break; }
    fi
    echo "settle $n" > "$OUT/phase"
    settle
    echo "idle $n" > "$OUT/phase"
    vm_sample idle 60
    if [ "$n" = "$LAST" ]; then
        echo "net $n" > "$OUT/phase"
        guest_all net "curl -sS -m 8 -o /dev/null -I -w 'http=%{http_code} connect_s=%{time_connect} tls_s=%{time_appconnect} total_s=%{time_total}' https://api.anthropic.com"
        NEXT=$((NEXT + 1))
        load_phase "bench-$((NEXT - 1))"
    else
        load_phase
    fi
    [ -s "$OUT/abort" ] && break
done
[ -s "$OUT/abort" ] && exit 3

# Parallel-create storm: 4 at once on an otherwise empty host (staircase agents destroyed first).
echo "== storm $(date +%T)"
for a in "${AGENTS[@]}"; do destroy "$a"; done
AGENTS=()
echo "purged 0" > "$OUT/phase"  # the purges' disk I/O: io PSI full limit 80 (bench-guard.sh)
settle
echo "storm 4" > "$OUT/phase"
pids=()
for k in 1 2 3 4; do
    id=bench-$NEXT; NEXT=$((NEXT + 1))
    ( create "$id" && ev storm_boot_s "$id" 4 "$BOOT" ) &
    pids+=($!)
done
wait "${pids[@]}"
for ((k = NEXT - 4; k < NEXT; k++)); do destroy "bench-$k"; done
[ -s "$OUT/abort" ] && exit 3

# One agent at a smaller default_memory: boot, guest memory and the 1 GiB toucher.
if [ "$SMALL_MEM" != 0 ]; then
    echo "== default_memory $SMALL_MEM $(date +%T)"
    sed "s/^default_memory = $VM_MIB\$/default_memory = $SMALL_MEM/" "$OUT/configuration.toml.orig" > "$OUT/configuration.toml.small"
    diff "$OUT/configuration.toml.orig" "$OUT/configuration.toml.small"
    cat "$OUT/configuration.toml.small" > "$KATA"
    id=bench-$NEXT; NEXT=$((NEXT + 1))
    echo "small 1" > "$OUT/phase"
    if create "$id"; then
        AGENTS=("$id")
        cat "$OUT/configuration.toml.orig" > "$KATA"  # only new VMs read it; restore at once
        ev small_boot_s "$id" 1 "$BOOT"
        gx "$id" 'free -m' | tee "$OUT/guest-free-small.txt"
        guest_all small 'timeout -k 5 100 python3 /workspace/.bench/load.py mem 20'
        destroy "$id"
        AGENTS=()
    fi
    cat "$OUT/configuration.toml.orig" > "$KATA"
fi
STOP="complete"
exit 0
