#!/bin/sh
# Fedora's kata-containers package builds a guest initrd whose kata-agent can miss
# shared libraries (seen: libseccomp.so.2 on Fedora 44, kata-containers 3.26.0).
# The VM boots, the agent dies, and Kata reports "timed out connecting to vsock".
# This adds every library the agent needs from the host into the initrd. Idempotent;
# the original is kept as <initrd>.orig. Re-run after a kernel update regenerates it.
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
find . | cpio -o -H newc --quiet | gzip -9 > "$INITRD.new"
mv "$INITRD.new" "$INITRD"
echo "rebuilt: $INITRD"
