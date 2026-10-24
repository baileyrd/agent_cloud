#!/bin/bash
# The job start gate (ADR-013) under real builds through the job API: how many builds run at once, why
# and how long a queued job waits, and what the gate costs a tiny job.
#   sudo scripts/burst-test.sh [--reps 3] [base] [B] [C] [D]      (default: base B C D)
# base: a1 runs the build alone --reps times by direct exec (no Claude call), the first cold.
# B: a build job on a1 and, at once, one on a2; once both run, a build job on a3. --reps runs, the first
#    cold. The first two jobs of every B and C run are experiment A (two builds submitted together).
# C: as B, but a3's job is the tiny "reply with exactly: pong".
# D: once, warm: B with the PSI and spacing gates off (AC_JOB_CPU_PSI_MAX=1000, AC_JOB_START_SPACING=0),
#    the control. The load gate (load1 < cores) has no setting and stays.
# The build is this repo at HEAD with `cargo vendor` output (as real-build-bench.sh) in /workspace/burst:
# `cargo build --release --workspace --offline --locked` from a clean target dir; a job is asked to run
# exactly that (PROMPT). Its wall time is read on the host side of the workspace: target/'s birth time to
# the newest mtime under target/release, so the model's latency is not in it. Cold, as in
# real-build-bench.sh: the agents boot fresh after a host page-cache drop, which is dropped again before
# the build.
# Needs a1, a2 and a3 paused, each with a Claude login, and localhost/agent-shell:burst-build in containerd
# (the shipped image plus rust and cargo; real-build-bench.sh's header has the build, with this tag). The
# gateway creates VMs from it for the run (a drop-in under /run) and the agents are resumed on it. Every
# exit path pauses them, removes /workspace/burst and the drop-in, and restarts the gateway.
# Token: throwaway, jobs:read,jobs:write,agents:read,agents:write for a1,a2,a3, revoked on exit.
# Watchdog: bench-guard.sh without its load trip (a burst is meant to load the CPU), plus 15 min per run
# and any gateway watchdog action (a "rung" or "unhealthy" line would be the watchdog misfiring).
# Output: /var/tmp/ac-burst-<date>/: burst.csv (1 s samples: CPU PSI, load, capacity, each job's state
# and queued_reason), jobs.csv (one row per job or direct build), checks.csv (between runs), host.csv,
# journal.txt.
# Exit: 0 done, 3 a threshold ended the run, 2 setup error, 1 cleanup left something behind.
set -u
REPS=3 EXPS= ARGS="$*"
while [ $# -gt 0 ]; do
    case $1 in
        --reps) REPS=${2:-}; shift ;;
        base|B|C|D) EXPS="$EXPS $1" ;;
        *) REPS=x; break ;;
    esac
    shift
done
EXPS=${EXPS:- base B C D}
case $REPS in ''|*[!0-9]*) sed -n '4p' "$0" | sed 's/^# *//' >&2; exit 2 ;; esac

IMAGE=localhost/agent-shell:burst-build
ALLOW=/etc/agent-cloud/egress-allow.txt
DROPIN=/run/systemd/system/ac-gateway.service.d/zz-burst-test.conf
GWENV=/run/ac-burst-test.env
REPO=$(cd "$(dirname "$0")/.." && pwd)
EXEC_TAG=bt
LOUD_PHASES="build boot"  # boot: cold boots read the image after a cache drop
NO_LOAD_TRIP=1
RUN_LIMIT=900
AGENTS=(a1 a2 a3)
BUILD="cargo build --release --workspace --offline --locked"
PROMPT="In /workspace/burst run exactly: $BUILD. Then reply with only the word \`built\`, or the first error line if it failed. Run nothing else."
PONG="reply with exactly: pong"
. "$(dirname "$0")/bench-guard.sh" || exit 2
[ "$(id -u)" = 0 ] || { echo "run as root (ctr, the tokens file, systemd, the workspaces)" >&2; exit 2; }
[ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != root ] || { echo "run through sudo: cargo vendor runs as you" >&2; exit 2; }
ctr -n "$NS" images ls -q | grep -qxF "$IMAGE" || { echo "$IMAGE is not in containerd (see the header)" >&2; exit 2; }
IMAGE0=$(gw_image)
JOBS0=$(journalctl -u ac-gateway -o cat -n 300 --no-pager | grep 'jobs: at most' | tail -n 1)
guard_token burst-test a1,a2,a3 jobs:read,jobs:write,agents:read,agents:write

