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

Plain `ctr run` containers have no network. `ac-ctl --network` adds `ctr run --cni`, which needs:

```
sudo dnf install -y containernetworking-plugins
sudo mkdir -p /opt/cni /etc/cni/net.d && sudo ln -sfn /usr/libexec/cni /opt/cni/bin
sudo tee /etc/cni/net.d/10-agents.conflist >/dev/null <<'EOF'
{
  "cniVersion": "1.0.0",
  "name": "agents",
  "plugins": [
    { "type": "bridge", "bridge": "acbr0", "isGateway": true, "ipMasq": true,
      "ipam": { "type": "host-local", "ranges": [[{ "subnet": "10.89.0.0/16" }]],
                "routes": [{ "dst": "0.0.0.0/0" }] } }
  ]
}
EOF
```

Until the egress allowlist exists (ADR-007), keep agents off your LAN with a drop rule for private ranges:

```
sudo nft add table inet agentfw
sudo nft add chain inet agentfw forward '{ type filter hook forward priority -10; policy accept; }'
sudo nft add rule inet agentfw forward iifname "acbr0" ip daddr '{ 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, 169.254.0.0/16, 100.64.0.0/10 }' drop
```

`--network` also gives each agent a `resolv.conf` (public resolvers 1.1.1.1 and 9.9.9.9, written to
`<data>/resolv.conf` on first use), because a VM cannot use the host's loopback resolver stub.
Queries go straight out until the egress proxy owns DNS.

This is not persistent and not a security boundary yet. Agents on the same bridge can still reach each other,
and DNS may need a nameserver set inside the guest. Do not leave agents running unattended on this setup.
