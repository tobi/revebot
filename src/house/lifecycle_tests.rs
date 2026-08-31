use super::*;
use crate::model::ToolSchema;
use crate::sandbox::tokio_util_lite::CancelRx;
use crate::tools::Tools;
use serde_json::{Map, Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Notify;

struct GateModel(AtomicUsize);
impl Model for GateModel {
    fn respond<'a>(
        &'a self,
        _: Request<'a>,
        _: Deltas<'a>,
    ) -> BoxFuture<'a, crate::model::Result<Assistant>> {
        Box::pin(async move {
            Ok(if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                Assistant::call("gate", json!({}))
            } else {
                Assistant::text("done")
            })
        })
    }
}
struct GateTools {
    started: Arc<Notify>,
    release: Arc<Notify>,
}
impl Tools for GateTools {
    fn replay(&self, _: &str) -> Option<crate::state::Replay> {
        Some(crate::state::Replay::Never)
    }
    fn schemas(&self) -> Vec<ToolSchema> {
        vec![ToolSchema {
            name: "gate".into(),
            description: "wait".into(),
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
            self.started.notify_one();
            self.release.notified().await;
            Ok("completed".into())
        })
    }
}

#[tokio::test]
async fn supervisor_stop_seals_the_session_and_waits_for_owned_effects() {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let session = Session::spawn(Storage::memory("stop"));
    let harness = Harness::new(
        session.clone(),
        HarnessConfig {
            model: Arc::new(GateModel(AtomicUsize::new(0))),
            tools: Arc::new(GateTools {
                started: started.clone(),
                release: release.clone(),
            }),
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
                active_tool_names: vec!["gate".into()],
            },
            event_capacity: 128,
        },
    );
    let (tx, rx) = mpsc::channel(16);
    let supervisor = spawn_supervisor(harness.clone(), rx, Weak::new(), "bot".into());
    let (reply, ack) = tokio::sync::oneshot::channel();
    tx.send(BotCmd::UserText {
        text: "start".into(),
        reply,
    })
    .await
    .unwrap();
    ack.await.unwrap().unwrap();
    started.notified().await;
    let (reply, mut stopped) = tokio::sync::oneshot::channel();
    tx.send(BotCmd::Stop(reply)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while session.stats().await.is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        matches!(
            stopped.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ),
        "a non-cooperative effect must finish before deletion is safe"
    );
    assert!(harness.begin_run(MAIN_LANE, "late").await.is_err());
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), stopped)
        .await
        .unwrap()
        .unwrap();
    supervisor.await.unwrap();
    assert!(tx.send(BotCmd::KickNow).await.is_err());
}