OUT=/var/tmp/ac-burst-$(date +%Y%m%d-%H%M%S)
mkdir -p -m 700 "$OUT" "$OUT/cur" || exit 2
exec > >(tee -a "$OUT/run.log") 2>&1
MAIN=$$
echo "output: $OUT"
echo "$ARGS" > "$OUT/args"

# --- preflight ---------------------------------------------------------------------------------
guard_services
jf() { sed -n "s/.*\"$1\":\"\\{0,1\\}\\([^\",}]*\\).*/\\1/p" <<< "$2"; }  # jf <field> <json>
agent_state() { jf state "$(API_SECS=10 api GET "/agents/$1")"; }
for a in "${AGENTS[@]}"; do
    s=$(agent_state "$a")
    [ "$s" = paused ] || { echo "$a is ${s:-unknown}, want paused" >&2; exit 2; }
    [ ! -e "$DATA/workspaces/$a/burst" ] || { echo "$DATA/workspaces/$a/burst exists: an earlier run did not finish" >&2; exit 2; }
done
[ ! -e "$DROPIN" ] || { echo "$DROPIN exists: an earlier run did not finish; remove it, daemon-reload, restart ac-gateway" >&2; exit 2; }
doc=$(ac-ctl doctor --profile build 2>&1) || { echo "ac-ctl doctor --profile build: exit $?"; echo "$doc"; exit 2; }
state > "$OUT/state.before"
cp -a "$KATA" "$OUT/configuration.toml.orig" || exit 2
sha256sum "$ALLOW" > "$OUT/allow.sha256" || exit 2
{
    echo "== baseline $(date -Is)"
    lscpu | grep -E 'Model name|^CPU\(s\)'
    uname -r
    grep -E 'MemTotal|MemAvailable' /proc/meminfo
    df -m /dev/shm | tail -n 1
    grep -E '^(default_vcpus|default_memory|default_maxmemory) ' "$KATA"
    echo "gateway AC_IMAGE=$IMAGE0"
    echo "gateway: $JOBS0"
    echo "doctor: $(echo "$doc" | grep -E '^AC_JOB_CPU_PSI_MAX|^AC_MAX_RUNNING_JOBS' | tr -s ' ' | tr '\n' ';') $(echo "$doc" | tail -n 1)"
    echo "loadavg $(cat /proc/loadavg)"
    echo "egress allowlist: $(cut -c1-16 "$OUT/allow.sha256")...; kata config: $(sha256sum "$KATA" | cut -c1-16)..."
    echo "repo HEAD $(runuser -u "$SUDO_USER" -- git -C "$REPO" rev-parse HEAD)"
    echo "experiments:$EXPS; reps $REPS (D once)"
} | tee "$OUT/baseline.txt"

echo "idle 0" > "$OUT/phase"
echo "idle -" > "$OUT/run"
guard_hold
echo "ts,event,agent,n,value" > "$OUT/events.csv"
echo "exp,rep,agent,kind,job,state,exit,result,submit_s,start_s,finish_s,queued_s,job_s,build_start_s,build_end_s,build_s,psi_at_start,load_at_start,queued_reasons" > "$OUT/jobs.csv"
echo "ts,exp,rep,when,running_jobs,queued_jobs,guest_procs,shm_used_mib,memavail_mib,load1,psi_cpu_some" > "$OUT/checks.csv"

