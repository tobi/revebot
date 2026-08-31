---- MODULE DurableHarness ----
\* Reve durable harness — docs/harness.md Parts 1, 3.8, 3.11–3.13, 4.5–4.6, 9.1.
\*
\* One writer. Every mutation is one atomic transaction. Crash loses only
\* process-local effect state; recovery is a point lookup of op.state, never
\* a history fold. Inbox types (steer, follow-up, deferred write, nextRun)
\* share the pending.entry exclusivity rule: a queued id has its register,
\* its entry, or neither — never both. A tool turn is a batch of BatchSize
\* calls: clearance/intent in source order, dispatch concurrent, result
\* commits in source order (§3.8, parallel mode; sequential is a subset).
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS
    Lanes,
    EntryIds,
    OpIds,
    MaxSeq,
    MaxAttempts,
    BatchSize

None == "none"
Steer == "steer"
Follow == "follow"
WriteQ == "write"
NextRun == "nextrun"

PendingKinds == {None, Steer, Follow, WriteQ, NextRun}
Phases == {"checkpoint", "gen_pending", "tool_pending"}
Controls == {"running", "cancel"}
Continuations == {"need_assistant", "may_finish"}
Replays == {"n/a", "safe", "never"}
Outcomes == {None, "completed", "aborted", "failed"}
CallStatus == {None, "planned", "effect_pending", "completed"}
Calls == 1..BatchSize

\* One tool call of the current batch (§3.8 ToolCallState plus its
\* op.tool_args register and the process-local dispatch bits).
CallRecord == [
    status: CallStatus,
    result: {None} \cup EntryIds,   \* reserved result entry id
    replay: Replays,
    args: BOOLEAN,                  \* op.tool_args/{O}:{step}:{i} exists
    terminate: BOOLEAN,
    armed: BOOLEAN,
    running: BOOLEAN,
    runs: 0..2,
    interrupted: BOOLEAN,
    startedCancelled: BOOLEAN   \* ghost: dispatched while control = cancel
]

NoCall == [
    status |-> None, result |-> None, replay |-> "n/a", args |-> FALSE,
    terminate |-> FALSE, armed |-> FALSE, running |-> FALSE, runs |-> 0,
    interrupted |-> FALSE, startedCancelled |-> FALSE
]

OpRecord == [
    present: BOOLEAN,
    phase: Phases,
    control: Controls,
    continuation: Continuations,
    steer: {None} \cup EntryIds,
    follow: {None} \cup EntryIds,
    writes: {None} \cup EntryIds,
    drainedSteer: {None} \cup EntryIds,
    drainedFollow: {None} \cup EntryIds,
    skipOnce: BOOLEAN,
    latest: {None} \cup EntryIds,
    \* generation effect
    reserved: {None} \cup EntryIds,
    attempt: 0..MaxAttempts,
    armed: BOOLEAN,      \* this process just committed intent; may dispatch
    running: BOOLEAN,    \* this process has a live effect
    runs: 0..2,          \* dispatches of the current pending effect
    interrupted: BOOLEAN, \* a crash happened while this effect was pending
    startedCancelled: BOOLEAN, \* ghost: dispatched while control = cancel
    \* tool batch
    calls: [Calls -> CallRecord]
]

