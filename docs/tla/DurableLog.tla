---- MODULE DurableLog ----
\* Reve JSONL session log — docs/harness.md §1.4, §1.7.
\*
\* The file is the replay recipe, not the state. One physical line per
\* transaction. A torn final line is discarded whole, so a crash can never
\* expose a prefix of a transaction. Compaction rewrites surviving writes
\* in seq order and does not change logical state.
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS
    Ids,     \* shared write-once namespace for entries and usage rows
    Keys,    \* register keys
    MaxTx    \* bound on committed transactions

None == 0
EntryKind == "entry"
UsageKind == "usage"
SetKind == "set"
DelKind == "delete"
Kinds == {EntryKind, UsageKind, SetKind, DelKind}

\* Uniform write record [kind, target, seq]: entry/usage target an Id,
\* set/delete target a Key. Typed by IsWrite below (Nat is not enumerable).

VARIABLES
    log,       \* Seq of txs; each tx is a non-empty Seq of Write
    inflight,  \* Seq of Write, empty if nothing is being written
    seq,       \* last assigned seq
    committed, \* number of flushed transactions (model bound)
    entries,   \* [Ids -> BOOLEAN]
    usage,     \* [Ids -> BOOLEAN]
    regs       \* [Keys -> BOOLEAN]  TRUE = key present

vars == <<log, inflight, seq, committed, entries, usage, regs>>

EmptyMaps ==
    [entries |-> [i \in Ids |-> FALSE],
     usage   |-> [i \in Ids |-> FALSE],
     regs    |-> [k \in Keys |-> FALSE]]

Maps ==
    [entries |-> entries, usage |-> usage, regs |-> regs]

-----------------------------------------------------------------------------
\* Applying writes. A transaction is applied all-or-none at Flush.

ApplyWrite(st, w) ==
    CASE w.kind = EntryKind ->
            [st EXCEPT !.entries = [@ EXCEPT ![w.target] = TRUE]]
      [] w.kind = UsageKind ->
            [st EXCEPT !.usage = [@ EXCEPT ![w.target] = TRUE]]
      [] w.kind = SetKind ->
            [st EXCEPT !.regs = [@ EXCEPT ![w.target] = TRUE]]
      [] w.kind = DelKind ->
            [st EXCEPT !.regs = [@ EXCEPT ![w.target] = FALSE]]

ApplyTx(st, tx) ==
    IF Len(tx) = 0 THEN st
    ELSE IF Len(tx) = 1 THEN ApplyWrite(st, tx[1])
    ELSE ApplyWrite(ApplyWrite(st, tx[1]), tx[2])

RECURSIVE ReplayLeft(_, _)
ReplayLeft(st, txs) ==
    IF txs = <<>> THEN st
    ELSE ReplayLeft(ApplyTx(st, Head(txs)), Tail(txs))

-----------------------------------------------------------------------------
\* Validation: write-once ids, shared entry/usage namespace, well-typed targets.
\* In-transaction visibility: an entry may be named later in the same tx,
\* but a second insert of the same id is corruption.

Occupied(st, id) == st.entries[id] \/ st.usage[id]

ValidWrite(st, w) ==
    CASE w.kind = EntryKind ->
            w.target \in Ids /\ ~Occupied(st, w.target)
      [] w.kind = UsageKind ->
            w.target \in Ids /\ ~Occupied(st, w.target)
      [] w.kind = SetKind ->
            w.target \in Keys
      [] w.kind = DelKind ->
            w.target \in Keys

ValidTx(st, tx) ==
    /\ tx # <<>>
    /\ Len(tx) \in {1, 2}
    /\ ValidWrite(st, tx[1])
    /\ (Len(tx) = 1 \/ ValidWrite(ApplyWrite(st, tx[1]), tx[2]))

Mk(kind, target) == [kind |-> kind, target |-> target, seq |-> None]

Stamp(w, n) == [w EXCEPT !.seq = n]

StampTx(tx, start) ==
    IF Len(tx) = 1
    THEN <<Stamp(tx[1], start)>>
    ELSE <<Stamp(tx[1], start), Stamp(tx[2], start + 1)>>

-----------------------------------------------------------------------------

Init ==
    /\ log = <<>>
    /\ inflight = <<>>
    /\ seq = 0
    /\ committed = 0
    /\ entries = [i \in Ids |-> FALSE]
    /\ usage = [i \in Ids |-> FALSE]
    /\ regs = [k \in Keys |-> FALSE]

\* Begin a 1- or 2-write transaction. Nothing is durable yet.
Propose ==
    /\ inflight = <<>>
    /\ committed < MaxTx
    /\ \E k1 \in Kinds, t1 \in (Ids \cup Keys):
         LET w1 == Mk(k1, t1)
         IN \/ /\ ValidTx(Maps, <<w1>>)
               /\ inflight' = <<w1>>
               /\ UNCHANGED <<log, seq, committed, entries, usage, regs>>
            \/ \E k2 \in Kinds, t2 \in (Ids \cup Keys):
                 LET w2 == Mk(k2, t2)
                     tx == <<w1, w2>>
                 IN /\ ValidTx(Maps, tx)
                    /\ inflight' = tx
                    /\ UNCHANGED <<log, seq, committed, entries, usage, regs>>

