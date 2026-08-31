---- MODULE DurableHarness ----
\* Reve durable harness — docs/harness.md Parts 1, 3.11–3.13, 4.5–4.6, 9.1.
\*
\* One writer. Every mutation is one atomic transaction. Crash loses only
\* process-local effect state; recovery is a point lookup of op.state, never
\* a history fold. Inbox types (steer, follow-up, deferred write, nextRun)
\* share the pending.entry exclusivity rule: a queued id has its register,
\* its entry, or neither — never both.
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS
    Lanes,
    EntryIds,
    OpIds,
    MaxSeq,
    MaxAttempts

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
    reserved: {None} \cup EntryIds,
    replay: Replays,
    attempt: 0..MaxAttempts,
    latest: {None} \cup EntryIds,
    armed: BOOLEAN,      \* this process just committed intent; may dispatch
    running: BOOLEAN,    \* this process has a live effect
    runs: 0..2,          \* dispatches of the current pending effect
    interrupted: BOOLEAN \* a crash happened while this effect was pending
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
    ops              \* [OpIds -> OpRecord]

vars == <<seq, placed, pending, parent, abortedStop, billed,
          laneOp, nextRun, leaf, lastOutcome, ops>>

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
    reserved |-> None,
    replay |-> "n/a",
    attempt |-> 0,
    latest |-> None,
    armed |-> FALSE,
    running |-> FALSE,
    runs |-> 0,
    interrupted |-> FALSE
]

-----------------------------------------------------------------------------

Free(e) ==
    /\ ~placed[e]
    /\ pending[e] = None
    /\ \A o \in OpIds: (~ops[o].present) \/ ops[o].reserved # e

Open(l) == laneOp[l] \in OpIds

OpOf(l) == laneOp[l]

Idle(l) == laneOp[l] = None

CanCommit == seq < MaxSeq

Place(e, p) ==
    /\ placed' = [placed EXCEPT ![e] = TRUE]
    /\ pending' = [pending EXCEPT ![e] = None]
    /\ parent' = [parent EXCEPT ![e] = p]

ClearOp(o) ==
    ops' = [ops EXCEPT ![o] = AbsentOp]

OwnedPending(o) ==
    { ops[o].steer, ops[o].follow, ops[o].writes,
      ops[o].drainedSteer, ops[o].drainedFollow } \ {None}

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
    /\ ops' = [ops EXCEPT ![o] = [
           present |-> TRUE,
           phase |-> "checkpoint",
           control |-> "running",
           continuation |-> "need_assistant",
           steer |-> None,
           follow |-> None,
           writes |-> None,
           drainedSteer |-> None,
           drainedFollow |-> None,
           skipOnce |-> FALSE,
           reserved |-> None,
           replay |-> "n/a",
           attempt |-> 0,
           latest |-> None,
           armed |-> FALSE,
           running |-> FALSE,
           runs |-> 0,
           interrupted |-> FALSE
       ]]
    /\ UNCHANGED <<abortedStop, billed, nextRun, lastOutcome>>

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
          /\ ops' = [ops EXCEPT ![o] = [
                 present |-> TRUE,
                 phase |-> "checkpoint",
                 control |-> "running",
                 continuation |-> "need_assistant",
                 steer |-> None,
                 follow |-> None,
                 writes |-> None,
                 drainedSteer |-> None,
                 drainedFollow |-> None,
                 skipOnce |-> FALSE,
                 reserved |-> None,
                 replay |-> "n/a",
                 attempt |-> 0,
                 latest |-> None,
                 armed |-> FALSE,
                 running |-> FALSE,
                 runs |-> 0,
                 interrupted |-> FALSE
             ]]
          /\ UNCHANGED <<abortedStop, billed, lastOutcome>>

QueueNextRun(l, e) ==
    /\ CanCommit
    /\ Free(e)
    /\ nextRun[l] = None
    /\ pending' = [pending EXCEPT ![e] = NextRun]
    /\ nextRun' = [nextRun EXCEPT ![l] = e]
    /\ seq' = seq + 1
    /\ UNCHANGED <<placed, parent, abortedStop, billed, laneOp, leaf, lastOutcome, ops>>

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
          /\ UNCHANGED <<placed, parent, abortedStop, billed, laneOp, nextRun, leaf, lastOutcome>>

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
          /\ UNCHANGED <<placed, parent, abortedStop, billed, laneOp, nextRun, leaf, lastOutcome>>

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
          /\ UNCHANGED <<placed, parent, abortedStop, billed, laneOp, nextRun, leaf, lastOutcome>>

IdleWrite(l, e) ==
    /\ CanCommit
    /\ Idle(l)
    /\ Free(e)
    /\ Place(e, leaf[l])
    /\ leaf' = [leaf EXCEPT ![l] = e]
    /\ seq' = seq + 1
    /\ UNCHANGED <<abortedStop, billed, laneOp, nextRun, lastOutcome, ops>>

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
    /\ UNCHANGED <<placed, parent, abortedStop, billed, laneOp, leaf, lastOutcome>>

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
          /\ UNCHANGED <<abortedStop, billed, laneOp, nextRun, lastOutcome>>

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
          /\ UNCHANGED <<abortedStop, billed, laneOp, nextRun, lastOutcome>>

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
          /\ UNCHANGED <<abortedStop, billed, laneOp, nextRun, lastOutcome>>

