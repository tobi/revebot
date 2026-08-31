# Model-checked invariants

Two TLA+ specifications pin the parts of [`docs/harness.md`](../harness.md) where a
wrong ordering silently corrupts a session. They are checked with
[tla-rs](https://github.com/fabracht/tla-rs) (`cargo install tla-checker --bin tla`)
by `make tla`, which `make ci` runs. Every reachable state of the bounded model is
checked against every `Inv*`/`TypeOK` definition; a violation prints the shortest
trace that reaches it.

These are models of the *design*, not of the Rust. Their job is to make the rules
in `AGENTS.md` ("intent before effect", "explicit state, never inferred", "an abort is
a commit") mechanically falsifiable before code is written against them, and to keep
the transaction tables in the specification honest. `docs/architecture.md` §5 still
names the Rust test that holds each invariant in the implementation.

## `DurableLog.tla` — the JSONL session file (§1.4, §1.7)

State: the file (`log`, a sequence of transaction lines), the line being written
(`inflight`), the `seq` counter, and the live maps (`entries`, `usage`, `regs`).
Actions: `Propose` a 1- or 2-write transaction, `Tear` it (crash mid-write), `Flush`
it (stamp seqs, append one line, apply), `Compact` (rewrite surviving writes as
singleton lines in seq order).

| Invariant | Rule |
|---|---|
| `InvReplayEqualsMaps` | Replaying the file from empty always yields the live maps — the file is the replay recipe. |
| `InvInflightInvisible` | A transaction touches the maps only after its line is durable; a torn line changes nothing. |
| `InvWriteOnce` | Entry and usage ids share one write-once namespace. |
| `InvSeqMono` | `seq` is strictly increasing across the file; compaction gaps are legal. |
| `InvCompactPreserves` | Compaction never changes what replay produces. |

Two modelling facts the checker forced out: compaction frees log space, so the number
of commits (not the line count) is the bound; and a compacted file can have *more*
lines than commits, because array lines become singletons.

## `DurableHarness.tla` — lanes, operations, inbox types, tool batches (§3.8, §3.11–3.13, §4.5–4.6, §9.1)

State: write-once `placed` entries with `parent`s, `pending` registers keyed by
reserved entry id and tagged with their queue kind (`steer`, `follow`, `write`,
`nextrun`), lane registers (`laneOp` claim, lane-owned `nextRun`, `leaf`,
`lastOutcome`), and one total `ops[o]` record per operation — the program counter
(`phase`, `control`, `continuation`, inbox ids, drained ids, `skipOnce`, the
`reserved` generation id, retry `attempt`) plus two process-local bits (`armed`,
`running`) that `Crash` clears, and a tool batch `calls[1..BatchSize]` — each call a
§3.8 `ToolCallState` (`planned | effect_pending | completed`, reserved `result` id,
`replay`, `terminate`) together with its `op.tool_args` register bit and its own
`armed`/`running`/`runs`/`interrupted` bits. Two ghost variables exist only to make
rules checkable: `owes[e]` (the result ids assistant `e` planned) and
`startedCancelled` (an effect was dispatched under `cancel_requested`).

Actions cover acceptance (`AcceptRun`, `AcceptCaptured` for a queued nextRun), every
queue admission (`QueueSteer`/`QueueFollow` refused under cancel, `QueueWrite` allowed
under cancel, `QueueNextRun` on any lane state, `IdleWrite`), `CancelQueued` triage,
the checkpoint drains, the generation sandwich (`IntentGen → DispatchGen →
SettleGen | SettleGenToTool`), the parallel tool batch (`ClearTool` writes
`op.tool_args` and commits intent in source order; `BlockTool` skips intent and commits
a synthetic result; `DispatchTool` does not wait for earlier calls; `SettleTool` commits
results in source order and folds batch completion — `op.tool_args` deletion,
`terminate` → `may_finish` — into the last one), recovery of an unknown effect
(`RecoverGen`, `RecoverTool`), `Abort`, `Terminal`, and `Crash`. Two lanes run
interleaved. Sequential tool mode is a strict subset of these behaviours.

| Invariant | Rule |
|---|---|
| `InvExclusivity` | A queued id has its register, its entry, or neither — never both (§9.1.15). |
| `InvReservedFresh`, `InvReservationRegimes` | A settlement id reserved in `op.state` is not placed and not queued. |
| `InvLaneOwnership`, `InvPresentIffClaimed` | An operation is open iff exactly one lane's claim names it; at most one per lane (§9.1.12, §9.1.17). |
| `InvClosedOpOwnsNothing` | After the terminal transaction the operation owns nothing (§9.1.13). |
| `InvNextRunLaneOwned` | `pendingNextRun` registers are lane-owned: terminal never deletes them. |
| `InvInboxHasRegister` | Every inbox id, drained or not, still has its `pending.entry` register. |
| `InvAbortKeepsWritesAndPayloads` | Abort drains steer/follow-up into `control.drained*` without deleting payloads; deferred writes stay queued. |
| `InvAbortedImpliesCancel` | A response with `stopReason: aborted` exists only under `cancel_requested` (§9.1.19). |
| `InvNoEffectWithoutIntent` | A live provider/tool effect requires a committed `effect_pending` phase with a reserved id. |
| `InvInterruptedNeverToolNotDispatched` | A `replay: never` tool whose intent survived a crash is never dispatched again (§4.5). |
| `InvUnsafeToolAtMostOnce` | A `replay: never` tool is dispatched at most once. |
| `InvNoNewEffectUnderCancel` | After the abort commit no provider request or tool starts; live ones may still settle (§4.6). |
| `InvCallsSourceOrder` | Clearance/intent and result commits happen in source order even though effects run concurrently (§3.8). |
| `InvToolArgsLifecycle` | `op.tool_args` exists only for a cleared call of the open batch; blocked calls never get one; batch completion and terminal delete them (§3.8, §9.1.13). |
| `InvToolResultsReserved` | Result ids are distinct across the batch, fresh until their result commits, placed exactly when the call completes. |
| `InvEveryToolCallHasResult` | Once no operation is working an assistant's batch, every result it planned is placed — cancellation must reconcile every call before the terminal transaction (§0.5). |
| `InvResponseHasUsage` | Usage rows commit with their response, never alone. |
| `InvParentsExist` | A placed entry's parent is placed — a missing parent is corruption (§9.1.11). |

### What the model caught

- `cancelQueued` on an id that a prior `abort()` drained. The obvious implementation
  ("pending register exists → delete it") deletes a payload that `AbortResult` and
  post-crash `SuspendedOperation.aborting` still dereference. §3.11 is right: triage
  on *queue-list membership*; a drained id is `not_found` and its register lives until
  the terminal transaction. `cancel_queued` is not yet implemented in Rust; when it
  is, it must follow `InQueueList`.
- Both bound mistakes in `DurableLog` above.

### Coverage

Bounds are only meaningful if the interesting interleavings are reached. `Cov*`
definitions are probes, not invariants; `--count-satisfying CovX` must be non-zero on
the CI configuration. Currently: two tools live at once (`CovTwoToolsLive`), a later
call settled while an earlier one is still running (`CovLaterCallSettledFirst`), a
completed batch (`CovBatchCompleted`), an interrupted `never` call synthesised
(`CovInterruptedNeverSynthesized`), both lanes open (`CovBothLanesOpen`).

### Mutation checks

The invariants have teeth. Each of these deliberate bugs is caught by the named
invariant on the small configuration:

| Injected bug | Caught by |
|---|---|
| Terminal deletes the lane's `nextRun` register | `InvNextRunLaneOwned` |
| Abort deletes a drained payload register | `InvInboxHasRegister` |
| Crash leaves `armed` set (dispatch after restart) | `InvInterruptedNeverToolNotDispatched` |
| Recovery re-arms a `never` tool | `InvUnsafeToolAtMostOnce` |
| Acceptance skips the idle check | `InvLaneOwnership` |
| Synthetic `aborted` settlement under running control | `InvAbortedImpliesCancel` |
| Clear call *i* before call *i−1* | `InvCallsSourceOrder` |
| Commit result *i* before result *i−1* | `InvCallsSourceOrder` |
| Batch completion keeps `op.tool_args` | `InvToolArgsLifecycle` |
| Both result ids minted the same | `InvToolResultsReserved` |
| Recovery re-arms a `never` call; crash keeps a call armed | `InvInterruptedNeverToolNotDispatched` |
| Dispatch allowed under cancel *and* abort leaves calls armed | `InvNoNewEffectUnderCancel` (each alone is covered by the other guard) |
| Terminal transaction while a call is still `effect_pending` | `InvEveryToolCallHasResult` |

## Running

```
make tla                         # CI size: ~2.5 min, ~78k states across both specs
make tla-deep                    # 5 entry ids, MaxSeq 8; opt-in, long
tla docs/tla/DurableHarness.tla --config docs/tla/DurableHarness.small.cfg \
    -s EntryIds -s OpIds -s Lanes -i                                            # step through
```

Configurations: `DurableLog.cfg` (2 ids, 1 key, 3 commits), `DurableHarness.small.cfg`
(2 lanes, 4 entry ids, 2 ops, batch of 2, 7 commits), `DurableHarness.cfg` (5 entry
ids, 8 commits). Entry ids, op ids and lanes are interchangeable model values, so
`-s` symmetry reduction is sound and cuts the state space roughly 8×. Deadlock checking
is off: an idle session with every id consumed is a legal final state.

### tla-rs parsing rules learned the hard way

- `A => B /\ C` is `(A => B) /\ C`. Parenthesise the consequent.
- A quantifier body that starts on the quantifier's line must not continue with a
  bulleted `/\` on the next line — the continuation is silently dropped. Put the body
  on its own lines as a bullet list, or keep it on one line.
- Same for `LET … IN /\ …` spanning lines inside a quantifier. Avoid `LET` in invariants.
- `CHOOSE` over state inside an invariant evaluates to false silently. Quantify instead.
- Nested `EXCEPT` paths (`!.calls[i] = …`) are unsupported; nest two `EXCEPT`s.
- `Seq(S)` is not enumerable; type-check sequences element-wise.
- Only one `SYMMETRY` line in a `.cfg` takes effect; pass `-s` per constant.
- A probe that passes is not evidence until a mutation makes it fail.

## What is not modelled

Deliberately small. One tool batch per turn (a second turn needs more entry ids than
the CI bound allows), no tool usage rows, no structural operations
(compaction/navigation), no `before_run_end` follow-up, no one-at-a-time queue mode
(the model queues at most one item per kind, so `all` and `one-at-a-time` coincide),
no deferred provider requests, no `after_tool`/`before_tool` hook timing. Each is a
bounded extension of `ops[o]`; add it when the corresponding Rust lands and give it an
invariant in the same change.