\* Crash during the write: the line is torn and discarded whole.
\* Live maps are unchanged — durable-first, then visible.
Tear ==
    /\ inflight # <<>>
    /\ inflight' = <<>>
    /\ UNCHANGED <<log, seq, committed, entries, usage, regs>>

\* Flush assigns seqs, appends one line, then applies every write.
Flush ==
    /\ inflight # <<>>
    /\ LET stamped == StampTx(inflight, seq + 1)
           st == ApplyTx(Maps, stamped)
       IN /\ log' = Append(log, stamped)
          /\ inflight' = <<>>
          /\ seq' = seq + Len(inflight)
          /\ committed' = committed + 1
          /\ entries' = st.entries
          /\ usage' = st.usage
          /\ regs' = st.regs

\* Snapshot compaction: drop dead register writes, keep entries/usage,
\* rewrite in seq order. Logical maps do not change.
IsLiveRegWrite(w, later) ==
    /\ w.kind = SetKind
    /\ regs[w.target]
    /\ ~\E i \in 1..Len(later):
          /\ later[i].target = w.target
          /\ later[i].kind \in {SetKind, DelKind}

KeepWrite(w, later) ==
    \/ w.kind \in {EntryKind, UsageKind}
    \/ IsLiveRegWrite(w, later)

\* Flatten log to a seq of writes, then filter. Bounded by MaxTx * 2.
RECURSIVE Flat(_)
Flat(txs) ==
    IF txs = <<>> THEN <<>>
    ELSE Head(txs) \o Flat(Tail(txs))

RECURSIVE FilterKeep(_, _)
FilterKeep(ws, acc) ==
    IF ws = <<>> THEN acc
    ELSE LET rest == Tail(ws)
             w == Head(ws)
         IN FilterKeep(rest,
                       IF KeepWrite(w, rest) THEN Append(acc, w) ELSE acc)

RECURSIVE ToLines(_, _)
ToLines(ws, acc) ==
    IF ws = <<>> THEN acc
    ELSE ToLines(Tail(ws), Append(acc, <<Head(ws)>>))

Compact ==
    /\ inflight = <<>>
    /\ log # <<>>
    /\ LET kept == FilterKeep(Flat(log), <<>>)
       IN /\ log' = ToLines(kept, <<>>)
          /\ UNCHANGED <<inflight, seq, committed, entries, usage, regs>>

Next ==
    \/ Propose
    \/ Tear
    \/ Flush
    \/ Compact

-----------------------------------------------------------------------------
\* Invariants. Names starting Inv / TypeOK are checked automatically.

IsWrite(w) ==
    /\ w.kind \in Kinds
    /\ w.target \in Ids \cup Keys
    /\ w.seq \in 0..MaxTx * 2

IsTx(tx) ==
    /\ Len(tx) \in 1..2
    /\ \A i \in 1..Len(tx): IsWrite(tx[i])

TypeOK ==
    /\ committed \in 0..MaxTx
    /\ seq \in 0..MaxTx * 2
    /\ entries \in [Ids -> BOOLEAN]
    /\ usage \in [Ids -> BOOLEAN]
    /\ regs \in [Keys -> BOOLEAN]
    /\ Len(inflight) \in 0..2
    /\ \A i \in 1..Len(inflight): IsWrite(inflight[i])
    /\ Len(log) <= MaxTx * 2
    /\ \A i \in 1..Len(log): IsTx(log[i])

\* Maps always equal replay of the durable prefix. An in-flight (torn or
\* not-yet-flushed) transaction is invisible.
InvReplayEqualsMaps ==
    ReplayLeft(EmptyMaps, log) = Maps

\* No crash prefix inside a transaction: if a write from inflight were in
\* the maps, some sibling write of the same tx could be missing. Durable
\* first means inflight never touches maps.
InvInflightInvisible ==
    inflight # <<>> => ReplayLeft(EmptyMaps, log) = Maps

\* Entry and usage ids share one write-once namespace.
InvWriteOnce ==
    \A i \in Ids: ~(entries[i] /\ usage[i])

\* seq is strictly increasing across the file. Gaps are legal (compaction).
RECURSIVE Seqs(_)
Seqs(txs) ==
    IF txs = <<>> THEN <<>>
    ELSE LET tx == Head(txs)
             s == IF Len(tx) = 1 THEN <<tx[1].seq>>
                  ELSE <<tx[1].seq, tx[2].seq>>
         IN s \o Seqs(Tail(txs))

InvSeqMono ==
    LET s == Seqs(log)
    IN \A i \in 1..(Len(s) - 1): s[i] < s[i + 1]

\* Compaction must not change what replay produces. Directly: maps, which
\* equal replay, are UNCHANGED by Compact; this restates that surviving
\* writes reconstruct the same maps.
InvCompactPreserves ==
    ReplayLeft(EmptyMaps, log) = Maps

====
