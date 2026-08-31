# Reve: the specification, mapped onto Rust

[`docs/harness.md`](harness.md) is the authority. This document is the map: how that
design lands in Rust modules, which parts exist, what was deliberately cut, and where
each invariant is actually tested. When the two disagree, the specification wins unless a
cut is recorded here.

Reve is one Rust crate (edition 2024, version 0.1.0) with Lua for scripting.

## 0. Why Rust, and what the type system is doing for us

The specification's central structural claim is **one writer per session**. In most
languages that is a rule the serving layer has to keep. Here it is the type system:
`Storage` (`src/storage/mod.rs`) is not `Sync`, is never wrapped in a mutex, and is moved
into a single owner task by `Session::spawn`. Everyone else holds a clonable `Session`
handle and sends commands. You cannot get a `&mut Storage` from a handle, so "two writers"
is not a bug you can write.

The second claim is **explicit state, not inferred state**. There is no code anywhere that
reconstructs what an operation was doing by replaying its history. `op.state/{id}` holds
one total value — a program counter — and every transition overwrites the whole register.
Recovery is five point lookups and a bounded validation of exactly what those lookups
name (`session::restore`). An operation that has ended has no state at all: there is no
`finished` member of the union, and the terminal transaction deletes the register.

Concurrency is tokio tasks. Cancellation is a one-bit watch channel (`tokio_util_lite`),
and it is only ever an *accelerator* — the durable meaning of an abort is a committed
`Control::CancelRequested`, so an abort that races a crash still ends the operation
aborted.

## 1. Module map

