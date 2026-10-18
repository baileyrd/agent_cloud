#!/usr/bin/env bash
# Boot the Kata guest kernel+initrd into a bare shell (no kata-agent) with one
# virtio NIC, to see what the guest can load. Exit QEMU with Ctrl-a x.
set -euo pipefail
ver=$(uname -r)
kernel=/usr/lib/modules/$ver/vmlinuz
initrd=/var/cache/kata-containers/osbuilder-images/$ver/kata-containers-initrd.img
[ -r "$kernel" ] && [ -r "$initrd" ] || { echo "missing kernel/initrd for $ver" >&2; exit 1; }
cat <<'TXT'
In the guest shell try:
  mount -t proc proc /proc; mount -t sysfs sys /sys
  lsmod; ls /sys/class/net; ls /sys/bus/virtio/devices
  modprobe -v virtio_net || insmod $(find /lib/modules /usr/lib/modules -name 'virtio_net.ko*' | head -1)
  ls /sys/class/net; dmesg | tail -20
TXT
exec qemu-system-x86_64 -machine q35,accel=kvm -cpu host -m 1024 -smp 1 -nographic \
  -kernel "$kernel" -initrd "$initrd" \
  -append "console=ttyS0 rdinit=/usr/bin/sh panic=1" \
  -netdev user,id=n0 -device virtio-net-pci,netdev=n0
