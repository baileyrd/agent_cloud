# agent-desktop image

[agent-shell](../agent-shell/README.md) plus a desktop: display `:0` on Xorg's dummy driver, an Xfce session and
Chromium under a managed policy. Everything runs as `agent` (uid 1000, no sudo); `tini` stays PID 1. Its main
process, `desktop-session`, keeps Xorg and Xfce running and restarts either one if it exits. `DISPLAY=:0` is
in the image's environment, so a job or the terminal viewer's tmux can start `chromium` and it opens on the
desktop. The gateway's desktop stream (ADR-006, desktop addendum) uses the image's `ffmpeg` (frames) and
`xdotool` (take-over input): open `https://<gateway>/ui/desk/<agent>`.

## Build and load (on the host)

```
scripts/build-images.sh            # agent-shell, then agent-desktop on top; imports both into containerd
sudo ac-ctl --image localhost/agent-desktop:dev --network create d1
sudo scripts/desktop-smoke.sh d1   # X on DUMMY0, Xfce up, Chromium runs under the policy, idle memory
```

## Chromium policy

[`chromium-policy.json`](chromium-policy.json) is installed root-owned at
`/etc/chromium/policies/managed/agent.json`, so the agent cannot loosen it.

- **Extensions:** none can be installed (`ExtensionInstallBlocklist: ["*"]`).
- **Downloads:** go to `/workspace/Downloads` without a prompt, so they survive pause and resume; files
  Chromium flags as dangerous are blocked (`DownloadRestrictions: 1`).
- **DevTools: off** (`DeveloperToolsAvailability: 2`, `RemoteDebuggingAllowed: false`). Decided for the
  production image: the browser is the agent's way in for prompt injection (docs/security.md), and a
  remote-debugging port would let any process in the VM drive the owner's take-over session. An agent that
  needs to automate a browser uses its own (Playwright's bundled Chromium, say) in `/workspace`, which this
  policy does not cover. To debug the agent's browsing by hand, build a dev image with the value set to `1`.
- **No accounts or sync:** sign-in, sync, the password manager, autofill, guest mode and new profiles are off.
- **Quiet:** no metrics reports, no default-browser prompt, nothing left running in the background, no
  microphone or camera.

Chromium still tries Google's services (Safe Browsing, component updates). The egress proxy refuses them,
since they are not on the allowlist (ADR-007).

## Memory

Kata's 2048 MiB default (agent-shell README) is the target. Measured figures go in
[docs/sizing.md](../../docs/sizing.md#desktop-image).
