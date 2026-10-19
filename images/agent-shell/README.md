# agent-shell image

Fedora 44 with Node, git, ripgrep and Claude Code, running as `agent` (uid 1000, no sudo). The container
sleeps; you attach to it. Desktop, browser and the agent API come later.

## Build and load (on the host)

```
podman build -t agent-shell:dev images/agent-shell
podman save agent-shell:dev | sudo ctr -n agents images import -
sudo ctr -n agents images ls            # note the name, expected localhost/agent-shell:dev
```

## Give each VM enough memory

Claude Code needs at least 4 GB of RAM ([setup docs](https://code.claude.com/docs/en/troubleshoot-install));
Kata's default VM is 2 GB. In `/etc/kata-containers/configuration.toml` set:

```
default_memory = 4096
default_maxmemory = 8192
```

## Start an agent and attach

```
sudo ac-ctl --image localhost/agent-shell:dev create a1          # add --network once networking is set up
sudo ctr -n agents task exec -t --exec-id sh1 a1 bash
whoami; pwd                                                        # expect: agent, /workspace
```

Then run `claude` inside (or in one step: `sudo ctr -n agents task exec --tty --exec-id t2 a1 sh -c 'cd /workspace && claude'`;
without `--tty` it has no terminal). First run asks for a theme, then "Select login method"; option 1 (Claude
subscription) prints a login URL to open in your own browser. Or export `ANTHROPIC_API_KEY`.
`pause` stops the VM (everything running in it ends) and keeps `/workspace`; `resume` boots a fresh VM
on it, with a new network namespace. The image sets `CLAUDE_CONFIG_DIR=/workspace/.claude`, so Claude Code's
`.claude.json`, login (`.credentials.json`) and sessions live on the workspace volume and survive
pause/resume (verified on a fresh agent: VM up 0 min after resume, `claude -p` still logged in). Agents
created from an older image keep them in `/home/agent`, on the image layer, and lose them on pause. Verified with Claude Code 2.1.302:
the VM reports 3.9 GB total with `default_memory = 4096`, and 1 vCPU by default.

A create that fails or times out cleans up after itself: it kills the sandbox's QEMU, shim and virtiofsd,
removes `/run/vc/{vm,sbs}/<id>`, the container, its snapshot and the network. Each create also first removes
a stale container and snapshot left under its id (the cause of `snapshot <id>: already exists`).

## Networking (needed for Claude Code to reach its API)

Verified on BaileyServ (2026-10-18): eth0 up, DNS resolves, `curl -I https://api.anthropic.com` answers, LAN
blocked by the rule below. Check an agent with:

```
sudo ctr -n agents task exec --exec-id t1 a1 sh -c 'cat /proc/net/dev; cat /etc/resolv.conf; getent hosts api.anthropic.com; curl -sS -m 8 -I https://api.anthropic.com | head -3'
```

Plain `ctr run` containers have no network, and `ctr run --cni` does not work with Kata: it attaches the
network after the VM has booted, so the guest never gets an interface. With `--network`, `ac-ctl` instead
creates a network namespace (`ip netns add ac-<id>`), attaches it with the CNI `bridge` plugin, then starts the
container inside it (`ctr run --with-ns network:/run/netns/ac-<id>`). Destroying the agent detaches and removes it.
Pass `--network` on `destroy` too, or the namespace is left behind.
Every external command (`ctr`, `ip`, the CNI plugin) is killed after 120 s. To see each command as it runs,
set `AC_TRACE=1` (with sudo: `sudo env AC_TRACE=1 ac-ctl ...`).

```
sudo dnf install -y containernetworking-plugins      # provides /usr/libexec/cni/bridge
```

On first use `--network` writes two files under the data directory, which you can edit:
- `cni-agents.json`: a NAT bridge `acbr0`, subnet 10.89.0.0/16.
- `resolv.conf`: public resolvers 1.1.1.1 and 9.9.9.9. A VM cannot use the host's loopback resolver stub.
  Queries go straight out until the egress proxy owns DNS.

Until the egress allowlist exists (ADR-007), keep agents off your LAN with a drop rule for private ranges.
Install it as a boot-time unit ([docs/install.md, Host services](../../docs/install.md#host-services)); by hand it is:

```
sudo nft add table inet agentfw
sudo nft add chain inet agentfw forward '{ type filter hook forward priority -10; policy accept; }'
sudo nft add rule inet agentfw forward iifname "acbr0" ip daddr '{ 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, 169.254.0.0/16, 100.64.0.0/10 }' drop
```

Added by hand it does not survive a reboot, and agents can then reach the LAN again (that is how it was found:
an agent got HTTP 301 from the LAN router until the rule was re-added). Check with `sudo nft list table inet agentfw`. The CNI bridge plugin turns on `net.ipv4.ip_forward` and adds the per-agent
masquerade itself (`ipMasq`), so nothing else is needed for egress. The rule is not a security boundary yet. Agents on the same bridge can still reach each other,
and firewalld on Fedora Workstation may need adjusting for forwarding. Do not leave agents running unattended
on this setup.
