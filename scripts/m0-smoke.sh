#!/bin/sh
# M0 host check (runbook Phases 0-3). Read-only except for pulling and running a
# throwaway busybox container. Run as root on the Fedora host. [unverified]
set -u
fail=0
ok()   { printf 'ok    %s\n' "$1"; }
bad()  { printf 'FAIL  %s\n' "$1"; fail=1; }
check() { desc=$1; shift; if "$@" >/dev/null 2>&1; then ok "$desc"; else bad "$desc"; fi; }

check "CPU virtualization flags (vmx/svm)" grep -qE 'vmx|svm' /proc/cpuinfo
check "/dev/kvm present"                    test -c /dev/kvm
check "ctr installed"                       command -v ctr
check "containerd active"                   systemctl is-active --quiet containerd
check "kata-runtime installed"              command -v kata-runtime
check "kata-runtime check"                  kata-runtime check
check "containerd-shim-kata-v2 on PATH"     command -v containerd-shim-kata-v2

printf 'info  SELinux: %s\n' "$(getenforce 2>/dev/null || echo unknown)"

if [ "$fail" -ne 0 ]; then
    echo "Fix the failures above before the isolation test."
    exit 1
fi

NS=ac-smoke
IMG=docker.io/library/busybox:latest
ctr -n "$NS" image pull "$IMG" >/dev/null 2>&1 || { bad "pull $IMG"; exit 1; }
host=$(uname -r)
err=$(mktemp)
guest=$(ctr -n "$NS" run --rm --runtime io.containerd.kata.v2 "$IMG" ac-smoke-1 uname -r 2>"$err")
if [ -z "$guest" ]; then
    bad "kata run (see: ausearch -m avc -ts recent; do not disable SELinux first)"
    echo "---- ctr stderr ----"; cat "$err"; rm -f "$err"
    echo "---- recent SELinux denials ----"; ausearch -m avc -ts recent 2>/dev/null | tail -n 15
    echo "---- containerd log ----"; journalctl -u containerd -n 20 --no-pager 2>/dev/null
    exit 1
fi
rm -f "$err"
if [ "$guest" != "$host" ]; then
    ok "isolation: guest kernel $guest differs from host $host"
else
    bad "guest kernel equals host kernel ($host): not running under Kata"
fi
exit "$fail"
