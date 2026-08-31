---
name: vm
description: >
  How Reve's microVM works: wrap desktop image, unprivileged user, mount,
  network, no host shell, no secrets in the guest. Use when the user asks
  about the sandbox, microsandbox, /workspace, ctx.sh, VNC, or runs /vm.
---

# The microVM

Reve does not run model-authored commands on the host. Every shell tool (`bash`, `ctx.sh`, `reve exec`) runs inside a microsandbox microVM. If the VM cannot boot, Reve refuses to start. There is no host/local fallback.

The default guest is `ghcr.io/tobi/wrap:desktop`. Workloads run as unprivileged `user` (`HOME=/home/user`), with uid/gid realigned to the host owner of `/workspace` so the bind mount is writable without chowning host files.

## What the guest can see

- `/workspace` is the house `workspace/` directory, bind-mounted, working directory. Relative paths mean the same thing on the host and in the VM.
- Bot identity, `config.yml`, sessions, and host Lua stay **outside** the mount. Do not write under `agents/*/sessions/`.
- Public internet is the default. `config.yml` / `sandbox.lua` `open: false` plus `allow` locks egress to named hosts.
- Unix `HOME=/home/user` is not your agent home (`/workspace/agents/<your-id>/`).

## Desktop

XFCE on `DISPLAY=:1`. The user sees a live preview in the house Screen panel and clicks it to take over (keyboard and mouse). `wrap-desktop start|stop|status`. Chrome and other GUI share that display. `/browser` for Chrome, `/computer` for everything else.

## What the guest cannot do

- No host shell. `os.execute` is deleted from Lua. There is no `ctx.host_exec`.
- **No real secrets.** Guest env for a secret is a placeholder. The host injects the real value only into HTTP(S) to the secret's `hosts` list (e.g. `Authorization: Bearer $GITHUB_TOKEN` toward `github.com`). Never write a credential into `/workspace`. To add one, call `AskUserForSecret`.

## Tools

`bash`, `read`, `write`, `edit`, `ls`, and Lua `ctx.sh` are guest-side. mise is in the image; use it to install language tools inside the VM. `sudo` is passwordless for the cases that need root.