# --- the gateway ---------------------------------------------------------------------------------
# A later EnvironmentFile overrides gateway.env; Environment= would not. gw_env [KEY=value...]
gw_env() {
    mkdir -p "${DROPIN%/*}" || return 1
    printf '%s\n' "AC_IMAGE=$IMAGE" "$@" > "$GWENV"
    printf '[Service]\nEnvironmentFile=-%s\n' "$GWENV" > "$DROPIN"
    systemctl daemon-reload && gw_restart && jobs_line
}
# The running gateway's startup line about the job gate, from the journal follower.
jobs_line() {
    local pid i
    pid=$(pgrep -x ac-gateway | head -n 1)
    for ((i = 0; i < 10; i++)); do
        grep -q "ac-gateway\[$pid\]: ac-gateway: jobs: at most" "$OUT/journal.txt" && break
        nap 1
    done
    echo "  startup: $(grep "ac-gateway\[$pid\]: ac-gateway: jobs: at most" "$OUT/journal.txt" | tail -n 1 | sed 's/^.*ac-gateway: jobs/jobs/')"
}
restore() {
    local fail=0 again=
    [ -e "$DROPIN" ] && again=1
    rm -f "$DROPIN" "$GWENV"
    rmdir "${DROPIN%/*}" 2>/dev/null
    systemctl daemon-reload
    [ "$(gw_image)" = "$IMAGE0" ] || again=1
    [ -z "$again" ] || { gw_restart && jobs_line; } || fail=1
    if [ "$(gw_image)" = "$IMAGE0" ]; then echo "ac-gateway AC_IMAGE=$IMAGE0, as before"
    else echo "ac-gateway AC_IMAGE=$(gw_image), was $IMAGE0"; fail=1; fi
    return $fail
}

# --- agents --------------------------------------------------------------------------------------
# Sources into /workspace/burst from the host side, owned like the workspace, while the agent is paused.
# No cp -a: the copies take the workspace's SELinux label, not /var/tmp's.
stage_ws() {
    local ws=$DATA/workspaces/$1 u g
    u=$(stat -c %u "$ws") g=$(stat -c %g "$ws")
    install -d -o "$u" -g "$g" -m 700 "$ws/burst" &&
        cp -R --preserve=mode,timestamps "$SRC/." "$ws/burst/" && chown -R "$u:$g" "$ws/burst"
}
# boot <agent>...: resume each (a fresh VM from $IMAGE), wait for an exec, check the login and cargo.
boot() {
    local a r t0 i out
    echo "boot $#" > "$OUT/phase"
    for a in "$@"; do
        t0=$(now)
        r=$(api POST "/agents/$a/resume")
        case $r in *'"state":"running"'*) ;; *) echo "resume $a: $r"; return 1 ;; esac
        i=0
        until gx "$a" true >/dev/null; do
            i=$((i + 1)); [ "$i" -lt 600 ] || { echo "$a: no exec after resume"; return 1; }
            nap 0.2
        done
        ev boot_s "$a" - "$(since "$t0")"
        out=$(gx "$a" 'claude auth status; cargo --version')
        case $out in *'"loggedIn": true'*cargo\ *) ;; *) echo "$a: $(echo "$out" | tr '\n' ' ' | cut -c1-200)"; return 1 ;; esac
        echo "  $a up in $(since "$t0") s: loggedIn, $(echo "$out" | grep '^cargo ')"
    done
    echo "idle 0" > "$OUT/phase"
}
pause() {  # FORCE=1: interrupt a job
    local a r
    for a in "$@"; do
        [ "$(agent_state "$a")" != paused ] || continue
        r=$(API_SECS=180 api POST "/agents/$a/pause${FORCE:+?force=true}")
        case $r in *'"state":"paused"'*) ;; *) echo "pause $a: $r"; return 1 ;; esac
    done
}
# Cold, as real-build-bench.sh: a fresh boot after a page-cache drop, then another drop before the build.
cold() { pause "${AGENTS[@]}" && drop_caches && boot "${AGENTS[@]}"; }

