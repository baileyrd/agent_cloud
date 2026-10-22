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
2. Confirm the workspace survives `DELETE /api/v1/agents/{id}` and is removed by `DELETE ...?purge=true`
   (checked on BaileyServ 2026-10-19: destroy, re-create, file still there; purge, directory gone).
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

6. **Networked agents** need `sudo dnf install -y containernetworking-plugins`, the agent firewall and the
   egress proxy, both installed as boot-time units in [Host services](#host-services).
7. **Developing on the host:** Fedora's `cargo` does not include clippy or rustfmt:
   `sudo dnf install -y clippy rustfmt`.
8. **Never pipe or redirect a file into `ctr task exec` (or an attached `ctr run`) against a live agent.**
   Without `-t`, `ctr` never passes stdin EOF to the guest, and when the host side of a pipe or file reaches
   EOF while the guest process lives, the Kata shim blocks every other call on that task: new execs fail with
   `context deadline exceeded` and the agent drops out of `ctr task ls`. SIGTERM does not help (`ctr` forwards
   it through the stuck shim); SIGKILL of that `ctr` client does. **`< /dev/null` is not safe either**: its EOF
   comes at once, while the guest process still runs, so every such exec is a race (measured 2026-10-21: 125
   execs of `sh -c 'sleep 0.2; echo x' </dev/null` and the 126th wedged a1; 200 with stdin held open did not).
   Hold stdin open instead, from a FIFO that a background `sleep` keeps open:
   `mkfifo /tmp/hold; sleep 100000 > /tmp/hold & ctr ... task exec ... < /tmp/hold` (kill the `sleep` once no
   exec runs). The gateway's `exec_once` holds a pipe open the same way, and `scripts/post-reboot-check.sh` uses
   the FIFO. To run a script, copy it into the workspace and run it there. (Measured 2026-10-20: `printf x |
   ctr task exec ... cat` wedged; this caused an earlier a1 freeze the same day.)

## Host services

Unit files and the rule live in [`deploy/`](../deploy). Install from the repo root.

**Agent firewall** (`ac-agentfw.service`, ADR-007): from the agent bridge `acbr0`, the host accepts only TCP to
the egress proxy (10.89.0.1:3128); everything else is rejected, and nothing is forwarded to or from the bridge.
Private ranges (10/8, 172.16/12, 192.168/16, 169.254/16, 100.64/10) are also dropped explicitly. A `bridge`
table drops frames switched between two agents' ports, which the `inet` hooks never see (no `br_netfilter`), so
agents cannot reach each other. It loads before containerd, has no `ExecStop` (stopping the unit leaves the rules
in place), and `reload`/`restart` replace both tables in one transaction, so reloading never duplicates rules.

firewalld filters the same packets after these rules, so it must accept the proxy port too. The `agents` zone
(`deploy/firewalld-agents.xml`) binds `acbr0` and accepts only TCP 3128. firewalld matches the interface by name,
so the binding applies when the first CNI ADD creates the bridge, with nothing to re-run. Without the zone,
`acbr0` falls into the default zone, and agents reach the proxy only if that zone happens to allow 3128
(Fedora Workstation's allows TCP 1025-65535).

```bash
sudo install -D -m 0644 deploy/agentfw.nft /etc/agent-cloud/agentfw.nft
sudo install -m 0644 deploy/ac-agentfw.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now ac-agentfw
sudo nft list table inet agentfw          # expect input: ... tcp dport 3128 accept, then reject
sudo nft list table bridge agentfw        # expect forward: meta ibrname "acbr0" ... drop
sudo install -m 0644 deploy/firewalld-agents.xml /etc/firewalld/zones/agents.xml
sudo firewall-cmd --reload && sudo firewall-cmd --zone=agents --list-all   # interfaces: acbr0, ports: 3128/tcp
```

**Egress proxy** (`ac-egress.service`, ADR-007): the agents' only way out. It allows hostnames on
`/etc/agent-cloud/egress-allow.txt` (root, 0600) on ports 443 and 80, resolves them on the host and refuses any
that resolve to a non-public address. It binds the bridge address, which exists only after the first agent's CNI
ADD creates `acbr0`, so until then it logs `10.89.0.1 not up yet` and retries every 2 s. That avoids ordering it
after an agent. The list is re-read when the file changes or on `systemctl reload ac-egress`.

```bash
cargo build --release -p ac-egress
sudo install -m 0755 target/release/ac-egress /usr/local/bin/
sudo install -m 0600 deploy/egress-allow.txt /etc/agent-cloud/egress-allow.txt
sudo install -m 0644 deploy/ac-egress.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now ac-egress
journalctl -u ac-egress -f                # one line per decision: allow|deny <agent ip> host:port [reason]
```

Hosts set up before ADR-007 keep the old `cni-agents.json` (NAT on) and `resolv.conf` (public resolvers) under
`/var/lib/agent-cloud`, since both are written only once. With every agent paused, set `"ipMasq": false` and
replace `resolv.conf` with a single comment line, then resume. Pausing first matters: a CNI DEL under the new
config would leave the old masquerade rules behind. Agents pick up the image's proxy variables when they boot
again (resume or create).

**Gateway** (`ac-gateway.service`): runs as root and reads its settings from `/etc/agent-cloud/gateway.env`.
`AC_BIND` must be loopback, RFC 1918 or a tailnet address. The unit strips `AC_ALLOW_PUBLIC_BIND`, so a
public bind is refused, and `IPAddressDeny`/`IPAddressAllow` drop traffic to or from public addresses as well.

```bash
cargo build --release -p ac-gateway
sudo install -m 0755 target/release/ac-gateway /usr/local/bin/
sudo install -m 0600 deploy/gateway.env /etc/agent-cloud/gateway.env      # edit AC_BIND / AC_IMAGE
# The owner's admin token. It is printed once and only its digest is stored; keep it in a root-only
# file for the host scripts (post-reboot-check.sh and ac-job.sh read it from there).
sudo sh -c 'umask 077; ac-gateway token create owner --scopes admin > /etc/agent-cloud/owner.token'
sudo install -m 0644 deploy/ac-gateway.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now ac-gateway
TOKEN=$(sudo cat /etc/agent-cloud/owner.token)
curl -sS -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8780/api/v1/agents/a1
```

**API tokens** (ADR-011): every caller gets its own token, with scopes and agents. Scopes: `agents:read`
(status, list), `agents:write` (create, pause, resume, destroy, purge), `jobs:read`, `jobs:write` (submit,
cancel), `viewer:read` (watch, history), `viewer:control` (take over, type), `admin` (everything). The route
table is in [ADR-011](decisions.md#adr-011-scoped-api-tokens). Tokens live in `/etc/agent-cloud/tokens` (root,
0600) as digests; the commands below edit only that file, so they work while the gateway runs, which picks
the change up within a second (or at once with `sudo systemctl reload ac-gateway`).

```bash
sudo ac-gateway token create ci-a1 --scopes jobs:read,jobs:write --agents a1   # prints the token once
sudo ac-gateway token create watcher --scopes viewer:read                       # every agent (--agents '*')
sudo ac-gateway token list          # NAME SCOPES AGENTS CREATED; never digests or tokens
sudo ac-gateway token revoke ci-a1
curl -sS -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8780/api/v1/whoami
# {"name":"owner","scopes":["admin"],"agents":"*"}
```

A token gets 403 for a route outside its scopes and 404 for an agent outside its list (so it cannot tell which
agents exist); `GET /api/v1/agents` lists only its agents. Failures are logged with the token's name, or for an
unknown token the first 8 hex digits of its SHA-256, never the token (`journalctl -u ac-gateway | grep auth:`).
Every state-changing call is logged as `audit token=<name> action=<...> status=<code>` there and in the agent's
history. A tokens file that is unreadable, not 0600, not root's or malformed stops the gateway at startup (and
is ignored, with a log line, if it turns up later); fix it and start again.

*Migrating from `AC_TOKEN_FILE`* (hosts set up before ADR-011). Nothing breaks on upgrade: the old token keeps
working as an `admin` token for every agent named `legacy`, and the gateway logs that it is deprecated. To move
off it:
1. `sudo sh -c 'umask 077; ac-gateway token create owner --scopes admin > /etc/agent-cloud/owner.token'`, and a
   scoped token for each other caller (a script that only runs jobs gets `jobs:read,jobs:write` for its agents).
2. Point clients at their token: the viewer page asks for it; `scripts/ac-job.sh` takes `AC_TOKEN` or reads
   `owner.token`; `scripts/post-reboot-check.sh` reads `owner.token` once `AC_TOKEN_FILE` is gone.
3. Remove `AC_TOKEN_FILE` from `/etc/agent-cloud/gateway.env`, `sudo systemctl restart ac-gateway`, then
   `sudo shred -u /etc/agent-cloud/gateway.token`.
`AC_TOKEN_FILE` will be removed in a later release.

**Terminal viewer** (ADR-006): served by the gateway, nothing else to install. Agents need an image with tmux
(`images/agent-shell` has it since 2026-10-20); a paused agent picks up a re-imported image when it is resumed.
How to use it:

1. From your own machine, tunnel to the gateway (it listens on loopback):
   `ssh -N -L 8780:127.0.0.1:8780 -p 30556 baileyrd@192.168.50.196` (in general:
   `ssh -N -L 8780:127.0.0.1:8780 [-p <ssh-port>] <user>@<host>`). With `AC_BIND` on a tailnet address,
   skip the tunnel and use that address instead of `localhost`.
2. Open `http://localhost:8780/ui/<agent-id>` (for example `http://localhost:8780/ui/a1`). The page asks for a
   token with `viewer:read` for that agent (or the owner's, `sudo cat /etc/agent-cloud/owner.token`), shows its
   name and scopes, and keeps it in memory only: reloading asks again. Without `viewer:control` there is no
   **Take over** button (and the gateway refuses take-over and input anyway).
3. You watch the agent's tmux session `main`, view-only. **Take over** gives you the keyboard (paste works;
   the window follows your browser's size); **Release** hands it back. If another viewer has control you get
   **Force take over**. Closing the tab leaves the session running; the next viewer sees its scrollback.
   The bar under the header lists the session's windows (`*` marks the agent's current one; `job-*` windows
   from the task API are highlighted). Click one to watch it, and while you have control your keys go to it;
   **Follow agent** goes back to whichever window the agent has current. This is per page: the agent and other
   viewers stay where they are (no `select-window`). If the window closes the page falls back to the agent's.
4. What was shown while someone watched, with take-over and audit events: `GET /api/v1/agents/<id>/history?limit=N`
   (plain text; for example `curl -sS -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8780/api/v1/agents/a1/history`).

The gateway also probes each running agent every 30 s (`AC_PROBE_INTERVAL` in `gateway.env`; 0 turns it off);
a wedged one is logged (`journalctl -u ac-gateway | grep watchdog`) and freed by killing `ctr` clients, or
reported as `"health":"unhealthy"` if that fails. Recordings live under `/var/lib/agent-cloud/history/` (0700).

The gateway locks the data directory (`/var/lib/agent-cloud`) while it runs. `ac-ctl` against the same directory
then exits with code 3 and says so (`/var/lib/agent-cloud is in use by another process, normally ac-gateway ...:
use the gateway API at http://<AC_BIND> with a token ..., or stop ac-gateway`); it never goes through the API on
its own. Use the API with a token that has `agents:read`/`agents:write`, or stop the gateway
(`sudo systemctl stop ac-gateway`) first. Stopping the gateway leaves its agents running.

**After a host reboot** the registry still records agents as running, but containerd has no task for them.
The gateway reconciles once at startup and logs each agent it changes
(`journalctl -u ac-gateway`: `reconcile: a1 had no VM; marked paused`). It also removes what the dead VM left
behind: Kata processes and state dirs, containerd's task bundle, the netns and the CNI lease. A leftover lease
would otherwise fail the next start with `duplicate allocation is not allowed`. With the gateway stopped,
`sudo ac-ctl --image <image> --network reconcile` does the same. Then resume each agent
(`POST /api/v1/agents/<id>/resume`), which boots a fresh VM on its workspace. Verified by killing a VM's QEMU
and shim and restarting the gateway (2026-10-19), and across real reboots (2026-10-20): with a1 running at the
reboot, the gateway logged `reconcile: a1 had no VM; marked paused` and `scripts/post-reboot-check.sh` passed
(no netns, Kata processes, task or lease left; a1 resumed with its login).

**Task API** (ADR-010): run a prompt in an agent as a job, with an id, live status and the result. Nothing to
install; the gateway serves it. Jobs run `claude -p` in the agent's tmux session `main`, in a window named
`job-<id>`, so you can watch one in the viewer (click it in the window list; `job-*` windows are highlighted). Settings
in `gateway.env` (restart the gateway after a change):

- `AC_JOB_PERMISSION_MODE`: `acceptEdits` (default), `auto`, `dontAsk`, `manual` or `plan`;
- `AC_JOB_ALLOWED_TOOLS`: comma-separated `claude --allowedTools` entries (default: read, search and edit files,
  `git` without push, `ls`, `cat`, `python3`, `cargo build|test|check|fmt`, `sleep`);
- `AC_JOB_UNSAFE=1`: `--dangerously-skip-permissions` instead. Read [security.md](security.md#task-api) first.

With curl (the token goes to curl on stdin, not on its command line; it needs `jobs:read` and `jobs:write` for
the agent):

```bash
. <(sudo grep -E '^AC_BIND=' /etc/agent-cloud/gateway.env)
H() { printf 'Authorization: Bearer %s\n' "$(sudo cat /etc/agent-cloud/owner.token)"; }
J=http://$AC_BIND/api/v1/agents/a1/jobs
H | curl -sS -H @- -d '{"prompt":"reply with exactly the word pong","timeout_secs":300}' $J
# {"job_id":"6b941feec45a4c25","agent":"a1","state":"queued",...}         202
H | curl -sS -H @- "$J/6b941feec45a4c25?wait=30"     # returns when the state changes (or after 30 s)
H | curl -sS -H @- $J/6b941feec45a4c25/output        # pong (the final message, plain text)
H | curl -sS -H @- "$J/6b941feec45a4c25/output?part=stream"   # the stream-json log
H | curl -sS -H @- -X POST $J/6b941feec45a4c25/cancel
H | curl -sS -H @- $J                                # the agent's jobs, newest first
```

Or `sudo scripts/ac-job.sh a1 'reply with exactly the word pong'` (prompt `-` reads stdin; `TIMEOUT=`, `KEY=` for
an idempotency key, `STREAM=1`): it submits, waits, prints the result and exits 0 if the job succeeded. Add
`"idempotency_key":"..."` to make a retried submit return the same job. The agent must be running (409
otherwise); pause and destroy refuse an agent with a job (409) unless `?force=true`, which interrupts it. Job state
lives in `/var/lib/agent-cloud/jobs.mmap`; prompts and collected output in `/var/lib/agent-cloud/jobs/<agent>/`
(root, 0700); state changes appear in `GET .../history` as `job <id> <state>`.

**Post-reboot check:** `sudo scripts/post-reboot-check.sh` prints PASS or FAIL per line and exits non-zero on
any FAIL. It checks that the three units are active, the `agentfw` rule counts, the `agents` zone, the
allowlist's owner and mode, and that the gateway and proxy listen only on private addresses. It checks that
every agent the gateway lists (`GET /api/v1/agents`) is paused or destroyed and has no netns, Kata processes,
containerd task or CNI lease left. Then it resumes `a1` through the gateway and, inside it, checks
`claude auth status`, `claude -p`, the API through the proxy, a denied host (example.com) and an unreachable
LAN address (192.168.50.1), and runs a job through the task API ("reply with exactly the word pong", expecting
`succeeded`). It pauses `a1` again afterwards (`KEEP_A1=1` leaves it running). It takes about 30 s.

Debugging tip: to see why a guest dies, boot it directly with `-serial file:/tmp/serial.log`
(do not pipe the console through `sudo ... | tail`; the output is lost).
