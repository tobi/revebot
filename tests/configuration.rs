//! Dynamic house configuration is captured at an idle run boundary.
use parking_lot::RwLock;
use reve::{
    entry::MAIN_LANE,
    harness::{Harness, HarnessConfig},
    hooks::Hooks,
    model::{Assistant, BoxFuture, Deltas, Model, Request, ToolSchema},
    sandbox::tokio_util_lite::CancelRx,
    session::Session,
    state::{LaneConfiguration, ModelRef, Replay, RetryPolicy, RunSettings},
    storage::Storage,
    tools::Tools,
};
use serde_json::{Map, Value};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::Notify;

struct Counting {
    name: &'static str,
    calls: Arc<AtomicUsize>,
    gate_first: bool,
}
impl Model for Counting {
    fn respond<'a>(
        &'a self,
        _: Request<'a>,
        _: Deltas<'a>,
    ) -> BoxFuture<'a, reve::model::Result<Assistant>> {
        Box::pin(async move {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(if self.gate_first && call == 0 {
                Assistant::call("gate", serde_json::json!({}))
            } else {
                Assistant::text(self.name)
            })
        })
    }
}
struct Gate {
    started: Arc<Notify>,
    release: Arc<Notify>,
}
impl Tools for Gate {
    fn replay(&self, _: &str) -> Option<Replay> {
        Some(Replay::Never)
    }
    fn schemas(&self) -> Vec<ToolSchema> {
        vec![ToolSchema {
            name: "gate".into(),
            description: "wait".into(),
            schema: serde_json::json!({"type":"object"}),
        }]
    }
    fn invoke<'a>(
        &'a self,
        _: &'a str,
        _: Map<String, Value>,
        _: Option<CancelRx>,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            self.started.notify_one();
            self.release.notified().await;
            Ok("done".into())
        })
    }
}
fn config(name: &str) -> LaneConfiguration {
    LaneConfiguration {
        model: ModelRef {
            provider: name.into(),
            model_id: name.into(),
        },
        thinking_level: "off".into(),
        active_tool_names: vec!["gate".into()],
    }
}

#[tokio::test]
async fn profile_model_changes_affect_the_next_run_not_the_running_drive() {
    let old_calls = Arc::new(AtomicUsize::new(0));
    let new_calls = Arc::new(AtomicUsize::new(0));
    let old: Arc<dyn Model> = Arc::new(Counting {
        name: "old",
        calls: old_calls.clone(),
        gate_first: true,
    });
    let new: Arc<dyn Model> = Arc::new(Counting {
        name: "new",
        calls: new_calls.clone(),
        gate_first: false,
    });
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let session = Session::spawn(Storage::memory("configuration"));
    let harness = Harness::new(
        session.clone(),
        HarnessConfig {
            model: old.clone(),
            tools: Arc::new(Gate {
                started: started.clone(),
                release: release.clone(),
            }),
            hooks: Hooks::new(),
            system_prompt: Arc::new(String::new),
            settings: RunSettings::default(),
            retry: RetryPolicy::default(),
            configuration: config("old"),
            event_capacity: 128,
        },
    );
    let desired = Arc::new(RwLock::new(config("old")));
    let source = desired.clone();
    harness.set_environment_sources(
        Arc::new(move || Ok(source.read().clone())),
        Arc::new(move |c| {
            if c.model.model_id == "old" {
                old.clone()
            } else {
                new.clone()
            }
        }),
    );
    let running = {
        let harness = harness.clone();
        tokio::spawn(async move { harness.prompt(MAIN_LANE, "start").await.unwrap() })
    };
    started.notified().await;
    *desired.write() = config("new");
    assert_eq!(
        session
            .lane_config(MAIN_LANE)
            .await
            .unwrap()
            .unwrap()
            .0
            .model
            .model_id,
        "old"
    );
    release.notify_one();
    running.await.unwrap();
    assert_eq!(old_calls.load(Ordering::SeqCst), 2);
    assert_eq!(new_calls.load(Ordering::SeqCst), 0);
    harness.prompt(MAIN_LANE, "next").await.unwrap();
    assert_eq!(new_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        session
            .lane_config(MAIN_LANE)
            .await
            .unwrap()
            .unwrap()
            .0
            .model
            .model_id,
        "new"
    );
    session.close().await;
}
