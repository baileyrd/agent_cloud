#!/bin/sh
# Run the checkpoint adapter's test against real btrfs (ADR-018): a 256 MiB loopback btrfs,
# mounted for the test and removed after. Needs root, btrfs-progs and a kernel with btrfs.
# Usage: sudo scripts/btrfs-checkpoint-test.sh   (from anywhere; cargo must be on root's PATH)
set -eu
cd "$(dirname "$0")/.."
[ "$(id -u)" -eq 0 ] || { echo "run as root" >&2; exit 2; }
tmp=$(mktemp -d)
cleanup() {
    umount "$tmp/mnt" 2>/dev/null || true
    rm -rf "$tmp"
}
trap cleanup EXIT
truncate -s 256M "$tmp/img"
mkfs.btrfs -q "$tmp/img"
mkdir "$tmp/mnt"
mount -o loop "$tmp/img" "$tmp/mnt"
AC_BTRFS_TEST_DIR="$tmp/mnt" cargo test -p ac-ctr --locked -- --ignored --nocapture on_real_btrfs
