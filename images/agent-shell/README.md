# agent-shell image

Fedora 44 with Node, git, ripgrep, tmux, vim and Claude Code, running as `agent` (uid 1000, no sudo). The
container sleeps; you attach to it, or watch it in a browser: the gateway's terminal viewer creates and shows
the tmux session `main` ([docs/install.md, Terminal viewer](../../docs/install.md#host-services)). Desktop,
browser and the agent API come later.

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

Never pipe or redirect a file into `ctr task exec` without `-t` (`... < script.py`, `printf ... | ctr ...`), and
not `< /dev/null` either: stdin EOF while the guest process runs wedges the Kata shim. Feed stdin from a FIFO a
background `sleep` holds open, or copy the file into the workspace and run it
([install.md, Fedora note 8](../../docs/install.md#fedora-44-notes-verified-on-baileyserv-i5-3550-2026-10-18)).

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

Agents reach the internet only through the egress proxy `ac-egress` on the bridge (10.89.0.1:3128), and only
hosts on its allowlist (ADR-007). The image sets `HTTPS_PROXY`, `HTTP_PROXY`, `NO_PROXY` (and their lowercase
forms, since curl reads only a lowercase `http_proxy`). These are a convenience: the host firewall drops every
direct connection, so a program that ignores them fails instead of bypassing the proxy. The image also sets
`CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1` and `ENABLE_CLAUDEAI_MCP_SERVERS=false`, so Claude Code contacts
only its API. Without them it also tried telemetry and the owner's claude.ai connectors (Gmail, Drive and
others, through the agent's login). Agents pick up the image's variables only when their VM starts: after
rebuilding, `pause` then `resume` each agent (or create it again).

Verified on BaileyServ (2026-10-19), from a1:

```
sudo ctr -n agents task exec --exec-id t1 a1 sh -c 'curl -sS -m 8 -I https://api.anthropic.com | grep ^HTTP; curl -sS -m 8 -o /dev/null https://example.com; curl -sS -m 8 --noproxy "*" -I https://1.1.1.1'
HTTP/1.1 200 Connection established
HTTP/2 404
curl: (56) CONNECT tunnel failed, response 403
curl: (7) Failed to connect to 1.1.1.1 port 443 after 0 ms: Could not connect to server
```

`claude -p` and `git ls-remote https://github.com/...` work. Denials are logged on the host
(`journalctl -u ac-egress`: `deny 10.89.0.34 example.com:443: not on allowlist`); to allow a host, add it to
`/etc/agent-cloud/egress-allow.txt`, which is re-read automatically.

Plain `ctr run` containers have no network, and `ctr run --cni` does not work with Kata: it attaches the
network after the VM has booted, so the guest never gets an interface. With `--network`, `ac-ctl` instead
creates a network namespace (`ip netns add ac-<id>`), attaches it with the CNI `bridge` plugin, then starts the
container inside it (`ctr run --with-ns network:/run/netns/ac-<id>`). Pausing or destroying the agent detaches and removes it.
Pass `--network` on `destroy` too, or the namespace is left behind.
Every external command (`ctr`, `ip`, the CNI plugin) is killed after 120 s. To see each command as it runs,
set `AC_TRACE=1` (with sudo: `sudo env AC_TRACE=1 ac-ctl ...`).

```
sudo dnf install -y containernetworking-plugins      # provides /usr/libexec/cni/bridge
```

On first use `--network` writes two files under the data directory, which you can edit:
- `cni-agents.json`: a bridge `acbr0`, subnet 10.89.0.0/16, without NAT (`ipMasq: false`). Nothing needs
  masquerading, since the only destination is the proxy on the bridge address.
- `resolv.conf`: no nameserver. Agents resolve nothing; the proxy resolves allowed names on the host.

The firewall (`deploy/agentfw.nft`) and the proxy run as boot-time units
([docs/install.md, Host services](../../docs/install.md#host-services)). Check the rules with
`sudo nft list table inet agentfw` and `sudo nft list table bridge agentfw` (agents cannot reach each other).
They are not a complete boundary: an agent can send data to any allowed host (see ADR-007).