```
src/
  ids.rs              UUIDv7 minting, EntryId / UsageId / OpId, follower ids
  entry.rs            JSONL v4 wire format: Entry, Usage, Namespace, Write,
                      Transaction, Line (header | object | array batch)
  state.rs            every typed register value, including OperationState —
                      the program counter — and its phases
  storage/mod.rs      in-memory projection; all-or-none commit, branch scans
  storage/jsonl.rs    one file, one writer, torn-tail-safe replay, file lock
  session.rs          the owner task, the Session handle, conditional commits
                      (Expect / CAS), typed reads, restore(), context projection
  lane.rs             the Driver: one step of the state machine per commit
  harness.rs          the public surface: prompt / steer / followUp / nextRun /
                      abort / compact / navigate / resume, and the lane claim
  hooks.rs            before_run, before_tool (fails closed), after_tool,
                      before_run_end, before_compaction, transform_context
  events.rs           the passive event stream
  compaction.rs       threshold arithmetic, tail selection, summary request
  model.rs            Model trait, streaming callback, ScriptedModel, StopReason
  provider/           models.yml, SSE decoder, OpenAI + Anthropic adapters,
                      post-startup model discovery (cached, best effort).
                      `$ENV` apiKey is resolved; a non-`$` value is a dummy
                      literal (local servers). One bad provider must not empty
                      the catalog.
  sandbox.rs          mandatory microsandbox VM; public internet by default,
                      lock down with `open = false` + `allow`. Secret
                      `source` is a host env var or `$(command)` resolved
                      at apply into `REVEBOT_SECRET_*` for microsandbox —
                      not a Lua host-exec path. Command argv and `file:`
                      paths expand a leading `~/`.
  lua.rs              host config/tools and a separate restricted workspace Lua
                      state; definitions retain their originating Lua state.
                      Workspace callbacks get explicit guest/messaging capabilities,
                      never ambient host IO/environment/module access.
  script_fs.rs        descriptor-relative NOFOLLOW source/profile/memory reads,
                      missing-only scaffold creation and session-directory opens.
                      Lua source is regular UTF-8 text <= 1 MiB.
  working_directory.rs  per-conversation cwd + ancestor AGENTS.md snapshot in
                      fact.custom/cwd/<lane>; fixed HOME, current cwd, guest-only
                      canonicalization/full root-to-leaf instruction reads.
  cron.rs             five-field cron for house routines
  tools.rs            the Tools trait plus seven built-ins, replay declarations
  skills.rs           recursive SKILL.md catalog; workspace ∪ bot, bot shadows
                      name. Folded YAML descriptions; a broken file is skipped
                      in the house catalog. Created/edited skills attach to the
                      next user wrap as a hidden card.
  heartbeat.rs        schedule reload and response-contract validation
  channels.rs         inbox broadcast and namespaced durable KV
  project.rs          house directory and `revebot init`; identity is
                      workspace/agents/<id>/; SOUL.md is the sole prose identity,
                      profile.json is authoritative metadata; HOME/workspace is cwd
  tui/                inline ratatui renderer and the terminal session
  house/              multi-bot house: roster, supervisors, HTTP/WS, routine ticker;
                      usage.jsonl in `.reve/` logs skill `/name` and Lua plugin
                      invocations (one JSON object per line) for later stats.
                      wrap.rs timestamp/cwd headers + [agent] arrivals.
                      profile.rs validates directory-owned ids and refreshes live
                      metadata from disk. home.rs agent-specific SOUL scaffolding.
                      memory.rs exact write/forget, agent/user/project scope,
                      profile/log/note Markdown tiers and bounded prompt projection.
                      files.rs serialised compare-before-replace guest writes.
                      resources.rs shared post-write notifications; Lua on_change
                      observes asynchronously, never vetoes writes.
                      SendUserMessage emits UserNotice on the bot harness
                      stream (the chat websocket), not only house_events.
                      @mention cards carry the bot's real id; `/skill` cards
                      attach the skill body. Boot never awaits `resume_all` /
                      drive; each supervisor resumes then kicks in the
                      background so the HTTP server binds even if a bot is mid-tool.
  web/                embedded local UI: OpenGrok roster + shadcn-style
                      bubbles, bloub avatars, routines rail. Assistant
                      `content` is a part list (text + toolCall); the page
                      parses it into bubbles and compact tool cards, never
                      JSON.stringify. GET / is Cache-Control: no-store.
                      House events refresh sidebar/header metadata from profile.json;
                      invalid edits show a labelled last-good profile, not silent stale UI.
                      Soul editor reads/writes SOUL.md through /api/bots/<id>/soul.
                      Single-log rendering remains upcoming.
                      Compose autocomplete: `/` skills, `@` other bots.
                      AskUserForSecret renders an inline host-secret form.
                      Right rail is tabbed (Screen / Files / Routines); Files
                      is hidden until chosen. Hovering chat text that is a
                      `/workspace/…` path or a PWD-resolvable name
                      (`KNOWLEDGE.md`) wrap it as a file-ref; click reveals
                      it in the explorer (GET /api/fs/stat). Tree itself is a
                      VS Code-style explorer of `/workspace` (GET /api/fs,
                      /api/fs/file): full-width rows, folder icons, indent
                      guides, arrows / Enter / typeahead, Collapse All +
                      Refresh. Rooted at the mount. Omits agents/*/sessions
                      JSONL. Paths stay under the mount.
                      The Screen tab is a live noVNC preview of the guest
                      desktop; click takes over (keyboard/mouse), Esc gives back.
                      Rail working-dots follow house `bot_busy` events, not
                      which chat is open. GET /api/bots includes `busy`.
                      SendUserMessage is hidden from Working… and live-pushed
                      via user_notice. Transcript restore treats SendUserMessage
                      tool calls and custom user_notice entries as bubbles;
                      assistant prose after the first tool of that turn is not.
  eval/               catalog runner for evals/cases (offline / live / microvm);
                      live defaults to openrouter/x-ai/grok-4.6 (OPENROUTER_API_KEY)
  main.rs             init / info / exec / tool / serve / tui / eval; bare `revebot` serves
tests/{harness,crash,microvm,provider_http,eval}.rs
evals/cases/<suite>/*.yaml   scored cases; offline is `make eval`, live is `--live`
```

## 2. The specification's concepts, in Rust

| Specification | Rust |
|---|---|
| Session: entries, registers, usage, one `seq` | `Storage`, owned by the `Session` task |
| One writer | structural: `Storage` is moved into the task; handles send commands |
| Register | `Namespace` + key → `Register { value, seq }` |
| The program counter `op.state/{id}` | `state::OperationState` (`Run` / `Compaction` / `Navigation`) |
| Atomic transaction | `Transaction` of `Write`s; `Storage::commit` validates all-or-none |
| Conditional commit | `Session::commit_if` with `Expect { namespace, key, seq }` |
| Lane claim (one operation per lane) | `Expect` on `lane.state`; the loser gets `HarnessError::Busy` |
| Intent before effect | commit `EffectPending` (+ `op.tool_args`), *then* invoke |
| Recovery | `Session::restore` → `Restored::Suspended(Current)` → `Driver::drive` |
| Terminal transaction | `Driver::terminal`: delete everything the operation owned, write `lane.lastResult`, clear the claim |
| Queued input | `pending.entry/{id}` payload + the id in the running operation's inbox |
| Abort | committed `Control::CancelRequested`; the watch channel only wakes the effect |
| Hooks (intercept) | `hooks.rs`, sequential, chained, `before_tool` fails closed |
| Events (observe) | `events::Event` on a broadcast channel; nothing can change execution |
| Tools | seven Rust built-ins plus Lua tools; every effect goes through `Sandbox` |

