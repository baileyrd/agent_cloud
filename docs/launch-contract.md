# Launch contract: running an untrusted agent in agent_cloud

How Let Em Loose's launcher (or any other caller) runs one untrusted job in its own agent_cloud VM, through
the gateway alone. Card #1066 (LEL D101). Every call is a REST route; each has the MCP tool from the same
catalog route (ADR-020), named below. `tests/jobs.rs::the_launch_contract_end_to_end_over_http` drives this
whole flow, and `scripts/untrusted-run.sh` runs it on a real host with the in-VM checks of #1067.

## What the operator sets up once
1. **The gateway profile** (ADR-016 addendum), `/etc/agent-cloud/egress/profiles/gateway.txt`, root 0600:
   ```
   !exclusive
   endpoint <lel-gateway-host>:<port>
   ```
   Plus the `IPAddressAllow=<address>/32` drop-in on `ac-egress` (docs/install.md). With `!exclusive`, the agent
   loses the base allowlist: it reaches that one host on that one port and nothing else.
2. **The model URL** (ADR-028 addendum), in `/etc/agent-cloud/gateway.env`:
   ```
   AC_JOB_MODEL_URL=https://<lel-gateway-host>:<port>
   ```
   Same host and port as the profile's endpoint (a path is allowed). A job that carries a credential then runs
   `claude` with `ANTHROPIC_BASE_URL` set to it and the credential as `ANTHROPIC_AUTH_TOKEN`, so the agent needs
   no Claude login and reaches the model only through LEL's gateway. See "Model access" below.
3. **A launcher token** for a fixed pool of agent ids:
   ```
   sudo ac-gateway token create lel-launcher \
     --scopes agents:read,agents:write,egress:write,jobs:read,jobs:write \
     --agents lel1,lel2,lel3,lel4 --expires 90d
   ```
   `egress:write` is needed to attach the profile when the agent is created. An agent outside the pool is
   answered 404, as if it did not exist. The token can create, pause, destroy and purge its pool. It cannot touch
   other agents, watch or type into any terminal, or change tokens.

## The calls
All calls go to `https://<gateway>/api/v1` with `Authorization: Bearer <token>` (HTTPS through `tailscale
serve`, ADR-024). Bodies are JSON, and unknown members are refused with 400.

| # | Call | MCP tool | Answer |
|---|---|---|---|
| 1 | `POST /agents/{id}` `{"egress_profiles":["gateway"]}` | `create_agent` | 201 `{"id","state":"running"}` |
| 2 | `POST /agents/{id}/jobs` `{"prompt","timeout_secs"?,"idempotency_key","credential"}` | `submit_job` | 202, the job (`job_id`, `state":"queued"`) |
| 3 | `GET /agents/{id}/jobs/{job}?wait=30`, repeated until the state is terminal | `get_job` | 200, the job |
| 4 | `GET /agents/{id}/jobs/{job}/output` (`?part=stream` for the stream-json log) | `get_job_output` | 200 `text/plain`, agent-written |
| 5 | `GET /agents/{id}/jobs/{job}/witness`, repeated while it says `changes pending` | `get_job_witness` | 200 `{"record","sha256"}` |
| 6 | `DELETE /agents/{id}?purge=true` | `destroy_agent` | 200 `{"id","state":"destroyed"}` |

Notes on each step:
1. **Create with the profile in the body**, not with a `PUT .../egress` afterwards. The agent then never boots
   with the base list. The VM is up when the 201 arrives: creates take about 4 s each, and since ADR-027 they
   run in parallel. Check `GET /egress` if you want to be sure: `"profiles":["gateway"]`.
2. **The credential** is the short-lived token LEL mints for this claim (ADR-028): 1 to 4096 bytes of
   `A-Za-z0-9-._~+/=`. The job's process reads it from the file named by `$AC_JOB_CREDENTIAL_FILE`, and with
   `AC_JOB_MODEL_URL` set `claude` uses it as its model token. It is gone when the job ends, however it ends. It is never in a reply, the registry, a log or the workspace.
   - Use one `idempotency_key` per claim. A retry with the same key returns the same job.
   - `timeout_secs` is 1 to 14400 and defaults to 1800; at the deadline the job ends `timed_out`.
   - The prompt is at most 64 KiB.
