#!/usr/bin/env bash
# Smoke-test a running desktop agent (images/agent-desktop): display :0 on the dummy driver, the Xfce
# session, Chromium's effective managed policy, and idle memory against the 2 GiB target. Exits non-zero
# unless every check passes. Writes /workspace/policy.png (a screenshot of chrome://policy) for a look by eye.
# Usage: sudo scripts/desktop-smoke.sh AGENT   (create it first, e.g. ac-ctl --image
#        localhost/agent-desktop:dev --network create AGENT, and let it idle a minute for the memory figures)
# Env: AC_NAMESPACE (agents).
set -euo pipefail
[ "$(id -u)" -eq 0 ] || { echo "run as root" >&2; exit 2; }
AGENT=${1:?usage: $0 AGENT}
NS=${AC_NAMESPACE:-agents} CG=/sys/fs/cgroup/${AC_NAMESPACE:-agents}/kata_$AGENT
LIMIT_MIB=2048 POLICIES="ExtensionInstallBlocklist DeveloperToolsAvailability RemoteDebuggingAllowed DownloadDirectory"
fail=0
ok() { echo "ok:   $*"; }
bad() { echo "FAIL: $*"; fail=1; }

# Never give `ctr task exec` stdin EOF while the guest process runs (ADR-006, Kata hazard).
WORK=$(mktemp -d) HOLDER=
trap 'if [ -n "$HOLDER" ]; then kill "$HOLDER" 2>/dev/null || true; fi; rm -rf "$WORK"' EXIT
HOLD=$WORK/stdin && mkfifo -m 600 "$HOLD"
sleep 100000 > "$HOLD" &
HOLDER=$!
gx() { ctr -n "$NS" task exec --exec-id "dsk-$1-$$" "$AGENT" sh -c "$2" < "$HOLD" 2>&1; }

echo "== display"
if out=$(gx xrandr 'xrandr --query'); then
    echo "$out" | head -3
    echo "$out" | grep -q '^DUMMY0 connected' && ok "xrandr shows DUMMY0" || bad "no DUMMY0 in xrandr"
else
    bad "xrandr failed: $out"
    gx xlog 'tail -20 "$HOME/.local/share/xorg/Xorg.log"' || true
fi
runs_as_agent() { gx "ps-$1" "ps -o user= -C $1" | grep -qx agent && ok "$1 runs as agent" || bad "$1 is not running as agent"; }

echo "== session"
gx ps 'ps -eo user,rss,comm' | grep -E 'Xorg|xfce4-session|xfwm4' || true
for p in Xorg xfce4-session xfwm4; do runs_as_agent "$p"; done

echo "== chromium policy"
gx policy 'cat /etc/chromium/policies/managed/agent.json' | grep -q '"ExtensionInstallBlocklist"' \
    && ok "policy file present" || bad "policy file missing"
# The effective policy is what Chromium itself reports on chrome://policy, not what the file says.
if gx dom 'timeout 60 chromium --headless=new --virtual-time-budget=5000 --dump-dom chrome://policy' > "$WORK/policy.html"; then
    for k in $POLICIES; do
        grep -q "$k" "$WORK/policy.html" && ok "effective policy: $k" || bad "chrome://policy does not show $k"
    done
else
    bad "chromium failed:"; tail -15 "$WORK/policy.html"
fi
gx shot 'timeout 60 chromium --headless=new --window-size=1280,2400 --screenshot=/workspace/policy.png chrome://policy' > "$WORK/shot" \
    && echo "see /workspace/policy.png in $AGENT's workspace" || { echo "note: screenshot failed"; tail -5 "$WORK/shot"; }

echo "== memory at idle"
under() { [ "$2" -le "$LIMIT_MIB" ] && ok "$1 ${2} MiB <= ${LIMIT_MIB} MiB" || bad "$1 ${2} MiB > ${LIMIT_MIB} MiB"; }
mem=$(gx free 'free -m') || true
echo "$mem" | head -2
used=$(echo "$mem" | awk '/^Mem:/ { print $3 }')
[ -n "$used" ] && under "guest used" "$used" || bad "no guest memory figure"
if grep -q . "$CG/memory.current" 2>/dev/null; then
    under "sandbox cgroup memory.current" "$(( $(cat "$CG/memory.current") / 1048576 ))"
else
    bad "no cgroup at $CG"
fi
echo "== image"
ctr -n "$NS" images ls | awk 'NR == 1 || /agent-desktop/'
[ "$fail" -eq 0 ] && echo "PASS: session, effective policy and idle memory" || echo "NOT PASSED: see FAIL lines"
exit "$fail"
