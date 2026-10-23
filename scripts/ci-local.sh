#!/bin/sh
# The checks CI runs (.github/workflows/ci.yml calls this script, so local and CI cannot drift).
# Usage: scripts/ci-local.sh rust|scripts   (run from anywhere; it works from the repo root)
set -eu
cd "$(dirname "$0")/.."

rust() {
    export RUSTFLAGS="-D warnings"
    cargo fmt --check
    cargo clippy --workspace --all-targets --locked
    cargo test --workspace --locked
}

# A file whose first line mentions bash is checked with bash -n, the rest with sh -n (a sourced
# file without a shebang is sh). CI's sh is dash; Fedora's is bash, which accepts bashisms, so
# use dash when it is installed and say so when it is not.
scripts() {
    posix=sh
    if command -v dash >/dev/null; then posix=dash
    else echo "warning: no dash; sh -n here is $(readlink -f "$(command -v sh)"), laxer than CI's dash" >&2
    fi
    rc=0
    for f in scripts/*.sh; do
        if head -1 "$f" | grep -q bash; then shell=bash; else shell=$posix; fi
        $shell -n "$f" || { echo "syntax error ($shell -n): $f" >&2; rc=1; }
    done
    [ "$rc" -eq 0 ] && echo "scripts: $(ls scripts/*.sh | wc -l) files pass"
    return "$rc"
}

case "${1:-}" in
    rust) rust ;;
    scripts) scripts ;;
    *) echo "usage: $0 rust|scripts" >&2; exit 2 ;;
esac
