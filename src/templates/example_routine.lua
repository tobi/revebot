-- Bot-editable, restricted Lua: loaded at house boot. No host IO or shell.
-- Cron is five fields: minute hour day-of-month month day-of-week.
-- `ctx.send(bot_id, text)` queues a user-visible turn for that bot.

routine("example", {
  name = "Example (disabled)",
  cron = "0 9 * * 1-5",
  enabled = false,
  -- `bot` defaults to the owning agent folder when this file lives under
  -- workspace/agents/<id>/routines/.
  message = "Good morning. Summarise anything that needs my attention.",
})

-- Several sends from one tick:
--
-- routine("standup", {
--   name = "Standup ping",
--   cron = "0 10 * * 1-5",
--   run = function(ctx)
--     ctx.send("chief-of-staff", "Collect standup notes from the roster.")
--   end,
-- })
