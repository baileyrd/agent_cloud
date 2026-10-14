# Architecture decision records

Status values: **accepted**, **proposed** (default until built and tested), **open**.

## ADR-001: Per-agent isolation with Kata Containers on KVM
**Status:** proposed.
**Context:** Agents run untrusted code without approval prompts. Containers share the host
kernel; full VMs are heavy to build and manage by hand.
**Decision:** One Kata microVM per agent, via containerd runtime class.
**Consequences:** Hardware isolation with a container workflow. Requires KVM. Kata's
Fedora packaging and SELinux interaction are **[unverified]**.
**Evidence:** ChatGPT Dots' sandbox boots under Kata on KVM ([research-notes.md](research-notes.md)).
**Alternative:** gVisor (no virtualization needed, weaker boundary, emulated kernel).

## ADR-002: Fedora Server as host
**Status:** accepted (owner's choice).
**Consequences:** SELinux enforcing by default; cgroup v2 by default. Cloudroom's Ubuntu-only
installer cannot be used directly.

## ADR-003: Do not run Cloudroom core as-is
**Status:** proposed.
**Context:** cloudroom-core is a pre-release Rust service (Apache-2.0) for running coding
harnesses with Postgres-backed history. Its installer assumes Ubuntu 24.04, systemd, ext4
filesystem quotas and `/etc/fstab`, and its docs put containers out of scope. Its README
claims Codex, Claude Code, Pi and Cursor; the setup guide documents Codex and Pi.
**Decision:** Borrow its API shape, session model and security ideas; build a thin runtime
and history layer instead. Keep one Cloudroom-in-a-full-VM instance as an optional fallback.
**Consequences:** More to build; no Ubuntu/quota porting.

## ADR-004: Pod shape
**Status:** open.
**Options:** (a) one container per agent running shell and desktop under a supervisor;
(b) two containers in one pod (shell, desktop) sharing a workspace volume.
**Leaning:** (a) first, since it needs no pod tooling (no `crictl runp` or k3s).
**Revisit when:** you need to restart or upgrade the desktop independently of the shell.

## ADR-005: Control-plane deployment shape
**Status:** open.
**Options:** one modular process (gateway, spawner, session store as modules with explicit
interfaces) versus separate services from the start.
**Leaning:** modular single process, extracting a service only for a concrete forcing
function (independent scaling, language boundary, fault isolation). Revisit if the
owner's microservices preference should override this default.

## ADR-006: Desktop streaming layer
**Status:** open.
**Context:** Dots' viewer protocol is unknown (the WebSocket was blocked by a network proxy
during inspection).
**Options:** KasmVNC (bundles X server, VNC and web client; simplest), Xvfb/x11vnc plus
noVNC, a WebRTC streamer such as Neko or Selkies (lower latency), ffmpeg `x11grab` into the
gateway (most control, most work).
**Leaning:** KasmVNC to get a working viewer, behind the gateway so clients use one
authenticated WSS. Note it brings its own X server; the dummy-driver Xorg design may be replaced.

## ADR-007: Egress control
**Status:** proposed.
**Decision:** Agents have no direct internet route; all traffic goes through an allowlist
proxy plus nftables rules on the host.
