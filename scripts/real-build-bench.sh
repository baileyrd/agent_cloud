#!/bin/bash
# What a real build needs in an agent: guest memory, vCPUs, and its cost to the host.
#   sudo scripts/real-build-bench.sh matrix [--sizes "2048 3072 4096"] [--vcpus 1] [--reps 3]
#   sudo scripts/real-build-bench.sh concurrent --sizes MB [--vcpus 1] [--max-n 3] [--reps 2]
# The build is this repo at HEAD: `cargo build --release` and `cargo test --no-run`, both --workspace
# --offline --locked, from a clean target dir each time. Sources and `cargo vendor` output (made as the
# sudo user, offline, from their cargo cache) are copied into the workspace; the egress allowlist is
# not touched, and the agents need no network.
# matrix: per size, a fresh rb-1 runs the release build --reps times, then the test build --reps
# times; the host page cache is dropped before boot and before the first build (the cold run).
# concurrent: rb-1 and rb-2 run the release build at once, --reps times; then rb-1..rb-3 if N=2 left
# headroom (one more VM's peak PSS above the 1.5 GiB floor plus 1 GiB, io PSI full below 60).
# The agents come from a measurement-only image with a toolchain (the shipped image is unchanged):
#   printf 'FROM localhost/agent-shell:dev\nUSER root\nRUN env -u HTTPS_PROXY -u https_proxy -u HTTP_PROXY -u http_proxy dnf install -y --setopt=install_weak_deps=False gcc rust cargo && dnf clean all\nUSER agent\n' |
#       podman build -t agent-shell:bench-build -f - .
#   podman save localhost/agent-shell:bench-build | sudo ctr -n agents images import -
# For the run the gateway creates agents from it (a drop-in under /run, so a reboot also undoes it), and
# Kata's default_memory and default_vcpus are set per size, the gateway restarted after each change.
# Both are restored and shown on every exit path. Agents are rb-1..rb-3 only, through the gateway API
# with a throwaway token for them, revoked on exit; a1, a2 and other workspaces are never touched.
# Watchdog and thresholds: bench-guard.sh (io PSI full limit 80 while builds, their writeback and cold
# boots run). Raw samples and the
# per-run table: /var/tmp/ac-rb-<date>/ (runs.csv, host.csv, pss.csv; guest samples in build-*.mem.csv).
# Exit: 0 done, 3 a threshold ended the run, 2 setup error, 1 cleanup left something behind.
set -u
STAGE=${1:-} SIZES= VCPUS=1 REPS= MAX_N=3 ARGS="$*"
shift
while [ $# -gt 0 ]; do
    case $1 in
        --sizes) SIZES=$2; shift ;;
        --vcpus) VCPUS=$2; shift ;;
        --reps) REPS=$2; shift ;;
        --max-n) MAX_N=$2; shift ;;
        *) STAGE= ; break ;;
    esac
    shift
done
case $STAGE in
    matrix) SIZES=${SIZES:-2048 3072 4096} REPS=${REPS:-3} ;;
    concurrent) REPS=${REPS:-2}; [ "$(echo $SIZES | wc -w)" = 1 ] || STAGE= ;;
    *) STAGE= ;;
esac
[ -n "$STAGE" ] || { sed -n '3,4p' "$0" | sed 's/^# *//' >&2; exit 2; }
case $SIZES$VCPUS$REPS$MAX_N in *[!0-9\ ]*) echo "numbers only" >&2; exit 2 ;; esac

IMAGE=localhost/agent-shell:bench-build
ALLOW=/etc/agent-cloud/egress-allow.txt
DROPIN=/run/systemd/system/ac-gateway.service.d/zz-real-build-bench.conf
IMGENV=/run/ac-real-build-bench.env
REPO=$(cd "$(dirname "$0")/.." && pwd)
EXEC_TAG=rb
LOUD_PHASES="build create"  # create: boots read the image cold (caches dropped)
. "$(dirname "$0")/bench-guard.sh" || exit 2
[ "$(id -u)" = 0 ] || { echo "run as root (ctr, smaps_rollup, the tokens file, the Kata config)" >&2; exit 2; }
[ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != root ] || { echo "run through sudo: cargo vendor runs as you" >&2; exit 2; }
ctr -n "$NS" images ls -q | grep -qxF "$IMAGE" || { echo "$IMAGE is not in containerd (see the header)" >&2; exit 2; }
[ -z "$(val AC_VM_MEMORY_MIB)" ] || { echo "AC_VM_MEMORY_MIB is set: admission would not follow default_memory" >&2; exit 2; }
gw_image() { tr '\0' '\n' < "/proc/$(pgrep -x ac-gateway | head -n 1)/environ" | sed -n 's/^AC_IMAGE=//p'; }
IMAGE0=$(gw_image)
MEM0=$(kata default_memory) VCPU0=$(kata default_vcpus)
guard_token real-build-bench rb-1,rb-2,rb-3

