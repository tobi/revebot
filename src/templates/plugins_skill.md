---
name: plugins
description: >
  Complete Reve Lua API reference: plugin tools, parameters, guards, cron routines,
  callback contexts, results, replay, cancellation, and host-only configuration.
  Use to write or debug Lua integrations, or when the user runs /plugins.
---

# Reve Lua API

This describes the **implemented API**, not planned features. All commands run
inside the mandatory microVM through `ctx.sh`. There is no host shell fallback.

## Files, loading, and scope

| Location | Declarations | Trust |
|---|---|---|
| `plugins/*.lua`, `tools/*.lua` at the house root | `tool`, `guard`, `cron`, `on_change` | Trusted host-installed code; outside the VM mount |
| `/workspace/plugins/*.lua` | `tool`, `guard`, `cron`, `on_change` | Restricted, bot-editable Lua |
| `/workspace/agents/<id>/plugins/*.lua` | `tool`, `guard`, `cron`, `on_change` | Restricted; definitions carry that folder's bot id |
| `/workspace/routines/*.lua` | `routine` | Restricted; specify `bot` or use `run` |
| `/workspace/agents/<id>/routines/*.lua` | `routine` | Restricted; `bot` defaults to that folder's id |

One file may contain several declarations. Only immediate `.lua` children load;
files are ordered by name. Keep declaration ids unique across the house.

**Current limitations:** source loads at house startup, not automatically on the
next turn. A broken file fails startup; last-good hot reload is not implemented.
Per-bot tool ownership is currently metadata: tools and guards are still shared
by all bots. Do not use folder placement as an authorization boundary. Duplicate
tool ids are unsupported (schema and invocation precedence currently differ).
Routines also share a global id namespace. Changes need a house restart.

Workspace source must be regular UTF-8 Lua text, at most 1 MiB per file. Symlinked
script files/directories and symlinked bot directories are rejected. Lua bytecode
is not accepted. Do not put secrets into source files.

## `tool(name, spec)`

Registers a model-callable tool. Rust house names (`update_state`, `cd`,
`CreateAgent`, `UpdateAgent`, `SendAgentMessage`, `SendUserMessage`,
`AskUserForSecret`) cannot be replaced. `update_state` supports `target="profile"`
(default) and `target="memory"`; the memory skill documents facts/tiers/scopes.
These are model tools, not Lua globals or context functions. `CreateAgent` accepts
`soul` for the initial SOUL.md, not a separate standing-instructions file.

`name` is a string; `spec` has:

| Field | Type | Default / meaning |
|---|---|---|
| `description` | string | `""`; model-facing explanation of the tool |
| `replay` | `"never"` or `"safe"` | `"never"`; see recovery below |
| `params` | array of parameter tables | `{}` |
| `run` | `function(args, ctx)` | Required; invoked after durable tool intent |

Each parameter table has:

| Field | Type | Default / meaning |
|---|---|---|
| `name` | string | Required; JSON property name |
| `type` | string | `"string"`; JSON Schema type, e.g. `integer`, `number`, `boolean`, `array`, `object` |
| `description` | string | `""`; model-facing field explanation |
| `required` | boolean | `false` |
| `default` | JSON-convertible Lua value | Absent; inserted if the property is missing |
| `enum` | array of JSON-convertible values | Absent; advertised allowed values |

The generated object schema disallows additional properties. Runtime preparation
applies defaults, then checks required properties. **Type, enum and extra-field
validation are not enforced by this Lua preparation layer**: check inputs before
using them. An empty `params` means no declared model arguments.

```lua
-- example: plugin
-- Read-only, but still uses the guest, never host IO.
tool("read_note", {
  description = "Read a small note in the shared workspace",
  replay = "safe",
  params = {
    { name = "path", type = "string", required = true,
      description = "Workspace-relative note path" },
  },
  run = function(args, ctx)
    assert(type(args.path) == "string", "path must be a string")
    return ctx.sh("head -c 16000 -- " .. ctx.shellescape(args.path))
  end,
})
```

### Tool callback: `run(args, ctx)`

