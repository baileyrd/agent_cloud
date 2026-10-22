#!/bin/sh
# Collect host specs and estimate how many agents fit, using docs/sizing.md numbers.
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

echo "== Capacity estimate (sizing.md: idle 2.2 GiB/0.2 core, active 5 GiB/1.5 core, disk 15 GB; host reserve 3 GiB + 1 core)"
mem_gib=$(awk '/MemTotal/ {printf "%.1f", $2/1048576}' /proc/meminfo)
cores=$(nproc)
free_gb=$(df -BG --output=avail "$WS" | tail -1 | tr -dc '0-9')
awk -v m="$mem_gib" -v c="$cores" -v d="$free_gb" 'BEGIN {
    usable = m - 3; if (usable < 0) usable = 0
    cpu = c - 1;    if (cpu < 0) cpu = 0
    act_ram = int(usable / 5);   act_cpu = int(cpu * 3 / 1.5)   # 3x CPU overcommit
    idle_ram = int(usable / 2.2); idle_cpu = int(cpu * 3 / 0.2)
    disk = int((d - 20) / 15); if (disk < 0) disk = 0
    printf "RAM %.1f GiB, %d cores, %d GB free for workspaces\n", m, c, d
    printf "active at once : %d  (RAM %d, CPU %d, disk %d)\n", min3(act_ram, act_cpu, disk), act_ram, act_cpu, disk
    printf "provisioned    : %d  (RAM %d, CPU %d, disk %d)\n", min3(idle_ram, idle_cpu, disk), idle_ram, idle_cpu, disk
}
function min3(a, b, c,   lo) { lo = a; if (b < lo) lo = b; if (c < lo) lo = c; return lo }'