### The shape of one step

`Driver::drive` is a loop, and every iteration is: read the phase, do at most one
irreversible thing, commit the next phase conditionally, reload. Because the commit is
conditional on the `op.state` seq the step was planned against, anything that landed in
between — an `abort`, a `steer` — makes the commit fail, and the driver replans from the
reloaded state instead of writing something it decided under stale assumptions.

## 3. What is built

- **Storage and format.** JSONL v4. Three line shapes: header, single object, array batch
  (one physical line per transaction, so a transaction cannot be half-read). Entries are
  write-once and form the conversation tree; registers are mutable state with no history;
  usage rows are separate. Payloads are flattened with reserved keys sanitised to
  `payload_*`. Flush every append; a torn last line is discarded whole on reopen; a
  malformed line anywhere else is corruption and we refuse to open. Bot sessions
  open relative to a held, no-symlink directory descriptor; replay uses the locked
  file descriptor, not a second pathname lookup. Compaction creates/locks its new
  inode before descriptor-relative rename, and owner drop explicitly releases
  the lock even if a fork temporarily duplicated the fd. Snapshot compaction
  rewrites through a temp file and a rename, **in seq order** (not grouped by kind:
  entries-then-usage-then-registers would put an early usage `seq` after a later
  entry and fail the next open). A grouped snapshot from an earlier build is
  accepted when seqs are unique, replayed sorted, and rewritten. Duplicate seq
  is still corruption. Cross-process exclusion via `File::try_lock`.
- **The session.** Owner task, `Commit` / `Read` / `Close` commands, CAS tokens, typed
  register reads, `ensure_lane`, `restore` with the specification's bounded validation, and
  `project_context` — which stops at a compaction, expands its summary plus retained tail,
  and drops error and aborted assistant turns so a failed attempt never reaches the model.
- **The driver.** Checkpoint (queue drain, threshold compaction, finish decision),
  assistant generation with durable retry state, the tool batch, in-run compaction, failure
  drain, and the terminal transaction. Structural work (compaction) shares one
  `deciding → generating → published` machine between the in-run and standalone paths.
- **The harness.** Every public entry point either claims a lane or amends a running
  operation, both as one conditional transaction, so a caller is never told something
  landed that is not on disk.
- **Recovery.** A resumed run continues; it does not abort. A tool interrupted mid-effect
  is re-executed only when the recorded *and* current replay declarations both say `safe`,
  and otherwise gets a synthetic result that admits the effect may or may not have
  happened. A prompt that was still a reservation is placed exactly once.
- **The sandbox.** Links `microsandbox =0.6.8` directly. The default guest is wrap's
  desktop image (`ghcr.io/tobi/wrap:desktop`): toolchain at absolute paths under `/opt`,
  unprivileged `user` (`HOME=/home/user`) with uid/gid realigned to the host workspace
  owner and virtiofs stat virtualization off, XFCE on `:1`, noVNC/VNC published on
  localhost, and a shared Chrome that `agent-browser` attaches to. Provisioning is off
  by default. Public-internet egress by default (`NetworkProfile::Public`);
  `open = false` plus `allow` is the lock-down. Scoped source-backed secrets, fail-closed boot,
  idle shutdown, workspace bind mount at `/workspace`. Default memory is 8192 MiB.
- **The scripting surface.** Trusted host `agent { }`, `sandbox { }` and installed
  tools use one Lua state. Bot-editable plugins/routines use a **separate** state
  with an allowlist of pure libraries/base functions: no `io`, `os`, `package`,
  `require`, `debug`, `load`, `loadfile`, `dofile`, host streams or bytecode loading.
  Registry keys retain their owning Lua state; host globals/cached capabilities
  cannot cross into workspace code. The original four-function command denylist
  (`os.execute`, `io.popen`, `os.exit`, `package.loadlib`) remains on both states.
  `ctx.sh` stays VM-only; routine `ctx.send` collects sends for house delivery and
  `ctx.bot` is the resolved target/owner. `script_fs` walks directory descriptors
  with `O_NOFOLLOW`, so a source-file or ancestor symlink cannot import host data
  before Lua even starts. The complete implemented API is documented in the
  `plugins` skill, whose examples are exercised by tests.
