# Bot-contract roadmap

The durable session/message engine stays. This roadmap hardens the bot layer
around it; it is not a claim that the planned features below already exist.

## Product model

Persistent named bots are the sidebar's primary objects. Each owns identity,
standing instructions, memory, tools and a collection of durable conversations.
Bots communicate asynchronously: persist the receiving inbox before acknowledging;
never block the sender waiting for a colleague to finish.

### Conversation tabs (user requirement)

```text
Main | Side conversation | +                              Routines
```

- **Main** is the bot's primary, important conversation.
- **+** creates a persistent **side conversation**. Use that terminology in the UI,
  not subagent, task, or temporary chat.
- Pin **Routines** at the far right. This is a special view listing the chats for
  that bot's routines, with navigation into each chat.
- Every routine has **one continuous conversation**. Subsequent ticks append to
  that same chat; a run is not a new conversation.
- Main, side conversations and routine chats share the owning bot's identity,
  memory and tool context, but have distinct transcripts, operation ownership and
  cancellation. Main/side typing or abort must not cancel a routine.
- Store conversation kind/identity and routine-to-conversation association
  explicitly. Do not reconstruct them from a transcript scan. Existing harness
  lanes are the likely execution primitive; preserve one session writer.
- Persist selected bot/conversation in navigation so reload restores the view.
  Clients must reconcile snapshot and events without mixing tabs during switches.

“Agent context” here means the owning bot's identity/resources. Whether a new side
conversation should optionally fork existing main history remains a product choice;
shared identity must not implicitly merge independently running transcripts.

## Ordered work

| Slice | Goal / done condition | Status |
|---|---|---|
| Workspace Lua boundary | Separate restricted state; no ambient host IO/environment/modules; descriptor-rooted source reads; regression tests | Implemented on `feat/bot-contracts` |
| Bot identities | Directory-derived immutable ids; reject profile mismatch/traversal/duplicate identities and symlink escapes before privileged host IO | Next |
| Live configuration | Current profile reaches prompt; a defined run boundary snapshots model/config; updates no longer report misleading success | Planned |
| Notice delivery | Durable unique notice identity; acknowledge/publish only acceptance; live/restore reconciliation; no bubbles from blocked tool intent | Planned |
| Roster lifecycle | Serialized create/update/delete decisions with revision/reservation identity around guest IO; preserve last-bot invariant | Planned |
| Routine conversations | Independent persistent chats/cancellation plus durable trigger/run identity, outcome history and explicit missed/duplicate tick policy | Planned |
| Plugin contracts | Last-good atomic reload, per-bot scope, consistent name resolution, complete documented bot/conversation/messaging context | Planned |
| Memory | Write/forget API, tiers/scopes, dedupe, filesystem reads and bounded profile/recent-log prompt projection | Planned |
| Tabs/status/client | Main/side/+ and pinned Routines UI, per-call descriptions, reconnect/resnapshot/lag handling, safe text rendering | Planned |

The entire implemented Lua surface lives in
[`src/templates/plugins_skill.md`](../src/templates/plugins_skill.md). Update that
reference and its executable-example tests with every API change. Do not describe
future context methods, reload behavior or conversation isolation as implemented.

## Separate runner work

`feat/background-runner` is an unfinished, separate worktree. Its intended contract:
self-detach one runner per house, HTTP over the private Unix socket and localhost,
exact executable-hash checking, consent before mismatched restart, and `revebot stop`.
It must route all normal CLI clients through that same instance and pass validation
before integration. Do not merge unfinished daemon work into these fixes.

## Validation gates

Each slice carries focused regression tests plus the existing suite, architecture
updates, and relevant Lua API docs. Keep microVM integration tests opt-in and never
introduce a host-shell fallback for testing. No automatic merge into main.