VARIABLES
    seq,
    placed,          \* [EntryIds -> BOOLEAN]  write-once entries
    pending,         \* [EntryIds -> PendingKinds]
    parent,          \* [EntryIds -> {None} \cup EntryIds]
    abortedStop,     \* [EntryIds -> BOOLEAN]  assistant stopReason = aborted
    billed,          \* [EntryIds -> BOOLEAN]  usage row committed with the entry
    laneOp,          \* [Lanes -> {None} \cup OpIds]
    nextRun,         \* [Lanes -> {None} \cup EntryIds]
    leaf,            \* [Lanes -> {None} \cup EntryIds]
    lastOutcome,     \* [Lanes -> Outcomes]
    ops,             \* [OpIds -> OpRecord]
    owes             \* [EntryIds -> SUBSET EntryIds] ghost: result ids an
                     \* assistant response planned (§0.5 "every tool call
                     \* has a result")

vars == <<seq, placed, pending, parent, abortedStop, billed,
          laneOp, nextRun, leaf, lastOutcome, ops, owes>>

NoCalls == [i \in Calls |-> NoCall]

AbsentOp == [
    present |-> FALSE,
    phase |-> "checkpoint",
    control |-> "running",
    continuation |-> "need_assistant",
    steer |-> None,
    follow |-> None,
    writes |-> None,
    drainedSteer |-> None,
    drainedFollow |-> None,
    skipOnce |-> FALSE,
    latest |-> None,
    reserved |-> None,
    attempt |-> 0,
    armed |-> FALSE,
    running |-> FALSE,
    runs |-> 0,
    interrupted |-> FALSE,
    startedCancelled |-> FALSE,
    calls |-> NoCalls
]

FreshOp == [AbsentOp EXCEPT !.present = TRUE]

-----------------------------------------------------------------------------

ReservedByCalls(o) == { ops[o].calls[i].result : i \in Calls } \ {None}

Free(e) ==
    /\ ~placed[e]
    /\ pending[e] = None
    /\ \A o \in OpIds:
         (~ops[o].present) \/ (ops[o].reserved # e /\ e \notin ReservedByCalls(o))

FreeSet == { e \in EntryIds : Free(e) }

Open(l) == laneOp[l] \in OpIds
OpOf(l) == laneOp[l]
Idle(l) == laneOp[l] = None
CanCommit == seq < MaxSeq

Place(e, p) ==
    /\ placed' = [placed EXCEPT ![e] = TRUE]
    /\ pending' = [pending EXCEPT ![e] = None]
    /\ parent' = [parent EXCEPT ![e] = p]

OwnedPending(o) ==
    { ops[o].steer, ops[o].follow, ops[o].writes,
      ops[o].drainedSteer, ops[o].drainedFollow } \ {None}

\* Earlier source positions have passed clearance (intent in source order).
EarlierCleared(o, i) ==
    \A j \in Calls: j < i => ops[o].calls[j].status # "planned"

\* Earlier source positions have committed results (results in source order).
EarlierCompleted(o, i) ==
    \A j \in Calls: j < i => ops[o].calls[j].status = "completed"

\* Result commit for call i. Folds batch completion into the last settlement:
\* every op.tool_args register of the batch is deleted and the phase returns
\* to checkpoint — may_finish iff every result terminated.
CompleteCall(o, i, term) ==
    LET c == [ops[o].calls[i] EXCEPT
                 !.status = "completed", !.terminate = term,
                 !.armed = FALSE, !.running = FALSE]
        calls2 == [ops[o].calls EXCEPT ![i] = c]
        done == \A j \in Calls: calls2[j].status = "completed"
        allTerm == \A j \in Calls: calls2[j].terminate
    IN IF done
       THEN [ops[o] EXCEPT
                !.calls = NoCalls,
                !.phase = "checkpoint",
                !.continuation = IF allTerm THEN "may_finish" ELSE "need_assistant"]
       ELSE [ops[o] EXCEPT !.calls = calls2]

-----------------------------------------------------------------------------

Init ==
    /\ seq = 0
    /\ placed = [e \in EntryIds |-> FALSE]
    /\ pending = [e \in EntryIds |-> None]
    /\ parent = [e \in EntryIds |-> None]
    /\ abortedStop = [e \in EntryIds |-> FALSE]
    /\ billed = [e \in EntryIds |-> FALSE]
    /\ laneOp = [l \in Lanes |-> None]
    /\ nextRun = [l \in Lanes |-> None]
    /\ leaf = [l \in Lanes |-> None]
    /\ lastOutcome = [l \in Lanes |-> None]
    /\ ops = [o \in OpIds |-> AbsentOp]
    /\ owes = [e \in EntryIds |-> {}]

-----------------------------------------------------------------------------
\* Lane admission. Prompt places immediately. nextRun never starts a run.

AcceptRun(l, e, o) ==
    /\ CanCommit
    /\ Idle(l)
    /\ ~ops[o].present
    /\ Free(e)
    /\ Place(e, leaf[l])
    /\ seq' = seq + 1
    /\ laneOp' = [laneOp EXCEPT ![l] = o]
    /\ leaf' = [leaf EXCEPT ![l] = e]
    /\ ops' = [ops EXCEPT ![o] = FreshOp]
    /\ UNCHANGED <<abortedStop, billed, nextRun, lastOutcome, owes>>

\* Capture a lane-owned nextRun as this run's prompt (the other order of
\* nextRun vs acceptance). Payload is already in pending.entry.
AcceptCaptured(l, o) ==
    /\ CanCommit
    /\ Idle(l)
    /\ ~ops[o].present
    /\ nextRun[l] \in EntryIds
    /\ LET e == nextRun[l]
       IN /\ pending[e] = NextRun
          /\ ~placed[e]
          /\ Place(e, leaf[l])
          /\ seq' = seq + 1
          /\ laneOp' = [laneOp EXCEPT ![l] = o]
          /\ leaf' = [leaf EXCEPT ![l] = e]
          /\ nextRun' = [nextRun EXCEPT ![l] = None]
          /\ ops' = [ops EXCEPT ![o] = FreshOp]
          /\ UNCHANGED <<abortedStop, billed, lastOutcome, owes>>

QueueNextRun(l, e) ==
    /\ CanCommit
    /\ Free(e)
    /\ nextRun[l] = None
    /\ pending' = [pending EXCEPT ![e] = NextRun]
    /\ nextRun' = [nextRun EXCEPT ![l] = e]
    /\ seq' = seq + 1
    /\ UNCHANGED <<placed, parent, abortedStop, billed, laneOp, leaf, lastOutcome, ops, owes>>

QueueSteer(l, e) ==
    /\ CanCommit
    /\ Open(l)
    /\ LET o == OpOf(l)
       IN /\ ops[o].present
          /\ ops[o].control = "running"
          /\ ops[o].steer = None
          /\ Free(e)
          /\ pending' = [pending EXCEPT ![e] = Steer]
          /\ ops' = [ops EXCEPT ![o] = [ops[o] EXCEPT !.steer = e]]
          /\ seq' = seq + 1
          /\ UNCHANGED <<placed, parent, abortedStop, billed, laneOp, nextRun, leaf, lastOutcome, owes>>

QueueFollow(l, e) ==
    /\ CanCommit
    /\ Open(l)
    /\ LET o == OpOf(l)
       IN /\ ops[o].present
          /\ ops[o].control = "running"
          /\ ops[o].follow = None
          /\ Free(e)
          /\ pending' = [pending EXCEPT ![e] = Follow]
          /\ ops' = [ops EXCEPT ![o] = [ops[o] EXCEPT !.follow = e]]
          /\ seq' = seq + 1
          /\ UNCHANGED <<placed, parent, abortedStop, billed, laneOp, nextRun, leaf, lastOutcome, owes>>

\* Deferred writes are accepted even under cancel_requested and survive abort.
QueueWrite(l, e) ==
    /\ CanCommit
    /\ Open(l)
    /\ LET o == OpOf(l)
       IN /\ ops[o].present
          /\ ops[o].writes = None
          /\ Free(e)
          /\ pending' = [pending EXCEPT ![e] = WriteQ]
          /\ ops' = [ops EXCEPT ![o] = [ops[o] EXCEPT !.writes = e]]
          /\ seq' = seq + 1
          /\ UNCHANGED <<placed, parent, abortedStop, billed, laneOp, nextRun, leaf, lastOutcome, owes>>

IdleWrite(l, e) ==
    /\ CanCommit
    /\ Idle(l)
    /\ Free(e)
    /\ Place(e, leaf[l])
    /\ leaf' = [leaf EXCEPT ![l] = e]
    /\ seq' = seq + 1
    /\ UNCHANGED <<abortedStop, billed, laneOp, nextRun, lastOutcome, ops, owes>>

\* cancelQueued triage: still in a queue list → cancelled (delete register);
\* placed → already_consumed (no write); else not_found (no write). An
\* abort-drained id is in no queue list: it is not_found and its register
\* survives for AbortResult until the terminal transaction.
InQueueList(e) ==
    \/ \E l \in Lanes: nextRun[l] = e
    \/ \E o \in OpIds:
         /\ ops[o].present
         /\ e \in {ops[o].steer, ops[o].follow, ops[o].writes}

CancelQueued(e) ==
    /\ CanCommit
    /\ pending[e] # None
    /\ ~placed[e]
    /\ InQueueList(e)
    /\ pending' = [pending EXCEPT ![e] = None]
    /\ seq' = seq + 1
    /\ nextRun' = [l \in Lanes |-> IF nextRun[l] = e THEN None ELSE nextRun[l]]
    /\ ops' = [o \in OpIds |->
           IF ~ops[o].present THEN ops[o]
           ELSE [ops[o] EXCEPT
               !.steer = IF ops[o].steer = e THEN None ELSE @,
               !.follow = IF ops[o].follow = e THEN None ELSE @,
               !.writes = IF ops[o].writes = e THEN None ELSE @]]
    /\ UNCHANGED <<placed, parent, abortedStop, billed, laneOp, leaf, lastOutcome, owes>>

