# Research notes

What the design is based on, and what is still unknown. Observations were made on
2026-10-14.

## Cloudroom core (davidondrej/cloudroom-core)

- Open-source Rust service, Apache-2.0, **pre-release**; "one Rust service per machine".
- HTTP API with sessions, prompts, SSE event streams; bearer token; listens on loopback.
- Agents run as a separate unprivileged Linux user; history is stored by the control plane (originally Postgres; see ADR-009).
- Setup guide targets Ubuntu 24.04 with systemd, cgroup v2, ext4 and filesystem quotas;
  default limit of 2 open harness sessions; a restart interrupts running work.
- Setup guide documents Codex and Pi; README also lists Claude Code and Cursor. Not
  verified which harnesses actually work.
- Optional desktop app (Apple Silicon Mac, Linux alpha); hosted version is invite-only.
- Not read: `docs/harnesses.md`, `install/storage.sh`, the source.

## ChatGPT Dots sandbox (observed from inside a dot)

| Probe | Result | Meaning |
|---|---|---|
| OS / resources | Debian 13, 9 vCPUs, about 10 GiB RAM, no swap, 32 GB disk | Per-agent slice |
| `systemd-detect-virt --container` | `container-other` | Container layer present |
| `systemd-detect-virt --vm` | `kvm` | Running in a KVM guest |
| `uname -r` | 6.18.44 | Custom guest kernel |
| `/proc/cmdline` | `systemd.unit=kata-containers.target`, `root=/dev/pmem0p1`, `agent.*` | **Kata Containers** |
| `/proc/version` | gcc 11.4 on Ubuntu 22.04 | Purpose-built guest kernel |
| `/dev/vd*`, `/dev/vsock` | absent | Not virtio-blk based |
| Desktop | Xfce (xfwm4), Chromium, `DISPLAY=:0`, no Wayland | X11 session |
| `xdpyinfo` | X.Org 21.1.16 | Upstream X server |
| `xrandr` | `DUMMY0` 1364x1024, `DUMMY1-15` disconnected | **Xorg dummy driver**; custom mode suggests viewer-driven resize |
| Chromium policy | Platform / Machine / Mandatory; autofill, audio capture, guest mode, Labs off | Machine-level managed policy; DevTools blocked |

Process lists inside both the shell and the desktop show no display server or viewer
process: the two environments are isolated from each other and the streaming backend is
not visible from inside.

## Viewer protocol: unknown

DevTools on the client showed the dot's computer view uses a WebSocket to
`codex-cloud-backend.chatgpt.com`, but it was blocked by a corporate web proxy before any
frames were exchanged. **Not determined:** VNC, WebRTC, DevTools-protocol screencast, or a
proprietary stream. Capture on a network where the socket connects, then read the first
frames (`RFB 003.008` means VNC; `Page.screencastFrame` means CDP screencast).

## Anthropic sandboxing (for context)

- Anthropic's docs list gVisor and VMs (Firecracker, QEMU) as strong isolation options.
- A third-party writeup says claude.ai's web sandbox uses gVisor; unconfirmed by Anthropic.
- Claude Code's local sandbox uses bubblewrap (Linux) and Seatbelt (macOS).

## Open unknowns

1. Fedora's Kata packaging and current install steps.
2. Whether SELinux needs policy work for Kata on this host.
3. Whether the host has KVM available (not yet checked).
4. Real per-agent RAM, CPU and disk numbers.
5. The Dots viewer protocol, and whether Dots' VMM is QEMU or Cloud Hypervisor.
