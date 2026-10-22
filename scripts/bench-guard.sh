# Sourced by capacity-bench.sh and real-build-bench.sh, never run: host readers, the gateway API with a
# scoped throwaway token, the watchdog (trip() names the threshold), held-stdin execs and the per-VM
# figures. The caller sets OUT and MAIN (its $$) and traps USR1 to its cleanup; EXEC_TAG prefixes exec
# ids; LOUD_PHASES lists the phases whose io PSI full limit is 80 instead of 40.
NS=agents
DATA=/var/lib/agent-cloud
KATA=/etc/kata-containers/configuration.toml
ENV=/etc/agent-cloud/gateway.env
CORES=$(nproc)
CLK=$(getconf CLK_TCK)
MEM_FLOOR_KIB=$((3 * 1024 * 1024 / 2))  # 1.5 GiB
SWAP_PAGES_S=256                        # pswpin+pswpout per second (1 MiB/s at 4 KiB pages)
val() { sed -n "s/^$1=//p" "$ENV" 2>/dev/null | tail -n 1; }

now() { echo "$EPOCHREALTIME"; }
since() { awk -v a="$1" -v b="$EPOCHREALTIME" 'BEGIN { printf "%.2f", b - a }'; }
kib() { awk -v k="$1" '/^[A-Za-z]/ && $1 == k":" { print $2 }' /proc/meminfo; }
vmstat() { awk -v k="$1" '$1 == k { print $2 }' /proc/vmstat; }
psi() { sed -n "s/^$2 avg10=\([0-9.]*\).*/\1/p" "/proc/pressure/$1"; }  # psi memory some -> 0.00
kata() { sed -n "s/^$1 = \([0-9]*\)$/\1/p" "$KATA" | head -n 1; }
# The disk under the data directory (sda for /dev/sda3[/subvol]) and its busy ms (/proc/diskstats).
DISK=$(lsblk -no PKNAME "$(findmnt -no SOURCE -T "$DATA" | sed 's/\[.*//')" 2>/dev/null | head -n 1)
busy_ms() { awk -v d="$DISK" '$3 == d { print $13 }' /proc/diskstats; }

# The gateway API with the bench token (on curl's stdin). api <method> <path> -> "<status> <body>".
api() {
    printf 'Authorization: Bearer %s\n' "$TOKEN" |
        curl -sS -m "${API_SECS:-300}" -H @- -X "$1" -w ' %{http_code}' "$URL/api/v1$2" 2>&1 |
        sed 's/^\(.*\) \([0-9]*\)$/\2 \1/'
}

# guard_token <name> <agents>: a token <name>-<pid> with agents:read,agents:write for <agents>, revoked
# by an EXIT trap until the caller's cleanup takes over. A leftover <name>-* token means an earlier run
# died: it is revoked and this run stops.
guard_token() {
    local old stale=
    URL=http://$(val AC_BIND)
    GW=$(command -v ac-gateway || echo /usr/local/bin/ac-gateway)
    [ -z "$(val AC_TOKENS)" ] || export AC_TOKENS="$(val AC_TOKENS)"
    for old in $("$GW" token list | awk -v p="^$1-" '$1 ~ p { print $1 }'); do
        "$GW" token revoke "$old" >/dev/null && echo "revoked leftover token $old; not running (start again)" >&2
        stale=1
    done
    [ -z "$stale" ] || exit 2
    TNAME=$1-$$
    TOKEN=$("$GW" token create "$TNAME" --scopes agents:read,agents:write --agents "$2") ||
        { echo "token create failed" >&2; exit 2; }
    trap '"$GW" token revoke "$TNAME" >/dev/null' EXIT
    sleep 2  # the gateway re-reads the tokens file within a second of a change
}

# guard_services: the proxy (pid, address) must be up; the bench token must work.
guard_services() {
    local r
    r=$(API_SECS=10 api GET /whoami)
    case $r in 200\ *"\"$TNAME\""*) ;; *) echo "bench token: $r" >&2; exit 2 ;; esac
    EGRESS_PID=$(pgrep -x ac-egress | head -n 1)
    GATEWAY_PID=$(pgrep -x ac-gateway | head -n 1)
    EGRESS_ADDR=$(ss -ltnpH | awk '/"ac-egress"/ && $4 ~ /:3128$/ { print $4; exit }')
    [ -n "$EGRESS_PID" ] && [ -n "$EGRESS_ADDR" ] || { echo "ac-egress not listening on :3128" >&2; exit 2; }
}
# Host state that cleanup must leave unchanged (nft -s: rules without counters).
state() {
    { systemctl is-active ac-egress ac-gateway; nft -s list ruleset; firewall-cmd --get-active-zones
      ip -o link show | awk -F': ' '{ print $2 }' | sort; ip netns; } 2>&1
}