# --- 1 s sampler: what the gate sees and decides ----------------------------------------------------
cpu_ticks() { awk '/^cpu / { print $2+$3+$4+$7+$8+$9, $2+$3+$4+$5+$6+$7+$8+$9 }' /proc/stat; }
burst_sampler() {
    local busy0 total0 busy total cap a id j row t0 w
    echo "ts,exp,rep,psi_cpu_some,load1,cpu_busy_pct,running_jobs,queued_jobs,max_running_jobs,a1_state,a1_reason,a2_state,a2_reason,a3_state,a3_reason" > "$OUT/burst.csv"
    read -r busy0 total0 < <(cpu_ticks)
    while nap 1; do
        read -r busy total < <(cpu_ticks)
        cap=$(API_SECS=3 api GET /capacity)
        row="$(now),$(tr ' ' , < "$OUT/run"),$(psi cpu some),$(cut -d' ' -f1 /proc/loadavg)"
        row="$row,$(awk -v b=$((busy - busy0)) -v t=$((total - total0)) 'BEGIN { printf "%.0f", t ? 100 * b / t : 0 }')"
        row="$row,$(jf running_jobs "$cap"),$(jf queued_jobs "$cap"),$(jf max_running_jobs "$cap")"
        busy0=$busy total0=$total
        for a in "${AGENTS[@]}"; do
            id=$(cat "$OUT/cur/$a" 2>/dev/null)
            if [ -n "$id" ]; then j=$(API_SECS=3 api GET "/agents/$a/jobs/$id"); row="$row,$(jf state "$j"),$(jf queued_reason "$j")"
            else row="$row,-,-"; fi
        done
        echo "$row" >> "$OUT/burst.csv"
        w=$(grep -E ' ac-gateway\[[0-9]+\]: .*(rung [0-9]|unhealthy)' "$OUT/journal.txt" | head -n 1)
        [ -z "$w" ] || trip "gateway watchdog acted: ${w#* }"
        t0=$(cat "$OUT/run_t0" 2>/dev/null)
        if [ -n "$t0" ] && awk -v a="$t0" -v b="$EPOCHREALTIME" -v m=$RUN_LIMIT 'BEGIN { exit !(b - a > m) }'; then
            trip "run $(cat "$OUT/run") longer than $RUN_LIMIT s"
        fi
    done
}

