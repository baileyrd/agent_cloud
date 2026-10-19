# Security

## Threat model

Single trusted owner, several semi-trusted agents. Agents run without approval prompts and
may execute untrusted code or be steered by prompt injection from web content.

| Threat | Mitigation |
|---|---|
| Agent escapes to host | Kata microVM (own guest kernel, KVM); no sudo; no containerd socket |
| Agent reads other agents' data | Separate microVMs and separate workspace volumes |
| Agent exfiltrates data | Egress allowlist proxy (`ac-egress`), firewall makes it the only route; limits destinations, not content (ADR-007) |
| Agent steals credentials | Control-plane secrets root-owned 0600; model logins confined to the agent's workspace volume |
| Prompt injection via browsing | Policy-locked Chromium, egress allowlist, human takeover |
| Network attacker reaches the API | Gateway on tailnet only, bearer token, TLS; no public port |
| Runaway agent starves the host | Per-agent CPU/memory caps, disk quotas, concurrent-build limit |
| Destructive commands | Command guard in the shell container (a backstop, not a boundary) |

## Hardening checklist

- [ ] `/dev/kvm` present; every agent runs under the Kata runtime class, never plain runc
- [ ] SELinux enforcing; denials reviewed instead of disabling it
- [ ] Gateway reachable only over Tailscale with HTTPS and a bearer token
- [x] nftables egress rules: agents reach only the egress proxy (verified 2026-10-19 from a1:
  direct connections to the internet, the LAN, 169.254.169.254 and other host ports refused;
  no DNS)
- [x] Proxy allowlist limited to the model APIs and Git hosts actually in use (measured
  2026-10-19: `api.anthropic.com`, `platform.claude.com` for token refresh, GitHub)
- [ ] Per-agent memory and CPU limits; cap on concurrent builds
- [ ] Per-agent disk quota on the workspace volume
- [ ] Secrets in 0600 root-owned files; never in images, repo, prompts or logs
- [ ] Agent user has no sudo and no access to the containerd socket
- [ ] No git remotes with embedded tokens in workspaces
- [ ] Data directory (registry + event logs) owned by the control-plane user, mode 0700; one process per directory is enforced by a lock
- [ ] Backups for the data directory and workspace volumes, with a tested restore
- [ ] Production desktop image: Chromium managed policy applied; DevTools policy decided

## Credentials for model access

Each agent uses its own login (for example a Codex or Claude Code login under the agent
user), as Cloudroom core does. Claude Code's login is kept on the agent's workspace
(`CLAUDE_CONFIG_DIR=/workspace/.claude`) so it survives pause and resume. On the host that
is `/var/lib/agent-cloud/workspaces/<id>/.claude`, owned by `AC_UID` (default 1000), so a
host user with that uid can read every agent's login. `AC_UID` must equal the image's agent
uid, so on a shared host rebuild the image with an otherwise unused uid and set `AC_UID` to it. Do not put inference credentials in control-plane
environment files. A request flow that asks the owner for a secret through a form, not
chat, is worth building (Cloudroom's `cloud-secrets` skill is a reference).

## Known gaps

- A container-in-VM boundary is only as strong as the hypervisor and guest kernel config.
  Keep Kata and the host kernel patched.
- The command guard only blocks known-bad patterns.
- Egress control limits destinations, not content: an agent can still send data to an allowed
  host (a GitHub gist, a model API request). TLS is not inspected, so domain fronting on shared
  CDN addresses is possible. Agents on the same bridge can reach each other. See ADR-007.
- Screen streaming exposes whatever the agent's browser shows, including anything a logged-in
  session reveals. Treat viewer access as equivalent to agent access.
