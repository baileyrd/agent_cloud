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
id="ac-smoke-$$"
err=$(mktemp)
# Fedora's Kata package boots the host's own kernel image as the guest kernel, so
# the versions can match. A VM is proven by the "hypervisor" CPU flag and a fresh uptime.
# Stdin is /dev/null: EOF from a pipe or file would wedge the Kata shim (install.md, Fedora note 8).
out=$(ctr -n "$NS" run --rm --runtime io.containerd.kata.v2 "$IMG" "$id" \
    sh -c 'uname -r; grep -c hypervisor /proc/cpuinfo; cut -d. -f1 /proc/uptime' </dev/null 2>"$err")
guest=$(echo "$out" | sed -n 1p)
hv=$(echo "$out" | sed -n 2p)
guest_up=$(echo "$out" | sed -n 3p)
host_up=$(cut -d. -f1 /proc/uptime)
if [ -z "$guest" ]; then
    bad "kata run (see: ausearch -m avc -ts recent; do not disable SELinux first)"
    echo "---- ctr stderr ----"; cat "$err"; rm -f "$err"
    echo "---- recent SELinux denials ----"; ausearch -m avc -ts recent 2>/dev/null | tail -n 15
    echo "---- containerd log ----"; journalctl -u containerd -n 20 --no-pager 2>/dev/null
    exit 1
fi
rm -f "$err"
if [ "${hv:-0}" -gt 0 ] && [ "${guest_up:-999999}" -lt "$host_up" ]; then
    ok "runs in a VM: hypervisor flag set, guest up ${guest_up}s vs host ${host_up}s (guest kernel $guest, host $host)"
else
    bad "does not look like a VM (hypervisor flag count '${hv:-?}', guest up '${guest_up:-?}'s, host up ${host_up}s)"
fi
exit "$fail"
