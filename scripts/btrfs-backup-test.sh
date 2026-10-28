#!/bin/sh
# Back up and restore workspaces on real btrfs through `ac-ctl` (ADR-021): the data directory on a
# 256 MiB loopback btrfs, the backups (send stream files) on a 256 MiB loopback ext4, as on a
# mounted share; both removed after. Covers a full then an incremental send, chains and
# retention, a stream cut short, restore over a workspace (kept as a checkpoint) and into a
# purged one, a broken chain, and the refusals. Needs root, btrfs-progs, e2fsprogs and a kernel
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
for fs in data:btrfs dest:ext4; do
    name=${fs%%:*}
    truncate -s 256M "$tmp/$name.img"
    "mkfs.${fs#*:}" -q "$tmp/$name.img"
    mkdir "$tmp/$name"
    mount -o loop "$tmp/$name.img" "$tmp/$name"
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
# back <contents> [flags...]: write <contents>, back a1 up, note which backup holds what.
back() {
    contents=$1; shift
    echo "$contents" > "$ws/file"
    line=$(ac "$@" backup "$dest" a1)
    echo "$line" | cut -d' ' -f4 | sed "s/\$/ $contents/" >> "$tmp/held"
    echo "$line"
}
held() { grep "^$1 " "$tmp/held" | cut -d' ' -f2; }

mkdir -p "$data/workspaces"
btrfs -q subvolume create "$ws"
dd if=/dev/urandom of="$ws/blob" bs=1M count=16 status=none
chown -R 1000:1000 "$ws"

back one | grep -q '^a1 backed up .* (full, 16\.[0-9] MiB)$' || fail "first backup is full"
out=$(back two)
echo "$out" | grep -Eq '\(incremental from .*, [0-9.]+ (B|KiB)\)$' || fail "second is a small increment: $out"
[ "$(ls "$data/backups/a1" | wc -l)" -eq 1 ] || fail "one parent kept on this host"
[ "$(ac backups "$dest" | wc -l)" -eq 2 ] || fail "two backups listed"
[ "$(stat -c %a "$dest/a1")" = 700 ] || fail "the agent's directory is private"

# Chains of at most 3, keeping 3: never more than 3 + 2 streams, the newest always restorable.
for n in 3 4 5 6 7; do back "$n" --keep 3 --full-every 3 >/dev/null; done
streams=$(ls "$dest/a1" | wc -l)
[ "$streams" -le 5 ] || fail "retention keeps whole chains only: $streams streams"
[ "$(ac backups "$dest" a1 | wc -l)" -ge 3 ] || fail "at least the newest 3 restorable"
ls "$dest/a1" | grep -q '\.full\.btrfs$' || fail "a full one every 3"

# A stream cut short (a .part) is not listed, and the next backup deletes it.
echo half > "$dest/a1/20991231-235959.full.btrfs.part"
ac backups "$dest" a1 | grep -q 20991231 && fail "a partial stream is listed"
back 8 --keep 3 --full-every 3 >/dev/null
[ -e "$dest/a1/20991231-235959.full.btrfs.part" ] && fail "the partial stream is deleted"

# Restore over a broken workspace: the chain is replayed, the broken one kept as a checkpoint.
newest=$(ac backups "$dest" a1 | tail -n 1 | cut -d' ' -f2)
echo broken > "$ws/file"
out=$(ac restore-backup "$dest" a1)
echo "$out" | grep -q "restored backup $newest; the workspace it replaced is checkpoint" ||
    fail "restore: $out"
[ "$(cat "$ws/file")" = "$(held "$newest")" ] || fail "restored the newest"
[ "$(stat -c %u "$ws/blob")" = 1000 ] || fail "owner kept"
cmp -s "$ws/blob" "$data/backups/a1/$newest/blob" || fail "data intact"
echo hello > "$ws/new" || fail "the restored workspace is writable"
kept=$(ls "$data/checkpoints/a1" | grep -v '\.label$')
[ "$(cat "$data/checkpoints/a1/$kept/file")" = broken ] || fail "the replaced workspace is kept"

# Restore the oldest backup into a purged workspace (or a new host): nothing to keep.
btrfs -q subvolume delete "$data/checkpoints/a1/$kept"
btrfs -q subvolume delete "$ws"
oldest=$(ac backups "$dest" a1 | head -n 1 | cut -d' ' -f2)
ac restore-backup "$dest" a1 "$oldest" | grep -q 'as a new workspace' || fail "restore into nothing"
[ "$(cat "$ws/file")" = "$(held "$oldest")" ] || fail "restored the named backup"
ls "$data/workspaces" | grep -q '\.' && fail "no staging left: $(ls "$data/workspaces")"

# A broken chain: without its full stream, nothing after it is listed or restored.
full=$(ls "$dest/a1" | grep '\.full\.btrfs$' | tail -n 1)
after=$(ls "$dest/a1" | grep "after-${full%.full.btrfs}" | head -n 1 | cut -d. -f1)
rm "$dest/a1/$full"
ac backups "$dest" a1 | grep -q "${full%.full.btrfs}" && fail "a lost full stream is listed"
if [ -n "$after" ]; then
    expect 'no such backup' ac restore-backup "$dest" a1 "$after"
fi

# Refusals.
mkdir "$data/workspaces/plain"
expect 'plain directory' ac backup "$dest" plain
expect 'no such backup' ac restore-backup "$dest" a1 20000101-000000
expect 'is not a directory' ac backup "$tmp/nowhere" a1
expect 'no such backup of agent b9' ac restore-backup "$dest" b9
echo "ok: btrfs backups and restores on $(uname -r), $(btrfs --version), streams on ext4"
