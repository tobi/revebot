//! One log across streaming, durable acceptance, settlement and reopen.
use reve::{
    entry::{MAIN_LANE, Namespace},
    events::Kind,
    harness::{Harness, HarnessConfig},
    hooks::Hooks,
    log::Status,
    model::{Assistant, BoxFuture, Deltas, Model, Request, ToolSchema},
    sandbox::tokio_util_lite::CancelRx,
    session::Session,
    state::{LaneConfiguration, ModelRef, PendingEntry, Replay, RetryPolicy, RunSettings},
    storage::Storage,
    tools::Tools,
};
use serde_json::{Map, Value, json};
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::Notify;

struct Notices {
    harness: Mutex<Weak<Harness>>,
    calls: AtomicUsize,
}
impl Tools for Notices {
    fn replay(&self, _: &str) -> Option<Replay> {
        Some(Replay::Never)
    }
    fn schemas(&self) -> Vec<ToolSchema> {
        vec![ToolSchema {
            name: "SendUserMessage".into(),
            description: "notice".into(),
            schema: json!({"type":"object"}),
        }]
    }
    fn invoke<'a>(
        &'a self,
        _: &'a str,
        _: Map<String, Value>,
        _: Option<CancelRx>,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let harness = self.harness.lock().unwrap().upgrade().unwrap();
            let id = harness
                .write_once(
                    MAIN_LANE,
                    "notice-key",
                    PendingEntry::custom("user_notice", json!({"text":"Delivered **message**"})),
                )
                .await
                .map_err(|e| e.to_string())?;
            Ok(format!("Message accepted: {id}"))
        })
    }
}
struct NoticeModel(AtomicUsize);
impl Model for NoticeModel {
    fn respond<'a>(
        &'a self,
        _: Request<'a>,
        delta: Deltas<'a>,
    ) -> BoxFuture<'a, reve::model::Result<Assistant>> {
        Box::pin(async move {
            if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                delta("Opening **markdown**");
                let mut a = Assistant::calls(vec![
                    ("SendUserMessage".into(), json!({})),
                    ("SendUserMessage".into(), json!({})),
                ]);
                a.text = "Opening **markdown**".into();
                Ok(a)
            } else {
                delta("internal prose");
                Ok(Assistant::text("internal prose"))
            }
        })
    }
}
fn setup(session: Session, model: Arc<dyn Model>) -> (Arc<Harness>, Arc<Notices>) {
    let tools = Arc::new(Notices {
        harness: Mutex::new(Weak::new()),
        calls: AtomicUsize::new(0),
    });
    let harness = Harness::new(
        session,
        HarnessConfig {
            model,
            tools: tools.clone(),
            hooks: Hooks::new(),
            system_prompt: Arc::new(String::new),
            settings: RunSettings::default(),
            retry: RetryPolicy::default(),
            configuration: LaneConfiguration {
                model: ModelRef {
                    provider: "test".into(),
                    model_id: "test".into(),
                },
                thinking_level: "off".into(),
                active_tool_names: vec!["SendUserMessage".into()],
            },
            event_capacity: 512,
        },
    );
    *tools.harness.lock().unwrap() = Arc::downgrade(&harness);
    (harness, tools)
}

#[tokio::test]
async fn notices_are_durable_once_and_known_tool_results_survive_state_changes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let session = Session::spawn(Storage::open(&path, "log", None).unwrap());
    let (harness, tools) = setup(session.clone(), Arc::new(NoticeModel(AtomicUsize::new(0))));
    let mut events = harness.subscribe();
    harness.prompt(MAIN_LANE, "hello").await.unwrap();
    let log = harness.log_snapshot(MAIN_LANE).await.unwrap();
    assert!(log.iter().all(|r| r.status == Status::Committed));
    assert_eq!(
        log.iter()
            .filter(|r| r.entry.custom_type.as_deref() == Some("user_notice"))
            .count(),
        1
    );
    assert_eq!(tools.calls.load(Ordering::SeqCst), 2);
    for row in &log {
        if row.entry.role() == Some("toolResult") {
            let message = row.entry.message_value().unwrap();
            assert_eq!(message["isError"], false);
            assert!(
                message.get("synthetic").is_none(),
                "known accepted write must not become interrupted"
            );
        }
    }
    let assistant: Vec<_> = log
        .iter()
        .filter(|r| r.entry.role() == Some("assistant"))
        .collect();
    assert_eq!(assistant[0].entry.payload["display"]["audience"], "chat");
    assert_eq!(
        assistant[1].entry.payload["display"]["audience"],
        "internal"
    );
    while let Ok(event) = events.try_recv() {
        if let Kind::EntryAdded { entry } = event.kind {
            assert!(entry.seq > 0);
            assert_eq!(Some(entry.clone()), session.entry(entry.id).await.unwrap());
        }
    }
    assert!(
        session.read(|s| s.register_count()).await.unwrap() < 10,
        "operation dedupe map must die at terminal"
    );
    session.close().await;
    let reopened = Session::spawn(Storage::open(&path, "log", None).unwrap());
    let restored = reopened.log_records(MAIN_LANE).await.unwrap();
    assert_eq!(
        serde_json::to_value(log).unwrap(),
        serde_json::to_value(restored).unwrap()
    );
    reopened.close().await;
}