OUT=/var/tmp/ac-rb-$(date +%Y%m%d-%H%M%S)
mkdir -p -m 700 "$OUT" || exit 2
exec > >(tee -a "$OUT/run.log") 2>&1
MAIN=$$
START=$(date +%s)
echo "output: $OUT"
echo "$ARGS" > "$OUT/args"

# --- preflight ---------------------------------------------------------------------------------
guard_services
r=$(API_SECS=10 api GET /agents)
case $r in 200\ *) ;; *) echo "list agents: $r" >&2; exit 2 ;; esac
live=$(echo "$r" | grep -oE '"id":"rb-[0-9]+","state":"[a-z]+"' | grep -v '"destroyed"')
[ -z "$live" ] || { echo "rb-* agents in use; clean up first: $live" >&2; exit 2; }
[ ! -e "$DROPIN" ] || { echo "$DROPIN exists: an earlier run did not finish; remove it, daemon-reload, restart ac-gateway" >&2; exit 2; }
state > "$OUT/state.before"
cp -a "$KATA" "$OUT/configuration.toml.orig" || exit 2
sha256sum "$ALLOW" > "$OUT/allow.sha256" || exit 2
{
    echo "== baseline $(date -Is)"
    lscpu | grep -E 'Model name|^CPU\(s\)|Thread|MHz'
    uname -r
    grep -E 'MemTotal|MemAvailable|SwapTotal|SwapFree' /proc/meminfo
    lsblk -d -o NAME,ROTA,MODEL,SIZE -e 7,252
    df -m /dev/shm | tail -n 1
    grep -E '^(default_vcpus|default_maxvcpus|default_memory|default_maxmemory|virtio_fs_cache) ' "$KATA"
    echo "gateway AC_IMAGE=$IMAGE0"
    echo "loadavg $(cat /proc/loadavg)"
    echo "running tasks: $(ctr -n "$NS" tasks ls -q | tr '\n' ' ')"
    ctr -n "$NS" images ls | awk 'NR == 1 || /agent-shell/' | awk '{ print $1, $2, $4, $5 }'
    echo "egress allowlist: $(cut -c1-16 "$OUT/allow.sha256")..."
    echo "repo HEAD $(runuser -u "$SUDO_USER" -- git -C "$REPO" rev-parse HEAD)"
} | tee "$OUT/baseline.txt"

echo "idle 0" > "$OUT/phase"
guard_hold
AGENTS=()
echo "ts,event,agent,n,value" > "$OUT/events.csv"
echo "ts,agent,pss_mib" > "$OUT/pss.csv"
echo "stage,n,mem_mib,vcpus,agent,kind,rep,exit,wall_s,user_s,sys_s,maxrss_mib,top,guest_total_mib,min_avail_mib,min_avail_pct,peak_used_mib,peak_cached_mib,swap_peak_mib,guest_oom,dmesg_oom,pss_before_mib,pss_peak_mib,pss_after_mib,shm_before_mib,shm_after_mib,host_min_avail_mib,psi_mem_some,psi_mem_full,psi_io_some,psi_io_full,psi_cpu_some,load_max,disk_busy_mean,disk_busy_max,egress_lines,writeback_s" > "$OUT/runs.csv"