# --- one run --------------------------------------------------------------------------------------
# Before a run: the CPU quiet (load1 < 1, CPU PSI < 2, io PSI some < 5; at most 300 s), so every run
# meets the gate the same way.
quiet() {
    local t0
    t0=$(now)
    while awk -v a="$t0" -v b="$EPOCHREALTIME" 'BEGIN { exit !(b - a < 300) }'; do
        awk -v l="$(cut -d' ' -f1 /proc/loadavg)" -v c="$(psi cpu some)" -v i="$(psi io some)" \
            'BEGIN { exit !(l < 1 && c < 2 && i < 5) }' && break
        nap 2
    done
    ev quiet_s - - "$(since "$t0")"
    echo "  quiet after $(since "$t0") s: load $(cut -d' ' -f1-3 /proc/loadavg), cpu PSI some $(psi cpu some)"
}
# Idle between runs: no job running or queued, no cargo, rustc or claude in any guest.
check() {
    local cap n=0 a c i
    for ((i = 0; i < 15; i++)); do
        cap=$(API_SECS=10 api GET /capacity) n=0
        for a in "${AGENTS[@]}"; do
            c=$(gx "$a" 'pgrep -c -x cargo; pgrep -c rustc; pgrep -cf "[c]laude -p"' | awk '{ s += $1 } END { print s + 0 }')
            n=$((n + c))
        done
        [ "$n" = 0 ] && [ "$(jf running_jobs "$cap")" = 0 ] && [ "$(jf queued_jobs "$cap")" = 0 ] && break
        nap 2
    done
    echo "$(date +%s),$(tr ' ' , < "$OUT/run"),$1,$(jf running_jobs "$cap"),$(jf queued_jobs "$cap"),$n,$(df -m --output=used /dev/shm | tail -n 1),$(($(kib MemAvailable) / 1024)),$(cut -d' ' -f1 /proc/loadavg),$(psi cpu some)" >> "$OUT/checks.csv"
    echo "  $1: running_jobs $(jf running_jobs "$cap"), queued $(jf queued_jobs "$cap"), guest cargo/rustc/claude $n, /dev/shm $(df -m --output=used /dev/shm | tail -n 1) MiB, MemAvailable $(($(kib MemAvailable) / 1024)) MiB"
    [ "$n" = 0 ] && [ "$(jf running_jobs "$cap")" = 0 ] && [ "$(jf queued_jobs "$cap")" = 0 ]
}
# The build's wall time from the host: target/'s birth time and the newest mtime under target/release.
build_times() {
    local t=$DATA/workspaces/$1/burst/target
    [ -d "$t/release" ] || { echo ",,"; return; }
    awk -v a="$(stat -c %.3W "$t")" -v b="$(find "$t/release" -printf '%T@\n' | sort -n | tail -n 1)" \
        'BEGIN { printf "%.3f,%.3f,%.1f", a, b, b - a }'
}
# The 1 s sample at or before time $1: "psi,load".
at() { awk -F, -v t="$1" 'NR > 1 && $1 <= t { v = $4 "," $5 } END { print (v == "" ? "," : v) }' "$OUT/burst.csv"; }
# queued_reason spans of agent $1's job in this run, from the 1 s samples: "start spacing 14s; host busy 40s".
spans() {
    local c
    c=$(( $(printf '%s\n' "${AGENTS[@]}" | grep -nx "$1" | cut -d: -f1) * 2 + 8 ))
    awk -F, -v e="$2" -v r="$3" -v c=$c '$2 == e && $3 == r && $c == "queued" {
            if ($(c + 1) != last) { if (last != "") out = out sprintf("%s %ds; ", last, n); last = $(c + 1); n = 0 } n++ }
        END { if (last != "") out = out sprintf("%s %ds", last, n); print out }' "$OUT/burst.csv"
}
submit() {  # submit <agent> <prompt>
    local r
    printf '{"prompt":"%s","timeout_secs":900}' "$2" > "$OUT/body.json"
    r=$(API_SECS=30 api POST "/agents/$1/jobs" "$OUT/body.json")
    case $r in 202\ *) jf job_id "$r" > "$OUT/cur/$1"; ev submit "$1" - "$(cat "$OUT/cur/$1")" ;;
        *) echo "submit $1: $r"; return 1 ;; esac
}
job() { API_SECS=60 api GET "/agents/$1/jobs/$(cat "$OUT/cur/$1")${2:-}"; }
until_running() {  # every job of agents $@ running (or already over)
    local a s
    for a in "$@"; do
        while s=$(jf state "$(job "$a")"); [ "$s" = queued ]; do nap 0.5; done
    done
}
until_done() {
    local a s
    for a in "$@"; do
        while s=$(jf state "$(job "$a" '?wait=30')"); [ "$s" = queued ] || [ "$s" = running ]; do :; done
    done
}
# One jobs.csv row per agent of the run.
record() {  # record <exp> <rep> <agent> <kind>
    local j id out c s f bt
    j=$(job "$3") id=$(cat "$OUT/cur/$3")
    out=$(API_SECS=30 api GET "/agents/$3/jobs/$id/output" | cut -d' ' -f2- | head -n 1 | tr -d ',\r' | cut -c1-60)
    c=$(jf created_ms "$j") s=$(jf started_ms "$j") f=$(jf finished_ms "$j")
    bt=$( [ "$4" = pong ] && echo ",," || build_times "$3")
    case $s in ''|null) s=0 ;; esac
    echo "$1,$2,$3,$4,$id,$(jf state "$j"),$(jf exit_code "$j"),$out,$(awk -v c="$c" -v s="$s" -v f="$f" 'BEGIN {
            printf "%.3f,%.3f,%.3f,%.1f,%.1f", c / 1000, s / 1000, f / 1000, (s - c) / 1000, (f - s) / 1000 }'),$bt,$(at "$(awk -v s="$s" 'BEGIN { printf "%.3f", s / 1000 }')"),$(spans "$3" "$1" "$2")" >> "$OUT/jobs.csv"
}
# burst <exp> <rep> <third kind: build|pong>: a1 and a2 build jobs at once; a3's once both run.
burst() {
    local a kind
    for a in "${AGENTS[@]}"; do gx "$a" 'rm -rf /workspace/burst/target' || return 1; done
    quiet
    [ "$2" = 1 ] && [ "$1" != D ] && drop_caches
    echo "$1 $2" > "$OUT/run"
    rm -f "$OUT/cur/"*
    check before || { STOP="not idle before $1 $2"; return 1; }
    echo "build 3" > "$OUT/phase"
    now > "$OUT/run_t0"
    submit a1 "$PROMPT" && submit a2 "$PROMPT" || return 1
    until_running a1 a2
    if [ "$3" = pong ]; then submit a3 "$PONG"; else submit a3 "$PROMPT"; fi || return 1
    until_done "${AGENTS[@]}"
    rm -f "$OUT/run_t0"
    echo "idle 0" > "$OUT/phase"
    nap 2
    for a in "${AGENTS[@]}"; do
        kind=build; [ "$a" = a3 ] && kind=$3
        record "$1" "$2" "$a" "$kind"
    done
    echo "idle -" > "$OUT/run"
    report "$1" "$2"
    check after || { STOP="not idle after $1 $2"; return 1; }
    awk -F, -v e="$1" -v r="$2" '$1 == e && $2 == r && $4 == "build" && $8 != "built" { bad = 1 } END { exit bad }' "$OUT/jobs.csv" ||
        { STOP="a build job did not reply 'built' in $1 $2"; return 1; }
}
# base <rep>: the build alone in a1, by exec, timed the same way.
base() {
    local out
    gx a1 'rm -rf /workspace/burst/target' || return 1
    quiet
    [ "$1" = 1 ] && drop_caches
    echo "base $1" > "$OUT/run"
    check before || { STOP="not idle before base $1"; return 1; }
    echo "build 1" > "$OUT/phase"
    now > "$OUT/run_t0"
    out=$(gx a1 "cd /workspace/burst && $BUILD > build.log 2>&1; echo \$?")
    rm -f "$OUT/run_t0"
    echo "idle 0" > "$OUT/phase"
    echo "base,$1,a1,direct,-,-,$out,$( [ "$out" = 0 ] && echo built || tail -n 1 "$DATA/workspaces/a1/burst/build.log" | tr -d ',' | cut -c1-60),,,,,,$(build_times a1),,," >> "$OUT/jobs.csv"
    echo "idle -" > "$OUT/run"
    report base "$1"
    [ "$out" = 0 ] || { STOP="base build $1 failed"; return 1; }
}
report() {
    awk -F, -v e="$1" -v r="$2" '$1 == e && $2 == r' "$OUT/jobs.csv" | while IFS=, read -r _ _ a k _ st ex res sub sta fin q js b0 b1 bs ps ld rs; do
        if [ "$k" = direct ]; then echo "  $1 #$2 $a: build ${bs} s, exit $ex ($res)"; continue; fi
        echo "  $1 #$2 $a $k: $st '$res'; queued ${q} s [${rs:-not seen queued}], job ${js} s, build ${bs:--} s; at start CPU PSI ${ps:-?} load ${ld:-?}"
    done
    awk -F, -v e="$1" -v r="$2" '$2 == e && $3 == r { if ($4 > p) p = $4; if ($7 > j) j = $7 } END {
        printf "  %s #%s: peak CPU PSI some %.2f, peak running_jobs %d\n", e, r, p, j }' "$OUT/burst.csv"
}

