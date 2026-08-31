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

## `DurableHarness.tla` — lanes, operations, inbox types (§3.11–3.13, §4.5–4.6, §9.1)

State: write-once `placed` entries with `parent`s, `pending` registers keyed by
reserved entry id and tagged with their queue kind (`steer`, `follow`, `write`,
`nextrun`), lane registers (`laneOp` claim, lane-owned `nextRun`, `leaf`,
`lastOutcome`), and one total `ops[o]` record per operation — the program counter
(`phase`, `control`, `continuation`, inbox ids, drained ids, `skipOnce`, the
`reserved` settlement id, `replay`, retry `attempt`) plus two process-local bits
(`armed`, `running`) that `Crash` clears.

Actions cover acceptance (`AcceptRun`, `AcceptCaptured` for a queued nextRun), every
queue admission (`QueueSteer`/`QueueFollow` refused under cancel, `QueueWrite` allowed
under cancel, `QueueNextRun` on any lane state, `IdleWrite`), `CancelQueued` triage,
the checkpoint drains, the effect sandwich for generation and one tool
(`Intent* → Dispatch* → Settle*`), recovery of an unknown effect (`RecoverGen`,
`RecoverTool`), `Abort`, `Terminal`, and `Crash`. Two lanes run interleaved.

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

## Running

```
make tla                         # CI size: ~1.5 min, ~53k states across both specs
make tla-deep                    # 4 entry ids, MaxSeq 7; tens of minutes
tla docs/tla/DurableHarness.tla --config docs/tla/DurableHarness.small.cfg -i   # step through
```

Configurations: `DurableLog.cfg` (2 ids, 1 key, 3 commits), `DurableHarness.small.cfg`
(2 lanes, 3 entry ids, 2 ops, 5 commits), `DurableHarness.cfg` (4 entry ids, 7 commits).
Deadlock checking is off: an idle session with every id consumed is a legal final state.

## What is not modelled

Deliberately small. No tool batches (one call per turn), no parallel tools, no
structural operations (compaction/navigation), no `before_run_end` follow-up, no
one-at-a-time queue mode (the model queues at most one item per kind, so `all` and
`one-at-a-time` coincide), no deferred provider requests, no `op.tool_args` register
lifecycle. Each is a bounded extension of `ops[o]`; add it when the corresponding Rust
lands and give it an invariant in the same change.
