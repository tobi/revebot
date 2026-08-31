---
name: secrets
description: >
  There are no secrets in the microVM, and egress is denied by default. Ask
  the user for a credential or for access to a host with
  AskUserSandboxPolicyChange. Use when a request answers HTTP 403 with a
  "Note to agent", when a tool needs an API key, token, or password, or the
  user runs /secrets.
---

# Sandbox policy: hosts and secrets

Egress is **denied by default**. Only the hosts in `config.yml` `sandbox.allow` (plus the hosts of configured secrets) are reachable. A request to any other host gets `HTTP 403` from the gateway with a body that starts *"Note to agent: `host` is not in the allowed-host list"*. That is not an outage: ask.

The guest never holds a real credential. Config stores a **source** (host env, host command, HTTP URL, or a host-side file under `.reve/secrets/`), a placeholder name, and a per-host map: each hostname has `allow: true` (also joining the sandbox allow list) and optional `headers` such as `Authorization: "Bearer $ENV"`. Microsandbox injects the real value at the network boundary for those hosts only.

Never write a token into `/workspace`, never print one, never `echo $SECRET` to debug.

## When you need a host, a secret, or both

Call **AskUserSandboxPolicyChange** once and stop. Do not retry the blocked request, do not route around the block, do not ask them to paste a key into chat.

```
AskUserSandboxPolicyChange
  title: Install Python packages
  reason: pip needs pypi.org and files.pythonhosted.org; both answered 403.
  hosts: [pypi.org, files.pythonhosted.org]
```

```
AskUserSandboxPolicyChange
  title: GitHub access
  reason: git and the GitHub API need a token; the VM cannot see it.
  secret:
    env: GITHUB_TOKEN
    hosts: [github.com, api.github.com]
```

The user gets one inline form. For hosts they can edit the list before allowing. For a secret they can change `env`, limit hosts, set a header overwrite (`Authorization` + `Bearer`, stored as `Authorization: "Bearer $ENV"` on each host), and choose how to obtain the value — paste (password field), host env var, host shell (`$(gh auth token)`), or HTTP GET. Applying writes `config.yml` (and a host file for paste). A secret's hosts (`allow: true`, the default) are allowed automatically.

Allowing a new host **restarts the sandbox** with the new policy: any command that was running ends. The tool result says so; re-run what was interrupted. The result is an ack with hosts and env names — **not the secret**.

## Using a secret

In the VM, `$ENV` is a placeholder. For allowed hosts, put it where the proxy should substitute:

```
curl -H "Authorization: Bearer $GITHUB_TOKEN" https://api.github.com/...
```

If the user declines, work without it or pick another approach. Do not ask again in a loop.
