//! The chat log: committed entries, durable queued entries, and in-memory drafts.
//! Only drafts are kept here. Durable data stays with the Session owner.
use crate::{
    entry::Entry,
    events::{Event, Kind},
    ids::EntryId,
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use tokio::sync::broadcast;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Committed,
    Accepted,
    Streaming,
    Interrupted,
    Cancelled,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub entry: Entry,
    pub status: Status,
    /// Storage sequence for committed/accepted records, intent sequence for drafts.
    pub order: u64,
    pub revision: u64,
}
impl Record {
    pub fn committed(entry: Entry) -> Self {
        Self {
            order: entry.seq,
            entry,
            status: Status::Committed,
            revision: 0,
        }
    }
}

/// Synchronously update the draft log before publishing an event. A subscriber
/// cannot observe a draft that a snapshot of this process cannot also see.
pub struct Bus {
    sender: broadcast::Sender<Event>,
    drafts: Mutex<BTreeMap<String, BTreeMap<EntryId, Record>>>,
    version: std::sync::atomic::AtomicU64,
}
impl Bus {
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity.max(16));
        Self {
            sender,
            drafts: Mutex::new(BTreeMap::new()),
            version: std::sync::atomic::AtomicU64::new(0),
        }
    }
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.sender.subscribe()
    }
    pub fn send(&self, mut event: Event) -> usize {
        {
            let mut lanes = self.drafts.lock();
            let drafts = lanes.entry(event.lane.clone()).or_default();
            match &mut event.kind {
                Kind::EntryDraft {
                    entry,
                    order,
                    version,
                } => {
                    *version = self
                        .version
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        + 1;
                    drafts.insert(
                        entry.id.clone(),
                        Record {
                            entry: entry.clone(),
                            order: *order,
                            status: Status::Streaming,
                            revision: *version,
                        },
                    );
                }
                Kind::EntryAdded { entry } => {
                    drafts.remove(&entry.id);
                }
                Kind::RunEnd { .. } | Kind::Fault { .. } => {
                    for draft in drafts.values_mut() {
                        if draft
                            .entry
                            .payload
                            .get("display")
                            .and_then(|d| d.get("run_id"))
                            .and_then(|v| v.as_str())
                            == event.run_id.as_deref()
                        {
                            draft.status = Status::Interrupted;
                        }
                    }
                }
                _ => {}
            }
        }
        self.sender.send(event).unwrap_or(0)
    }
    pub fn drafts(&self, lane: &str) -> Vec<Record> {
        self.drafts
            .lock()
            .get(lane)
            .map(|d| d.values().cloned().collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn settlement_replaces_the_draft_by_reserved_entry_id() {
        let bus = Bus::new(16);
        let draft = Entry::message(
            serde_json::json!({"role":"assistant","content":[{"type":"text","text":"hello"}]}),
        )
        .display("run", "chat");
        let _ = bus.send(Event::new(
            "main",
            Some("run"),
            Kind::EntryDraft {
                entry: draft.clone(),
                order: 5,
                version: 0,
            },
        ));
        assert_eq!(bus.drafts("main")[0].entry.id, draft.id);
        let mut settled = draft;
        settled.seq = 10;
        let _ = bus.send(Event::new(
            "main",
            Some("run"),
            Kind::EntryAdded { entry: settled },
        ));
        assert!(bus.drafts("main").is_empty());
    }
}
