//! The model's invariants, over the real registers, after every commit.
//!
//! `docs/tla/DurableHarness.tla` proves that the *design* cannot reach these
//! states. This module is the bridge to the implementation: the same
//! predicates, re-expressed over `Storage`, evaluated by the session owner
//! after each successful commit. A violation faults the session
//! (`docs/harness.md` §1.4: a failed admitted commit faults the harness), so a
//! transition that would have written a state the model forbids stops the
//! process instead of being resumed from later.
//!
//! Every check is bounded: register listings plus point lookups of exactly the
//! ids those registers name. Nothing here folds history or scans the tree —
//! the same discipline as `session::restore`, which this reuses per lane.
//!
//! Each function names the `Inv*` it mirrors. Add one here whenever one is
//! added to the spec; `docs/tla/README.md` keeps the table.

use std::collections::{BTreeMap, BTreeSet};

use crate::entry::Namespace;
use crate::ids::{EntryId, OpId};
use crate::state::{
    Control, Intent, LaneState, Operation, OperationState, PendingEntry, RunPhase, ToolBatch,
    ToolCallState,
};
use crate::storage::Storage;

/// One violated invariant, named after its spec counterpart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub invariant: &'static str,
    pub detail: String,
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.invariant, self.detail)
    }
}

/// Evaluate every invariant. Empty means the session is in a state the model
/// allows.
pub fn check(s: &Storage) -> Vec<Violation> {
    let mut out = Vec::new();
    let lanes = lane_states(s);
    let ops = op_states(s);
    exclusivity(s, &mut out);
    lane_ownership(s, &lanes, &ops, &mut out);
    closed_op_owns_nothing(s, &ops, &mut out);
    pending_is_owned(s, &lanes, &ops, &mut out);
    tool_batches(s, &ops, &mut out);
    per_lane(s, &lanes, &mut out);
    out
}

fn push(out: &mut Vec<Violation>, invariant: &'static str, detail: impl Into<String>) {
    out.push(Violation {
        invariant,
        detail: detail.into(),
    });
}

fn lane_states(s: &Storage) -> BTreeMap<String, LaneState> {
    s.list_registers(Namespace::LaneState, "")
        .into_iter()
        .filter_map(|r| {
            serde_json::from_value::<LaneState>(r.value.clone())
                .ok()
                .map(|v| (r.key.clone(), v))
        })
        .collect()
}

fn op_states(s: &Storage) -> BTreeMap<String, OperationState> {
    s.list_registers(Namespace::OpState, "")
        .into_iter()
        .filter_map(|r| {
            serde_json::from_value::<OperationState>(r.value.clone())
                .ok()
                .map(|v| (r.key.clone(), v))
        })
        .collect()
}

/// `InvExclusivity` (§9.1.15, §3.11): a queued id has its register, its
/// entry, or neither — never both.
fn exclusivity(s: &Storage, out: &mut Vec<Violation>) {
    for r in s.list_registers(Namespace::PendingEntry, "") {
        if s.entry(&EntryId::from(r.key.as_str())).is_some() {
            push(
                out,
                "InvExclusivity",
                format!(
                    "{} is both a pending.entry register and a placed entry",
                    r.key
                ),
            );
        }
    }
}

/// `InvLaneOwnership` (§9.1.12, §9.1.17): an operation is open iff exactly
/// one lane's claim names it, and its meta/state agree with that lane.
fn lane_ownership(
    s: &Storage,
    lanes: &BTreeMap<String, LaneState>,
    ops: &BTreeMap<String, OperationState>,
    out: &mut Vec<Violation>,
) {
    let mut claimed: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for (lane, state) in lanes {
        if let Some(op) = &state.current_operation_id {
            claimed
                .entry(op.as_str().to_string())
                .or_default()
                .push(lane.as_str());
        }
    }
    for (op, lanes_naming) in &claimed {
        if lanes_naming.len() > 1 {
            push(
                out,
                "InvLaneOwnership",
                format!("operation {op} is claimed by lanes {lanes_naming:?}"),
            );
        }
        let Some(state) = ops.get(op) else {
            push(
                out,
                "InvLaneOwnership",
                format!("claimed op.state/{op} missing"),
            );
            continue;
        };
        match s.register_value::<Operation>(Namespace::OpMeta, op) {
            None => push(
                out,
                "InvLaneOwnership",
                format!("claimed op.meta/{op} missing"),
            ),
            Some((meta, _)) => {
                if Some(meta.lane.as_str()) != lanes_naming.first().copied() {
                    push(
                        out,
                        "InvLaneOwnership",
                        format!(
                            "op.meta/{op} names lane {} but is claimed by {lanes_naming:?}",
                            meta.lane
                        ),
                    );
                }
                if meta.intent.kind() != state.kind() {
                    push(
                        out,
                        "InvLaneOwnership",
                        format!("op.state/{op} kind does not match its intent"),
                    );
                }
            }
        }
    }
    for op in ops.keys() {
        if !claimed.contains_key(op) {
            push(
                out,
                "InvLaneOwnership",
                format!("op.state/{op} exists but no lane claims it"),
            );
        }
    }
    for r in s.list_registers(Namespace::OpMeta, "") {
        if !claimed.contains_key(&r.key) {
            push(
                out,
                "InvLaneOwnership",
                format!("op.meta/{} exists but no lane claims it", r.key),
            );
        }
    }
}