-----------------------------------------------------------------------------
\* Effect sandwich: intent commit, then process-local dispatch, then settle.
\* Crash clears armed/running; recovery then follows the stored phase.

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
                 !.replay = "n/a",
                 !.skipOnce = FALSE]]
          /\ seq' = seq + 1
          /\ UNCHANGED <<placed, pending, parent, abortedStop, billed,
                         laneOp, nextRun, leaf, lastOutcome>>

DispatchGen(l) ==
    /\ Open(l)
    /\ LET o == OpOf(l)
       IN /\ ops[o].present
          /\ ops[o].phase = "gen_pending"
          /\ ops[o].armed
          /\ ~ops[o].running
          /\ ops[o].runs < 2
          /\ ops' = [ops EXCEPT ![o] = [ops[o] EXCEPT
                 !.armed = FALSE, !.running = TRUE, !.runs = ops[o].runs + 1]]
          /\ UNCHANGED <<seq, placed, pending, parent, abortedStop, billed,
                         laneOp, nextRun, leaf, lastOutcome>>

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
          /\ UNCHANGED <<laneOp, nextRun, lastOutcome>>

\* Settlement that plans a tool: same response+usage, then tool_pending
\* with a reserved result id and a replay declaration.
SettleGenToTool(l, result, replay) ==
    /\ CanCommit
    /\ Open(l)
    /\ LET o == OpOf(l)
           e == ops[o].reserved
       IN /\ ops[o].present
          /\ ops[o].phase = "gen_pending"
          /\ ops[o].running
          /\ ops[o].control = "running"
          /\ e \in EntryIds
          /\ result \in EntryIds
          /\ result # e
          /\ Free(result)
          /\ ~placed[e]
          /\ replay \in {"safe", "never"}
          /\ Place(e, leaf[l])
          /\ billed' = [billed EXCEPT ![e] = TRUE]
          /\ abortedStop' = [abortedStop EXCEPT ![e] = FALSE]
          /\ leaf' = [leaf EXCEPT ![l] = e]
          /\ ops' = [ops EXCEPT ![o] = [ops[o] EXCEPT
                 !.phase = "tool_pending",
                 !.reserved = result,
                 !.replay = replay,
                 !.latest = e,
                 !.armed = TRUE,
                 !.running = FALSE,
                 !.runs = 0,
                 !.interrupted = FALSE,
                 !.attempt = 0]]
          /\ seq' = seq + 1
          /\ UNCHANGED <<laneOp, nextRun, lastOutcome>>

DispatchTool(l) ==
    /\ Open(l)
    /\ LET o == OpOf(l)
       IN /\ ops[o].present
          /\ ops[o].phase = "tool_pending"
          /\ ops[o].armed
          /\ ~ops[o].running
          /\ ops[o].runs < 2
          /\ ops' = [ops EXCEPT ![o] = [ops[o] EXCEPT
                 !.armed = FALSE, !.running = TRUE, !.runs = ops[o].runs + 1]]
          /\ UNCHANGED <<seq, placed, pending, parent, abortedStop, billed,
                         laneOp, nextRun, leaf, lastOutcome>>

SettleTool(l) ==
    /\ CanCommit
    /\ Open(l)
    /\ LET o == OpOf(l)
           e == ops[o].reserved
       IN /\ ops[o].present
          /\ ops[o].phase = "tool_pending"
          /\ ops[o].running
          /\ e \in EntryIds
          /\ ~placed[e]
          /\ Place(e, leaf[l])
          /\ billed' = [billed EXCEPT ![e] = FALSE]
          /\ abortedStop' = [abortedStop EXCEPT ![e] = FALSE]
          /\ leaf' = [leaf EXCEPT ![l] = e]
          /\ ops' = [ops EXCEPT ![o] = [ops[o] EXCEPT
                 !.phase = "checkpoint",
                 !.continuation = "need_assistant",
                 !.reserved = None,
                 !.replay = "n/a",
                 !.armed = FALSE,
                 !.running = FALSE]]
          /\ seq' = seq + 1
          /\ UNCHANGED <<laneOp, nextRun, lastOutcome>>

\* Recovery of an unknown effect: intent durable, this process did not start it.
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
          /\ \/ /\ ops[o].control = "cancel"
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
                /\ UNCHANGED <<laneOp, nextRun, lastOutcome>>
             \/ /\ ops[o].control = "running"
                /\ ops[o].attempt < MaxAttempts
                /\ ops' = [ops EXCEPT ![o] = [ops[o] EXCEPT
                       !.attempt = ops[o].attempt + 1,
                       !.armed = TRUE]]
                /\ UNCHANGED <<seq, placed, pending, parent, abortedStop, billed,
                               laneOp, nextRun, leaf, lastOutcome>>
             \/ /\ ops[o].control = "running"
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
                /\ UNCHANGED <<laneOp, nextRun, lastOutcome>>

