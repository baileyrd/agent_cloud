# Install runbook

Run on the Fedora host over SSH. Each phase ends with a checkpoint; do not continue
until it passes. Package names and containerd config syntax **[unverified]** against
current upstream docs: the Kata install pages found while researching were older release
branches. Verify before copy-pasting.

## Phase 0: Prepare the host

```bash
sudo dnf upgrade -y && sudo reboot
grep -cE 'vmx|svm' /proc/cpuinfo && ls -l /dev/kvm
sudo dnf install -y qemu-kvm libvirt-daemon-kvm
sudo virt-host-validate qemu
```

**Checkpoint:** `/dev/kvm` exists and `virt-host-validate` passes the KVM and cgroup checks.
If the CPU flags are present but `/dev/kvm` is missing, enable VT-x / AMD-V (SVM) in the
BIOS. If the server is itself a VM, enable nested virtualization on the hypervisor
(Proxmox: CPU type `host`).

## Phase 1: Install containerd and Kata

```bash
sudo dnf install -y containerd
dnf search kata
sudo dnf install -y kata-containers   # if Fedora packages it
```

If the package is missing or stale, use the upstream release tarball:

```bash
sudo ln -sf /opt/kata/bin/containerd-shim-kata-v2 /usr/local/bin/
sudo ln -sf /opt/kata/bin/kata-runtime /usr/local/bin/
kata-runtime check
```

**Checkpoint:** `kata-runtime check` reports the system can run Kata.

## Phase 2: Wire containerd to Kata

```bash
sudo mkdir -p /etc/containerd
containerd config default | sudo tee /etc/containerd/config.toml >/dev/null
```

Add a runtime handler with `runtime_type = "io.containerd.kata.v2"`. The table path
differs between containerd 1.x and 2.x, so inspect the generated file before editing.

```bash
sudo systemctl enable --now containerd
```

## Phase 3: Prove the isolation

```bash
uname -r
sudo ctr image pull docker.io/library/busybox:latest
sudo ctr run --runtime io.containerd.kata.v2 --rm -t \
  docker.io/library/busybox:latest kata-test uname -r
```

**Checkpoint:** the two kernel versions differ. If SELinux blocks the run, read the
denials with `sudo ausearch -m avc -ts recent`; do not disable SELinux as a first fix.

## Phase 4: Build the agent images

Build with podman, import into containerd.

- **Desktop layer:** Xorg with the dummy driver (see below), Xfce, Chromium, the managed
  policy file, and the streaming layer chosen in [ADR-006](decisions.md).
- **Shell layer:** unprivileged user, agent harness, git, language toolchains.

```bash
podman build -t agent-desktop ./desktop
podman save agent-desktop | sudo ctr images import -
```

Minimal Xorg dummy config:

```
Section "Device"
    Identifier "dummy"
    Driver "dummy"
    VideoRam 256000
EndSection

Section "Screen"
    Identifier "screen0"
    Device "dummy"
    DefaultDepth 24
    SubSection "Display"
        Depth 24
        Virtual 1920 1080
    EndSubSection
EndSection
```

Chromium managed policy, at `/etc/chromium/policies/managed/agent.json`
(use `/etc/opt/chrome/policies/managed/` for Google Chrome):

```json
{
  "AutofillAddressEnabled": false,
  "AutofillCreditCardEnabled": false,
  "AudioCaptureAllowed": false,
  "BrowserGuestModeEnabled": false,
  "BrowserAddPersonEnabled": false,
  "BookmarkBarEnabled": false
}
```

Leave `DeveloperToolsAvailability` unset while developing so you can debug the agent's
browsing; set it to `2` for production images if you want to match the observed lockdown.

**Checkpoint:** run the desktop image under Kata; Xorg starts on `:0` and `xrandr` shows `DUMMY0`.

## Phase 5: Decide the pod shape

See [ADR-004](decisions.md). Start with one container per agent running both stacks under a
supervisor (supervisord or s6). Move to two containers in one pod only if you need the split.

## Phase 6: Control plane

```bash
curl -fsSL https://tailscale.com/install.sh | sh && sudo tailscale up
```

Build the gateway, spawner and session store (module boundaries in
[architecture.md](architecture.md)). Put the gateway behind HTTPS on the tailnet only.

## Phase 7: Lock it down

Work through the checklist in [security.md](security.md): egress rules, per-agent
CPU/memory/disk limits, secrets handling, no sudo and no containerd socket for the agent
user, backups.

## Phase 8: End-to-end test

1. Spawn one agent, send a prompt through the gateway, open the desktop stream, take over the
   mouse, then destroy it.
2. Confirm the workspace is cleaned up on destroy.
3. Confirm a second agent cannot see the first one's files.
4. Spawn five agents; record RAM and CPU to calibrate [sizing.md](sizing.md).

## Rough effort