- **Agent identity and memory.** Each bot has SOUL.md, structured profile metadata,
  private memory and a workspace in its own home. No global SOUL/KNOWLEDGE fallback.
  SOUL.md is the only prose identity file. New homes receive agent-specific defaults;
  edited files are never overwritten. VM.md is shared machine knowledge. AGENTS.md follows cwd's
  ancestor chain (including any existing workspace-root rules), not persona.
  Profile/log/note memory is Markdown with exact managed blocks; private by default,
  explicit user sharing, project shards only for profile.projects members. Writes
  preserve unmanaged prose, dedupe within scope, compare original bytes before
  atomic guest replacement, and acknowledge success only after the VM result.
  Prompts are bounded to 16 KiB of memory; older/omitted facts stay on disk.
- **Live profile updates.** API views and prompts reread profile.json. Post-write
  notifications refresh caches and publish roster changes for the web sidebar/header.
  Model configuration refreshes conditionally only at idle run admission; a drive
  resolves once from captured lane configuration and keeps that model throughout.
  Invalid profile edits are visible and prevent using a substituted identity.
- **Directory and resource changes.** The cd tool validates inside the VM, snapshots
  all ancestor AGENTS.md files in full, and persists the total cwd state through
  the session owner. HOME does not move. Built-in paths, bash and Lua ctx.sh follow
  cwd. Known write/edit paths and conservative unknown shell/plugin effects enter
  one ResourcesChanged notification path. Profile and directory-rule caches refresh;
  soul/memory are read on the next request. on_change subscribers have path/kind
  filters, owner scope and optional unknown-event handling; they can queue messages
  but have no shell. Notifications are best-effort, in-memory observations, not a
  durable exactly-once scheduler. No filesystem polling loop is added.
- **The terminal.** Ratatui inline renderer driven by the passive event stream. A run is a
  spawned task, so a steer typed mid-run is a conditional commit rather than a message the
  loop has to be free to receive.

## 4. Deliberate cuts

The specification describes more than this crate implements. These are choices, not gaps:

- **No deferred provider requests.** A generation is attempted when the driver reaches it.
- **No summarised navigation.** `navigate()` moves the leaf; it does not generate a summary
  of what it skipped.
- **Sequential tool execution only.** A batch runs one call at a time, in order.
- **No "missing identities" concept.** An unknown tool produces a synthetic error result
  the model can read, not a distinct state.
- **No SQLite backend, no v3 compatibility.** Memory plus JSONL, v4 only. This is a new
  agent; there is nothing to be compatible with.
- **Exactly one sandbox transport**, pinned `=0.6.8`. No second transport and no host-shell
  fallback, ever.
- **The microVM tests are opt-in** (`#[ignore]`). The unit suite provisions no VM and makes
  no model request.
- **Workspace Lua capability restriction is not process isolation.** Pure Lua is
  not preemptively cancelled or CPU/memory-budgeted; use the VM for substantial
  computation. Host-installed Lua is still trusted and must not import bot-authored
  files or expose host callbacks to the restricted state.
- **Plugin/routine loading is startup-only for now.** No next-turn hot reload or
  last-good fallback yet; a broken file fails startup. Per-bot ownership filtering,
  consistent duplicate resolution, complete tool messaging contexts and routine-chat
  isolation remain follow-up work. on_change is implemented, but its notifications
  can be lost at crash/overflow; handlers must avoid self-triggering message loops.
- **Memory manual-edit concurrency.** Runtime writers serialize and guest publication
  checks the old content hash. Non-cooperating manual writers still have the normal
  POSIX compare/rename race; arbitrary shared-VM filesystem mutation is not isolated.
  Memory privacy is prompt separation, not filesystem access control. Managed facts
  have TTL-based prompt inclusion; unmanaged log/notes prose is read on demand. The plugins skill states these limits.

## 5. Invariants, and the test that holds each one

Every row names a real test. A claim with no test says so instead of appearing covered.