-----------------------------------------------------------------------------
\* Checkpoint drains. Projecting consumption places the entry, deletes the
\* pending register, moves the leaf, and sets skipInboxOnce.

DrainSteer(l) ==
    /\ CanCommit
    /\ Open(l)
    /\ LET o == OpOf(l)
           e == ops[o].steer
       IN /\ ops[o].present
          /\ ops[o].phase = "checkpoint"
          /\ ~ops[o].skipOnce
          /\ e \in EntryIds
          /\ pending[e] = Steer
          /\ Place(e, leaf[l])
          /\ leaf' = [leaf EXCEPT ![l] = e]
          /\ ops' = [ops EXCEPT ![o] = [ops[o] EXCEPT
                 !.steer = None,
                 !.skipOnce = TRUE,
                 !.continuation = "need_assistant"]]
          /\ seq' = seq + 1
          /\ UNCHANGED <<abortedStop, billed, laneOp, nextRun, lastOutcome, owes>>

DrainFollow(l) ==
    /\ CanCommit
    /\ Open(l)
    /\ LET o == OpOf(l)
           e == ops[o].follow
       IN /\ ops[o].present
          /\ ops[o].phase = "checkpoint"
          /\ ~ops[o].skipOnce
          /\ e \in EntryIds
          /\ pending[e] = Follow
          /\ Place(e, leaf[l])
          /\ leaf' = [leaf EXCEPT ![l] = e]
          /\ ops' = [ops EXCEPT ![o] = [ops[o] EXCEPT
                 !.follow = None,
                 !.skipOnce = TRUE,
                 !.continuation = "need_assistant"]]
          /\ seq' = seq + 1
          /\ UNCHANGED <<abortedStop, billed, laneOp, nextRun, lastOutcome, owes>>

DrainWrite(l) ==
    /\ CanCommit
    /\ Open(l)
    /\ LET o == OpOf(l)
           e == ops[o].writes
       IN /\ ops[o].present
          /\ ops[o].phase = "checkpoint"
          /\ e \in EntryIds
          /\ pending[e] = WriteQ
          /\ Place(e, leaf[l])
          /\ leaf' = [leaf EXCEPT ![l] = e]
          /\ ops' = [ops EXCEPT ![o] = [ops[o] EXCEPT
                 !.writes = None,
                 !.skipOnce = TRUE,
                 !.continuation = IF ops[o].control = "cancel"
                                  THEN ops[o].continuation
                                  ELSE "need_assistant"]]
          /\ seq' = seq + 1
          /\ UNCHANGED <<abortedStop, billed, laneOp, nextRun, lastOutcome, owes>>

-----------------------------------------------------------------------------
\* Generation: intent commit, process-local dispatch, settlement. Crash
\* clears armed/running; recovery then follows the stored phase.