`args` is a Lua table converted from the effective JSON arguments (after guards /
Rust hooks and Lua defaults). The context is:

| Member | Contract |
|---|---|
| `ctx.sh(command: string) -> string` | Execute in the house microVM, using default guest execution options. Returns stdout; appends stderr if nonempty. No options-table overload. |
| `ctx.shellescape(value: string) -> string` | Quote one shell argument; concatenate only the quoted result into commands. |
| `ctx.workdir: string`, `ctx.cwd: string` | Current conversation's guest cwd. Starts at `/workspace/agents/<id>/workspace`; `cd` changes it. Unscoped CLI calls use the configured guest workdir. Never a host path. |
| `ctx.home: string \| nil` | Fixed agent home `/workspace/agents/<id>` for bot calls; nil for unscoped calls. Shell `HOME` matches it even after `cd`. |
| `ctx.bot: string \| nil` | Invoking bot's id in a house conversation, including for shared plugins. Unscoped calls retain definition-owner metadata when present. |

`ctx.sh` and built-in relative-path tools follow the conversation cwd, not the
host's directory. Use the Rust `cd` tool (not a persistent shell `cd`) to change
it. The change survives restart, preserves HOME, and loads full ancestor
`AGENTS.md` files root-to-leaf. Each incoming message has a `<cwd>` header snapshot.
`SOUL.md` and memory remain anchored at HOME, independent of cwd.

`ctx.sh` is asynchronous from Rust's perspective: Lua writes ordinary sequential
code while the guest command runs. Sandbox/transport errors raise a Lua error.
A nonzero guest exit status is **not** automatically a Lua error: this API returns
text only, not `success`, `exit_code`, or `cancelled`. Implement a guest-side
output protocol if you need structured command status. Use guest command limits
(e.g. `timeout`, `curl --max-time`) when appropriate; no Lua timeout parameter exists.

Return values:

- string → tool result text;
- nil / no return → empty result;
- JSON-convertible table/number/boolean → serialized JSON result text;
- `error(message)` / failed `assert` / invalid return conversion → failed tool
  result, visible to the model. Avoid cycles and functions in returned values.

There is currently **no** `ctx.send`, `ctx.bots`, host exec, host read/write,
filesystem object, progress callback, or cancellation-token API on tool context.
Use Rust house tools such as `SendAgentMessage`, `SendUserMessage`, and
`AskUserForSecret` for those workflows. Lua cannot replace those reserved names.

### Replay and cancellation

`replay = "safe"` permits automatic re-execution after interruption only if both
the persisted declaration and the current declaration say safe. Use it only when
repeating the operation is safe. Sending messages, writes, deletes, deployments,
etc. should retain `"never"` (the default). Unknown replay strings currently map
to `"never"`; use the two documented values.

Tool arguments/intent are durable before invocation. A cancelled in-flight
`ctx.sh` receives the guest cancellation signal. Pure Lua itself is not
preemptively cancelled: don't spin or do long CPU work in Lua. There is no
process-level resource isolation for the host-side interpreter; use the VM for
substantial work. A cancelled command's text-only result does not expose its
cancelled flag, so avoid chaining unrelated effects blindly.

## `guard(id, spec)`

Registers a before-tool guard. `id` is a string. Fields:

- `tools: {string, ...}` — optional; empty/absent matches all tools.
- `run: function(event)` — required.

`event` contains **only** `tool_name: string` and `args: table`. There is no `ctx`,
bot id, conversation id, run id, or shell capability. Return nil to allow. Return
`{ block = "reason", terminate = false }` to block; `terminate = true` requests
termination through the harness's tool-result rules. Other return fields do not
rewrite arguments. Guards run in load order; the first block stops the chain.
An error fails closed and blocks the call.

```lua
-- example: plugin
guard("no_force_push", {
  tools = { "bash" },
  run = function(event)
    local cmd = event.args.command or ""
    if cmd:find("git push", 1, true) and cmd:find("--force", 1, true) then
      return { block = "Force-push needs explicit user approval", terminate = false }
    end
  end,
})
```