| Invariant | Test |
|---|---|
| A transaction is all-or-none | `storage::tests::a_failing_transaction_applies_nothing` |
| `seq` is shared and strictly increasing across every kind of write | `storage::tests::a_transaction_assigns_strictly_increasing_seq_across_all_kinds` |
| An entry may name a parent created in the same transaction | `storage::tests::an_entry_may_name_a_parent_created_earlier_in_the_same_transaction` |
| Registers have no history: set, delete, recreate | `storage::tests::registers_set_delete_and_recreate_without_history` |
| Deleting every register still leaves a valid conversation | `storage::tests::deleting_every_register_leaves_a_valid_tree` |
| A branch scan stops inclusively at a compaction | `storage::tests::a_branch_scan_stops_inclusively_at_a_compaction` |
| A torn tail is discarded whole; a malformed line elsewhere is corruption | `storage::jsonl::tests::{a_torn_array_line_is_discarded_whole, a_malformed_line_in_the_middle_is_corruption}` |
| A future format or storage version is refused, not guessed at | `storage::jsonl::tests::{a_future_format_version_is_refused_rather_than_guessed_at, a_newer_storage_version_is_refused}` |
| One writer per session, across processes | `storage::jsonl::tests::a_second_process_cannot_open_a_live_session`, `tests/crash.rs` |
| A payload can never collide with the envelope | `entry::tests::a_payload_cannot_collide_with_the_envelope` |
| A conditional commit is rejected when its token moved | `session::tests::a_conditional_commit_is_rejected_when_its_token_moved` |
| Restore refuses a state that contradicts itself | `session::tests::restore_rejects_an_aborted_response_under_running_control` |
| Context stops at a compaction and drops failed attempts | `session::tests::context_projection_reads_nothing_past_a_compaction_and_drops_errors` |
| A prompt becomes a user entry and an assistant reply, leaving no registers behind | `tests/harness.rs::a_prompt_becomes_a_user_entry_and_an_assistant_reply` |
| One operation per lane; a second is refused | `tests/harness.rs::a_second_operation_on_a_busy_lane_is_refused` |
| `before_tool` decides the arguments that are persisted and run | `tests/harness.rs::before_tool_rewrites_the_arguments_that_get_persisted` |
| `before_tool` fails closed | `tests/harness.rs::a_throwing_before_tool_hook_fails_the_call_closed`, `hooks::tests::a_throwing_before_tool_handler_blocks_the_tool` |
| `after_tool` decides the result that is persisted | `tests/harness.rs::after_tool_rewrites_the_result_that_gets_persisted` |
| A truncated response never executes its tool call | `tests/harness.rs::a_truncated_response_never_executes_its_tool_call` |
| A retryable failure is retried; an exhausted budget fails the run without losing the prompt | `tests/harness.rs::{a_retryable_provider_failure_is_retried_then_succeeds, an_exhausted_retry_budget_fails_the_run_and_keeps_the_prompt}` |
| An abort ends the run aborted and drops queued input | `tests/harness.rs::an_abort_ends_the_run_aborted_and_drops_queued_input` |
| A run dropped before its first generation resumes and places its prompt once | `tests/harness.rs::a_run_dropped_before_its_first_generation_resumes_and_finishes` |
| A replay-safe tool interrupted mid-effect is re-executed from its persisted arguments | `tests/harness.rs::a_safe_tool_interrupted_mid_effect_is_re_executed`, `tests/crash.rs::a_killed_replay_safe_tool_is_re_executed_from_its_persisted_arguments` |
| An effectful tool interrupted mid-effect is never re-executed | `tests/harness.rs::an_effectful_tool_interrupted_mid_effect_is_never_re_executed`, `tests/crash.rs::a_killed_effectful_tool_is_reported_interrupted_never_re_run` |
| A completed tool call is not run again on resume | `tests/harness.rs::a_completed_tool_is_not_run_again_on_resume` |
| An abort committed before the crash survives it | `tests/harness.rs::an_abort_committed_before_the_crash_ends_the_resumed_run_aborted` |
| A really-killed process leaves a resumable session | `tests/crash.rs` (spawns and SIGKILLs a real child) |
| The compaction tail is widened to a user turn | `compaction::tests::the_tail_is_widened_to_a_user_turn_and_the_head_is_summarised` |
| Lua cannot execute a command on the host | `lua::tests::{the_host_command_path_is_gone_before_any_script_runs, a_tool_that_tries_to_shell_out_on_the_host_fails_to_load}` |
| Workspace Lua has no ambient host authority and cannot inherit trusted aliases | `lua::workspace_tests::{workspace_lua_has_no_ambient_host_authority, workspace_scripts_cannot_read_or_write_host_sentinels, workspace_mutation_cannot_change_the_host_lua_state}` |
| All workspace loaders and callbacks use the restricted state | `lua::workspace_tests::{all_workspace_script_locations_use_the_restricted_loader, workspace_guards_and_routines_execute_in_the_restricted_state, deferred_workspace_callback_cannot_use_host_io}` |
| Workspace source cannot traverse symlinks, load nonfiles/oversized files, or import bytecode | `script_fs::tests::{script_and_ancestor_symlinks_are_refused, traversal_nonfiles_and_oversized_sources_are_refused}`, `lua::workspace_tests::{symlinked_script_files_and_bot_directories_are_rejected_before_load, workspace_bytecode_is_never_loaded}` |
| The complete plugins reference's examples load; pure callbacks execute | `lua::workspace_tests::plugins_skill_examples_load_and_pure_callbacks_execute` |
| The documented workspace tool actually enters the microVM | `tests/workspace_microvm.rs::documented_workspace_tool_reads_a_note_inside_the_microvm` (opt-in) |
| Directory/profile ids reject traversal, mismatches and symlinks | `house::profile::tests::{ids_are_directory_owned_and_paths_or_mismatches_are_rejected, profiles_refuse_file_and_parent_symlinks}` |
| Session compaction stays on its held directory and lock ownership ends with Storage | `storage::jsonl::tests::{rooted_sessions_and_compaction_never_follow_replaced_paths, owner_drop_releases_the_lock_even_with_a_stray_descriptor}` |
| Soul/private memory do not bleed between agents; disk profile changes reach prompts | `house::prompt::tests::souls_profiles_and_memory_do_not_bleed_across_agents` |
| Agent-specific home scaffolding keeps edited souls and refuses symlink redirects | `project::tests::{each_agent_gets_its_own_home_and_edited_souls_are_kept, init_does_not_follow_a_symlinked_home_subdirectory}` |
| Memory dedup/exact forget/scope/recency/bounds preserve manual prose | `house::memory::tests` |
| Metadata refresh/error handling and UI header updates use current profiles | `house::profile::tests::direct_profile_edits_refresh_metadata_and_report_invalid_files`, `node tests/profile-ui.cjs` |
| In-flight drives keep their model; new profiles affect the next run | `tests/configuration.rs::profile_model_changes_affect_the_next_run_not_the_running_drive` |
| Cwd rules are full/root-to-leaf, HOME stays fixed, contexts and headers are isolated | `working_directory::tests`, `house::wrap::tests::cwd_is_an_escaped_message_header_not_part_of_the_user_query` |
| Change observers are filtered/scoped and failures do not veto another observer | `lua::workspace_tests::change_observers_are_filtered_scoped_and_errors_do_not_veto_other_observers` |
| Home/cwd/memory/profile effects work against the real VM | `house::microvm_tests::homes_cwd_memory_and_profile_notifications_work_in_the_guest` (opt-in) |
| The default guest is wrap desktop, unprivileged, and git reads its token from the environment | `sandbox::tests::{the_default_policy_boots_a_preprovisioned_guest, wrap_images_get_a_unix_user_and_desktop_display, git_reads_its_token_from_the_environment_not_a_credential_store}` |
| Every invokable tool is offered to the model | `tools::tests::the_active_tool_list_covers_every_builtin` |
| An unmentioned Lua flag keeps its default | `lua::tests::an_unmentioned_flag_keeps_its_default` |
| Model discovery contacts only upstreams whose key is set, and never fails the agent | `provider::discovery::tests::{only_upstreams_that_have_a_key_are_probed, an_unreachable_upstream_is_recorded_not_fatal, a_missing_or_corrupt_cache_is_simply_absent}` |
| A namespaced model id survives discovery intact (`openrouter/x-ai/grok-4.6`) | `provider::discovery::tests::the_openrouter_shape_yields_a_pasteable_reference` |
| Public-internet egress by default; lock-down is `open = false` | `sandbox::tests::default_egress_is_the_public_internet`, `tests/microvm.rs` (opt-in) |

**Not covered yet.** Standalone `compact()` and `navigate()` have no end-to-end test — the
machinery is shared with the in-run compaction path, which is exercised only through the
overflow route. Lane concurrency is implemented (a lane claim per operation, drivers as
independent tasks) but there is no test that runs two lanes at once.