IntentGen(l, e) ==
    /\ CanCommit
    /\ Open(l)
    /\ LET o == OpOf(l)
       IN /\ ops[o].present
          /\ ops[o].phase = "checkpoint"
          /\ ops[o].control = "running"
          /\ ops[o].continuation = "need_assistant"
          /\ Free(e)
          /\ ops' = [ops EXCEPT ![o] = [ops[o] EXCEPT
                 !.phase = "gen_pending",
                 !.reserved = e,
                 !.attempt = 1,
                 !.armed = TRUE,
                 !.running = FALSE,
                 !.runs = 0,
                 !.interrupted = FALSE,
                 !.skipOnce = FALSE]]
          /\ seq' = seq + 1
          /\ UNCHANGED <<placed, pending, parent, abortedStop, billed,
                         laneOp, nextRun, leaf, lastOutcome, owes>>

DispatchGen(l) ==
    /\ Open(l)
    /\ LET o == OpOf(l)
       IN /\ ops[o].present
          /\ ops[o].phase = "gen_pending"
          /\ ops[o].armed
          /\ ~ops[o].running
          /\ ops[o].runs < 2
          /\ ops' = [ops EXCEPT ![o] = [ops[o] EXCEPT
                 !.armed = FALSE, !.running = TRUE, !.runs = ops[o].runs + 1,
                 !.startedCancelled = @ \/ ops[o].control = "cancel"]]
          /\ UNCHANGED <<seq, placed, pending, parent, abortedStop, billed,
                         laneOp, nextRun, leaf, lastOutcome, owes>>

\* Settlement with no tool calls: response + usage, then checkpoint may_finish.
SettleGen(l) ==
    /\ CanCommit
    /\ Open(l)
    /\ LET o == OpOf(l)
           e == ops[o].reserved
       IN /\ ops[o].present
          /\ ops[o].phase = "gen_pending"
          /\ ops[o].running
          /\ e \in EntryIds
          /\ ~placed[e]
          /\ Place(e, leaf[l])
          /\ billed' = [billed EXCEPT ![e] = TRUE]
          /\ abortedStop' = [abortedStop EXCEPT ![e] = FALSE]
          /\ leaf' = [leaf EXCEPT ![l] = e]
          /\ ops' = [ops EXCEPT ![o] = [ops[o] EXCEPT
                 !.phase = "checkpoint",
                 !.continuation = "may_finish",
                 !.reserved = None,
                 !.latest = e,
                 !.armed = FALSE,
                 !.running = FALSE,
                 !.attempt = 0]]
          /\ seq' = seq + 1
          /\ UNCHANGED <<laneOp, nextRun, lastOutcome, owes>>

\* Settlement that plans a batch: same response + usage, then tool_pending
\* with BatchSize result ids reserved as followers of the response (§1.2).
\* Which fresh ids are minted is irrelevant, so CHOOSE keeps it deterministic.
RECURSIVE PickN(_, _)
PickN(pool, n) ==
    IF n = 0 THEN <<>>
    ELSE LET r == CHOOSE x \in pool: TRUE
         IN <<r>> \o PickN(pool \ {r}, n - 1)

SettleGenToTool(l) ==
    /\ CanCommit
    /\ Open(l)
    /\ LET o == OpOf(l)
           e == ops[o].reserved
           pool == FreeSet \ {e}
       IN /\ ops[o].present
          /\ ops[o].phase = "gen_pending"
          /\ ops[o].running
          /\ ops[o].control = "running"
          /\ e \in EntryIds
          /\ ~placed[e]
          /\ Cardinality(pool) >= BatchSize
          /\ LET results == PickN(pool, BatchSize)
             IN /\ Place(e, leaf[l])
                /\ billed' = [billed EXCEPT ![e] = TRUE]
                /\ abortedStop' = [abortedStop EXCEPT ![e] = FALSE]
                /\ leaf' = [leaf EXCEPT ![l] = e]
                /\ ops' = [ops EXCEPT ![o] = [ops[o] EXCEPT
                       !.phase = "tool_pending",
                       !.reserved = None,
                       !.latest = e,
                       !.armed = FALSE,
                       !.running = FALSE,
                       !.attempt = 0,
                       !.calls = [i \in Calls |->
                           [NoCall EXCEPT !.status = "planned", !.result = results[i]]]]]
                /\ owes' = [owes EXCEPT ![e] = { results[i] : i \in Calls }]
                /\ seq' = seq + 1
                /\ UNCHANGED <<laneOp, nextRun, lastOutcome>>