# --- the gateway and Kata ------------------------------------------------------------------------
# Restart and wait until it answers: it reports active before it listens.
gw_restart() {
    local i=0 ph
    ph=$(cat "$OUT/phase"); echo "restart 0" > "$OUT/phase"
    systemctl restart ac-gateway || return 1
    until [ "$(API_SECS=3 api GET /whoami | cut -d' ' -f1)" = 200 ]; do
        i=$((i + 1)); [ "$i" -lt 60 ] || { echo "ac-gateway not answering 60 s after restart"; return 1; }
        sleep 1
    done
    echo "$ph" > "$OUT/phase"
    echo "  ac-gateway restarted, answering after ${i} s; AC_IMAGE=$(gw_image)"
    echo "  capacity: $(api GET /capacity)"
}
# A later EnvironmentFile overrides gateway.env; Environment= would not.
use_image() {
    mkdir -p "${DROPIN%/*}" || return 1
    echo "AC_IMAGE=$IMAGE" > "$IMGENV"
    printf '[Service]\nEnvironmentFile=-%s\n' "$IMGENV" > "$DROPIN"
    systemctl daemon-reload
}
# kata_set <MiB> <vcpus>: from the backup, then restart the gateway so admission reads it.
kata_set() {
    sed -e "s/^default_memory = $MEM0\$/default_memory = $1/" -e "s/^default_vcpus = $VCPU0\$/default_vcpus = $2/" \
        "$OUT/configuration.toml.orig" > "$OUT/configuration.toml.run"
    cat "$OUT/configuration.toml.run" > "$KATA"
    [ "$(kata default_memory) $(kata default_vcpus)" = "$1 $2" ] || { echo "kata config: not $1 MiB, $2 vCPU"; return 1; }
    echo "  $KATA: default_memory $1, default_vcpus $2"
    diff "$OUT/configuration.toml.orig" "$KATA" | sed 's/^/  /'
    gw_restart
}
restore() {
    local fail=0 again=
    [ -e "$DROPIN" ] && again=1
    rm -f "$DROPIN" "$IMGENV"
    rmdir "${DROPIN%/*}" 2>/dev/null
    systemctl daemon-reload
    if ! cmp -s "$OUT/configuration.toml.orig" "$KATA"; then
        cat "$OUT/configuration.toml.orig" > "$KATA" && echo "kata config restored"; again=1
    fi
    echo "diff $KATA against the backup:"
    diff "$OUT/configuration.toml.orig" "$KATA" && echo "  (identical)" || fail=1
    [ "$(gw_image)" = "$IMAGE0" ] || again=1
    [ -z "$again" ] || gw_restart || fail=1
    if [ "$(gw_image)" = "$IMAGE0" ]; then echo "ac-gateway AC_IMAGE=$IMAGE0, as before"
    else echo "ac-gateway AC_IMAGE=$(gw_image), was $IMAGE0"; fail=1; fi
    return $fail
}

# --- the build -----------------------------------------------------------------------------------
# HEAD plus vendored dependencies, made as the sudo user (their cargo cache, offline).
bundle() {
    SRC=$(runuser -u "$SUDO_USER" -- mktemp -d /var/tmp/rb-src.XXXXXX) || return 1
    runuser -u "$SUDO_USER" -- sh -c 'git -C "$1" archive HEAD | tar -x -C "$2" && cd "$2" &&
        cargo vendor --offline --locked vendor > config.toml 2> vendor.log && mkdir .cargo && mv config.toml .cargo/' \
        sh "$REPO" "$SRC" || { echo "source bundle: $(tail -n 3 "$SRC/vendor.log" 2>/dev/null)"; return 1; }
    echo "sources: $(du -sm "$SRC" | cut -f1) MiB, $(ls "$SRC/vendor" | wc -l) vendored crates"
}

BUILD=$OUT/build.py
cat > "$BUILD" <<'PY'
import os, resource, shutil, subprocess, sys, threading, time

kind, rep = sys.argv[1], sys.argv[2]
SRC, LOG = "/workspace/src", f"/workspace/.bench/{kind}-{rep}"
CMD = {"release": ["cargo", "build", "--release"], "test": ["cargo", "test", "--no-run"]}[kind]
CMD += ["--workspace", "--offline", "--locked"]

def meminfo():
    with open("/proc/meminfo") as f:
        return {l.split(":")[0]: int(l.split()[1]) for l in f}

def counter(path, key):
    try:
        with open(path) as f:
            return next((int(l.split()[1]) for l in f if l.split()[0] == key), None)
    except OSError:
        return None

def top():
    """(VmHWM KiB, name) of the biggest live process."""
    best = (0, "-")
    for p in os.listdir("/proc"):
        if not p.isdigit():
            continue
        try:
            with open(f"/proc/{p}/status") as f:
                s = dict(l.split(":", 1) for l in f if ":" in l)
        except OSError:
            continue
        if "VmHWM" in s and int(s["VmHWM"].split()[0]) > best[0]:
            best = (int(s["VmHWM"].split()[0]), s["Name"].split()[0])
    return best