# --- watchdog: one host sample every 2 s; the first threshold crossed ends the run ---------------
trip() { echo "$1" > "$OUT/abort"; echo "ABORT: $1"; kill -USR1 "$MAIN"; exit 0; }
sampler() {
    local busy0 total0 sw0 eg0 oom0 t0 d0 over= miss=0 i=0 line busy total sw eg t d ph lim
    echo "ts,phase,n,memavail_mib,swap_used_mib,swap_pages_s,psi_mem_some,psi_mem_full,psi_io_some,psi_io_full,psi_cpu_some,load1,cpu_busy_pct,egress_rss_kib,egress_cpu_pct,shm_used_mib,disk_busy_pct" > "$OUT/host.csv"
    read -r busy0 total0 < <(awk '/^cpu / { print $2+$3+$4+$7+$8+$9, $2+$3+$4+$5+$6+$7+$8+$9 }' /proc/stat)
    sw0=$(( $(vmstat pswpin) + $(vmstat pswpout) )) oom0=$(vmstat oom_kill) d0=$(busy_ms)
    eg0=$(sed 's/^.*) //' /proc/$EGRESS_PID/stat | awk '{ print $12 + $13 }') t0=$EPOCHREALTIME
    while sleep 2; do
        i=$((i + 1))
        read -r busy total < <(awk '/^cpu / { print $2+$3+$4+$7+$8+$9, $2+$3+$4+$5+$6+$7+$8+$9 }' /proc/stat)
        sw=$(( $(vmstat pswpin) + $(vmstat pswpout) ))
        eg=$(sed 's/^.*) //' /proc/$EGRESS_PID/stat 2>/dev/null | awk '{ print $12 + $13 }')
        d=$(busy_ms)
        t=$EPOCHREALTIME
        ph=$(cat "$OUT/phase")
        line=$(awk -v ph="$ph" -v ma="$(kib MemAvailable)" -v st="$(kib SwapTotal)" \
            -v sf="$(kib SwapFree)" -v sw=$((sw - sw0)) -v ms="$(psi memory some)" -v mf="$(psi memory full)" \
            -v is="$(psi io some)" -v io="$(psi io full)" -v cs="$(psi cpu some)" -v l="$(cut -d' ' -f1 /proc/loadavg)" \
            -v b=$((busy - busy0)) -v tt=$((total - total0)) -v er="$(awk '/^VmRSS/ { print $2 }' /proc/$EGRESS_PID/status 2>/dev/null)" \
            -v e=$((${eg:-0} - eg0)) -v clk="$CLK" -v t0="$t0" -v t="$t" -v shm="$(df -m --output=used /dev/shm | tail -n 1)" \
            -v db=$((${d:-0} - ${d0:-0})) 'BEGIN {
                dt = t - t0; split(ph, p, " ")
                printf "%d,%s,%s,%d,%d,%.0f,%s,%s,%s,%s,%s,%s,%.0f,%s,%.1f,%d,%.0f\n", t, p[1], p[2], ma / 1024, (st - sf) / 1024,
                    sw / dt, ms, mf, is, io, cs, l, tt ? 100 * b / tt : 0, er, 100 * e / clk / dt, shm, db / 10 / dt }')
        echo "$line" >> "$OUT/host.csv"
        busy0=$busy total0=$total sw0=$sw eg0=${eg:-0} t0=$t d0=$d
        IFS=, read -r _ _ _ ma _ sps ms mf _ iof _ l _ <<< "$line"
        [ "$ma" -ge $((MEM_FLOOR_KIB / 1024)) ] || trip "MemAvailable ${ma} MiB < 1536 MiB"
        [ "${sps%.*}" -le "$SWAP_PAGES_S" ] || trip "swap in+out ${sps} pages/s > $SWAP_PAGES_S"
        awk -v v="$mf" 'BEGIN { exit !(v > 10) }' && trip "memory PSI full avg10 $mf > 10"
        awk -v v="$ms" 'BEGIN { exit !(v > 40) }' && trip "memory PSI some avg10 $ms > 40"
        # io pressure only slows the host: 80 while the disk-heavy phases run, 40 otherwise.
        lim=40; case " ${LOUD_PHASES:-load} " in *" ${ph%% *} "*) lim=80 ;; esac
        awk -v v="$iof" -v m=$lim 'BEGIN { exit !(v > m) }' && trip "io PSI full avg10 $iof > $lim"
        [ "$(vmstat oom_kill)" = "$oom0" ] || trip "host OOM kill ($(dmesg | grep -iE 'killed process|out of memory' | tail -n 1))"
        if awk -v v="$l" -v c="$CORES" 'BEGIN { exit !(v > 2 * c) }'; then
            [ -n "$over" ] || over=$t
            awk -v a="$over" -v b="$t" 'BEGIN { exit !(b - a >= 60) }' && trip "load average $l > $((2 * CORES)) for 60 s"
        else over=; fi
        [ $((i % 3)) = 0 ] || continue
        # Both services: alive and answering. The gateway may be busy with a create (one API lock),
        # so it counts as down after three missed 10 s probes in a row (about 30 s); a deliberate
        # restart (phase "restart") is not counted.
        kill -0 "$EGRESS_PID" 2>/dev/null && timeout 3 bash -c "exec 3<>/dev/tcp/${EGRESS_ADDR%:*}/${EGRESS_ADDR##*:}" \
            || trip "ac-egress not answering on $EGRESS_ADDR"
        [ "${ph%% *}" != restart ] || { miss=0; continue; }
        case $(API_SECS=10 api GET /whoami) in
            200\ *) miss=0 ;;
            *) miss=$((miss + 1)); [ "$miss" -lt 3 ] || trip "ac-gateway not answering for 30 s" ;;
        esac
    done
}

