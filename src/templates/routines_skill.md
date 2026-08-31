---
name: routines
description: >
  Create Reve cron routines in Lua (house or per-bot). Use when the user wants a
  scheduled job, a heartbeat, morning ping, or runs /routines.
---

# Routines

Read `/workspace/skills/plugins/SKILL.md` for the complete Lua API, including
routine fields, callback context, errors, loading behavior and current limits.

Bot-editable routines execute in restricted Lua at house startup. A broken file
fails startup; automatic reload/last-good fallback is not yet implemented.

- This bot: `/workspace/agents/<your-id>/routines/<file>.lua`
- House: `/workspace/routines/<file>.lua`

Use a house-unique id. Cron has five numeric fields:
`minute hour day-of-month month day-of-week`.

```lua
routine("chief_standup", {
  name = "Standup ping",
  cron = "0 10 * * 1-5",
  enabled = false, -- enable after testing
  -- bot defaults to the owning folder in agents/<id>/routines/.
  bot = "chief-of-staff", -- required for this house-level example
  message = "Collect standup notes from the roster.",
})
```

Instead of `message`, `run = function(ctx) ... end` can collect multiple sends
with `ctx.send(bot_id, text)`. `ctx.bot` is the resolved target/owning bot, or nil
for a house routine without `bot`. There is no `ctx.bots()` or priority option.
The sends are delivered only after the callback succeeds; collecting is not a
durable receipt. There is no `ctx.sh` in routines: tell the bot to do guest work.

`enabled = false` disables scheduled firing. Current ticks enter the bot's main
conversation; persistent independent routine chats and durable run history are
planned. Do not promise isolation or recovery of missed ticks yet.
