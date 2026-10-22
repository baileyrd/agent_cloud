#!/bin/sh
# Collect host specs and estimate how many agents fit, using the measured model in docs/sizing.md.
# Read-only. Usage: scripts/capacity.sh [workspace-dir]   (default /var/lib/agent-cloud)
set -u
WS=${1:-/var/lib/agent-cloud}
[ -d "$WS" ] || WS=/

echo "== CPU"
lscpu | grep -E 'Model name|^CPU\(s\)|Thread|Core|Socket|MHz|L3|Virtualization|Flags' | sed -E 's/(Flags:).*/\1 (see below)/'
printf 'vmx/svm flag count: %s\n' "$(grep -cE 'vmx|svm' /proc/cpuinfo)"
printf 'ept/npt: %s\n' "$(grep -m1 -oE 'ept|npt' /proc/cpuinfo | head -1)"
echo "== Memory"
free -h | sed -n 1,3p
dmidecode -t memory 2>/dev/null | grep -E 'Size:|Type:|Speed:|Maximum Capacity|Number Of Devices' | grep -v 'No Module' | sort | uniq -c
echo "== Storage"
lsblk -d -o NAME,SIZE,ROTA,TRAN,MODEL
df -h "$WS" | tail -1
src=$(df --output=source "$WS" | tail -1)
if [ "$(lsblk -no ROTA "$src" 2>/dev/null | head -1 | tr -d ' ')" = "1" ]; then
    echo "WARNING: $WS is on a rotational disk ($src). Expect slow VM boots and builds; use an SSD for /var/lib/containerd and the data dir."
fi
echo "== System"
printf 'kernel %s | virt: %s | selinux: %s | /dev/kvm: %s\n' "$(uname -r)" "$(systemd-detect-virt)" \
    "$(getenforce 2>/dev/null || echo n/a)" "$([ -c /dev/kvm ] && echo yes || echo NO)"
. /etc/os-release 2>/dev/null && echo "$PRETTY_NAME"

# Measured per-agent model (docs/sizing.md, 2026-10-22): a running agent ends up holding its guest's
# default_memory + 0.15 GiB (guests never return memory), all of it in /dev/shm; ~0.05 core idle, one
# core per vCPU when busy. Host reserve 4 GiB and 1 core.
kata=/etc/kata-containers/configuration.toml
vm_mib=$(sed -n 's/^default_memory = \([0-9]*\)$/\1/p' "$kata" 2>/dev/null | head -n 1)
vcpus=$(sed -n 's/^default_vcpus = \([0-9]*\)$/\1/p' "$kata" 2>/dev/null | head -n 1)
echo "== Capacity estimate (sizing.md model; default_memory ${vm_mib:-2048} MiB, default_vcpus ${vcpus:-1})"
mem_gib=$(awk '/MemTotal/ {printf "%.1f", $2/1048576}' /proc/meminfo)
shm_gib=$(df -k --output=size /dev/shm | tail -1 | awk '{printf "%.1f", $1/1048576}')
cores=$(nproc)
awk -v m="$mem_gib" -v shm="$shm_gib" -v c="$cores" -v vm="${vm_mib:-2048}" -v v="${vcpus:-1}" 'BEGIN {
    per = vm / 1024 + 0.15
    ram = int((m - 4) / per); if (ram < 0) ram = 0
    sh = int(shm / (vm / 1024))
    busy = int((c - 1) / v); if (busy < 0) busy = 0
    printf "RAM %.1f GiB, /dev/shm %.1f GiB, %d cores\n", m, shm, c
    printf "running agents : %d  (RAM %d, /dev/shm %d; idle CPU is not the limit)\n", (ram < sh ? ram : sh), ram, sh
    printf "CPU-busy at once: %d  (rotational disk: 2, see sizing.md)\n", busy
}'