\* Recovery of an unknown generation effect (§4.5): intent durable, this
\* process did not start it.
RecoverGen(l) ==
    /\ CanCommit
    /\ Open(l)
    /\ LET o == OpOf(l)
           e == ops[o].reserved
       IN /\ ops[o].present
          /\ ops[o].phase = "gen_pending"
          /\ ~ops[o].armed
          /\ ~ops[o].running
          /\ e \in EntryIds
          /\ ~placed[e]
          /\ \/ \* cancellation durable: synthetic aborted under the reserved id
                /\ ops[o].control = "cancel"
                /\ Place(e, leaf[l])
                /\ billed' = [billed EXCEPT ![e] = TRUE]
                /\ abortedStop' = [abortedStop EXCEPT ![e] = TRUE]
                /\ leaf' = [leaf EXCEPT ![l] = e]
                /\ ops' = [ops EXCEPT ![o] = [ops[o] EXCEPT
                       !.phase = "checkpoint",
                       !.continuation = "may_finish",
                       !.reserved = None,
                       !.latest = e,
                       !.attempt = 0]]
                /\ seq' = seq + 1
                /\ UNCHANGED <<laneOp, nextRun, lastOutcome, owes>>
             \/ \* captured retry policy allows a later numbered attempt
                /\ ops[o].control = "running"
                /\ ops[o].attempt < MaxAttempts
                /\ ops' = [ops EXCEPT ![o] = [ops[o] EXCEPT
                       !.attempt = ops[o].attempt + 1,
                       !.armed = TRUE]]
                /\ UNCHANGED <<seq, placed, pending, parent, abortedStop, billed,
                               laneOp, nextRun, leaf, lastOutcome, owes>>
             \/ \* budget exhausted: synthetic error under the reserved id
                /\ ops[o].control = "running"
                /\ ops[o].attempt >= MaxAttempts
                /\ Place(e, leaf[l])
                /\ billed' = [billed EXCEPT ![e] = TRUE]
                /\ abortedStop' = [abortedStop EXCEPT ![e] = FALSE]
                /\ leaf' = [leaf EXCEPT ![l] = e]
                /\ ops' = [ops EXCEPT ![o] = [ops[o] EXCEPT
                       !.phase = "checkpoint",
                       !.continuation = "may_finish",
                       !.reserved = None,
                       !.latest = e,
                       !.attempt = 0]]
                /\ seq' = seq + 1
                /\ UNCHANGED <<laneOp, nextRun, lastOutcome, owes>>

-----------------------------------------------------------------------------
\* Tool batch (§3.8, parallel mode).

\* Clearance passed: op.tool_args written and call i = effect_pending with
\* its replay declaration, in source order. Forbidden under cancel.
ClearTool(l, i, p) ==
    /\ CanCommit
    /\ Open(l)
    /\ LET o == OpOf(l)
       IN /\ ops[o].present
          /\ ops[o].phase = "tool_pending"
          /\ ops[o].control = "running"
          /\ ops[o].calls[i].status = "planned"
          /\ EarlierCleared(o, i)
          /\ ops' = [ops EXCEPT ![o] = [ops[o] EXCEPT !.calls =
                 [ops[o].calls EXCEPT ![i] =
                     [@ EXCEPT !.status = "effect_pending", !.replay = p,
                               !.args = TRUE, !.armed = TRUE, !.runs = 0,
                               !.interrupted = FALSE]]]]
          /\ seq' = seq + 1
          /\ UNCHANGED <<placed, pending, parent, abortedStop, billed,
                         laneOp, nextRun, leaf, lastOutcome, owes>>

\* Clearance failed (unknown tool, invalid args, before_tool blocked, or
\* control cancelled): no intent, no effect, no op.tool_args; a synthetic
\* error result commits at the source position. Results commit in order.
BlockTool(l, i, term) ==
    /\ CanCommit
    /\ Open(l)
    /\ LET o == OpOf(l)
           e == ops[o].calls[i].result
       IN /\ ops[o].present
          /\ ops[o].phase = "tool_pending"
          /\ ops[o].calls[i].status = "planned"
          /\ EarlierCompleted(o, i)
          /\ e \in EntryIds
          /\ ~placed[e]
          /\ Place(e, leaf[l])
          /\ leaf' = [leaf EXCEPT ![l] = e]
          /\ ops' = [ops EXCEPT ![o] = CompleteCall(o, i, term)]
          /\ seq' = seq + 1
          /\ UNCHANGED <<abortedStop, billed, laneOp, nextRun, lastOutcome, owes>>

\* Dispatch does not await earlier calls. Never under cancel.
DispatchTool(l, i) ==
    /\ Open(l)
    /\ LET o == OpOf(l)
           c == ops[o].calls[i]
       IN /\ ops[o].present
          /\ ops[o].phase = "tool_pending"
          /\ ops[o].control = "running"
          /\ c.status = "effect_pending"
          /\ c.armed
          /\ ~c.running
          /\ c.runs < 2
          /\ ops' = [ops EXCEPT ![o] = [ops[o] EXCEPT !.calls =
                 [ops[o].calls EXCEPT ![i] =
                     [@ EXCEPT !.armed = FALSE, !.running = TRUE, !.runs = @ + 1,
                               !.startedCancelled = @ \/ ops[o].control = "cancel"]]]]
          /\ UNCHANGED <<seq, placed, pending, parent, abortedStop, billed,
                         laneOp, nextRun, leaf, lastOutcome, owes>>

\* A live effect settles; its result commits only at its source turn. A
\* live tool that outlives an abort keeps its raw result (§4.6).
SettleTool(l, i, term) ==
    /\ CanCommit
    /\ Open(l)
    /\ LET o == OpOf(l)
           c == ops[o].calls[i]
           e == c.result
       IN /\ ops[o].present
          /\ ops[o].phase = "tool_pending"
          /\ c.status = "effect_pending"
          /\ c.running
          /\ EarlierCompleted(o, i)
          /\ e \in EntryIds
          /\ ~placed[e]
          /\ Place(e, leaf[l])
          /\ leaf' = [leaf EXCEPT ![l] = e]
          /\ ops' = [ops EXCEPT ![o] = CompleteCall(o, i, term)]
          /\ seq' = seq + 1
          /\ UNCHANGED <<abortedStop, billed, laneOp, nextRun, lastOutcome, owes>>