3. **States:** `queued`, `running`, then exactly one of `succeeded`, `failed` (`exit_code` and `reason` say
   why), `cancelled`, `timed_out` or `interrupted` (the VM went away). A `queued` job can carry
   `queued_reason` (ADR-013) while the host is busy. `POST .../jobs/{job}/cancel` (`cancel_job`) stops it.
4. **The output** is what the agent wrote, so treat it as untrusted text. Copies of the credential are replaced
   with `[credential redacted]`.
5. **The witness** (ADR-029) is the evidence for the BBP claim. Every line comes from outside the VM:
   - state, times and exit code;
   - the paths the job changed;
   - the proxy's allow/deny counts for this agent during the job.

   To verify it, recompute SHA-256 over `record` and compare it with `sha256`. Keep the record, not only the
   digest: the egress lines are rebuilt from the journal and can change once old lines rotate out. Keep
   fetching while the record says `changes pending`. The compare runs after the job ends, usually within
   seconds.
6. **Purge** removes the agent's workspace, checkpoints, job records and proxy credential. Always purge. Without
   `purge=true` the workspace is kept, and the next create of that pool id gets it back, along with whatever
   the last untrusted job left in it.

## Failure handling

| Answer | Where | What it means | What to do |
|---|---|---|---|
| 400 | any | bad id, bad body, bad credential | fix the request; don't retry as is |
| 401 | any | missing, revoked or expired token | stop; the operator issues a new token |
| 404 | any | agent or job unknown, or outside the token's pool | check the id against the pool |
| 409 `is being created` | 1, 6, anything | another create of this id is booting | wait a few seconds, then retry |
| 409 `already exists` / not destroyed | 1 | the id is in use | use another pool id, or destroy and purge it first |
| 409 `is paused` | 2 | the agent is not running | `POST /agents/{id}/resume` |
| 409 | 4, 5 | the job has not ended | keep polling step 3 |
| 409 | 6 | a job is running | cancel it, or `DELETE ...?purge=true&force=true` (interrupts it) |
| 429 | 2, 3 | 8 jobs already queued for the agent, or too many long-polls | back off and retry |
| 429 | any | too many failed authentications, gateway-wide (ADR-024) | back off; valid tokens keep working |
| 500 | any | host error, message included | retry once, then report it with the message |
| 507 | 1 | the guest does not fit in `/dev/shm` (ADR-012); the body has the figures | wait for another agent to be destroyed |

- **A failed create** (500) leaves the agent `creating`. Its egress credential is revoked. `DELETE
  ...?purge=true` clears it.
- **A gateway restart** keeps agents and jobs, but not credentials, which live in memory only. A job still
  queued when the gateway restarts fails with `its credential was lost when the gateway restarted; submit it
  again`. Mint a new credential and submit with a new `idempotency_key`. A running job keeps its file and
  finishes.
- **The witness** says `egress unavailable` if the journal could not be read. Treat that as missing evidence, not
  as zero requests.
- **Always destroy and purge**, also after a failure. An agent left running holds memory that admission counts.

## Model access
Decided (Nano and LEL): *LEL's gateway fronts the model.* With only `gateway` attached the agent cannot reach
`api.anthropic.com`, so a Claude Code job reaches the model through LEL's gateway instead, authenticated with the
job credential: no model login lives in the VM, and model access ends with LEL's token. LEL's side:
- proxy the Anthropic Messages API at `AC_JOB_MODEL_URL` (Claude Code sends `Authorization: Bearer
  <credential>` to `$ANTHROPIC_BASE_URL/v1/messages`);
- accept the credential minted for the claim, and refuse it once the claim ends.

A job without a credential keeps using the agent's own login, so it fails on a gateway-only agent. The witness
counts the model calls under `egress_allow <endpoint>`; an `egress_deny api.anthropic.com:443` there means the
harness tried the model directly.

## What is in the VM
Today the image is `images/agent-shell`: Fedora, Claude Code, an unprivileged `agent` user, tini as PID 1, no
sudo, and no secrets in the image. The VM is the jail's outer wall:
- a Kata VM with its own kernel;
- egress through the proxy only (ADR-007, ADR-016);
- CPU, memory and disk limits (ADR-023).

LEL's in-VM layer (rusty_sandbox's Landlock, seccomp and rlimits, plus LEL's harness) is not baked in yet. It is
LEL code with LEL's version pins, so it belongs in an `images/agent-jail` built `FROM` agent-shell once LEL
names the binary and the version to pin. That is a follow-up coordinated with the Let Em Loose project, not part
of this contract.