Phases 0-3: an hour or two if KVM cooperates. Images and spawner: most of a weekend for a
first version. Phases 7-8 are what make it safe to leave running.

## Fedora 44 notes (verified on BaileyServ, i5-3550, 2026-10-18)

Phases 0-3 worked with these changes. Each was found by failure; `scripts/m0-smoke.sh` is the check.

1. **Enable VT-x in firmware.** Symptom: no `vmx` flag, no `/dev/kvm`. On the Gigabyte UEFI BIOS it is
   BIOS Features, Intel Virtualization Technology = Enabled; then cold-boot.
2. **Install Kata from Fedora:** `sudo dnf install -y kata-containers containerd`, then
   `sudo systemctl enable --now containerd`. Persist the vhost modules (one per line; `modprobe a b c`
   treats `b` and `c` as parameters, not modules):
   `printf 'vhost\nvhost_net\nvhost_vsock\n' | sudo tee /etc/modules-load.d/kata.conf`.
   Run `sudo kata-runtime check` (needs root).
3. **Cap guest memory on old CPUs.** Symptom: `qemu-system-x86_64: Address space limit ... phys-bits too low (36)`.
   Ivy Bridge CPUs address only 36 bits (64 GiB); Kata's default `default_maxmemory = 0` (all host RAM, plus
   hotplug room) overflows it. Copy the packaged config and cap it (it also bounds any one agent's growth):
   ```
   sudo mkdir -p /etc/kata-containers
   sudo cp /usr/share/kata-containers/defaults/configuration.toml /etc/kata-containers/configuration.toml
   sudo sed -i 's/^#\?default_maxmemory *=.*/default_maxmemory = 8192/' /etc/kata-containers/configuration.toml
   sudo sed -i 's/^#\?default_memory *=.*/default_memory = 4096/' /etc/kata-containers/configuration.toml   # Claude Code needs >= 4 GB
   ```
4. **Repair the guest initrd.** Symptom: `timed out connecting to vsock`; booting the initrd by hand shows
   `kata-agent: error while loading shared libraries: libseccomp.so.2`. Run `sudo scripts/fix-kata-initrd.sh`
   and re-run it after any kernel update that regenerates the initrd.
5. **The guest kernel equals the host kernel.** Fedora's package boots the host's own kernel image as the guest
   kernel, so `uname -r` matches. Proof of a VM is the `hypervisor` CPU flag and a fresh uptime, which the
   smoke test checks.

6. **Networked agents** need `sudo dnf install -y containernetworking-plugins` and the LAN-drop rule, installed
   as a boot-time unit in [Host services](#host-services). Forwarding and NAT come from the CNI bridge plugin.
7. **Developing on the host:** Fedora's `cargo` does not include clippy or rustfmt:
   `sudo dnf install -y clippy rustfmt`.

## Host services

Unit files and the rule live in [`deploy/`](../deploy). Install from the repo root.

**LAN-drop rule** (`ac-agentfw.service`): drops anything from the agent bridge `acbr0` to 10/8, 172.16/12,
192.168/16, 169.254/16 and 100.64/10 (the tailnet). It loads before containerd. It has no `ExecStop`, so
stopping the unit leaves the rule in place, and `restart` swaps it in a single transaction.

```bash
sudo install -D -m 0644 deploy/agentfw.nft /etc/agent-cloud/agentfw.nft
sudo install -m 0644 deploy/ac-agentfw.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now ac-agentfw
sudo nft list table inet agentfw          # expect the iifname "acbr0" ... drop rule
```

**Gateway** (`ac-gateway.service`): runs as root and reads its settings from `/etc/agent-cloud/gateway.env`.
`AC_BIND` must be loopback, RFC 1918 or a tailnet address. The unit strips `AC_ALLOW_PUBLIC_BIND`, so a
public bind is refused, and `IPAddressDeny`/`IPAddressAllow` drop traffic to or from public addresses as well.

```bash
cargo build --release -p ac-gateway
sudo install -m 0755 target/release/ac-gateway /usr/local/bin/
sudo install -m 0600 deploy/gateway.env /etc/agent-cloud/gateway.env      # edit AC_BIND / AC_IMAGE
sudo sh -c 'umask 077; head -c 32 /dev/urandom | base64 > /etc/agent-cloud/gateway.token'
sudo install -m 0644 deploy/ac-gateway.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now ac-gateway
TOKEN=$(sudo cat /etc/agent-cloud/gateway.token)
curl -sS -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8780/api/v1/agents/a1
```

The gateway locks the data directory (`/var/lib/agent-cloud`), so stop it (`sudo systemctl stop ac-gateway`)
before running `ac-ctl` against the same directory. Stopping the gateway leaves its agents running.

Debugging tip: to see why a guest dies, boot it directly with `-serial file:/tmp/serial.log`
(do not pipe the console through `sudo ... | tail`; the output is lost).