RecoverTool(l) ==
    /\ CanCommit
    /\ Open(l)
    /\ LET o == OpOf(l)
           e == ops[o].reserved
       IN /\ ops[o].present
          /\ ops[o].phase = "tool_pending"
          /\ ~ops[o].armed
          /\ ~ops[o].running
          /\ e \in EntryIds
          /\ ~placed[e]
          /\ \/ /\ ops[o].replay = "safe"
                /\ ops[o].control = "running"
                /\ ops' = [ops EXCEPT ![o] = [ops[o] EXCEPT !.armed = TRUE]]
                /\ UNCHANGED <<seq, placed, pending, parent, abortedStop, billed,
                               laneOp, nextRun, leaf, lastOutcome>>
             \/ /\ \/ ops[o].replay = "never"
                   \/ ops[o].control = "cancel"
                /\ Place(e, leaf[l])
                /\ billed' = [billed EXCEPT ![e] = FALSE]
                /\ abortedStop' = [abortedStop EXCEPT ![e] = FALSE]
                /\ leaf' = [leaf EXCEPT ![l] = e]
                /\ ops' = [ops EXCEPT ![o] = [ops[o] EXCEPT
                       !.phase = "checkpoint",
                       !.continuation = "need_assistant",
                       !.reserved = None,
                       !.replay = "n/a"]]
                /\ seq' = seq + 1
                /\ UNCHANGED <<laneOp, nextRun, lastOutcome>>

-----------------------------------------------------------------------------
\* Abort is control, not a phase. First abort drains steer/follow-up into
\* drained* and does not delete their pending.entry registers. Writes stay.

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
                 !.armed = FALSE]]
          /\ seq' = seq + 1
          /\ UNCHANGED <<placed, pending, parent, abortedStop, billed,
                         laneOp, nextRun, leaf, lastOutcome>>

\* Terminal: delete every operation-owned register, write lastResult, clear
\* the claim. Never deletes lane-owned pendingNextRun.
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
          /\ UNCHANGED <<placed, parent, abortedStop, billed, nextRun, leaf>>

\* Process death. Durable maps are untouched. Live effects and the "armed
\* to dispatch" bit die with the process.
Crash ==
    /\ ops' = [o \in OpIds |->
           [ops[o] EXCEPT
               !.armed = FALSE,
               !.running = FALSE,
               !.interrupted = @ \/ (ops[o].present /\ ops[o].phase # "checkpoint")]]
    /\ UNCHANGED <<seq, placed, pending, parent, abortedStop, billed,
                   laneOp, nextRun, leaf, lastOutcome>>

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
    \/ \E l \in Lanes, r \in EntryIds, p \in {"safe", "never"}: SettleGenToTool(l, r, p)
    \/ \E l \in Lanes: DispatchTool(l)
    \/ \E l \in Lanes: SettleTool(l)
    \/ \E l \in Lanes: RecoverGen(l)
    \/ \E l \in Lanes: RecoverTool(l)
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

\* §9.1.15 / §3.11: a queued id has its register, its entry, or neither.
InvExclusivity ==
    \A e \in EntryIds: ~(placed[e] /\ pending[e] # None)

\* §9.1.1 write-once: placed entries stay placed. Checked by no unplace action
\* plus reserved ids are not already placed when settled.
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
\* being equivalent to the lane claim. Inbox ids of a closed op are gone.
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

\* nextRun is lane-owned: terminal does not drop it, and its pending kind
\* stays nextRun until captured or cancelled.
InvNextRunLaneOwned ==
    \A l \in Lanes:
        nextRun[l] \in EntryIds =>
            /\ pending[nextRun[l]] = NextRun
            /\ ~placed[nextRun[l]]

\* Inbox ids that are not drained still have their pending register.
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

\* Intent before effect: a live effect requires a pending phase.
InvNoEffectWithoutIntent ==
    \A o \in OpIds:
        ops[o].running =>
            /\ ops[o].present
            /\ ops[o].phase \in {"gen_pending", "tool_pending"}
            /\ ops[o].reserved \in EntryIds

\* §4.5: a tool whose intent survived a crash is re-executed only if replay
\* is safe. A never tool that was interrupted is never dispatched again, not
\* even once.
InvInterruptedNeverToolNotDispatched ==
    \A o \in OpIds:
        ops[o].running /\ ops[o].replay = "never" => ~ops[o].interrupted

\* An unsafe tool is dispatched at most once. Crash clears armed so the
\* only remaining path is synthetic settlement, which does not increment runs.
InvUnsafeToolAtMostOnce ==
    \A o \in OpIds:
        ops[o].replay = "never" => ops[o].runs <= 1

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

\* At most one terminal per open operation: Terminal requires present and
\* clears it, so two terminals cannot fire without an Accept in between.
InvPresentIffClaimed ==
    \A l \in Lanes:
        Idle(l) <=> \A o \in OpIds: ~(ops[o].present /\ laneOp[l] = o)

====
