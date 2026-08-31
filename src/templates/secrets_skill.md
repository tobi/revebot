---
name: secrets
description: >
  There are no secrets in the microVM. Ask the user for a credential with
  AskUserForSecret. Use when a tool needs an API key, token, or password, or
  the user runs /secrets.
---

# Secrets

The guest never holds a real credential. Config stores a **source** (host env, host command, HTTP URL, or a host-side file under `.reve/secrets/`), a placeholder name, and the HTTP **hosts** that may receive the value. Microsandbox injects the real value at the network boundary for those hosts only.

Never write a token into `/workspace`, never print one, never `echo $SECRET` to debug.

## When you need one

Call **AskUserForSecret** and stop. Do not ask them to paste a key into chat.

```
AskUserForSecret
  title: GitHub access
  description: Clone and push to GitHub as the user.
  reason: git and the GitHub API need a token; the VM cannot see it.
  env: GITHUB_TOKEN
```

The user gets an inline form: they can change `env`, limit hosts (e.g. `github.com`, `api.github.com`), set a header overwrite (`Authorization` + `Bearer`), and choose how to obtain the value — paste (password field), host env var, host shell (`$(gh auth token)`), or HTTP GET. Saving writes `config.yml` (and a host file for paste). The tool result is an ack with the env name and hosts — **not the secret**.

## Using it

In the VM, `$ENV` is a placeholder. For allowed hosts, put it where the proxy should substitute:

```
curl -H "Authorization: Bearer $GITHUB_TOKEN" https://api.github.com/...
```

If the user declines, work without it or pick another approach. Do not retry the tool in a loop.