shutil.rmtree(SRC + "/target", ignore_errors=True)  # every run builds everything
m0 = meminfo()
oom0, cg0 = counter("/proc/vmstat", "oom_kill"), counter("/sys/fs/cgroup/memory.events", "oom_kill")
peak = {"avail": m0["MemAvailable"], "used": 0, "cached": 0, "swap": 0, "top": (0, "-")}
done = threading.Event()

def sample():
    # Once a second: MemAvailable, used without cache (as free(1)), page cache, swap, biggest process.
    t0 = time.monotonic()
    with open(LOG + ".mem.csv", "w") as out:
        out.write("t,memavail_kib,used_kib,cached_kib,dirty_kib,swap_used_kib,top_hwm_kib,top\n")
        while True:
            m = meminfo()
            used = m["MemTotal"] - m["MemFree"] - m["Buffers"] - m["Cached"] - m["SReclaimable"]
            swap, t = m["SwapTotal"] - m["SwapFree"], top()
            peak.update(avail=min(peak["avail"], m["MemAvailable"]), used=max(peak["used"], used),
                        cached=max(peak["cached"], m["Cached"]), swap=max(peak["swap"], swap),
                        top=max(peak["top"], t))
            out.write(f"{time.monotonic() - t0:.0f},{m['MemAvailable']},{used},{m['Cached']},{m['Dirty']},{swap},{t[0]},{t[1]}\n")
            if done.wait(1):
                return

th = threading.Thread(target=sample)
th.start()
t = time.monotonic()
env = dict(os.environ, CARGO_HOME="/workspace/.cargo-home", CARGO_TERM_COLOR="never")
with open(LOG + ".log", "w") as log:
    rc = subprocess.call(CMD, cwd=SRC, stdout=log, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL, env=env)
wall = time.monotonic() - t
done.set()
th.join()
ru, mib = resource.getrusage(resource.RUSAGE_CHILDREN), lambda k: k // 1024
oom, cg = counter("/proc/vmstat", "oom_kill"), counter("/sys/fs/cgroup/memory.events", "oom_kill")
print(f"exit={rc} wall_s={wall:.1f} user_s={ru.ru_utime:.0f} sys_s={ru.ru_stime:.0f} maxrss_mib={mib(ru.ru_maxrss)} "
      f"top={peak['top'][1]}:{mib(peak['top'][0])} guest_total_mib={mib(m0['MemTotal'])} "
      f"min_avail_mib={mib(peak['avail'])} min_avail_pct={100 * peak['avail'] / m0['MemTotal']:.0f} "
      f"peak_used_mib={mib(peak['used'])} peak_cached_mib={mib(peak['cached'])} swap_total_mib={mib(m0['SwapTotal'])} "
      f"swap_peak_mib={mib(peak['swap'])} guest_oom={oom - oom0} cgroup_oom={'na' if cg is None else cg - cg0} cpus={os.cpu_count()}")
if rc:
    with open(LOG + ".log") as f:
        print("# " + "# ".join(f.readlines()[-5:]), end="")
PY