\* Recovery of an unknown tool effect (§4.5): re-execute from the persisted
\* op.tool_args only if replay is safe and control is running; otherwise a
\* synthetic interrupted result under the reserved id.
RecoverTool(l, i) ==
    /\ CanCommit
    /\ Open(l)
    /\ LET o == OpOf(l)
           c == ops[o].calls[i]
           e == c.result
       IN /\ ops[o].present
          /\ ops[o].phase = "tool_pending"
          /\ c.status = "effect_pending"
          /\ ~c.armed
          /\ ~c.running
          /\ e \in EntryIds
          /\ ~placed[e]
          /\ \/ /\ c.replay = "safe"
                /\ ops[o].control = "running"
                /\ c.args
                /\ ops' = [ops EXCEPT ![o] = [ops[o] EXCEPT !.calls =
                       [ops[o].calls EXCEPT ![i] = [@ EXCEPT !.armed = TRUE]]]]
                /\ UNCHANGED <<seq, placed, pending, parent, abortedStop, billed,
                               laneOp, nextRun, leaf, lastOutcome, owes>>
             \/ /\ \/ c.replay = "never"
                   \/ ops[o].control = "cancel"
                /\ EarlierCompleted(o, i)
                /\ Place(e, leaf[l])
                /\ leaf' = [leaf EXCEPT ![l] = e]
                /\ ops' = [ops EXCEPT ![o] = CompleteCall(o, i, FALSE)]
                /\ seq' = seq + 1
                /\ UNCHANGED <<abortedStop, billed, laneOp, nextRun, lastOutcome, owes>>

-----------------------------------------------------------------------------
\* Abort is control, not a phase. First abort drains steer/follow-up into
\* drained* and does not delete their pending.entry registers. Writes stay.
\* The process-local armed bits are dropped: nothing new may start.

Abort(l) ==
    /\ CanCommit
    /\ Open(l)
    /\ LET o == OpOf(l)
       IN /\ ops[o].present
          /\ ops[o].control = "running"
          /\ ops' = [ops EXCEPT ![o] = [ops[o] EXCEPT
                 !.control = "cancel",
                 !.drainedSteer = ops[o].steer,
                 !.drainedFollow = ops[o].follow,
                 !.steer = None,
                 !.follow = None,
                 !.armed = FALSE,
                 !.calls = [i \in Calls |-> [ops[o].calls[i] EXCEPT !.armed = FALSE]]]]
          /\ seq' = seq + 1
          /\ UNCHANGED <<placed, pending, parent, abortedStop, billed,
                         laneOp, nextRun, leaf, lastOutcome, owes>>

\* Terminal: delete every operation-owned register (op.state, op.tool_args
\* prefix, operation-owned pending.entry), write lastResult, clear the
\* claim. Never deletes lane-owned pendingNextRun.
Terminal(l) ==
    /\ CanCommit
    /\ Open(l)
    /\ LET o == OpOf(l)
           outcome == IF ops[o].control = "cancel" THEN "aborted"
                      ELSE IF ops[o].continuation = "may_finish" THEN "completed"
                      ELSE "failed"
           drop == OwnedPending(o)
       IN /\ ops[o].present
          /\ ops[o].phase = "checkpoint"
          /\ ~ops[o].running
          /\ ops[o].writes = None
          /\ \/ ops[o].control = "cancel"
             \/ /\ ops[o].steer = None
                /\ ops[o].follow = None
                /\ ops[o].continuation = "may_finish"
          /\ pending' = [e \in EntryIds |->
                 IF e \in drop THEN None ELSE pending[e]]
          /\ laneOp' = [laneOp EXCEPT ![l] = None]
          /\ lastOutcome' = [lastOutcome EXCEPT ![l] = outcome]
          /\ ops' = [ops EXCEPT ![o] = AbsentOp]
          /\ seq' = seq + 1
          /\ UNCHANGED <<placed, parent, abortedStop, billed, nextRun, leaf, owes>>

\* Process death. Durable maps are untouched. Live effects and the "armed
\* to dispatch" bits die with the process; pending effects are interrupted.
Crash ==
    /\ ops' = [o \in OpIds |->
           [ops[o] EXCEPT
               !.armed = FALSE,
               !.running = FALSE,
               !.interrupted = @ \/ (ops[o].present /\ ops[o].phase = "gen_pending"),
               !.calls = [i \in Calls |->
                   [ops[o].calls[i] EXCEPT
                       !.armed = FALSE,
                       !.running = FALSE,
                       !.interrupted = @ \/ (ops[o].calls[i].status = "effect_pending")]]]]
    /\ UNCHANGED <<seq, placed, pending, parent, abortedStop, billed,
                   laneOp, nextRun, leaf, lastOutcome, owes>>

Next ==
    \/ \E l \in Lanes, e \in EntryIds, o \in OpIds: AcceptRun(l, e, o)
    \/ \E l \in Lanes, o \in OpIds: AcceptCaptured(l, o)
    \/ \E l \in Lanes, e \in EntryIds: QueueNextRun(l, e)
    \/ \E l \in Lanes, e \in EntryIds: QueueSteer(l, e)
    \/ \E l \in Lanes, e \in EntryIds: QueueFollow(l, e)
    \/ \E l \in Lanes, e \in EntryIds: QueueWrite(l, e)
    \/ \E l \in Lanes, e \in EntryIds: IdleWrite(l, e)
    \/ \E e \in EntryIds: CancelQueued(e)
    \/ \E l \in Lanes: DrainSteer(l)
    \/ \E l \in Lanes: DrainFollow(l)
    \/ \E l \in Lanes: DrainWrite(l)
    \/ \E l \in Lanes, e \in EntryIds: IntentGen(l, e)
    \/ \E l \in Lanes: DispatchGen(l)
    \/ \E l \in Lanes: SettleGen(l)
    \/ \E l \in Lanes: SettleGenToTool(l)
    \/ \E l \in Lanes: RecoverGen(l)
    \/ \E l \in Lanes, i \in Calls, p \in {"safe", "never"}: ClearTool(l, i, p)
    \/ \E l \in Lanes, i \in Calls, t \in BOOLEAN: BlockTool(l, i, t)
    \/ \E l \in Lanes, i \in Calls: DispatchTool(l, i)
    \/ \E l \in Lanes, i \in Calls, t \in BOOLEAN: SettleTool(l, i, t)
    \/ \E l \in Lanes, i \in Calls: RecoverTool(l, i)
    \/ \E l \in Lanes: Abort(l)
    \/ \E l \in Lanes: Terminal(l)
    \/ Crash