/// `InvClosedOpOwnsNothing` / `InvToolArgsLifecycle` (§9.1.13): every
/// `op.tool_args/{op}:…` and `op.preparation/{op}:…` register belongs to an
/// open operation.
fn closed_op_owns_nothing(
    s: &Storage,
    ops: &BTreeMap<String, OperationState>,
    out: &mut Vec<Violation>,
) {
    for (ns, name) in [
        (Namespace::OpToolArgs, "op.tool_args"),
        (Namespace::OpPreparation, "op.preparation"),
    ] {
        for r in s.list_registers(ns, "") {
            let owner = r.key.split(':').next().unwrap_or("");
            if !ops.contains_key(owner) {
                push(
                    out,
                    "InvClosedOpOwnsNothing",
                    format!("{name}/{} outlives its operation", r.key),
                );
            }
        }
    }
}

/// Every `pending.entry` register is referenced by something that will place
/// or cancel it: a lane's `pendingNextRun`, an open run's inbox or abort-drain
/// lists, or an open operation's prompt reservation. An unreferenced register
/// would linger forever (§1.3 lifetimes).
fn pending_is_owned(
    s: &Storage,
    lanes: &BTreeMap<String, LaneState>,
    ops: &BTreeMap<String, OperationState>,
    out: &mut Vec<Violation>,
) {
    let mut owned: BTreeSet<String> = BTreeSet::new();
    for state in lanes.values() {
        owned.extend(
            state
                .pending_next_run
                .iter()
                .map(|id| id.as_str().to_string()),
        );
    }
    for (op, state) in ops {
        if let Some((meta, _)) = s.register_value::<Operation>(Namespace::OpMeta, op)
            && let Intent::Run {
                prompt_entry_ids, ..
            } = &meta.intent
        {
            owned.extend(prompt_entry_ids.iter().map(|id| id.as_str().to_string()));
        }
        if let OperationState::Run(run) = state {
            owned.extend(
                run.inbox
                    .steer
                    .iter()
                    .chain(&run.inbox.follow_up)
                    .chain(&run.inbox.writes)
                    .map(|id| id.as_str().to_string()),
            );
            owned.extend(
                run.accepted_writes
                    .values()
                    .map(|id| id.as_str().to_string()),
            );
            if let Control::CancelRequested {
                drained_steer,
                drained_follow_up,
                ..
            } = &run.control
            {
                owned.extend(
                    drained_steer
                        .iter()
                        .chain(drained_follow_up)
                        .map(|id| id.as_str().to_string()),
                );
            }
        }
    }
    for r in s.list_registers(Namespace::PendingEntry, "") {
        if !owned.contains(&r.key) {
            push(
                out,
                "InvInboxHasRegister",
                format!(
                    "pending.entry/{} is referenced by no queue, drain list or reservation",
                    r.key
                ),
            );
        }
        if serde_json::from_value::<PendingEntry>(r.value.clone()).is_err() {
            push(
                out,
                "InvInboxHasRegister",
                format!("pending.entry/{} does not decode", r.key),
            );
        }
    }
}

