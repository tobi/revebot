# reve

A durable coding agent. The core is a Rust crate; the scripting surface an agent author
touches — configuration, project tools, sandbox policy — is Lua.

## Read first

- **`docs/harness.md`** — the durable-harness specification Reve implements (vendored from
  Pi's `packages/agent/docs/harness.md`; the upstream `harness-v2.md` link it replaced is
  dead). It is the authority on storage shape, the operation state machine, recovery,
  abort, queues, hooks, and events. When this crate and that document disagree, the
  document wins unless `docs/architecture.md` records a deliberate cut.
- **`docs/architecture.md`** — how the specification maps onto Rust modules, which parts are
  built, which are deliberately cut, and where each invariant is tested. Update it in the
  same change as the code it describes.

## Rules

- **Sandbox or no Reve.** Reve links the `microsandbox` Rust crate directly (pinned
  `=0.6.8` in `Cargo.toml`). There is exactly one transport, no FFI shim, no CLI, no
  daemon, and no host-shell fallback — ever, not for tests, diagnostics, degraded
  operation, or convenience. Every shell command a tool issues — `ctx.sh`, `reve exec` —
  executes inside that VM. Reve must refuse to start if the microVM cannot boot. Never
  retain, add, or silently select a host/local shell fallback.
- **No host command path exposed to Lua.** A tool's Lua body runs on the host, but
  `ctx.sh` is its only command path and it goes to the microVM. There is no `ctx.host_exec`
  or equivalent. The host side orchestrates; it does not authorize host effects.
- **Intent before effect.** Commit the state that names what is about to happen — the
  reserved entry ids, the effective tool arguments — *before* doing it. Every step is: plan
  against a state you read, do at most one irreversible thing, then commit the next state
  conditionally on that same read. If the conditional commit fails, something else landed;
  replan from the reload rather than writing a decision made under stale assumptions.
- **Explicit state, never inferred.** `op.state/{id}` is a total value and a program
  counter. Do not add code that reconstructs what an operation was doing by scanning its
  history — recovery is point lookups plus bounded validation of exactly what they name.
  An ended operation has no state at all; the terminal transaction deletes it and the
  outcome lives in `lane.lastResult`.
- **Single-owner session state.** `Storage` is deliberately not thread-safe and not shared.
  `Session::spawn` moves it into one owner task; everyone else holds a clonable handle and
  sends commands. That is how reve gets the single-writer guarantee structurally instead of
  by convention. Do not wrap it in an `Arc<Mutex>` to "share" it, and do not hand out
  `&mut Storage`.
- **An abort is a commit, not a signal.** The durable meaning of cancellation is
  `Control::CancelRequested` in `op.state`. The watch channel exists only to wake an
  in-flight request or tool early, so an abort that races a crash still ends the operation
  aborted.
- **One JSONL session, one writer, flush every append.** A crash can only tear the last
  line; on reopen we truncate the torn tail and resume. A malformed line in the middle is
  corruption and we refuse to open. An agent that reports work it did not persist is worse
  than one that is slow.
- **Lua has two separate trust states.** Host `plugins/*.lua` and leftover
  `tools/*.lua` sit outside the mount and remain trusted. Bot-editable workspace
  plugins/routines run in a separate restricted Lua state, with no ambient host
  IO, environment, modules, dynamic loading or bytecode. Read their source with
  the descriptor-rooted `script_fs` loader, never an ambient host Lua loader.
  Both states still have no host command path: `ctx.sh` goes only to the microVM;
  routine `ctx.send` collects messages for house delivery, not shell execution.
  Roster tools (`CreateAgent`, `SendAgentMessage`, …) stay Rust and win on name.
  `Runtime::new` deletes `os.execute`, `io.popen`, `os.exit`, and `package.loadlib`
  before any script runs; keep that list closed. Loading is currently at startup
  and fails on bad source; next-turn/last-good reload is not implemented yet.
