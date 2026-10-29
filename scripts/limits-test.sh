#!/usr/bin/env bash
# Check per-agent limits (ADR-023) on a running agent: the sandbox cgroup's cpu.max and memory.max,
# throttling under a CPU load from inside the guest, and the workspace's btrfs quota refusing a write
# past it. The disk test sets a small temporary limit and puts the configured one back after.
# Usage: sudo scripts/limits-test.sh [--enable-quota] AGENT
#   --enable-quota  first run `btrfs quota enable --simple` on the workspace filesystem (once per
#                   filesystem; undo with `btrfs quota disable <mount>`).
# Env: AC_DATA (default /var/lib/agent-cloud), AC_NAMESPACE (agents), AC_GATEWAY_ENV
# (/etc/agent-cloud/gateway.env, read for AC_AGENT_DISK_GIB), LOAD_SECS (15).
set -euo pipefail
[ "$(id -u)" -eq 0 ] || { echo "run as root" >&2; exit 2; }
enable=0
[ "${1:-}" = --enable-quota ] && { enable=1; shift; }
AGENT=${1:?usage: $0 [--enable-quota] AGENT}
DATA=${AC_DATA:-/var/lib/agent-cloud} NS=${AC_NAMESPACE:-agents}
ENVFILE=${AC_GATEWAY_ENV:-/etc/agent-cloud/gateway.env} LOAD_SECS=${LOAD_SECS:-15}
WS=$DATA/workspaces/$AGENT CG=/sys/fs/cgroup/$NS/kata_$AGENT
fail=0
ok() { echo "ok:   $*"; }
bad() { echo "FAIL: $*"; fail=1; }

HOLD=$(mktemp -u /tmp/ac-limits-stdin.XXXXXX) && mkfifo -m 600 "$HOLD"
sleep 100000 > "$HOLD" &
HOLDER=$!
cleanup() { kill "$HOLDER" 2>/dev/null || true; rm -f "$HOLD"; }
trap cleanup EXIT
gx() { ctr -n "$NS" task exec --exec-id "lim-$1-$$" "$AGENT" sh -c "$2" < "$HOLD" 2>&1; }

echo "== cgroup $CG"
[ -s "$CG/cgroup.procs" ] && ok "holds $(wc -l < "$CG/cgroup.procs") process(es)" || bad "no processes in $CG"
for f in cpu.max memory.max memory.current memory.peak; do
    printf '%-15s %s\n' "$f" "$(cat "$CG/$f" 2>&1)"
done
grep -E '^(oom|oom_kill|max) ' "$CG/memory.events" 2>/dev/null | tr '\n' ' '; echo

echo "== cpu: $LOAD_SECS s of 4 busy loops in the guest"
stat() { awk -v k="$1" '$1 == k { print $2 }' "$CG/cpu.stat"; }
u0=$(stat usage_usec) t0=$(stat nr_throttled)
gx cpu "for i in 1 2 3 4; do timeout $LOAD_SECS sh -c 'while :; do :; done' & done; wait" > /dev/null || true
u1=$(stat usage_usec) t1=$(stat nr_throttled)
cores=$(awk -v u=$((u1 - u0)) -v s="$LOAD_SECS" 'BEGIN { printf "%.2f", u / s / 1e6 }')
echo "used $cores cores; throttled $((t1 - t0)) periods"
case $(cut -d' ' -f1 "$CG/cpu.max") in
    max) echo "cpu.max is max: nothing to throttle" ;;
    *) [ $((t1 - t0)) -gt 0 ] && ok "cpu.max throttled the sandbox" || bad "no throttling under load" ;;
esac

echo "== disk: $WS"
fs=$(findmnt -no TARGET -T "$WS")
if [ "$enable" -eq 1 ]; then
    btrfs quota enable --simple "$fs" && ok "simple quotas enabled on $fs"
fi
if ! btrfs qgroup show -re "$WS" > /dev/null 2>&1; then
    bad "quotas are off on $fs (rerun with --enable-quota)"
    exit 1
fi
btrfs qgroup show -re --mbytes -f "$WS"
configured=$(sed -n 's/^AC_AGENT_DISK_GIB=//p' "$ENVFILE" 2>/dev/null | tail -1)
used=$(btrfs qgroup show -re --raw -f "$WS" | awk 'NR > 2 { print $2 }')
trial=$((used + 256 * 1024 * 1024))
btrfs qgroup limit "$trial" "$WS"
out=$(gx disk 'dd if=/dev/urandom of=/workspace/.ac-limit-test bs=1M count=512 conv=fsync 2>&1; echo rc=$?')
gx rm 'rm -f /workspace/.ac-limit-test' > /dev/null || true
btrfs qgroup limit "${configured:+${configured}G}${configured:-none}" "$WS"
echo "$out" | tail -3
case $out in
    *"quota exceeded"*|*"Disk quota"*) ok "a write past the quota was refused" ;;
    *) bad "512 MiB went past a limit 256 MiB above use" ;;
esac
echo "limit back to: ${configured:+${configured} GiB}${configured:-none}"
exit "$fail"