This example is a heuristic for one tool name, not a complete security policy:
other tools/commands can perform equivalent operations. Guards apply to harness
calls, not every direct CLI/HTTP tool invocation.

## `on_change(id, spec)` — post-write observations

Registers an observer in a plugin file. This is notification **after** a write,
not authorization; use `guard` for pre-tool policy.

| Field | Type | Default / meaning |
|---|---|---|
| `paths` | string array | Empty matches any known path. Guest absolute glob patterns; any match passes. |
| `resources` | string array | Empty matches any kind. Supported: `profile`, `soul`, `memory`, `directory_rules`, `skills`, `plugins`, `routines`, `vm`. |
| `include_unknown` | boolean | `false`. Opt into shell/plugin effects whose changed paths are unknown. Those events bypass path/kind filters conservatively. |
| `run` | `function(event, ctx)` | Required. Return value ignored. |

Known events must pass both nonempty filters. Bot-local observers only receive
events originating from their owning bot; house observers see all bots. Ordinary
files can be watched by path even when they have no special resource kind.

`event` contains `bot: string`, `cwd: string`, `paths: {string,...}`,
`resources: {string,...}`, `unknown: boolean`. It contains **no file contents or
credentials**. `unknown` means the tool may have written indirectly; it is not a
claim that any particular file changed. `write`/`edit` supply known paths;
shell/plugin effects are conservative unknown notifications. Supported structured
profile/memory/SOUL writes also publish events.

An unscoped HTTP/CLI exec/tool call has `event.bot = ""`; its `ctx.bot` is nil.
Do not invent an originating bot. Bot-local observers do not receive those events.

Observer context has `ctx.bot`, `ctx.cwd`, and
`ctx.send(bot_id: string, text: string)`. Sends are collected, then queued durably
for a later bot run only if that observer succeeds (max 32 sends per callback).
No `ctx.sh`, `ctx.home`, `ctx.workdir`, `ctx.bots`, or direct mutation API is exposed
here. Ask a bot to do work rather than blocking the write path.

```lua
-- example: plugin
on_change("review_memory_updates", {
  resources = { "memory" },
  paths = { "/workspace/agents/*/memory/*" },
  run = function(event, ctx)
    -- Pure callback: an error here cannot undo the original write.
    assert(event.unknown == false)
    -- Optional: ctx.send("chief-of-staff", "Review the changed memory file.")
    -- Only send when action is needed, to avoid self-triggering message loops.
  end,
})
```

Observers run asynchronously in registration order, outside the original tool's
result path. An error discards that callback's sends and is logged; other
observers still run. Delivery is **best effort, in memory**: events can be lost
at crash or subscriber overflow, and repeated operations can produce repeated
notifications. This is not an exactly-once job scheduler. There is no automatic
observer replay or registration/unsubscribe API at runtime yet; definitions load
at startup. External-editor changes are reread on the next API/model input; this
facility is a write-path observer, not a continuously polling filesystem watcher.

## `routine(id, spec)` and `cron(id, spec)`

`routine` is the declaration in routine files. `cron` is the corresponding
schedule declaration in plugin files. Both have these fields:

| Field | Type | Default / meaning |
|---|---|---|
| `cron` | string | Required; five fields: minute, hour, day-of-month, month, day-of-week |
| `name` | string | Defaults to `id`; display label |
| `enabled` | boolean | `true`; enables scheduled firing |
| `bot` | string | Defaults to owning bot folder; absent for house-wide files |
| `message` | string | Message sent to `bot` when no `run` function is supplied |
| `run` | `function(ctx)` | Optional; takes precedence over `bot` + `message` |

Provide either `bot` + `message` (including the inherited bot) or `run`.
The cron parser accepts numeric values, `*`, lists, ranges and step expressions;
scheduling uses the host's local time. Use unique ids, including between bots.

```lua
-- example: routine
routine("chief_weekday_briefing", {
  name = "Weekday briefing",
  cron = "0 9 * * 1-5",
  enabled = false, -- enable after testing
  bot = "chief-of-staff",
  message = "Summarize what needs my attention.",
})
```

