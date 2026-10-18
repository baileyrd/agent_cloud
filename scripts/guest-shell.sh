#!/usr/bin/env bash
# Boot the Kata guest kernel+initrd into a bare shell (no kata-agent) with one
# virtio NIC, to see what the guest can load. Exit QEMU with Ctrl-a x.
set -euo pipefail
ver=$(uname -r)
first() { for f in "$@"; do [ -r "$f" ] && { echo "$f"; return; }; done; }
kernel=$(first "/usr/lib/modules/$ver/vmlinuz" "/boot/vmlinuz-$ver" || true)
initrd=$(first /var/cache/kata-containers/osbuilder-images/"$ver"/*.initrd \
  /var/cache/kata-containers/osbuilder-images/"$ver"/kata-containers-initrd.img \
  /var/cache/kata-containers/osbuilder-images/*/kata-containers-initrd.img || true)
if [ -z "$kernel" ] || [ -z "$initrd" ]; then
  echo "missing kernel='$kernel' initrd='$initrd' for $ver" >&2
  ls -l /usr/lib/modules/*/vmlinuz /boot/vmlinuz-* /var/cache/kata-containers/osbuilder-images/*/ 2>&1 >&2
  exit 1
fi
echo "kernel=$kernel initrd=$initrd"
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
