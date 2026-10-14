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
sudo dnf install -y postgresql-server postgresql-contrib
sudo postgresql-setup --initdb && sudo systemctl enable --now postgresql
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
