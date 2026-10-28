#!/bin/sh
# Back up and restore workspaces on real btrfs through `ac-ctl` (ADR-021): two 256 MiB loopback
# btrfs filesystems, one for the data directory and one for the backups, removed after. Covers a
# full then an incremental send, retention, a backup cut short, restore over a workspace (kept as
# a checkpoint) and into a purged one, and the refusals. Needs root, btrfs-progs and a kernel
# with btrfs; no containerd or Kata.
# Usage: sudo scripts/btrfs-backup-test.sh   (from anywhere; cargo must be on root's PATH)
set -eu
cd "$(dirname "$0")/.."
[ "$(id -u)" -eq 0 ] || { echo "run as root" >&2; exit 2; }
cargo build -q --locked -p ac-ctl
ctl="$PWD/target/debug/ac-ctl"
tmp=$(mktemp -d)
cleanup() {
    umount "$tmp/data" "$tmp/dest" 2>/dev/null || true
    rm -rf "$tmp"
}
trap cleanup EXIT
for fs in data dest; do
    truncate -s 256M "$tmp/$fs.img"
    mkfs.btrfs -q "$tmp/$fs.img"
    mkdir "$tmp/$fs"
    mount -o loop "$tmp/$fs.img" "$tmp/$fs"
done
data="$tmp/data" dest="$tmp/dest"
ws="$data/workspaces/a1"
ac() { "$ctl" --data "$data" "$@"; }
fail() { echo "FAIL: $*" >&2; exit 1; }
expect() { # expect <pattern> <command...>: the command fails and says <pattern>
    pattern=$1; shift
    if out=$("$@" 2>&1); then fail "$* succeeded: $out"; fi
    echo "$out" | grep -q -- "$pattern" || fail "$*: wanted '$pattern', got: $out"
}

mkdir -p "$data/workspaces"
btrfs -q subvolume create "$ws"
echo one > "$ws/file"
dd if=/dev/urandom of="$ws/blob" bs=1M count=16 status=none
chown -R 1000:1000 "$ws"

ac backup "$dest" | grep -q '^a1 backed up .* (full)$' || fail "first backup is full"
echo two > "$ws/file"
out=$(ac backup "$dest")
echo "$out" | grep -q '(incremental from ' || fail "second backup is incremental: $out"
[ "$(ls "$data/backups/a1" | wc -l)" -eq 1 ] || fail "one parent kept on this host"
[ "$(ac backups "$dest" | wc -l)" -eq 2 ] || fail "two backups listed"
first=$(ac backups "$dest" a1 | head -n 1 | cut -d' ' -f2)
[ "$(cat "$dest/a1/$first/file")" = one ] || fail "first backup holds the first contents"
[ "$(stat -c %u "$dest/a1/$first/file")" = 1000 ] || fail "owner kept"
cmp -s "$ws/blob" "$dest/a1/$first/blob" || fail "data intact"
if touch "$dest/a1/$first/x" 2>/dev/null; then fail "backups are read-only"; fi

# Retention: four more with --keep 3 leaves the newest three, in order, even within a second.
for n in 3 4 5 6; do
    echo "$n" > "$ws/file"
    ac --keep 3 backup "$dest" a1 >/dev/null
done
[ "$(ac backups "$dest" a1 | wc -l)" -eq 3 ] || fail "retention keeps 3"
kept_contents=$(for b in $(ac backups "$dest" a1 | cut -d' ' -f2); do cat "$dest/a1/$b/file"; done)
[ "$(echo $kept_contents)" = "4 5 6" ] || fail "the newest three, oldest first: $kept_contents"

# A backup cut short (a writable subvolume) is not listed, and the next backup deletes it.
btrfs -q subvolume create "$dest/a1/20991231-235959"
ac backups "$dest" a1 | grep -q 20991231 && fail "a partial backup is listed"
ac --keep 3 backup "$dest" a1 >/dev/null
[ -e "$dest/a1/20991231-235959" ] && fail "the partial backup is deleted"

# Restore over a broken workspace: it comes back, the broken one is kept as a checkpoint.
newest=$(ac backups "$dest" a1 | tail -n 1 | cut -d' ' -f2)
echo broken > "$ws/file"
out=$(ac restore-backup "$dest" a1)
echo "$out" | grep -q "restored backup $newest; the workspace it replaced is checkpoint" ||
    fail "restore: $out"
[ "$(cat "$ws/file")" = 6 ] || fail "restored the newest"
echo hello > "$ws/new" || fail "the restored workspace is writable"
kept=$(ls "$data/checkpoints/a1" | grep -v '\.label$')
[ "$(cat "$data/checkpoints/a1/$kept/file")" = broken ] || fail "the replaced workspace is kept"

# Restore a named backup into a purged workspace (or a new host): nothing to keep.
btrfs -q subvolume delete "$data/checkpoints/a1/$kept"
btrfs -q subvolume delete "$ws"
oldest=$(ac backups "$dest" a1 | head -n 1 | cut -d' ' -f2)
ac restore-backup "$dest" a1 "$oldest" | grep -q 'as a new workspace' || fail "restore into nothing"
[ "$(cat "$ws/file")" = 5 ] || fail "restored the named backup"
ls "$data/workspaces" | grep -q '\.' && fail "no staging left: $(ls "$data/workspaces")"

# Refusals.
mkdir "$data/workspaces/plain"
expect 'plain directory' ac backup "$dest" plain
expect 'no such backup' ac restore-backup "$dest" a1 20000101-000000
mkdir "$tmp/notbtrfs"
expect 'is not on btrfs' ac backup "$tmp/notbtrfs" a1
expect 'no such backup of agent b9' ac restore-backup "$dest" b9
echo "ok: btrfs backups and restores on $(uname -r), $(btrfs --version)"
