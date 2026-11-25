#!/bin/sh
# Build the agent images with podman and import them into containerd's agents namespace (on the host).
# Usage: scripts/build-images.sh [shell|desktop|all]   (default all; desktop builds on agent-shell:dev)
# Env: CLAUDE_CODE_VERSION (default latest), AC_NAMESPACE (agents).
set -eu
cd "$(dirname "$0")/.."
NS=${AC_NAMESPACE:-agents}
what=${1:-all}

build_shell() {
    podman build --build-arg "CLAUDE_CODE_VERSION=${CLAUDE_CODE_VERSION:-latest}" \
        -t agent-shell:dev -f images/agent-shell/Containerfile images/agent-shell
}
build_desktop() {
    podman build --build-arg BASE=localhost/agent-shell:dev \
        -t agent-desktop:dev -f images/agent-desktop/Containerfile images/agent-desktop
}
import() {
    podman save "localhost/$1:dev" | sudo ctr -n "$NS" images import -
    podman image ls --format '{{.Repository}}:{{.Tag}} {{.Size}}' "localhost/$1:dev"
}

case $what in
    shell) build_shell; import agent-shell ;;
    desktop) build_desktop; import agent-desktop ;;
    all) build_shell; build_desktop; import agent-shell; import agent-desktop ;;
    *) echo "usage: $0 [shell|desktop|all]" >&2; exit 2 ;;
esac