# Sources and build.py into the agent's workspace, owned like it. No cp -a: the copies take the
# workspace's SELinux label, not /var/tmp's.
stage_ws() {
    local ws=$DATA/workspaces/$1 u g
    u=$(stat -c %u "$ws") g=$(stat -c %g "$ws")
    install -d -o "$u" -g "$g" -m 700 "$ws/src" "$ws/.bench" &&
        cp -R --preserve=mode,timestamps "$SRC/." "$ws/src/" && chown -R "$u:$g" "$ws/src" &&
        install -o "$u" -g "$g" -m 600 "$BUILD" "$ws/.bench/build.py"
}
create() {  # create <id>: through the API; BOOT = create call to first exec
    local t0 r i=0
    t0=$(now)
    r=$(api POST "/agents/$1")
    case $r in 201\ *) ;; *) echo "create $1: $r"; return 1 ;; esac
    until gx "$1" true >/dev/null; do
        i=$((i + 1)); [ "$i" -lt 600 ] || { echo "$1: no exec after create"; return 1; }
        sleep 0.2
    done
    BOOT=$(since "$t0")
    AGENTS+=("$1")
    ev boot_s "$1" "${#AGENTS[@]}" "$BOOT"
    stage_ws "$1" || { echo "$1: staging the sources failed"; return 1; }
    echo "  $1 up in ${BOOT}s; guest: $(GX_USER=0 gx "$1" 'echo "$(nproc) cpu, $(free -m | awk "/^Mem/ { print \$2 }") MiB, swap $(free -m | awk "/^Swap/ { print \$2 }") MiB, /tmp on $(df --output=fstype /tmp | tail -n 1), dmesg_restrict $(cat /proc/sys/kernel/dmesg_restrict)"')"
}
drop_caches() { sync; echo 3 > /proc/sys/vm/drop_caches; }
pss() { set -- $(mem $(vm_pids "$1")); echo $(( ${2:-0} / 1024 )); }
shm() { df -m --output=used /dev/shm | tail -n 1; }
kv() { awk -v k="$1" '{ for (i = 1; i <= NF; i++) if (index($i, k "=") == 1) { print substr($i, length(k) + 2); exit } }' <<< "$2"; }

# run_builds <stage> <mem> <kind> <rep> <agent>...: the build in every agent at once; one runs.csv row each.
run_builds() {
    local stage=$1 mmb=$2 kind=$3 rep=$4 a pids=() t0 t1 w wb line host eg
    local -A p0
    local s0
    shift 4
    for a in "$@"; do p0[$a]=$(pss "$a"); done
    s0=$(shm)
    echo "build $#" > "$OUT/phase"
    t0=$(date +%s)
    for a in "$@"; do
        ( gx "$a" "python3 /workspace/.bench/build.py $kind $rep" > "$OUT/build-$a-$kind-$rep.out" ) &
        pids+=($!)
    done
    ( while nap 2; do for a in "$@"; do echo "$(date +%s),$a,$(pss "$a")"; done; done >> "$OUT/pss.csv" ) &
    w=$!
    wait "${pids[@]}"
    t1=$(date +%s)
    # The host writes the build's output back to disk a few seconds after it ends: part of the
    # build's cost. Wait at least 10 s, then for two samples under 20% busy (at most 180 s).
    while nap 2; awk -F, -v a="$t1" '{ d[NR % 2] = $17 } END { t = $1 - a
            exit !(t < 180 && (t < 10 || d[0] >= 20 || d[1] >= 20)) }' "$OUT/host.csv"; do :; done
    wb=$(( $(date +%s) - t1 ))
    kill "$w"
    echo "idle $#" > "$OUT/phase"
    host=$(awk -F, -v a="$t0" -v b="$((t1 + wb))" 'NR > 1 && $1 >= a && $1 <= b + 2 {
            if (!n++ || $4 < ma) ma = $4
            for (i = 7; i <= 12; i++) if ($i > mx[i]) mx[i] = $i
            if ($17 > dm) dm = $17; ds += $17 }
        END { printf "%d,%.1f,%.1f,%.1f,%.1f,%.1f,%.2f,%.0f,%.0f", ma, mx[7], mx[8], mx[9], mx[10], mx[11], mx[12], n ? ds / n : 0, dm }' "$OUT/host.csv")
    eg=$(awk -v a="$t0" -v b="$((t1 + wb + 1))" '$1 >= a && $1 <= b && / ac-egress\[/ && /: (allow|deny) /' "$OUT/journal.txt" |
        tee -a "$OUT/egress.log" | wc -l)
    for a in "$@"; do
        line=$(grep -m 1 '^exit=' "$OUT/build-$a-$kind-$rep.out")
        [ -n "$line" ] || { echo "  $a $kind $rep: no result: $(tail -n 3 "$OUT/build-$a-$kind-$rep.out")"; line="exit=noresult"; }
        grep '^# ' "$OUT/build-$a-$kind-$rep.out" | sed "s/^/  $a: /"
        local dm peak
        dm=$(GX_USER=0 gx "$a" 'dmesg >/dev/null 2>&1 && dmesg | grep -ciE "out of memory|oom-kill|killed process" || echo na')
        peak=$(awk -F, -v a="$t0" -v b="$((t1 + wb))" -v id="$a" '$2 == id && $1 >= a && $1 <= b && $3 > m { m = $3 } END { print m + 0 }' "$OUT/pss.csv")
        echo "$stage,$#,$mmb,$VCPUS,$a,$kind,$rep,$(for k in exit wall_s user_s sys_s maxrss_mib top guest_total_mib min_avail_mib min_avail_pct peak_used_mib peak_cached_mib swap_peak_mib guest_oom; do printf '%s,' "$(kv $k "$line")"; done)${dm//[!0-9a-z]/},${p0[$a]},$peak,$(pss "$a"),$s0,$(shm),$host,$eg,$wb" >> "$OUT/runs.csv"
        echo "  $a $kind #$rep: $(kv exit "$line") in $(kv wall_s "$line") s, guest used peak $(kv peak_used_mib "$line") MiB, MemAvailable min $(kv min_avail_mib "$line") MiB ($(kv min_avail_pct "$line")%), biggest $(kv top "$line") MiB (maxrss $(kv maxrss_mib "$line")), oom $(kv guest_oom "$line"), PSS ${p0[$a]} -> $peak -> $(pss "$a") MiB, /dev/shm $(shm) MiB, writeback ${wb} s, egress lines $eg"
    done
}

