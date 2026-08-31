//! Harness runner: ScriptedModel or live HttpModel, recording tools, no VM.

use serde_json::{Map, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;

use crate::entry::MAIN_LANE;
use crate::harness::{Harness, HarnessConfig};
use crate::hooks::Hooks;
use crate::model::{Assistant, BoxFuture, Deltas, Model, Request, ScriptedModel, ToolSchema};
use crate::provider::HttpModel;
use crate::provider::config::Models;
use crate::sandbox::tokio_util_lite::CancelRx;
use crate::session::Session;
use crate::state::{LaneConfiguration, ModelRef, PendingEntry, Replay, RetryPolicy, RunSettings};
use crate::storage::Storage;
use crate::tools::Tools;

use super::case::{Case, ScriptTurn};
use super::grade::{Trace, TraceMessage, TraceTool};

pub struct RecordingTools {
    replay: Vec<(String, Replay)>,
    results: std::collections::BTreeMap<String, String>,
    calls: std::sync::Mutex<Vec<TraceTool>>,
}

impl RecordingTools {
    pub fn new(case: &Case) -> Arc<Self> {
        let mut replay = Vec::new();
        let mut results = std::collections::BTreeMap::new();
        for (name, stub) in &case.tools {
            replay.push((
                name.clone(),
                Replay::parse(stub.replay.as_deref().unwrap_or("never")),
            ));
            results.insert(name.clone(), stub.result.clone());
        }
        for turn in &case.script {
            for call in &turn.tool_calls {
                if !replay.iter().any(|(n, _)| n == &call.name) {
                    replay.push((call.name.clone(), Replay::Never));
                }
            }
        }
        Arc::new(Self {
            replay,
            results,
            calls: std::sync::Mutex::new(Vec::new()),
        })
    }

    pub fn recorded(&self) -> Vec<TraceTool> {
        self.calls.lock().unwrap().clone()
    }
}

impl Tools for RecordingTools {
    fn replay(&self, name: &str) -> Option<Replay> {
        self.replay
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, r)| *r)
            .or(Some(Replay::Never))
    }

    fn schemas(&self) -> Vec<ToolSchema> {
        self.replay
            .iter()
            .map(|(name, _)| ToolSchema {
                name: name.clone(),
                description: format!("eval stub `{name}`"),
                schema: serde_json::json!({"type": "object", "additionalProperties": true}),
            })
            .collect()
    }

    fn invoke<'a>(
        &'a self,
        name: &'a str,
        arguments: Map<String, Value>,
        _cancel: Option<CancelRx>,
    ) -> BoxFuture<'a, Result<String, String>> {
        self.calls.lock().unwrap().push(TraceTool {
            name: name.to_string(),
            arguments: arguments.clone(),
        });
        let result = self
            .results
            .get(name)
            .cloned()
            .unwrap_or_else(|| format!("{name} ok"));
        Box::pin(async move { Ok(result) })
    }
}

struct LiveModel {
    inner: Arc<dyn Model>,
}

impl Model for LiveModel {
    fn respond<'a>(
        &'a self,
        request: Request<'a>,
        on_text: Deltas<'a>,
    ) -> BoxFuture<'a, crate::model::Result<Assistant>> {
        self.inner.respond(request, on_text)
    }
}

pub async fn run(case: &Case, live: bool) -> anyhow::Result<Trace> {
    let dir = TempDir::new()?;
    let session_path = dir.path().join("session.jsonl");
    let cursor = dir.path().join("cursor");
    let storage = Storage::open(&session_path, &case.id, None)?;
    let session = Session::spawn(storage);
    let tools = RecordingTools::new(case);
    let needs_model = case.actions.is_empty()
        || case
            .actions
            .iter()
            .any(|action| action.prompt.is_some() || action.kick);
    let model: Arc<dyn Model> = if !needs_model {
        Arc::new(ScriptedModel::new(vec![], cursor))
    } else if case.script.is_empty() {
        if !live {
            anyhow::bail!(
                "case {} has no script; pass --live to use a real model",
                case.id
            );
        }
        Arc::new(LiveModel {
            inner: load_live_model()?,
        })
    } else {
        Arc::new(ScriptedModel::new(
            script_to_assistants(&case.script),
            cursor,
        ))
    };
    let system = case
        .system
        .clone()
        .unwrap_or_else(|| "you are a test".into());
    let names: Vec<String> = tools.schemas().into_iter().map(|s| s.name).collect();
    let harness = Harness::new(
        session.clone(),
        HarnessConfig {
            model,
            tools: tools.clone(),
            hooks: Hooks::new(),
            system_prompt: {
                let system = system.clone();
                Arc::new(move || system.clone())
            },
            settings: RunSettings::default(),
            retry: RetryPolicy {
                max_attempts: 3,
                base_delay_ms: 1,
            },
            configuration: LaneConfiguration {
                model: ModelRef {
                    provider: "eval".into(),
                    model_id: "eval".into(),
                },
                thinking_level: "off".into(),
                active_tool_names: names,
            },
            event_capacity: 256,
        },
    );
    let mut last_result: Option<crate::lane::OperationResult> = None;
    if case.actions.is_empty() {
        let prompt = case
            .prompt
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("{}: harness runner needs a prompt", case.id))?;
        last_result = Some(harness.prompt(MAIN_LANE, prompt).await?);
    } else {
        for action in &case.actions {
            if let Some(text) = &action.prompt {
                last_result = Some(harness.prompt(MAIN_LANE, text).await?);
            }
            if let Some(text) = &action.next_run {
                harness.next_run(MAIN_LANE, text).await?;
            }
            if action.kick {
                last_result = Some(harness.kick(MAIN_LANE).await?);
            }
            if let Some(idle) = &action.place_idle {
                harness
                    .place_idle(
                        MAIN_LANE,
                        PendingEntry::custom(
                            idle.custom_type.clone(),
                            serde_json::json!({ "text": idle.text }),
                        ),
                    )
                    .await?;
            }
        }
    }
    let result_outcome = last_result.as_ref().map(|r| r.outcome.as_str().to_string());
    let result_text = last_result.and_then(|r| r.final_text);
    let entries = session.transcript(MAIN_LANE).await?;
    session.close().await;
    let transcript = messages_from_entries(&entries);
    let final_text = result_text
        .or_else(|| {
            transcript
                .iter()
                .rev()
                .find(|m| m.role == "assistant")
                .map(|m| m.text.clone())
        })
        .unwrap_or_default();
    Ok(Trace {
        outcome: result_outcome.or(Some("completed".into())),
        final_text,
        transcript,
        tools: tools.recorded(),
        extras: Default::default(),
        root: None,
    })
}

