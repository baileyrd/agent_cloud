#!/usr/bin/env bash
# Smoke-test a running desktop agent (images/agent-desktop): display :0 on the dummy driver, the Xfce
# session, Chromium under its managed policy, and idle memory. Writes /workspace/policy.png (a
# screenshot of chrome://policy) for a look by eye.
# Usage: sudo scripts/desktop-smoke.sh AGENT   (create it first, e.g. ac-ctl --image
#        localhost/agent-desktop:dev --network create AGENT, and let it idle a minute for the memory figures)
# Env: AC_NAMESPACE (agents).
set -euo pipefail
[ "$(id -u)" -eq 0 ] || { echo "run as root" >&2; exit 2; }
AGENT=${1:?usage: $0 AGENT}
NS=${AC_NAMESPACE:-agents} CG=/sys/fs/cgroup/${AC_NAMESPACE:-agents}/kata_$AGENT
fail=0
ok() { echo "ok:   $*"; }
bad() { echo "FAIL: $*"; fail=1; }

# Never give `ctr task exec` stdin EOF while the guest process runs (ADR-006, Kata hazard).
HOLD=$(mktemp -u /tmp/ac-desktop-stdin.XXXXXX) && mkfifo -m 600 "$HOLD"
sleep 100000 > "$HOLD" &
HOLDER=$!
trap 'kill "$HOLDER" 2>/dev/null || true; rm -f "$HOLD"' EXIT
gx() { ctr -n "$NS" task exec --exec-id "dsk-$1-$$" "$AGENT" sh -c "$2" < "$HOLD" 2>&1; }

echo "== display"
if out=$(gx xrandr 'xrandr --query'); then
    echo "$out" | head -3
    echo "$out" | grep -q '^DUMMY0 connected' && ok "xrandr shows DUMMY0" || bad "no DUMMY0 in xrandr"
else
    bad "xrandr failed: $out"
    gx xlog 'tail -20 "$HOME/.local/share/xorg/Xorg.log"' || true
fi
gx ps 'ps -eo user,rss,comm' | grep -E 'Xorg|xfce4-session|xfwm4' || bad "Xorg or Xfce not running"
gx root 'ps -o user= -C Xorg' | grep -qx agent && ok "Xorg runs as agent" || bad "Xorg is not running as agent"

echo "== chromium policy"
gx policy 'cat /etc/chromium/policies/managed/agent.json' | grep -q '"ExtensionInstallBlocklist"' \
    && ok "policy file present" || bad "policy file missing"
if gx shot 'timeout 60 chromium --headless=new --window-size=1280,2400 --screenshot=/workspace/policy.png chrome://policy' > /tmp/ac-desktop-shot.$$ ; then
    ok "chromium ran headless; see /workspace/policy.png in $AGENT's workspace"
else
    bad "chromium failed:"; tail -15 /tmp/ac-desktop-shot.$$
fi
rm -f /tmp/ac-desktop-shot.$$

echo "== memory at idle"
gx free 'free -m' | head -2
if grep -q . "$CG/memory.current" 2>/dev/null; then
    echo "sandbox cgroup memory.current: $(( $(cat "$CG/memory.current") / 1048576 )) MiB (host side)"
else
    echo "note: no cgroup at $CG"
fi
echo "== image"
ctr -n "$NS" images ls | awk 'NR == 1 || /agent-desktop/'
exit "$fail"