# --- agents -------------------------------------------------------------------------------------
# Exec stdin is a FIFO a background sleep holds open, so no exec ever sees EOF on stdin while its
# guest process lives (that wedges the Kata shim, ADR-010; /dev/null is no exception).
guard_hold() {
    HOLD=$OUT/stdin.fifo
    mkfifo -m 600 "$HOLD" || exit 2
    sleep 1000000 > "$HOLD" &
    HOLDER=$!
}
# gx <agent> <sh -c script>; GX_USER=0 runs it as root in the guest.
gx() { ctr -n "$NS" task exec ${GX_USER:+--user "$GX_USER"} --exec-id "$EXEC_TAG-$BASHPID-$SRANDOM" "$1" sh -c "$2" < "$HOLD" 2>&1; }

ev() { echo "$(date +%s),$1,$2,$3,$4" >> "$OUT/events.csv"; }  # ev <event> <agent> <n> <value>

# PIDs of agent $1's QEMU, virtiofsd and containerd-shim-kata-v2 (the id as a whole word).
vm_pids() { pgrep -f "(^|[^a-z0-9-])$1([^a-z0-9-]|\$)" | while read -r p; do
    case $(cat /proc/$p/comm 2>/dev/null) in qemu-system-*|virtiofsd|containerd-shim*) echo "$p" ;; esac; done; }
ticks() { for p in "$@"; do sed 's/^.*) //' /proc/$p/stat 2>/dev/null; done | awk '{ s += $12 + $13 } END { print s + 0 }'; }
mem() { for p in "$@"; do cat /proc/$p/smaps_rollup 2>/dev/null; done |
    awk '/^Rss:/ { r += $2 } /^Pss:/ { p += $2 } END { print r + 0, p + 0 }'; }
nap() { sleep "$1" & wait $!; }

destroy() {  # 404: already gone
    local r
    r=$(API_SECS=180 api DELETE "/agents/$1?purge=true&force=true")
    case $r in 200\ *|404\ *) ;; *) echo "destroy $1: $r"; return 1 ;; esac
}

# Settle: wait (10 to 120 s) until the last host sample shows cpu busy < 25% and io PSI some < 5.
settle() {
    local t0 l
    t0=$(now)
    nap 10
    while awk -v a="$t0" -v b="$EPOCHREALTIME" 'BEGIN { exit !(b - a < 120) }'; do
        l=$(tail -n 1 "$OUT/host.csv")
        awk -F, '{ exit !($13 < 25 && $9 < 5) }' <<< "$l" && break
        nap 2
    done
    ev settle_s - "${#AGENTS[@]}" "$(since "$t0")"
}