# --- cleanup: every exit path ---------------------------------------------------------------------
cleanup() {
    trap '' USR1
    trap - EXIT INT TERM
    echo "== cleanup"
    local a fail=0 left
    kill $(jobs -p | grep -vxE "${HOLDER:-x}|${JOURNAL:-x}") 2>/dev/null
    rm -f "$OUT/run_t0"
    FORCE=1 pause "${AGENTS[@]}" || fail=1
    pkill -KILL -f "^([^ ]*/)?ctr .* task exec --exec-id $EXEC_TAG-" 2>/dev/null
    kill "${HOLDER:-}" 2>/dev/null
    restore || fail=1
    if [ "$fail" != 0 ]; then  # a pause refused while the gateway was down: again, once it is back
        fail=0
        FORCE=1 pause "${AGENTS[@]}" || fail=1
    fi
    for a in "${AGENTS[@]}"; do
        if [ "$(agent_state "$a")" = paused ]; then rm -rf -- "$DATA/workspaces/$a/burst"
        else echo "$a not paused: /workspace/burst kept"; fail=1; fi
        [ ! -e "$DATA/workspaces/$a/burst" ] || fail=1
    done
    echo "agents: $(for a in "${AGENTS[@]}"; do printf '%s %s; ' "$a" "$(agent_state "$a")"; done)/workspace/burst removed: $(ls -d "$DATA"/workspaces/a[123]/burst 2>/dev/null | wc -l | sed 's/^0$/yes/')"
    nap 3
    state > "$OUT/state.after"
    left=$( { ctr -n "$NS" tasks ls -q; ip netns; ps -C qemu-system-x86_64,virtiofsd,containerd-shim-kata-v2 -o args=
              pgrep -af "exec-id $EXEC_TAG-"; } 2>&1 | grep -v '^$')
    if [ -n "$left" ]; then echo "LEFT BEHIND:"; echo "$left"; fail=1
    else echo "no task, netns, VM process or exec left"; fi
    if diff "$OUT/state.before" "$OUT/state.after" > "$OUT/state.diff"; then
        echo "ac-egress, ac-gateway, nftables, firewalld zones, links and netns as before"
    else echo "HOST STATE CHANGED:"; cat "$OUT/state.diff"; fail=1; fi
    sha256sum -c --quiet "$OUT/allow.sha256" && echo "egress allowlist unchanged (sha256)" || fail=1
    cmp "$OUT/configuration.toml.orig" "$KATA" && echo "kata config byte-identical" || fail=1
    echo "ac-egress during the run: $(sed -n 's/.* ac-egress\[[0-9]*\]: ac-egress: \(allow\|deny\) [0-9.]* \([^ :]*\).*/\1 \2/p' "$OUT/journal.txt" | sort | uniq -c | tr -s ' ' | tr '\n' ';')"
    echo "ac-gateway watchdog actions (rung/unhealthy): $(grep ' ac-gateway\[' "$OUT/journal.txt" | grep -cE 'rung [0-9]|unhealthy')," \
        "'not acting' lines: $(grep -c 'not acting' "$OUT/journal.txt")"
    kill "${JOURNAL:-}" 2>/dev/null
    case ${SRC:-} in /var/tmp/bt-src.*) rm -rf -- "$SRC" ;; esac
    rm -f "$HOLD" "$OUT/body.json"
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
    cut -d, -f1-4,6,8,12,13,16-19 "$OUT/jobs.csv" | column -t -s,
    echo "-- build s, min / median / max: cold = run 1 (not D), warm = the rest"
    awk -F, 'NR > 1 && $16 != "" { k = $1 " " ($2 == 1 && $1 != "D" ? "cold" : "warm") " " $3; v[k] = v[k] " " $16 }
        function s(x,  n, y) { n = split(substr(x, 2), y, " "); asort(y)
            return y[1] " / " (n % 2 ? y[(n + 1) / 2] : (y[n / 2] + y[n / 2 + 1]) / 2) " / " y[n] " (n=" n ")" }
        END { for (k in v) printf "%-16s %s\n", k, s(v[k]) }' "$OUT/jobs.csv" | sort
    echo "-- between runs:"
    column -t -s, "$OUT/checks.csv"
}

