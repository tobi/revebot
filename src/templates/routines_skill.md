---
name: routines
description: >
  Create Reve cron routines in Lua (house or per-bot). Use when the user wants a
  scheduled job, a heartbeat, morning ping, or runs /routines.
---

# Routines

Trusted Lua, loaded at house boot — not model output. A broken file is skipped.

## Where

- This bot: `/workspace/agents/<your-id>/routines/<file>.lua`
- Whole house: `/workspace/routines/<file>.lua`

Cron is five fields: `minute hour day-of-month month day-of-week`.

## Declare

```lua
routine("standup", {
  name = "Standup ping",
  cron = "0 10 * * 1-5",
  enabled = true,
  -- `bot` defaults to this agent folder when the file lives under agents/<id>/routines/.
  message = "Collect standup notes from the roster.",
})
```

Several sends from one tick:

```lua
routine("morning", {
  name = "Morning",
  cron = "0 9 * * 1-5",
  run = function(ctx)
    ctx.send(ctx.bot, "What needs attention?")
  end,
})
```

`ctx.send(id, text, { priority = true })` queues a turn for that bot (same as SendAgentMessage). `ctx.bot` is this bot's id. `ctx.bots()` is the ready roster.

`enabled = false` keeps the file without firing. The user can run an enabled routine from the rail.

Do not `os.execute`. There is no host shell. If a routine needs a command, it does not belong here — a tool with `ctx.sh` runs in the microVM.