Routine `run(ctx)` has a **different** context from tool `run(args, ctx)`:

| Member | Contract |
|---|---|
| `ctx.bot: string \| nil` | Resolved routine `bot`, including owning-folder default. |
| `ctx.send(bot_id: string, text: string)` | Collect a send; returns no value. After the callback succeeds, Rust delivers collected messages in order. |

```lua
-- example: routine
routine("reviewer_morning", {
  cron = "0 10 * * 1-5",
  bot = "chief-of-staff",
  enabled = false,
  run = function(ctx)
    ctx.send(ctx.bot, "What needs attention?")
  end,
})
```

There is no `ctx.sh`, `ctx.shellescape`, `ctx.workdir`, `ctx.bots`, or priority
options table in a routine callback. Ask the bot to do guest work in the message.
The callback's return value is ignored. If it errors, its collected sends are not
delivered. Successful `ctx.send` calls are collection, **not durable receipts**;
later delivery may partially succeed if another target fails.

Current scheduling uses in-memory tick deduplication and the bot's normal main
conversation. Persistent per-routine conversations, isolation from human turns,
run history, and missed-tick/retry policy are planned, not implemented here.
`enabled = false` disables scheduled firing; the backend's explicit run endpoint
can still run a disabled definition (the current UI hides that action).

## Host-only configuration appendix

`config.yml` is preferred host configuration. Only when it is absent, the trusted
host loads `agent.lua` and `sandbox.lua`. These declarations are **not available**
to workspace Lua. Do not modify host launch code through a bot plugin.

`agent { ... }`: optional string fields `model`, `thinking`, and `name`.

`sandbox { ... }`: all fields optional; omitted fields retain policy defaults:

| Field | Type / meaning |
|---|---|
| `image` | string; guest image |
| `cpus` | unsigned 8-bit integer; default 2 |
| `memory` | unsigned 32-bit integer, MiB; default 8192 |
| `root_disk` | unsigned 32-bit integer, MiB |
| `workdir` | string; default `/workspace` |
| `name` | string; explicit sandbox name |
| `provision` | boolean; default false for the pre-provisioned image |
| `mount_workspace` | boolean; default true |
| `open` | boolean; default true for public-internet egress |
| `allow` | string array; allowed hosts, merged/deduplicated |
| `packages`, `mise`, `npm`, `bootstrap` | string arrays; provisioning packages/tools/guest commands |
| `env` | string-to-string table; merged guest environment, not a secret store |
| `secrets` | array of scoped secret definitions below |

Each secret needs `env: string`, `source: string`, and nonempty `hosts: {string,...}`.
Optional strings: `placeholder`, `header`, `prefix`. Sources are host environment
references or supported host-configured sources (`$(command)`, `file:...`, HTTP(S)).
No literal `value` field. `$(command)` sources reject nested substitution. Secret
resolution is host configuration authority, never a workspace Lua command API;
the guest receives placeholders, with values injected only for configured hosts.

Both Lua states remove `os.execute`, `io.popen`, `os.exit`, and `package.loadlib`.
Trusted host Lua otherwise retains ambient host capabilities and must never load
bot-provided source or expose its host functions to workspace code.

## Restricted workspace standard library

Workspace Lua lives in a separate state from trusted host Lua. It gets `table`,
`string` (without `dump`), `math`, `utf8`, and `coroutine`, plus these base names:

`_G`, `_VERSION`, `assert`, `error`, `getmetatable`, `ipairs`, `next`, `pairs`,
`pcall`, `rawequal`, `rawget`, `rawlen`, `rawset`, `select`, `setmetatable`,
`tonumber`, `tostring`, `type`, `xpcall`.

No `io`, `os`, `package`, `require`, `debug`, `load`, `loadfile`, `dofile`,
`print`, `warn`, or `collectgarbage`. Host globals/registry values are not shared.
Use local helpers within a file; modules and dynamic loading are not supported.
Keep top-level code to declarations and pure initialization.