# --- cleanup: every exit path ---------------------------------------------------------------------
cleanup() {
    trap '' USR1
    trap - EXIT INT TERM
    echo "== cleanup"
    local a fail=0 left
    kill $(jobs -p | grep -vx "${HOLDER:-x}") 2>/dev/null
    for a in rb-1 rb-2 rb-3; do destroy "$a" >/dev/null || destroy "$a" || fail=1; done
    pkill -KILL -f "^([^ ]*/)?ctr .* task exec --exec-id $EXEC_TAG-" 2>/dev/null
    kill "${HOLDER:-}" 2>/dev/null
    restore || fail=1
    if [ "$fail" != 0 ]; then  # a destroy refused while the gateway was down: again, once it is back
        fail=0
        for a in rb-1 rb-2 rb-3; do destroy "$a" || fail=1; done
        diff -q "$OUT/configuration.toml.orig" "$KATA" >/dev/null && [ "$(gw_image)" = "$IMAGE0" ] || fail=1
    fi
    nap 3
    state > "$OUT/state.after"
    left=$( { ctr -n "$NS" containers ls -q; ctr -n "$NS" tasks ls -q; ip netns; ls "$DATA/workspaces"
              ps -C qemu-system-x86_64,virtiofsd,containerd-shim-kata-v2,ctr -o args=; } 2>&1 |
              grep -E '(^|[^a-z0-9-]|ac-)rb-[0-9]([^0-9-]|$)' )
    if [ -n "$left" ]; then echo "LEFT BEHIND:"; echo "$left"; fail=1
    else echo "no rb-* container, task, netns, workspace or process left"; fi
    if diff "$OUT/state.before" "$OUT/state.after" > "$OUT/state.diff"; then
        echo "ac-egress, ac-gateway, nftables, firewalld zones, links and netns as before"
    else echo "HOST STATE CHANGED:"; cat "$OUT/state.diff"; fail=1; fi
    sha256sum -c --quiet "$OUT/allow.sha256" && echo "egress allowlist unchanged (sha256)" || fail=1
    echo "ac-egress allow/deny lines during the run: $(grep -cE ' ac-egress\[.*: (allow|deny) ' "$OUT/journal.txt" 2>/dev/null)"
    echo "ac-gateway watchdog lines about rb-*: $(grep ' ac-gateway\[' "$OUT/journal.txt" 2>/dev/null | grep -E 'rung|unhealthy' | grep -c 'rb-')"
    case ${SRC:-} in /var/tmp/rb-src.*) rm -rf -- "$SRC" ;; esac
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

summary() {
    echo "== summary ($OUT)"
    [ -s "$OUT/abort" ] && echo "ended by: $(cat "$OUT/abort")" || echo "ended by: ${STOP:-complete}"
    cut -d, -f1-9,12,13,15-17,20,21,23,24,26,27,31,33-37 "$OUT/runs.csv" | column -t -s,
    echo "-- warm runs (rep > 1): wall s, guest used peak MiB, min MemAvailable MiB, min / median / max"
    awk -F, 'NR > 1 && $7 > 1 { k = $1 " n=" $2 " " $3 "MiB " $4 "vcpu " $6; w[k] = w[k] " " $9; u[k] = u[k] " " $17; a[k] = a[k] " " $15 }
        function s(v,  n, x) { n = split(substr(v, 2), x, " "); asort(x)
            return x[1] " / " (n % 2 ? x[(n + 1) / 2] : (x[n / 2] + x[n / 2 + 1]) / 2) " / " x[n] " (n=" n ")" }
        END { for (k in w) printf "%-36s wall %s; used %s; avail %s\n", k, s(w[k]), s(u[k]), s(a[k]) }' "$OUT/runs.csv" | sort
}

