//! Passive events (`docs/harness.md` §5.5).
//!
//! One flat stream. Events observe execution and cannot change it; hooks
//! intercept. Durable-fact events fire **after** the commit — `EntryAdded`
//! means queryable. Events are not persisted and not replayed: a client takes
//! a snapshot, then subscribes.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::entry::{Entry, Usage, UsageRow};
use crate::ids::EntryId;
use crate::state::{CompactionReason, OperationError, Outcome};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Kind {
    RunStart,
    RunResume {
        recovery: bool,
    },
    RunAbort {
        steer: Vec<Value>,
        follow_up: Vec<Value>,
    },
    RunEnd {
        outcome: Outcome,
        leaf_id: Option<EntryId>,
        final_entry_id: Option<EntryId>,
        final_text: Option<String>,
        error: Option<OperationError>,
    },
    TurnStart {
        turn_id: String,
    },
    TurnEnd {
        turn_id: String,
    },
    RetryScheduled {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        error_message: String,
    },
    /// Streaming text for the assistant message in flight.
    MessageUpdate {
        delta: String,
    },
    /// The assistant message settled (pre-commit).
    MessageEnd {
        message: Value,
    },
    ToolStart {
        turn_id: String,
        tool_call_id: String,
        tool_name: String,
        args: Map<String, Value>,
    },
    ToolEnd {
        turn_id: String,
        tool_call_id: String,
        tool_name: String,
        content: String,
        is_error: bool,
        terminate: bool,
    },
    EntryAdded {
        entry: Entry,
    },
    /// A partial entry in this process's in-memory log, under its reserved id.
    EntryDraft {
        entry: Entry,
        order: u64,
        version: u64,
    },
    /// Content is durable in pending.entry but not placed in the tree yet.
    EntryAccepted {
        entry: Entry,
        order: u64,
    },
    WritePending {
        entry_id: EntryId,
    },
    QueueUpdate {
        steer: Vec<EntryId>,
        follow_up: Vec<EntryId>,
        next_run: Vec<EntryId>,
    },
    CompactionStart {
        reason: CompactionReason,
    },
    CompactionEnd {
        reason: CompactionReason,
        outcome: Outcome,
        entry_id: Option<EntryId>,
    },
    NavigationEnd {
        outcome: Outcome,
        old_leaf_id: Option<EntryId>,
        new_leaf_id: Option<EntryId>,
    },
    Usage {
        row: UsageRow,
        totals: Usage,
    },
    HandlerError {
        hook: String,
        error: String,
    },
    Fault {
        message: String,
    },
    LaneCreated {
        at: Option<EntryId>,
    },
    RosterChanged {
        ids: Vec<String>,
    },
    ResourcesChanged {
        bot: String,
        cwd: String,
        paths: Vec<String>,
        resources: Vec<String>,
        unknown: bool,
    },
    /// A bot started or finished a run. House-wide; the rail uses it so a
    /// working animation is not tied to which chat is open.
    BotBusy {
        bot_id: String,
        busy: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Event {
    pub lane: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(flatten)]
    pub kind: Kind,
}

impl Event {
    pub fn new(lane: &str, run_id: Option<&str>, kind: Kind) -> Self {
        Self {
            lane: lane.to_string(),
            run_id: run_id.map(str::to_string),
            kind,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_serialise_with_a_flat_type_tag() {
        let event = Event::new("main", Some("op"), Kind::RunStart);
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "run_start");
        assert_eq!(json["lane"], "main");
        assert_eq!(json["runId"].as_str(), None, "run_id keeps its field name");
        assert_eq!(json["run_id"], "op");
    }

    #[test]
    fn accepted_notices_use_the_same_entry_shape_as_the_log() {
        let entry = Entry::custom(
            "user_notice",
            Some(serde_json::json!({"bot":"rune","text":"What's your timezone?"})),
        );
        let event = Event::new("main", None, Kind::EntryAccepted { entry, order: 1 });
        let json = serde_json::to_value(event).unwrap();
        assert_eq!(json["entry"]["customType"], "user_notice");
        assert_eq!(json["entry"]["data"]["text"], "What's your timezone?");
    }

    #[test]
    fn a_bot_busy_event_names_the_bot() {
        let event = Event::new(
            "house",
            None,
            Kind::BotBusy {
                bot_id: "rune".into(),
                busy: true,
            },
        );
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "bot_busy");
        assert_eq!(json["bot_id"], "rune");
        assert_eq!(json["busy"], true);
    }
}
