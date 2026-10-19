#!/bin/sh
# Fedora's kata-containers package builds a guest initrd whose kata-agent can miss
# shared libraries (seen: libseccomp.so.2 on Fedora 44, kata-containers 3.26.0).
# The VM boots, the agent dies, and Kata reports "timed out connecting to vsock".
# This adds every library the agent needs from the host into the initrd. It also adds the
# guest network driver (virtio_net and its dependencies, loadable modules on Fedora) so a VM
# with a network card can configure eth0; without it the shim retries update_interface forever.
# The initrd has no udev and nothing that reads kata-modules.conf, so the modules are also
# decompressed (busybox insmod cannot read .xz) and insmod'ed by a kata-agent.service drop-in.
# Idempotent; the original is kept as <initrd>.orig. Re-run after a kernel update regenerates it.
# Run as root. [verified once: BaileyServ, Fedora 44]
set -eu
[ "$(id -u)" -eq 0 ] || { echo "run as root" >&2; exit 1; }
AGENT=/usr/libexec/kata-containers/agent/usr/bin/kata-agent
INITRD=$(readlink -f /var/cache/kata-containers/kata-containers-initrd.img)
[ -e "$AGENT" ] || { echo "missing $AGENT" >&2; exit 1; }
[ -e "$INITRD.orig" ] || cp "$INITRD" "$INITRD.orig"
W=$(mktemp -d /var/tmp/kata-initrd.XXXXXX)
trap 'rm -rf "$W"' EXIT
cd "$W"
zcat "$INITRD.orig" | cpio -idm --quiet
LIBDIR=$(dirname "$(find usr -name libc.so.6 | head -1)")
echo "libs go in: $LIBDIR"
for lib in $(ldd "$AGENT" | awk '/=> \//{print $3}'); do
    [ -e "$LIBDIR/$(basename "$lib")" ] || { echo "adding $(basename "$lib")"; cp -L "$lib" "$LIBDIR/$(basename "$lib")"; }
done

# Guest network driver modules, copied from the host kernel tree into the initrd's.
KVER=$(ls usr/lib/modules | head -1)
changed=0
for m in virtio_net net_failover failover; do
    src=$(modinfo -k "$KVER" -n "$m" 2>/dev/null || true)
    case "$src" in /*) ;; *) echo "module $m: built in or not found, skipping"; continue ;; esac
    rel=${src#/usr/lib/modules/$KVER/}; rel=${rel#/lib/modules/$KVER/}
    if [ -e "usr/lib/modules/$KVER/$rel" ]; then echo "module $m: already in initrd"; continue; fi
    echo "adding module $m"
    mkdir -p "usr/lib/modules/$KVER/$(dirname "$rel")"
    cp -L "$src" "usr/lib/modules/$KVER/$rel"
    changed=1
done
# Kept for initrds that do process this file; the drop-in below is what loads them on Fedora.
conf=etc/modules-load.d/kata-modules.conf
mkdir -p etc/modules-load.d
[ -f "$conf" ] && sed -i -e '$a\' "$conf"
if ! grep -qx virtio_net "$conf" 2>/dev/null; then
    echo "modules-load: adding virtio_net to $conf"
    echo virtio_net >> "$conf"
    changed=1
else
    echo "modules-load: virtio_net already listed"
fi
if [ "$changed" -eq 1 ]; then
    command -v depmod >/dev/null || { echo "depmod not found (install kmod)" >&2; exit 1; }
    depmod -b "$W" "$KVER"
fi

# Plain .ko copies + a drop-in that loads them, in dependency order, before the agent starts.
KO=usr/lib/kata-net
mkdir -p "$KO" usr/lib/systemd/system/kata-agent.service.d
dropin=usr/lib/systemd/system/kata-agent.service.d/10-net-modules.conf
echo "[Service]" > "$dropin"
for m in failover net_failover virtio_net; do
    src=$(modinfo -k "$KVER" -n "$m" 2>/dev/null || true)
    case "$src" in /*) ;; *) continue ;; esac
    case "$src" in
        *.xz)  xz -dc "$src" > "$KO/$m.ko" ;;
        *.zst) zstd -dc "$src" > "$KO/$m.ko" ;;
        *)     cp -L "$src" "$KO/$m.ko" ;;
    esac
    # "-": an already-loaded module (EEXIST) must not stop the agent.
    echo "ExecStartPre=-/usr/bin/insmod /usr/lib/kata-net/$m.ko" >> "$dropin"
done
echo "load drop-in:"; cat "$dropin"

find . | cpio -o -H newc --quiet | gzip -9 > "$INITRD.new"
mv "$INITRD.new" "$INITRD"
echo "rebuilt: $INITRD"
