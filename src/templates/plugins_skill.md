---
name: plugins
description: >
  Write Reve workspace Lua plugins — tools, cron, and before-tool guards. ctx.sh
  is the only command path and runs in the microVM. Use when the user wants a
  plugin, a custom tool, a guard, or runs /plugins.
---

# Lua plugins

Drop a `.lua` file in `workspace/plugins/` (every bot) or
`workspace/agents/<id>/plugins/` (that bot). Routines go in
`workspace/agents/<id>/routines/` or `workspace/routines/`.

Edits are syntax-checked and reloaded. A broken file is skipped; the last
good set stays. New tools show up on the next turn.

Host-trusted roster tools (`CreateAgent`, `SendAgentMessage`, `AskUserForSecret`, …)
are Rust. You cannot replace them.

```lua
tool("web_fetch", {
  description = "Fetch a URL",
  replay = "safe",
  params = { { name = "url", type = "string", required = true } },
  run = function(args, ctx)
    return ctx.sh("curl -fsSL --max-time 30 -- " .. ctx.shellescape(args.url))
  end,
})

guard("no-force-push", {
  tools = { "bash" },
  run = function(event)
    local cmd = event.args.command or ""
    if cmd:find("git push") and cmd:find("%-%-force") then
      return { block = "force-push is user-gated", terminate = false }
    end
  end,
})
```

`ctx.sh(cmd)` — microVM only.
`ctx.shellescape(s)` — quote for the guest shell.
`ctx.workdir` — `/workspace`.
`ctx.bot` — this bot's id (agent plugins and routines).
`ctx.bots()` — `{ {id, name, title}, ... }` ready roster.
`ctx.send(id, text, { priority = true })` — async, same as SendAgentMessage.

Do not use `os.execute`. There is no host shell. Do not put secrets in plugin files; call `AskUserForSecret`.
