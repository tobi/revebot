# Reve

[![CI](https://github.com/tobi/reve/actions/workflows/ci.yml/badge.svg)](https://github.com/tobi/reve/actions/workflows/ci.yml)
[![MIT License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Reve is a durable coding agent built in Rust with Lua for scripting. Its central idea is:

> **The directory is the agent.**

An agent is not a profile hidden in a home directory or state scattered across services.
It is one portable directory containing identity, model configuration, tools, mutable
memory, skills, sandbox policy, and durable history. Copy the directory and you copy the
agent.

## Philosophy

Reve combines three ideas:

1. **Rust is the core; Lua is the agent language.** The engine is a Rust crate. Everything
   an agent author touches — its configuration, project tools, sandbox policy — is Lua:
   ordinary, readable, vendored into the binary, started in microseconds. Concurrency is
   tokio tasks over single-owner session state, not a shared mutable graph. A small task
   boundary keeps the durable core independent from anything that renders it.
2. **The environment is a real sandbox.** Every model-authored command and every tool's
   `ctx.sh` runs in a full microVM — never in a host-shell fallback. Reve links the
   official [`microsandbox`](https://github.com/superradcompany/microsandbox) Rust crate
   directly (pinned `=0.6.8`): no FFI shim, no CLI, no daemon, no host shell. The host
   only orchestrates; the agent works inside its mounted `workspace/` with public
   internet by default and explicitly scoped secrets. Set `open = false` in
   `sandbox.lua` to lock down to an allowlist.
3. **State is durable data, not process memory.** Reve's append-only conversation tree and
   intent-before-effect records are based on the durable harness work in
   [Pi](https://github.com/badlogic/pi-mon). A crash can leave an incomplete operation,
   but not an ambiguous one: recovery has the identifiers and intent needed to reconcile
   it safely.

The result is an agent you can inspect with normal filesystem tools, constrain as an
operating environment, stop at any moment, and resume without pretending the
interruption never happened.

There is no machine-wide Reve profile, home-directory prompt, global model file, or session
store outside the agent directory.

## Install and create an agent

Requirements:

- Rust 1.91 or newer.
- Linux with KVM, or macOS on Apple Silicon, as required by the embedded microsandbox
  runtime.
- Network access on first use to fetch the microsandbox runtime once into
  `~/.microsandbox` and pull the VM image.

```bash
cargo fetch --locked            # once after setup or a lockfile update
make install                    # selected toolchain; locked, offline, revebot only
mkdir my-house && cd my-house
revebot init
export OPENROUTER_API_KEY=...
revebot                         # boots the microVM and serves http://127.0.0.1:7420
```

`make install` makes an inherited `RUSTUP_TOOLCHAIN` selection explicit. Use
`make install INSTALL_TOOLCHAIN=<toolchain>` to choose a different installed toolchain;
without either variable, Cargo uses its normal default. The reviewed lockfile is always
used, and installation stays offline after dependencies are fetched.

The first launch builds and provisions the microVM and shows live startup progress. Bare
`reve` verifies that the VM can boot, then releases it until the first sandbox effect.
Later launches reuse its persisted root disk.

## The CLI

`revebot` subcommands:

| Command | Purpose |
|---|---|
| `revebot` / `revebot serve` | Boot the house: one microVM, HTTP+WS on `127.0.0.1:7420`. A running Tailscale is detected and served too (token required). |
| `revebot tui` | Terminal UI for the first bot (`chief-of-staff`). |
| `revebot init [dir]` | Scaffold a house (idempotent). |
| `revebot info` | Show model, sandbox policy, egress hosts, and tools. |
| `revebot exec <cmd...>` | Run a command inside the house microVM. |
| `revebot tool [name] [--args JSON]` | Run one of this house's Lua tools. |
| `revebot eval` | Run `evals/` (offline by default; `--live` for real models). |
| `revebot curator …` | Maintain the skill library (status, prune, pin, adopt, archive). |
| `revebot --version` | Print the version. |

A worked session:

```bash
$ mkdir my-agent && cd my-agent
$ reve init
initialised /home/you/my-agent
  + config.yml
  + tools/example.lua
  + models.yml
  + workspace/VM.md
  + workspace/HEARTBEAT.yml
  + workspace/agents/chief-of-staff/SOUL.md
  + workspace/agents/chief-of-staff/AGENTS.md
  + workspace/agents/chief-of-staff/profile.json
  + workspace/agents/chief-of-staff/memory/profile.md
  + .gitignore

  edit workspace/agents/chief-of-staff/SOUL.md, then run revebot here

$ reve info
root      /home/you/my-agent
model     openrouter/x-ai/grok-4.6
thinking  low
sandbox   debian:trixie-slim (2 cpu, 2048MB)
egress    api.github.com, codeload.github.com, github.com, objects.githubusercontent.com, raw.githubusercontent.com
tools     example

$ reve exec -- git log --oneline -n 3
  · restarting microVM reve-my-agent-1b3d5e7f92
  ✓ sandbox ready
abc1234 init
def5678 add tools
ghi9012 fix provision

$ reve tool example --args '{"commits":2}'
branch: main
status:
(clean)

recent commits:
abc1234 init
def5678 add tools
```

`reve exec` joins the command vector and runs it through `sh -lc` in the guest, so the
provisioned login PATH (mise shims included) is in effect. Its exit status becomes the
process exit code. `reve tool` loads the agent, boots the VM, runs the named Lua tool, and
stops the VM; with no `name` it lists every tool and its description.

In the TUI, type `@` at the start of a token to complete a file under `workspace/`.
Candidates are workspace-relative, skip hidden files, show file sizes, and refresh after
each run so files the agent just created are immediately available. Press Tab to accept the
highlighted path.

## Agent directory

`revebot init` scaffolds this layout without overwriting edited files:

```text
my-house/                      the house root
├── config.yml                 house runtime: model, sandbox, secrets
├── tools/
│   └── example.lua            a host-trusted Lua tool
├── models.yml                 provider catalog
├── workspace/                 the shared VM filesystem, not a shared persona
│   ├── VM.md                  machine facts only
│   ├── memory/user/           intentionally shared user facts
│   ├── projects/              shared project work and explicitly scoped memory
│   ├── skills/                house-level skills, including memory and plugins
│   ├── plugins/               restricted Lua plugins (e.g. web_fetch)
│   └── agents/
│       └── chief-of-staff/    this bot's fixed HOME
│           ├── SOUL.md        sole identity, voice and standing remit
│           ├── AGENTS.md      neutral inherited home operating rules
│           ├── profile.json   authoritative name/title/avatar/model/projects
│           ├── workspace/     initial working directory
│           ├── skills/        bot skills
│           ├── memory/
│           │   ├── profile.md       enduring facts
│           │   ├── log/YYYY-MM.md   dated history (created on first write)
│           │   └── notes/YYYY-MM.md short-lived notes (created on first write)
│           ├── knowledge/     longer reference material, read on demand
│           ├── notes/         working notebook, read on demand
│           ├── routines/      this bot's cron definitions
│           ├── plugins/       this bot's restricted Lua definitions
│           └── sessions/      durable JSONL (gitignored)
├── .gitignore                 ignores .reve/ and bot sessions
└── .reve/                     durable host state (created on first launch)
```

`reve init` is idempotent: a file that matches the template is left `unchanged`; a file you
have edited is reported as `changed` and kept as you wrote it; a missing file is created.
Every agent has its own SOUL.md. Missing home files are scaffolded; edited files stay
unchanged. Shared context comes from VM.md and explicitly shared memory, not a global
personality or project-knowledge file.

`revebot` refuses to run in a directory that is not a house. A house directory needs
`config.yml` (or a leftover `agent.lua`). Each bot's personality and standing remit live
in `workspace/agents/<id>/SOUL.md`. Its metadata is reread from `profile.json`; tool writes
trigger sidebar/header refresh, and malformed edits produce a visible error.

### House config

`config.yml` selects the house model, thinking level, and sandbox. Each bot's prose
identity is its own `workspace/agents/<id>/SOUL.md`. Model changes take effect at the
next idle run boundary; an in-flight drive keeps its captured model. The real template:

```yaml
model: openrouter/x-ai/grok-4.6
thinking: low
sandbox:
  image: ghcr.io/tobi/wrap:desktop
  memory: 8192
  open: true
  secrets:
    - env: GITHUB_TOKEN
      source: "$(gh auth token)"
      hosts:
        github.com:
          allow: true
        api.github.com:
          allow: true
```

There is no `host-exec`, and omitting a secret does not create an invisible host fallback.

### Working directories and memory

Every bot starts at `$HOME/workspace`, where the **agent** HOME is `/workspace/agents/<id>`.
The guest Unix account is `user` (`HOME=/home/user`). The `cd` tool changes the conversation's cwd without moving agent HOME. Relative filesystem
tools, bash and Lua `ctx.sh` follow cwd. Each incoming message carries a `<cwd>` header.
`cd` reads the full guest ancestor chain of AGENTS.md files, root-to-leaf; nearest rules
win. Existing workspace-root AGENTS.md still applies by ancestry, not as a persona file.
Directory state is persisted in the conversation's session and refreshed on restore.

`update_state` supports `target: "memory"`, `action: "write" | "forget"`, exact `fact`,
`profile | log | note` tiers and `agent | user | project` scopes. Private by default;
project context requires explicit `profile.projects` membership. Read with ordinary
filesystem tools. The `memory` skill documents dedupe, recency, limits and exact forget.

Write/edit and indirect shell/plugin effects share a resource-change notification path.
Lua plugins can subscribe with `on_change`; this is best-effort observation, not a durable
scheduler. See the complete API and executable examples in the `plugins` skill.

### Add a tool by dropping in one Lua file

Host-trusted tools live in `tools/*.lua` (outside the mount). Bot-editable plugins live in
`workspace/plugins/` and `workspace/agents/<id>/plugins/` — they reload on the next house
start and are documented in full by the `plugins` skill. Workspace Lua uses a separate,
restricted state with no host IO, environment or module loading; symlinked source is
refused. `ctx.sh` always runs in the microVM. A bad script currently fails startup;
last-good hot reload is not implemented yet.
The real host template, `tools/example.lua`:

```lua
-- Every tools/*.lua file is trusted launch code, loaded before any work runs.
-- There is no plugin manifest and no registry to edit: drop in a file.
--
-- The Lua body runs on the host, but `ctx.sh` executes in the microVM. That is
-- the only command path a tool has.

tool("example", {
  description = "Summarize the working tree: branch, status, and recent commits",
  replay = "safe", -- read-only, so recovery may re-run it

  params = {
    { name = "commits", type = "integer", description = "How many commits to list", default = 5 },
  },

  run = function(args, ctx)
    local branch = ctx.sh("git rev-parse --abbrev-ref HEAD 2>/dev/null || echo '(no repo)'")
    local status = ctx.sh("git status --short 2>/dev/null || true")
    local log = ctx.sh("git log --oneline -n " .. tostring(args.commits) .. " 2>/dev/null || true")

    if status == "" then status = "(clean)\n" end
    return ("branch: %sstatus:\n%s\nrecent commits:\n%s"):format(branch, status, log)
  end,
})
```

A tool declares `params`, and each parameter becomes one property of the JSON schema the
model sees (`name`, `type`, `description`, `required`, `default`, `enum`). Declared
defaults are applied to the arguments the model supplies, and a missing required argument
is rejected before the tool runs. `replay` is `"safe"` (read-only, may be re-run during
recovery) or `"never"` (the default). The `ctx` table passed to `run` exposes:

- `ctx.sh(command)` — run a command in the microVM; returns stdout, or stdout+stderr when
  stderr is non-empty. Nonzero exit is not automatically a Lua error; this API does not
  return the exit code or cancellation flag.
- `ctx.workdir` — the guest working directory (`/workspace`), not a host path.
- `ctx.shellescape(s)` — quote a string for safe shell interpolation.

A file may declare several tools and use any Lua logic needed to implement a complex
integration.

### Local configuration

`models.yml` is YAML and belongs to the agent root. Runtime reads only this file — there
is no home-directory or global fallback. Every `apiKey` must be a `$ENV_VAR` reference;
secrets do not belong in this file. The real template:

```yaml
# This agent's model configuration. Runtime reads only <agent>/models.yml —
# there is no home-directory or global fallback.
#
# Every apiKey must be a $ENV_VAR reference; secrets do not belong in this file.
providers:
  openrouter:
    baseUrl: https://openrouter.ai/api/v1
    api: openai-responses
    apiKey: $OPENROUTER_API_KEY
    models:
      - id: x-ai/grok-4.6
        reasoning: true
        contextWindow: 500000
        maxTokens: 8192
```

## The mandatory sandbox

Reve links the `microsandbox` Rust crate directly (pinned `=0.6.8` in `Cargo.toml`). There
is no CLI, no daemon, no FFI shim, and no host-shell path: if the VM cannot boot, the agent
refuses to run rather than quietly executing model-authored commands on your machine.

Egress is **open to the public internet by default**. The policy uses microsandbox's
`NetworkProfile::Public` (plus the gateway-DNS rule names need). Set `open = false` in
`sandbox.lua` and list hosts in `allow` to lock down: that path starts from
`NetworkPolicy::none()` and only those hosts are reachable. Private/LAN and cloud
metadata stay off unless you opt into them.

Secrets are scoped, never borrowed implicitly. Each secret names a host environment
`source` and a per-host map (`allow`, optional `headers` such as
`Authorization: "Bearer $ENV"`). Hosts with `allow: true` also join the sandbox allow
list. Microsandbox persists only that source reference, resolves its value when the VM
starts, exposes only the placeholder in the guest, and substitutes the real value at the
network boundary. An unscoped secret (no `hosts`) is refused at load time. Removing a
secret from `sandbox.lua` removes its persisted definition before a reused VM starts.

The VM is reused by fingerprint. A stable hash of the disk and toolchain shape is written
to `.reve/sandbox-fingerprint`; runtime environment values and secrets are excluded. A
matching launch restarts the provisioned VM instead of reinstalling APT and language tools.
Changing a secret source value refreshes the source-only definition and restarts the same
disk; it does not rebuild the VM. Ordinary sandbox environment values are applied to each
exec and likewise do not affect disk reuse.

Bare TUI sessions verify that the VM boots, then release it until a command or tool needs
the sandbox. After the last effect, a 30-second idle window keeps follow-up commands fast;
then the VM stops while retaining its disk. Idle TUI sessions therefore do not reserve the
sandbox. A VM actively owned by another `reve` process is never adopted or replaced,
because that would break isolation for both processes.

Cancellation kills the guest command. `Sandbox::exec` takes an optional cancel receiver; on
cancel it calls `control.kill()` through the exec control channel and returns a cancelled
result (exit 130). The VM stays usable for follow-up effects and then follows the same idle
shutdown lifecycle. The hand-rolled cancel channel (`tokio_util_lite`) is one bit,
delivered once.

`workspace/` is the only writable bind mount, mounted at `/workspace` and set as the
working directory, so relative paths mean the same thing on the host and in the VM. The
agent's own definition files stay outside it. The default image is `debian:trixie-slim`,
provisioned with `git`, `gh`, `ripgrep`, `fd-find`, `jq`, `build-essential`, and Node
(via mise); ast-grep comes from npm. A non-zero exit is data, not an error: the model reads
the code and stderr and decides what to do next.

Built-in `read` accepts Pi-compatible 1-indexed `offset` and `limit` parameters, reports an
offset past end-of-file, and gives the next offset when a limit stops early. Tool results
longer than 24,000 characters are shortened in model context; the complete result is saved
inside the VM under `/tmp/reve-tool-output-*.log`, and the model receives that path.

These guarantees are verified live against a real microVM by the opt-in integration tests:

```bash
cargo test --test microvm -- --ignored
```

They confirm a Lua tool's `ctx.sh` running in the guest with the workspace bind mount
readable and writable; a released VM restarting on its next effect; `github.com` reachable
while an unlisted host is blocked; and cancellation killing the guest command with the VM
still usable afterwards.

## Durable records

One session is one JSONL file under `.reve/sessions/`, one line per mutation, in exactly
three shapes — `header`, `record`, and `entry`:

```jsonl
{"kind":"header","version":4,"id":"...","cwd":"workspace"}
{"kind":"record","type":"operation_started","lane":"main","intent":{"kind":"run"}}
{"kind":"entry","lane":"main","type":"message","message":{"role":"user"}}
```

The format version is `4`; there is no v3 compatibility (reve is new). **Entries are the
conversation tree; records are metadata.** Deleting every record must still leave a valid
conversation — that invariant is what lets compaction and recovery rewrite bookkeeping
without touching history. An entry's `parent_id` is what makes it a tree rather than a log:
compaction and branching re-parent instead of deleting, so history is never destroyed.

The envelope is typed; the payload stays `serde_json::Value`. `header` carries the version,
session id, and `cwd`. `record` carries bookkeeping (`operation_started`,
`operation_finished`, `tool_started`, `lane_leaf_set`, `fact_set`, …). `entry` carries a
`message` (conversation turns) or `data` (custom entries).

The durability rule: write the intent record before the effect, name the ids it will
produce, then append the result with exactly those ids. Ids are provisioned *before* the
effect they name — a `tool_started` record carries the id of the result entry that does not
exist yet, so recovery can tell "never ran" from "ran, result lost" without guessing.
Replay is only safe when the recorded declaration **and** the current one both say `safe`;
a tool that became effectful must not be replayed on the strength of an old record.

Every append is flushed. The only failure a crash can produce is a torn last line; on
reopen, reve truncates back to the last complete line and resumes appending. A malformed
line anywhere *else* is not something a crash can do, so it is corruption and reve refuses
to open the file.

## The durable harness

Every mutation follows one sequence: **record the intent, perform the effect, append the
result under the id the intent named.** One run writes:

```text
record operation_started  { runId, intent: { kind: "run" } }
entry  user
record task_attempt       { runId, attempt }
entry  assistant
record tool_started       { runId, toolName, resultEntryId, replay }   <- intent
entry  toolResult                                                      <- effect, that id
record operation_finished { runId, outcome }
```

A crash therefore leaves either a completed operation or an incomplete one that says
exactly what it was about to do — never an ambiguous one.

**Recovery** is a reduction over two bounded reads: which operations were opened and never
finished, and which of their declared results never landed. Every missing result is
produced. A tool is re-run only when the recorded declaration *and* the current one both
say `safe` — a tool that has since become effectful is never replayed on the strength of an
old record; it gets a synthetic interrupted result instead. Recovery closes every operation
it touches, so running it twice is a no-op.

**Abort** is reconciliation, not abandonment. A cancelled run still writes the result entry
it promised and still closes its operation, and in the VM the guest command is actually
killed through the agent's control channel.

This is tested against a really-killed process: `tests/crash.rs` spawns a child, waits
until it is inside a tool, `SIGKILL`s it, and then reduces whatever the dead process left
on disk. Nothing about it is simulated.

## Built runtime surface

The durable harness and the user-facing runtime are implemented:

- **Providers** — OpenAI Responses, OpenAI Chat Completions, and Anthropic
  Messages are thin `reqwest` adapters with SSE streaming, partial tool-call assembly,
  usage accounting,
  cache diagnostics, transient retries, and provider-specific auth/compat
  handling. The scripted model remains available for deterministic tests.
- **Lane owner task** — a `SessionTask` owns `Storage`; callers communicate
  through commands and oneshot replies. The TUI never receives a mutable
  storage handle.
- **Observer and hooks** — ordered broadcast snapshots/events and awaited
  before-tool hooks are available to integrations.
- **Compaction** — `/compact` writes a summary entry and moves the lane leaf
  with durable start/finish records.
- **Heartbeats** — `HEARTBEAT.yml` reload and strict `SILENCE`/`Message:`/`Steer:`
  response validation are implemented as the scheduler seam.
- **Skills** — `workspace/skills/**/SKILL.md` discovery, frontmatter validation,
  and live catalog injection into the system prompt. Hidden dirs (`skills/.archive/`)
  are skipped. The **curator** (`revebot curator`, `/curator`) tracks usage,
  marks long-unused *adopted* skills stale, and archives them after 90 days —
  never deletes. Bundled scaffold skills are off-limits unless
  `curator.prune_builtins: true`. **Skill improvement** is `skill_manage` /
  `skill_view` / `skills_list` plus `/learn`: the bot records class-level
  workflows as it goes; a hidden nudge every 15 turns asks it to.
- **Channels** — ordered in-process inbox events and namespaced durable KV state.
- **TUI** — ratatui inline rendering, subagent/inbox/steer/follow-up states,
  slash-command and workspace `@file` completion, checkpointed streaming Markdown, and
  animated startup progress.

Every terminal prompt now writes through the durable lane sequence and reopens
the same `.reve/sessions/main-*.jsonl` conversation on the next launch. A real
tool-using model test proves the sequence: `bash` writes in the VM, `read`
reads the result, the follow-up request receives the user, assistant, and
tool-result history, and the final answer is persisted.

The remaining engine-level limitation is provider tool continuation from the
standalone CLI tool command; normal TUI turns run the durable lane.

## Development

```bash
make ci                                 # the same strict gate as GitHub Actions
make warnings                           # rustc warnings are errors, all targets
make clippy                             # strict lint policy, all targets, -D warnings
make test                               # cargo test --locked
make fmt-check                          # check formatting
cargo test --locked --test microvm -- --ignored  # opt-in real microVM tests
revebot --version
```

The lint policy lives in `Cargo.toml`: pedantic checks, production panic paths,
unsafe hygiene, debugging leftovers, and locks/borrows held across await. Tests get
narrow panic-path exemptions from `clippy.toml`. Exceptions require a local,
reviewed justification—not a crate-wide allow or a weakened CI command.

Requirements: Rust 1.91+, Linux with KVM or macOS on Apple Silicon. The repository itself
is also an ordinary Reve agent directory for development purposes. Tests create isolated
temporary agent folders; they do not consult a user profile or write persistent state
outside their fixture folder.
