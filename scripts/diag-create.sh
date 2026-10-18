#!/bin/sh
# Start an agent, capture the guest console from boot, snapshot Kata/QEMU/containerd state while
# create runs, then report and clean up (including leftover Kata processes for this id).
# Usage: sudo scripts/diag-create.sh <image> <agent-id> [wait-seconds] [network|none]
set -u
IMAGE=${1:?image}; ID=${2:?agent id}; WAIT=${3:-45}; MODE=${4:-network}
CTL=${CTL:-target/release/ac-ctl}
[ "$(id -u)" -eq 0 ] || { echo "run as root (sudo)" >&2; exit 1; }
OUT=$(mktemp); CONS=/tmp/diag-console-$ID.log; : >"$CONS"
NETFLAG=--network; [ "$MODE" = none ] && NETFLAG=

kill_leftovers() {
    pkill -f "sandbox-$ID," 2>/dev/null
    pkill -f "containerd-shim-kata-v2.* -id $ID( |\$)" 2>/dev/null
    pkill -f "shared/sandboxes/$ID/shared" 2>/dev/null
    sleep 2
    rm -rf "/run/vc/vm/$ID" "/run/vc/sbs/$ID"
}

echo "== clean start ($MODE)"
kill_leftovers
# shellcheck disable=SC2086
"$CTL" --image "$IMAGE" $NETFLAG destroy "$ID" 2>&1 | tail -2
kill_leftovers

echo "== guest console reader (background)"
python3 - "$ID" "$CONS" >/dev/null 2>&1 <<'PY' &
import os, socket, sys, time
p, log = f"/run/vc/vm/{sys.argv[1]}/console.sock", sys.argv[2]
t = time.time()
while not os.path.exists(p) and time.time() - t < 90:
    time.sleep(0.05)
s = socket.socket(socket.AF_UNIX)
s.settimeout(120)
try:
    s.connect(p)
except Exception as e:
    open(log, "a").write(f"[console connect failed: {e}]\n")
    sys.exit()
with open(log, "ab") as f:
    end = time.time() + 130
    while time.time() < end:
        try:
            d = s.recv(65536)
        except Exception:
            break
        if not d:
            break
        f.write(d); f.flush()
PY

echo "== create (traced), in background"
# shellcheck disable=SC2086
AC_TRACE=1 "$CTL" --image "$IMAGE" $NETFLAG create "$ID" >"$OUT" 2>&1 &
PID=$!
sleep "$WAIT"

echo "== snapshot after ${WAIT}s"
echo "-- processes"
ps -eo pid,ppid,etimes,stat,wchan:22,args | grep -iE 'qemu|kata|shim|ctr |virtiofsd' | grep -v grep | cut -c1-160
echo "-- qemu command line (full)"
for p in $(pgrep -f "qemu-system.*sandbox-$ID,"); do tr '\0' ' ' </proc/"$p"/cmdline; echo; done
echo "-- /run/vc/vm/$ID"; ls -la "/run/vc/vm/$ID" 2>&1
echo "-- netns / host links"; ls /run/netns 2>&1; ip -br link | grep -E 'acbr0|veth|tap' 2>&1
echo "-- containerd log (last 2 min)"
journalctl -u containerd --since "2 min ago" --no-pager -o cat 2>&1 | cut -c1-300 | tail -40
echo "-- guest console (last 6000 bytes)"; tail -c 6000 "$CONS"

echo "== waiting for create to finish"
wait "$PID"; echo "create exit=$?"
cat "$OUT"; rm -f "$OUT"

echo "== cleanup"
# shellcheck disable=SC2086
"$CTL" --image "$IMAGE" $NETFLAG destroy "$ID" 2>&1 | tail -2
echo "-- leftovers before kill:"; ps -eo pid,etimes,args | grep -iE "sandbox-$ID,|kata-v2.*-id $ID" | grep -v grep | cut -c1-120
kill_leftovers
echo "-- leftovers after kill:"; ps -eo pid,etimes,args | grep -iE "sandbox-$ID,|kata-v2.*-id $ID" | grep -v grep | cut -c1-120
echo "(console log kept at $CONS)"