# --- run ------------------------------------------------------------------------------------------
# The journal followed from now (not `journalctl --since` later: from a cold cache it reads the whole
# journal and trips the io limit itself); the watchdog starts once its start-up scan is done.
journalctl -f -n 0 -o short-unix -u ac-egress -u ac-gateway > "$OUT/journal.txt" 2>&1 &
JOURNAL=$!
for ((i = 0; i < 60; i++)); do
    nap 2
    [ "$i" -ge 5 ] && awk -v v="$(psi io full)" 'BEGIN { exit !(v < 10) }' && break
done
sampler &
burst_sampler &
bundle || { STOP="source bundle failed"; exit 2; }
for a in "${AGENTS[@]}"; do stage_ws "$a" || { STOP="staging $a failed"; exit 2; }; done
echo "staged /workspace/burst in ${AGENTS[*]}"
echo "== gateway on $IMAGE"
gw_env || { STOP="drop-in failed"; exit 2; }
[ "$(gw_image)" = "$IMAGE" ] || { STOP="ac-gateway did not take AC_IMAGE=$IMAGE"; exit 2; }
boot "${AGENTS[@]}" || { STOP="boot failed"; exit 2; }
check idle || { STOP="not idle after boot"; exit 2; }

for exp in $EXPS; do
    echo "== $exp $(date +%T)"
    n=$REPS; [ "$exp" = D ] && n=1
    if [ "$exp" = D ]; then
        gw_env AC_JOB_CPU_PSI_MAX=1000 AC_JOB_START_SPACING=0 || { STOP="drop-in for D failed"; exit 2; }
    fi
    for ((rep = 1; rep <= n; rep++)); do
        if [ "$rep" = 1 ] && [ "$exp" != D ]; then cold || { STOP="cold boot failed"; exit 2; }; fi
        case $exp in
            base) base "$rep" ;;
            B) burst B "$rep" build ;;
            C) burst C "$rep" pong ;;
            D) burst D "$rep" build ;;
        esac || exit 2
    done
    if [ "$exp" = D ]; then
        echo "== gateway back to the live gate"
        gw_env || { STOP="drop-in after D failed"; exit 2; }
    fi
done
exit 0
