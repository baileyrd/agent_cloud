#!/bin/sh
# Start an agent with --network, snapshot what Kata/QEMU/containerd are doing while it runs,
# then report the result and clean up. Read-only apart from the create/destroy itself.
# Usage: sudo scripts/diag-create.sh <image> <agent-id> [wait-seconds]
set -u
IMAGE=${1:?image}; ID=${2:?agent id}; WAIT=${3:-45}
CTL=${CTL:-target/release/ac-ctl}
[ "$(id -u)" -eq 0 ] || { echo "run as root (sudo)" >&2; exit 1; }
OUT=$(mktemp)

echo "== clean start"
"$CTL" --image "$IMAGE" --network destroy "$ID" 2>&1 | tail -2

echo "== create (traced), in background"
AC_TRACE=1 "$CTL" --image "$IMAGE" --network create "$ID" >"$OUT" 2>&1 &
PID=$!
sleep "$WAIT"

echo "== snapshot after ${WAIT}s"
echo "-- processes (qemu args in full)"
ps -eo pid,ppid,etimes,stat,wchan:22,args | grep -iE 'qemu|kata|shim|ctr ' | grep -v grep | cut -c1-400
echo "-- /run/vc/vm/$ID"; ls -la "/run/vc/vm/$ID" 2>&1
echo "-- netns"; ls /run/netns 2>&1
echo "-- host links"; ip -br link | grep -E 'acbr0|veth|tap' 2>&1
echo "-- containerd log (last 2 min)"
journalctl -u containerd --since "2 min ago" --no-pager -o cat 2>&1 | cut -c1-300 | tail -50
echo "-- SELinux denials"; journalctl -b --no-pager 2>&1 | grep -i 'avc.*denied' | tail -3
echo "-- dmesg"; dmesg | tail -8

echo "== waiting for create to finish"
wait "$PID"; echo "create exit=$?"
cat "$OUT"; rm -f "$OUT"

echo "== after"
ps -eo pid,etimes,args | grep -iE 'qemu|kata-v2' | grep -v grep | cut -c1-200
"$CTL" --image "$IMAGE" --network destroy "$ID" 2>&1 | tail -2
