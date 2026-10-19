# Security

## Threat model

Single trusted owner, several semi-trusted agents. Agents run without approval prompts and
may execute untrusted code or be steered by prompt injection from web content.

| Threat | Mitigation |
|---|---|
| Agent escapes to host | Kata microVM (own guest kernel, KVM); no sudo; no containerd socket |
| Agent reads other agents' data | Separate microVMs and separate workspace volumes |
| Agent exfiltrates data | Egress allowlist (model APIs, Git hosts only) |
| Agent steals credentials | Control-plane secrets root-owned 0600; model logins confined to the agent's workspace volume |
| Prompt injection via browsing | Policy-locked Chromium, egress allowlist, human takeover |
| Network attacker reaches the API | Gateway on tailnet only, bearer token, TLS; no public port |
| Runaway agent starves the host | Per-agent CPU/memory caps, disk quotas, concurrent-build limit |
| Destructive commands | Command guard in the shell container (a backstop, not a boundary) |

## Hardening checklist

- [ ] `/dev/kvm` present; every agent runs under the Kata runtime class, never plain runc
- [ ] SELinux enforcing; denials reviewed instead of disabling it
- [ ] Gateway reachable only over Tailscale with HTTPS and a bearer token
- [ ] nftables egress rules: agents reach only the egress proxy
- [ ] Proxy allowlist limited to the model APIs and Git hosts actually in use
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
host user with that uid can read every agent's login. Use an otherwise unused uid on shared
hosts. Do not put inference credentials in control-plane
environment files. A request flow that asks the owner for a secret through a form, not
chat, is worth building (Cloudroom's `cloud-secrets` skill is a reference).

## Known gaps

- A container-in-VM boundary is only as strong as the hypervisor and guest kernel config.
  Keep Kata and the host kernel patched.
- The command guard only blocks known-bad patterns.
- Screen streaming exposes whatever the agent's browser shows, including anything a logged-in
  session reveals. Treat viewer access as equivalent to agent access.
