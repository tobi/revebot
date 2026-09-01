# Changelog

All notable changes to Reve are documented here. The project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0]

Reve is now a Rust crate with Lua for scripting, superseding an earlier Ruby prototype as a
fresh 0.1.0. The core is Rust (edition 2024); everything an agent author writes —
configuration, project tools, sandbox policy — is Lua that vendors into the binary and
starts in microseconds. Concurrency is tokio tasks over single-owner session state.

### Fixed

- A failed or interrupted microVM rebuild could leave `.reve/sandbox-fingerprint`
  naming the *previous* policy while the disk had already been replaced; reverting
  `config.yml` then reused a disk built for a different policy. The fingerprint is now
  forgotten before `build()`.
- A secret saved through `AskUserForSecret` (or rotated in the host environment) never
  reached the guest while the house held the VM: the hold counted as a live effect, so
  the restart that applies `next_start` secret definitions could not run, and
  `upsert_secret` recorded the new digest as if it had. Holds are now tracked apart
  from effects, and the digest keeps describing the running guest, so the next
  effect-idle acquire restarts with the current secrets.

### Added

- **Tailscale** — `revebot serve` detects a running host `tailscaled` via the
  LocalAPI socket and binds the same HTTP/WS surface on the node's Tailscale
  IPv4 (same port as `--bind`). The bearer token is still required; the tailnet
  URL is printed at start. No userspace node, no TUN, no `--tailnet` flag.
- **Skill improvement** — Hermes `skill_manage` / `skill_view` / `skills_list` as
  house tools. Class-level create/patch, archive-on-delete, `/learn`, and a
  hidden nudge every 15 user turns. Creates are curator-managed. Bundled skills
  are off-limits. No aux-model background fork (nudge + foreground tools instead).
- **Curator** — Hermes-style skill-library maintenance, host-side. Usage sidecar
  at `.reve/curator/usage.json`, `active → stale → archived` (never delete),
  pin/adopt, interval prune, snapshots under `.reve/curator/backups/`. CLI:
  `revebot curator`. Bundled `/curator` skill for the optional umbrella-building
  pass. Catalog skips hidden dirs so archives do not reappear as live skills.
- TLA+ models of the durable rules, checked by `make tla` inside `make ci` with
  [tla-rs](https://github.com/fabracht/tla-rs): `docs/tla/DurableLog.tla` (the JSONL file
  as replay recipe, torn-line atomicity, write-once ids, seq monotonicity, compaction
  equivalence) and `docs/tla/DurableHarness.tla` (two interleaved lanes; steer, follow-up,
  deferred-write and nextRun inboxes; abort as control; the effect sandwich; parallel tool
  batches with source-ordered intent and result commits; crash and recovery; the terminal
  transaction) and `docs/tla/VmLifecycle.tla` (the shared microVM under updates: policy
  edits, secret saves and rotations, house crash and restart). Twenty-seven `Inv*` properties, each proven falsifiable
  by an injected bug. Modelling `cancelQueued` fixed its triage before it is implemented:
  an abort-drained id is `not_found` and keeps its payload register.
- Default guest is [`ghcr.io/tobi/wrap:desktop`](https://github.com/tobi/wrap): unprivileged
  `user`, XFCE on `:1`, noVNC/VNC on localhost, shared Chrome with `agent-browser` on CDP
  9222. The house Screen panel is a live preview; click takes over the desktop. House
  skills `/browser` and `/computer` teach the bot to steer Chrome and other GUI. Default
  memory is 8192 MiB.
- Direct dependency on the [`microsandbox`](https://github.com/superradcompany/microsandbox)
  Rust crate (pinned `=0.6.8` in `Cargo.toml`) and `microsandbox-network`. No FFI shim, no
  CLI, no daemon, no host shell: the crate is linked and called directly. Mandatory microVM
  isolation with deny-by-default egress — the policy is built in Rust from
  `NetworkPolicy::none()` plus one narrow gateway-DNS rule plus one allow rule per host
  named by `sandbox.lua` — workspace-only bind mounts, source-backed host-scoped secret
  substitution with placeholders, fingerprint-based VM reuse, runtime env/secret refresh
  without disk rebuild, fail-closed boot verification, effect-driven restart, 30-second
  idle shutdown, and cancellation that kills the guest command through the exec control
  channel. Verified live against a real microVM by the opt-in
  `cargo test --test microvm -- --ignored` tests.
- The agent directory: `reve init` scaffolds `agent.lua`, `sandbox.lua`,
  `tools/example.lua`, `instructions.md`, `models.yml`, `workspace/{AGENTS.md,SOUL.md,
  KNOWLEDGE.md,HEARTBEAT.yml,knowledge/,notes/,skills/}`, and `.gitignore`. Idempotent; an
  agent-dir guard refuses to run outside one.
- The Lua scripting surface (`src/lua.rs`): `agent { … }`, `sandbox { … }`, and
  `tool("name", { … })`. Tool `params` become the JSON schema the model sees; `ctx.sh`
  runs in the microVM and is a tool's only command path; `ctx.workdir`, `ctx.shellescape`.
- The CLI (`src/main.rs`): `reve init [dir]`, `reve info`, `reve exec <cmd...>`,
  `reve tool [name] [--args JSON]`, `--version`.
- Pi-compatible `read` offsets and limits, plus bounded model context for long tool output;
  complete results spill to a model-readable guest `/tmp` path.
- The durable wire format (`src/records.rs`): JSONL version 4, one line per mutation in
  three shapes — `header`, `record`, `entry`. Entries are the conversation tree; records
  are metadata. Intent-before-effect records with provisioned ids; `Replay::{Safe,Never}`.
- Single-owner session state (`src/storage/`): entries, records, lanes, facts, one
  monotonic `seq`. JSONL append with flush, torn-tail truncation on reopen, and a
  malformed line in the middle refused as corruption.
- The inline ratatui terminal, including durable turns, streaming Markdown, slash commands,
  and workspace-relative `@file` completion with live post-run refresh.
- OpenAI Chat Completions transport with streaming text and tool-call assembly, usage
  accounting, configurable developer/system roles and token-cap fields, and durable tool
  continuation repair.

### Fixed

- Serialized microVM stop and restart transitions so concurrent tools and the first effect
  after idle shutdown reuse one sandbox without racing its persisted runtime.
- Kept every house-tool parameter schema rooted at a plain JSON object so strict
  OpenRouter providers accept turns that expose `SendUserMessage`.
- Added `/compact`, `/new`, and `/fork` to browser slash autocomplete alongside
  the current skills and plugin commands.

### Pending

The remaining engine-level limitation is provider tool continuation from the standalone
CLI tool command; normal TUI turns run the durable lane.

### Security

- No host-shell, local, CLI, or FFI fallback. Reve fails closed if the microVM cannot boot.
  Credentials require explicit host-scoped configuration; the guest sees only a placeholder
  and the real value is injected at the network boundary.

[0.1.0]: https://github.com/tobi/reve/releases/tag/v0.1.0