struct StreamingGate {
    started: Arc<Notify>,
    release: Arc<Notify>,
    calls: AtomicUsize,
}
impl Model for StreamingGate {
    fn respond<'a>(
        &'a self,
        _: Request<'a>,
        delta: Deltas<'a>,
    ) -> BoxFuture<'a, reve::model::Result<Assistant>> {
        Box::pin(async move {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                delta("**Partial**");
                self.started.notify_one();
                self.release.notified().await;
                Ok(Assistant::text("**Partial** complete"))
            } else {
                Ok(Assistant::text("Answer to steer"))
            }
        })
    }
}
#[tokio::test]
async fn streaming_entry_keeps_its_id_and_visibility_when_a_steer_races_settlement() {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let model = Arc::new(StreamingGate {
        started: started.clone(),
        release: release.clone(),
        calls: AtomicUsize::new(0),
    });
    let session = Session::spawn(Storage::memory("stream"));
    let (harness, _) = setup(session.clone(), model.clone());
    let running = {
        let h = harness.clone();
        tokio::spawn(async move { h.prompt(MAIN_LANE, "start").await.unwrap() })
    };
    started.notified().await;
    let snapshot = harness.log_snapshot(MAIN_LANE).await.unwrap();
    let draft = snapshot
        .iter()
        .find(|r| r.status == Status::Streaming)
        .unwrap();
    let id = draft.entry.id.clone();
    assert_eq!(draft.entry.payload["display"]["audience"], "chat");
    harness.steer(MAIN_LANE, "steer").await.unwrap();
    release.notify_one();
    running.await.unwrap();
    assert_eq!(
        model.calls.load(Ordering::SeqCst),
        2,
        "a CAS miss must not repeat a completed provider effect"
    );
    let committed = session.entry(id).await.unwrap().unwrap();
    assert_eq!(committed.payload["display"]["audience"], "chat");
    assert!(
        committed
            .message_value()
            .unwrap()
            .to_string()
            .contains("complete")
    );
    session.close().await;
}

#[tokio::test]
async fn the_user_log_keeps_history_past_model_compaction() {
    use reve::entry::{Entry, Transaction, Write};
    let session = Session::spawn(Storage::memory("history"));
    let old = Entry::message(json!({"role":"user","content":"OLD_HISTORY"}));
    let compact = Entry::compaction("summary", vec![], 10, false).with_parent(Some(old.id.clone()));
    let new = Entry::message(json!({"role":"user","content":"new"}))
        .with_parent(Some(compact.id.clone()));
    session
        .commit(
            Transaction::new()
                .with(Write::entry(old))
                .with(Write::entry(compact))
                .with(Write::entry(new.clone()))
                .with(Write::set(Namespace::LaneLeaf, MAIN_LANE, Some(new.id))),
        )
        .await
        .unwrap();
    assert_eq!(session.log_records(MAIN_LANE).await.unwrap().len(), 3);
    assert_eq!(session.transcript(MAIN_LANE).await.unwrap().len(), 2);
    session.close().await;
}

#[tokio::test]
async fn failed_acceptance_does_not_publish_a_notice() {
    let session = Session::spawn(Storage::memory("failure"));
    let (harness, _) = setup(session.clone(), Arc::new(NoticeModel(AtomicUsize::new(0))));
    harness.begin_run(MAIN_LANE, "start").await.unwrap();
    let mut events = harness.subscribe();
    session.close().await;
    assert!(
        harness
            .write_once(
                MAIN_LANE,
                "key",
                PendingEntry::custom("user_notice", json!({"text":"not sent"}))
            )
            .await
            .is_err()
    );
    assert!(events.try_recv().is_err());
}