- **Document the entire Lua API in the plugins skill.** Update
  `src/templates/plugins_skill.md` in the same change as any Lua API. Cover all
  declarations, fields, contexts, results, scope, replay/cancellation and limits.
  Test executable examples. Do not describe planned APIs as implemented.
- **Each agent owns its home.** `workspace/agents/<id>/SOUL.md` is its sole prose
  identity and standing remit; `profile.json` is authoritative metadata. Never
  inject global SOUL.md/KNOWLEDGE.md or sibling private memory. HOME is fixed;
  default cwd is HOME/workspace. AGENTS.md is directory-scoped and follows cwd's
  full guest ancestor chain, root-to-leaf. Home-level defaults must remain neutral
  operating rules, not a project assignment or persona. Never overwrite edited souls.
- **Pre-release: implement the current contract directly.** Do not add migrations,
  compatibility aliases or routes for superseded feature designs.
- **Special-file updates use the shared post-write path.** Publish resource changes
  after effects, refresh profile/directory contexts, and keep files authoritative.
  Lua on_change observes, never vetoes; its notifications are not durable work.
- **Host config is `config.yml`** (model, sandbox, secrets). `models.yml` stays the
  provider catalog. Existing `agent.lua` / `sandbox.lua` still load if `config.yml` is
  missing.
- **Never silently overwrite files a user has edited.** `reve init` is idempotent: a
  matching file is left `unchanged`, an edited file is reported `changed` and kept, a
  missing file is created.
- **Strict lint policy is a merge gate.** `Cargo.toml` defines Rust/Clippy lints;
  `make clippy` checks all targets with `-D warnings`. Production unwrap/expect,
  panic, unchecked indexing/string slicing/time subtraction, debugging placeholders,
  undocumented unsafe and synchronous locks across await are flagged. `clippy.toml`
  contains narrow test exemptions. Fix violations; do not add blanket allows or
  weaken the gate. Any genuinely necessary exception must be narrowly scoped and
  explain the invariant/safety argument. A SAFETY comment is not proof by itself.
- Every new behaviour gets a test. Keep `make ci` green (format, rustc warnings,
  strict Clippy, tests, TLA+ model checks); the microVM tests stay opt-in.
- **The durable rules are model-checked.** `docs/tla/DurableLog.tla`,
  `docs/tla/DurableHarness.tla` and `docs/tla/VmLifecycle.tla` are the executable form
  of the storage, lane, inbox, abort, recovery, terminal and shared-microVM rules above; `make tla` explores every reachable
  state of the bounded models. A change to a transaction shape, a queue rule, a
  recovery policy or an invariant in `docs/harness.md` — or to `Sandbox` start, hold,
  idle-stop, fingerprint or secret handling — changes the spec in the same commit, and a new durable rule gets an `Inv*` definition plus a mutation that
  violates it (see `docs/tla/README.md`). Do not weaken an invariant to make a
  trace pass; the trace is the bug report.

## Commands

    cargo build                              build the crate and the `revebot` binary
    make install                             install revebot only, --locked --offline --force
                                             (honors RUSTUP_TOOLCHAIN explicitly;
                                             INSTALL_TOOLCHAIN overrides it)
    cargo fetch --locked                     fill the cache after a lockfile update
    make ci                                  run the same strict gate as CI
    make warnings                            reject rustc warnings on all targets
    make clippy                              strict Cargo.toml policy, -D warnings
    make test                                run the locked test suite
    make tla                                 model-check docs/tla (needs `cargo install tla-checker --bin tla`)
    make tla-deep                            larger harness configuration, opt-in
    make eval                                offline eval catalog (no VM, no model)
    make eval ARGS='--live'                  live cases; OPENROUTER_API_KEY by default
    cargo test --test microvm -- --ignored   opt-in microVM integration tests
    make fmt-check                           check formatting without changing files
    cargo fmt                                format