fn script_to_assistants(script: &[ScriptTurn]) -> Vec<Assistant> {
    script
        .iter()
        .map(|turn| {
            if turn.tool_calls.is_empty() {
                Assistant::text(&turn.text)
            } else {
                let mut a = Assistant::calls(
                    turn.tool_calls
                        .iter()
                        .map(|c| (c.name.clone(), Value::Object(c.arguments.clone())))
                        .collect(),
                );
                a.text = turn.text.clone();
                a
            }
        })
        .collect()
}

fn messages_from_entries(entries: &[crate::entry::Entry]) -> Vec<TraceMessage> {
    let mut out = Vec::new();
    for entry in entries {
        if entry.entry_type == "custom" {
            let text = entry
                .payload
                .get("data")
                .and_then(|d| d.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            out.push(TraceMessage {
                role: entry.custom_type.clone().unwrap_or_else(|| "custom".into()),
                text,
            });
            continue;
        }
        let Some(message) = entry.message_value() else {
            continue;
        };
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let text = if role == "assistant" {
            Assistant::from_message(message)
                .map(|a| a.text)
                .unwrap_or_default()
        } else {
            message
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        out.push(TraceMessage { role, text });
    }
    out
}

pub fn try_live_model() -> Option<Arc<dyn Model>> {
    load_live_model().ok()
}

/// Default live id when no key is set, so the missing-env error names
/// `OPENROUTER_API_KEY` rather than an unused OpenAI key.
const DEFAULT_LIVE_MODEL: &str = "openrouter/x-ai/grok-4.6";

fn load_live_model() -> anyhow::Result<Arc<dyn Model>> {
    let root = std::env::current_dir()?;
    let path = if root.join("models.yml").is_file() {
        root.join("models.yml")
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/templates/models.yml")
    };
    let models = Models::load(&path)?;
    let spec = pick_live_spec(
        &models,
        std::env::var("REVEBOT_EVAL_MODEL").ok().as_deref(),
        &|var| std::env::var(var).ok(),
    );
    let resolved = models.resolve(&spec).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(Arc::new(HttpModel::new(resolved)))
}

/// Choose a live model: `REVEBOT_EVAL_MODEL` wins, otherwise the first
/// configured provider whose `$ENV` key is actually set, preferring OpenRouter.
fn pick_live_spec(
    models: &Models,
    eval_model: Option<&str>,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> String {
    if let Some(spec) = eval_model.map(str::trim).filter(|s| !s.is_empty()) {
        return spec.to_string();
    }
    let mut names: Vec<String> = vec!["openrouter".into()];
    for name in models.providers.keys() {
        if name != "openrouter" {
            names.push(name.clone());
        }
    }
    for name in names {
        let Some(provider) = models.providers.get(&name) else {
            continue;
        };
        let key_set = match provider.api_key.as_deref() {
            Some(key) => match key.strip_prefix('$') {
                Some(var) => lookup(var).is_some_and(|v| !v.is_empty()),
                None => true,
            },
            None => true,
        };
        if !key_set {
            continue;
        }
        if let Some(model) = provider.models.first() {
            return format!("{name}/{}", model.id);
        }
    }
    DEFAULT_LIVE_MODEL.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Models {
        Models::load(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/templates/models.yml"))
            .expect("template models.yml")
    }

    fn lookup<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |var| {
            pairs
                .iter()
                .find(|(k, _)| *k == var)
                .map(|(_, v)| (*v).to_string())
        }
    }

    #[test]
    fn live_model_prefers_openrouter_when_its_key_is_set() {
        let models = sample();
        assert_eq!(
            pick_live_spec(
                &models,
                None,
                &lookup(&[("OPENROUTER_API_KEY", "sk"), ("OPENAI_API_KEY", "sk")])
            ),
            "openrouter/x-ai/grok-4.6"
        );
    }

    #[test]
    fn live_model_falls_back_to_a_provider_whose_key_is_set() {
        let models = sample();
        assert_eq!(
            pick_live_spec(&models, None, &lookup(&[("OPENAI_API_KEY", "sk")])),
            "openai/gpt-5.6-luna"
        );
    }

    #[test]
    fn live_model_env_override_wins() {
        let models = sample();
        assert_eq!(
            pick_live_spec(
                &models,
                Some("openai/gpt-5.6-luna"),
                &lookup(&[("OPENROUTER_API_KEY", "sk")])
            ),
            "openai/gpt-5.6-luna"
        );
    }

    #[test]
    fn live_model_defaults_to_openrouter_when_no_key_is_set() {
        let models = sample();
        assert_eq!(
            pick_live_spec(&models, None, &lookup(&[])),
            "openrouter/x-ai/grok-4.6"
        );
    }
}