# --- run ------------------------------------------------------------------------------------------
# Both services' journal lines, followed from now. Not `journalctl --since` per run: from a cold cache
# it reads the whole journal (~1 GiB here, ~30 s at 100% disk busy) and trips the io threshold itself.
# A follower pays that once, here, before the watchdog starts (until io PSI full avg10 < 10).
journalctl -f -n 0 -o short-unix -u ac-egress -u ac-gateway > "$OUT/journal.txt" 2>&1 &
for ((i = 0; i < 60; i++)); do
    nap 2
    [ "$i" -ge 5 ] && awk -v v="$(psi io full)" 'BEGIN { exit !(v < 10) }' && break
done
sampler &
bundle || { STOP="source bundle failed"; exit 2; }
use_image || { STOP="drop-in failed"; exit 2; }

if [ "$STAGE" = matrix ]; then
    for mem in $SIZES; do
        echo "== $mem MiB, $VCPUS vCPU $(date +%T)"
        kata_set "$mem" "$VCPUS" || { STOP="kata_set $mem"; exit 2; }
        [ "$(gw_image)" = "$IMAGE" ] || { STOP="ac-gateway did not take AC_IMAGE=$IMAGE"; exit 2; }
        echo "settle 0" > "$OUT/phase"; settle
        drop_caches
        echo "create 1" > "$OUT/phase"
        create rb-1 || { STOP="create rb-1 failed"; exit 2; }
        drop_caches  # the first build reads its sources cold
        for kind in release test; do
            for ((rep = 1; rep <= REPS; rep++)); do run_builds matrix "$mem" "$kind" "$rep" rb-1; done
        done
        destroy rb-1 || { STOP="destroy rb-1 failed"; exit 2; }
        AGENTS=()
    done
else
    mem=$SIZES
    kata_set "$mem" "$VCPUS" || { STOP="kata_set $mem"; exit 2; }
    [ "$(gw_image)" = "$IMAGE" ] || { STOP="ac-gateway did not take AC_IMAGE=$IMAGE"; exit 2; }
    for n in 2 3; do
        [ "$n" -le "$MAX_N" ] || break
        if [ "$n" = 3 ]; then
            # One more VM at the worst PSS seen must leave the 1.5 GiB floor plus 1 GiB; io PSI full < 60.
            read -r minav pk iof < <(awk -F, 'NR > 1 && $1 == "concurrent" && $2 == 2 {
                if (!n++ || $27 < m) m = $27; if ($23 > p) p = $23; if ($31 > f) f = $31 } END { print m + 0, p + 0, f + 0 }' "$OUT/runs.csv")
            echo "N=3 check: host MemAvailable min $minav MiB at N=2, peak VM PSS $pk MiB, io PSI full max $iof" | tee "$OUT/n3-check"
            if [ $((minav - pk)) -lt $((MEM_FLOOR_KIB / 1024 + 1024)) ] || awk -v v="$iof" 'BEGIN { exit !(v >= 60) }'; then
                STOP="N=3 skipped: not enough headroom at N=2"; echo "$STOP" | tee -a "$OUT/n3-check"; break
            fi
        fi
        echo "== N=$n at $mem MiB, $VCPUS vCPU $(date +%T)"
        echo "settle 0" > "$OUT/phase"; settle
        drop_caches
        echo "create $n" > "$OUT/phase"
        for ((k = 1; k <= n; k++)); do create "rb-$k" || { STOP="create rb-$k failed"; exit 2; }; done
        drop_caches
        for ((rep = 1; rep <= REPS; rep++)); do run_builds concurrent "$mem" release "$rep" "${AGENTS[@]}"; done
        for a in "${AGENTS[@]}"; do destroy "$a" || { STOP="destroy $a failed"; exit 2; }; done
        AGENTS=()
    done
fi
exit 0
