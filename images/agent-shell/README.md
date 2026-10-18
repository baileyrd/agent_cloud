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

Then run `claude` inside; it prints a login URL to open in your own browser (or export `ANTHROPIC_API_KEY`).
Logins live in the agent's home, inside the VM, and go away with `destroy`.

## Networking (needed for Claude Code to reach its API) **[unverified]**

Plain `ctr run` containers have no network, and `ctr run --cni` does not work with Kata: it attaches the
network after the VM has booted, so the guest never gets an interface. With `--network`, `ac-ctl` instead
creates a network namespace (`ip netns add ac-<id>`), attaches it with the CNI `bridge` plugin, then starts the
container inside it (`ctr run --with-ns network:/run/netns/ac-<id>`). Destroying the agent detaches and removes it.
Pass `--network` on `destroy` too, or the namespace is left behind.

```
sudo dnf install -y containernetworking-plugins      # provides /usr/libexec/cni/bridge
```

On first use `--network` writes two files under the data directory, which you can edit:
- `cni-agents.json`: a NAT bridge `acbr0`, subnet 10.89.0.0/16.
- `resolv.conf`: public resolvers 1.1.1.1 and 9.9.9.9. A VM cannot use the host's loopback resolver stub.
  Queries go straight out until the egress proxy owns DNS.

Until the egress allowlist exists (ADR-007), keep agents off your LAN with a drop rule for private ranges:

```
sudo nft add table inet agentfw
sudo nft add chain inet agentfw forward '{ type filter hook forward priority -10; policy accept; }'
sudo nft add rule inet agentfw forward iifname "acbr0" ip daddr '{ 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, 169.254.0.0/16, 100.64.0.0/10 }' drop
```

This is not persistent and not a security boundary yet. Agents on the same bridge can still reach each other,
and firewalld on Fedora Workstation may need adjusting for forwarding. Do not leave agents running unattended
on this setup.
