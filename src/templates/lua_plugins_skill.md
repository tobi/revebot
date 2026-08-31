---
name: lua-plugins
description: >
  Write or debug Reve Lua plugins. Read the plugins skill for the complete,
  tested API reference covering tools, guards, routines and sandbox restrictions.
---

# Lua plugins

Read `/workspace/skills/plugins/SKILL.md` for the full implemented API. It is the
single reference for declarations, parameter schemas, callback contexts, results,
replay/cancellation, host-only configuration, and restricted workspace Lua.

Workspace scripts have no ambient host IO, environment or module access.
`ctx.sh` in a tool is the only command path and always enters the microVM.
Current source loading is at house startup; automatic next-turn reload is not
implemented. Do not assume undocumented context methods exist.