-----------------------------------------------------------------------------

TypeOK ==
    /\ seq \in 0..MaxSeq
    /\ placed \in [EntryIds -> BOOLEAN]
    /\ pending \in [EntryIds -> PendingKinds]
    /\ parent \in [EntryIds -> {None} \cup EntryIds]
    /\ abortedStop \in [EntryIds -> BOOLEAN]
    /\ billed \in [EntryIds -> BOOLEAN]
    /\ laneOp \in [Lanes -> {None} \cup OpIds]
    /\ nextRun \in [Lanes -> {None} \cup EntryIds]
    /\ leaf \in [Lanes -> {None} \cup EntryIds]
    /\ lastOutcome \in [Lanes -> Outcomes]
    /\ ops \in [OpIds -> OpRecord]
    /\ owes \in [EntryIds -> SUBSET EntryIds]

\* §9.1.15 / §3.11: a queued id has its register, its entry, or neither.
InvExclusivity ==
    \A e \in EntryIds: ~(placed[e] /\ pending[e] # None)

\* §9.1.15 settlement regime: a generation id reserved in op.state is not
\* placed and not queued.
InvReservedFresh ==
    \A o \in OpIds:
        ops[o].present /\ ops[o].reserved \in EntryIds =>
            /\ ~placed[ops[o].reserved]
            /\ pending[ops[o].reserved] = None

\* §9.1.17 / §9.1.12: at most one open operation per lane, and the
\* claim register names it.
InvLaneOwnership ==
    /\ \A o \in OpIds:
           ops[o].present <=> \E l \in Lanes: laneOp[l] = o
    /\ \A l1, l2 \in Lanes:
           l1 # l2 /\ laneOp[l1] \in OpIds /\ laneOp[l2] \in OpIds =>
               laneOp[l1] # laneOp[l2]

\* §9.1.13: op.* exist iff the operation is open. Modelled by ops[o].present
\* being equivalent to the lane claim. Inbox ids and tool args of a closed
\* op are gone.
InvClosedOpOwnsNothing ==
    \A o \in OpIds:
        ~ops[o].present =>
            /\ ops[o].steer = None
            /\ ops[o].follow = None
            /\ ops[o].writes = None
            /\ ops[o].drainedSteer = None
            /\ ops[o].drainedFollow = None
            /\ ops[o].reserved = None
            /\ ~ops[o].armed
            /\ ~ops[o].running
            /\ ops[o].calls = NoCalls

\* nextRun is lane-owned: terminal does not drop it, and its pending kind
\* stays nextRun until captured or cancelled.
InvNextRunLaneOwned ==
    \A l \in Lanes:
        nextRun[l] \in EntryIds =>
            /\ pending[nextRun[l]] = NextRun
            /\ ~placed[nextRun[l]]

\* Inbox ids, drained or not, still have their pending register.
InvInboxHasRegister ==
    \A o \in OpIds:
        ops[o].present =>
            /\ (ops[o].steer \in EntryIds => pending[ops[o].steer] = Steer)
            /\ (ops[o].follow \in EntryIds => pending[ops[o].follow] = Follow)
            /\ (ops[o].writes \in EntryIds => pending[ops[o].writes] = WriteQ)
            /\ (ops[o].drainedSteer \in EntryIds =>
                    pending[ops[o].drainedSteer] = Steer)
            /\ (ops[o].drainedFollow \in EntryIds =>
                    pending[ops[o].drainedFollow] = Follow)

\* Abort does not delete drained payloads. Writes are not drained by abort.
InvAbortKeepsWritesAndPayloads ==
    \A o \in OpIds:
        ops[o].present /\ ops[o].control = "cancel" =>
            /\ ops[o].steer = None
            /\ ops[o].follow = None
            /\ (ops[o].drainedSteer \in EntryIds =>
                    (pending[ops[o].drainedSteer] = Steer /\ ~placed[ops[o].drainedSteer]))
            /\ (ops[o].drainedFollow \in EntryIds =>
                    (pending[ops[o].drainedFollow] = Follow /\ ~placed[ops[o].drainedFollow]))

\* §9.1.19: an aborted stopReason is committed only under cancel_requested.
\* After terminal the op is gone; the entry remains as history.
InvAbortedImpliesCancel ==
    \A e \in EntryIds:
        abortedStop[e] =>
            /\ placed[e]
            /\ billed[e]
            /\ \A o \in OpIds:
                   (ops[o].present /\ ops[o].latest = e) =>
                       ops[o].control = "cancel"

\* Intent before effect: a live effect requires a committed pending intent
\* with its ids reserved — for the generation and for every call.
InvNoEffectWithoutIntent ==
    \A o \in OpIds:
        /\ (ops[o].running =>
                /\ ops[o].present
                /\ ops[o].phase = "gen_pending"
                /\ ops[o].reserved \in EntryIds)
        /\ \A i \in Calls:
               ops[o].calls[i].running =>
                   /\ ops[o].present
                   /\ ops[o].phase = "tool_pending"
                   /\ ops[o].calls[i].status = "effect_pending"
                   /\ ops[o].calls[i].args

\* §4.5: a tool whose intent survived a crash is re-executed only if replay
\* is safe. A never tool that was interrupted is never dispatched again, not
\* even once.
InvInterruptedNeverToolNotDispatched ==
    \A o \in OpIds, i \in Calls:
        ops[o].calls[i].running /\ ops[o].calls[i].replay = "never" =>
            ~ops[o].calls[i].interrupted

\* §4.6: after the abort commit nothing new starts — no provider request,
\* no tool. (A live effect may still settle; that is a different bit.)
InvNoNewEffectUnderCancel ==
    \A o \in OpIds:
        /\ ~ops[o].startedCancelled
        /\ \A i \in Calls: ~ops[o].calls[i].startedCancelled

\* An unsafe tool is dispatched at most once.
InvUnsafeToolAtMostOnce ==
    \A o \in OpIds, i \in Calls:
        ops[o].calls[i].replay = "never" => ops[o].calls[i].runs <= 1

\* §3.8 source order: clearance/intent commits and result commits both
\* happen in source order, even though effects run concurrently.
InvCallsSourceOrder ==
    \A o \in OpIds, i, j \in Calls:
        i < j /\ ops[o].phase = "tool_pending" =>
            /\ (ops[o].calls[j].status # "planned" => ops[o].calls[i].status # "planned")
            /\ (ops[o].calls[j].status = "completed" => ops[o].calls[i].status = "completed")

\* §3.8 / §9.1.13: op.tool_args is written at clearance, never for a
\* blocked call, and is gone once the batch completes or the op ends.
InvToolArgsLifecycle ==
    /\ \A o \in OpIds, i \in Calls:
           ops[o].calls[i].args =>
               (/\ ops[o].present
                /\ ops[o].phase = "tool_pending"
                /\ ops[o].calls[i].status \in {"effect_pending", "completed"})
    /\ \A o \in OpIds:
           ops[o].phase # "tool_pending" => ops[o].calls = NoCalls

\* Reserved result ids are fresh until their result commits, distinct across
\* the batch, and placed exactly when the call is completed.
InvToolResultsReserved ==
    \A o \in OpIds:
        ops[o].present /\ ops[o].phase = "tool_pending" =>
            /\ \A i \in Calls:
                   (/\ ops[o].calls[i].result \in EntryIds
                    /\ pending[ops[o].calls[i].result] = None
                    /\ (ops[o].calls[i].status = "completed" <=> placed[ops[o].calls[i].result]))
            /\ \A i, j \in Calls:
                   i # j => ops[o].calls[i].result # ops[o].calls[j].result

\* Settlement writes response and usage together. Tool results are not billed.
InvResponseHasUsage ==
    \A e \in EntryIds:
        billed[e] => placed[e]

\* Missing parent is corruption: a placed entry's parent is none or placed.
InvParentsExist ==
    \A e \in EntryIds:
        placed[e] /\ parent[e] \in EntryIds => placed[parent[e]]

\* Reserved settlement ids are not also queued.
InvReservationRegimes ==
    \A o \in OpIds:
        ops[o].present /\ ops[o].reserved \in EntryIds =>
            pending[ops[o].reserved] = None

\* §0.5: every tool call has a result. Once no open operation is still
\* working the batch of assistant e, every result it planned is placed.
\* Cancellation reconciliation must therefore settle every call before the
\* terminal transaction.
InvEveryToolCallHasResult ==
    \A e \in EntryIds:
        (\A o \in OpIds:
            ~(ops[o].present /\ ops[o].phase = "tool_pending" /\ ops[o].latest = e))
        => \A r \in owes[e]: placed[r]

\* Coverage probes (not invariants): `tla ... --count-satisfying CovX`.
\* A bound that makes any of these zero is too small to test the batch.
CovTwoToolsLive ==
    \E o \in OpIds: \A i \in Calls: ops[o].calls[i].running
CovLaterCallSettledFirst ==
    \E o \in OpIds, i, j \in Calls:
        /\ i < j
        /\ ops[o].calls[i].running
        /\ ~ops[o].calls[j].running
        /\ ops[o].calls[j].status = "effect_pending"
        /\ ops[o].calls[j].runs > 0
CovBatchCompleted ==
    \E l \in Lanes:
        /\ Open(l)
        /\ ops[laneOp[l]].phase = "checkpoint"
        /\ ops[laneOp[l]].latest \in EntryIds
        /\ leaf[l] # ops[laneOp[l]].latest
CovInterruptedNeverSynthesized ==
    \E o \in OpIds, i \in Calls:
        /\ ops[o].calls[i].status = "completed"
        /\ ops[o].calls[i].replay = "never"
        /\ ops[o].calls[i].interrupted
CovBothLanesOpen ==
    \A l \in Lanes: Open(l)

\* At most one terminal per open operation: Terminal requires present and
\* clears it, so two terminals cannot fire without an Accept in between.
InvPresentIffClaimed ==
    \A l \in Lanes:
        Idle(l) <=> \A o \in OpIds: ~(ops[o].present /\ laneOp[l] = o)

====