/// `InvCallsSourceOrder`, `InvToolResultsReserved`, `InvToolArgsLifecycle`
/// (§3.8): within an open batch, clearance and result commits are in source
/// order; reserved result ids are fresh until completed and placed once
/// completed; `op.tool_args` exists exactly for cleared calls of the current
/// batch.
fn tool_batches(s: &Storage, ops: &BTreeMap<String, OperationState>, out: &mut Vec<Violation>) {
    let mut expected_args: BTreeSet<String> = BTreeSet::new();
    for (op, state) in ops {
        let OperationState::Run(run) = state else {
            continue;
        };
        let RunPhase::Tools { batch } = &run.phase else {
            continue;
        };
        let op_id = OpId::from(op.as_str());
        let mut calls: Vec<&ToolCallState> = batch.calls.iter().collect();
        calls.sort_by_key(|c| c.source_index());
        let mut seen_planned = false;
        let mut seen_incomplete = false;
        for call in calls {
            let i = call.source_index();
            let result = call.result_entry_id();
            let placed = s.entry(result).is_some();
            let pending = s
                .register(Namespace::PendingEntry, result.as_str())
                .is_some();
            match call {
                ToolCallState::Planned { .. } => {
                    seen_planned = true;
                    seen_incomplete = true;
                    if placed || pending {
                        push(
                            out,
                            "InvToolResultsReserved",
                            format!("{op} call {i}: planned result {result} already exists"),
                        );
                    }
                }
                ToolCallState::EffectPending { .. } => {
                    if seen_planned {
                        push(
                            out,
                            "InvCallsSourceOrder",
                            format!("{op} call {i} cleared before an earlier planned call"),
                        );
                    }
                    seen_incomplete = true;
                    if placed || pending {
                        push(
                            out,
                            "InvToolResultsReserved",
                            format!("{op} call {i}: pending result {result} already exists"),
                        );
                    }
                    let key = ToolBatch::args_key(&op_id, &batch.turn_id, i);
                    if s.register(Namespace::OpToolArgs, &key).is_none() {
                        push(
                            out,
                            "InvToolArgsLifecycle",
                            format!("{op} call {i} is effect_pending without op.tool_args/{key}"),
                        );
                    }
                    expected_args.insert(key);
                }
                ToolCallState::Completed { .. } => {
                    if seen_incomplete {
                        push(
                            out,
                            "InvCallsSourceOrder",
                            format!("{op} call {i} completed before an earlier call"),
                        );
                    }
                    if !placed {
                        push(
                            out,
                            "InvToolResultsReserved",
                            format!("{op} call {i}: completed result {result} is not placed"),
                        );
                    }
                    // A completed call may keep its args until batch completion.
                    expected_args.insert(ToolBatch::args_key(&op_id, &batch.turn_id, i));
                }
            }
        }
    }
    for r in s.list_registers(Namespace::OpToolArgs, "") {
        if !expected_args.contains(&r.key) {
            push(
                out,
                "InvToolArgsLifecycle",
                format!(
                    "op.tool_args/{} exists for no cleared call of an open batch",
                    r.key
                ),
            );
        }
    }
}

/// Everything `restore` validates per lane (§3.3), for every lane, including
/// `InvAbortedImpliesCancel` and `InvNextRunLaneOwned`.
fn per_lane(s: &Storage, lanes: &BTreeMap<String, LaneState>, out: &mut Vec<Violation>) {
    for lane in lanes.keys() {
        if let Err(e) = crate::session::restore(s, lane) {
            push(out, "InvRestoreValid", e.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{Entry, Transaction, Write};
    use serde_json::json;

    fn tx(writes: Vec<Write>) -> Transaction {
        Transaction { writes }
    }

    #[test]
    fn an_empty_session_and_an_idle_lane_are_clean() {
        let mut s = Storage::memory("s");
        assert!(check(&s).is_empty());
        s.commit(tx(vec![
            Write::set(Namespace::LaneLeaf, "main", Option::<EntryId>::None),
            Write::set(Namespace::LaneState, "main", LaneState::default()),
        ]))
        .unwrap();
        assert!(check(&s).is_empty(), "{:?}", check(&s));
    }

    #[test]
    fn a_placed_id_with_a_surviving_pending_register_is_caught() {
        let mut s = Storage::memory("s");
        let e = Entry::message(json!({"role": "user", "content": "x"}));
        s.commit(tx(vec![
            Write::entry(e.clone()),
            Write::set(
                Namespace::PendingEntry,
                e.id.as_str(),
                PendingEntry::message(json!({"role": "user", "content": "x"})),
            ),
        ]))
        .unwrap();
        let v = check(&s);
        assert!(v.iter().any(|v| v.invariant == "InvExclusivity"), "{v:?}");
    }

    #[test]
    fn an_orphan_pending_register_and_leaked_tool_args_are_caught() {
        let mut s = Storage::memory("s");
        s.commit(tx(vec![
            Write::set(
                Namespace::PendingEntry,
                "e_orphan",
                PendingEntry::message(json!({"role": "user", "content": "x"})),
            ),
            Write::set(Namespace::OpToolArgs, "op_gone:s1:0", json!({})),
        ]))
        .unwrap();
        let v = check(&s);
        let names: Vec<_> = v.iter().map(|v| v.invariant).collect();
        assert!(names.contains(&"InvInboxHasRegister"), "{v:?}");
        assert!(names.contains(&"InvClosedOpOwnsNothing"), "{v:?}");
        assert!(names.contains(&"InvToolArgsLifecycle"), "{v:?}");
    }

    #[test]
    fn an_unclaimed_operation_state_is_caught() {
        let mut s = Storage::memory("s");
        s.commit(tx(vec![
            Write::set(Namespace::LaneLeaf, "main", Option::<EntryId>::None),
            Write::set(Namespace::LaneState, "main", LaneState::default()),
            Write::set(
                Namespace::OpState,
                "op_x",
                json!({"kind": "navigation", "control": {"status": "running"}, "targetId": null}),
            ),
        ]))
        .unwrap();
        let v = check(&s);
        assert!(v.iter().any(|v| v.invariant == "InvLaneOwnership"), "{v:?}");
    }
}
