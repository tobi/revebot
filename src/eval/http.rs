//! Drive a running house over the same HTTP surface the UI uses.

use std::time::{Duration, Instant};

use serde_json::Value;

use crate::house::profile::FIRST_BOT;
use crate::model::Assistant;

use super::case::Case;
use super::grade::{Trace, TraceMessage, TraceTool};

pub async fn run(case: &Case, base: &str, token: &str) -> anyhow::Result<Trace> {
    let bot = case.bot.as_deref().unwrap_or(FIRST_BOT);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(case.timeout_seconds.max(5)))
        .build()?;
    let auth = format!("Bearer {token}");
    let sends: Vec<String> = if case.actions.is_empty() {
        case.prompt.iter().cloned().collect()
    } else {
        case.actions
            .iter()
            .filter_map(|a| a.prompt.clone())
            .collect()
    };
    if sends.is_empty() {
        anyhow::bail!("{}: HTTP target needs a send/prompt", case.id);
    }
    let mut last_replies = 0usize;
    for text in &sends {
        let url = format!("{}/api/bots/{}/messages", base.trim_end_matches('/'), bot);
        let res = client
            .post(&url)
            .header("Authorization", &auth)
            .json(&serde_json::json!({ "text": text }))
            .send()
            .await?;
        if !res.status().is_success() {
            let body = res.text().await.unwrap_or_default();
            anyhow::bail!("{}: house returned {body}", case.id);
        }
        last_replies =
            wait_for_assistant(&client, &url, &auth, last_replies, case.timeout_seconds).await?;
    }
    let url = format!("{}/api/bots/{}/messages", base.trim_end_matches('/'), bot);
    let body: Value = client
        .get(&url)
        .header("Authorization", &auth)
        .send()
        .await?
        .json()
        .await?;
    Ok(trace_from_messages(&body))
}

async fn wait_for_assistant(
    client: &reqwest::Client,
    url: &str,
    auth: &str,
    before: usize,
    timeout_s: u64,
) -> anyhow::Result<usize> {
    let deadline = Instant::now() + Duration::from_secs(timeout_s.max(1));
    loop {
        let body: Value = client
            .get(url)
            .header("Authorization", auth)
            .send()
            .await?
            .json()
            .await?;
        let n = settled_replies(&body);
        let idle = body
            .get("operation_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .is_empty();
        if n > before && idle {
            return Ok(n);
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for house reply");
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

fn settled_replies(body: &Value) -> usize {
    body.get("records")
        .and_then(Value::as_array)
        .map_or(0, |records| {
            records
                .iter()
                .filter(|record| {
                    record.get("status").and_then(Value::as_str) == Some("committed")
                        && (record
                            .pointer("/entry/message/role")
                            .and_then(Value::as_str)
                            == Some("assistant")
                            || record.pointer("/entry/type").and_then(Value::as_str)
                                == Some("custom"))
                })
                .count()
        })
}

fn trace_from_messages(body: &Value) -> Trace {
    let mut transcript = Vec::new();
    let mut tools = Vec::new();
    let mut final_text = String::new();
    if let Some(entries) = body.get("records").and_then(Value::as_array) {
        for record in entries {
            if record["status"] != "committed" {
                continue;
            }
            let entry = &record["entry"];
            if let Some(message) = entry.get("message") {
                let role = message
                    .get("role")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                if role == "assistant" {
                    if let Some(a) = Assistant::from_message(message) {
                        if !a.text.is_empty() {
                            final_text.clone_from(&a.text);
                        }
                        for call in a.tool_calls {
                            tools.push(TraceTool {
                                name: call.name,
                                arguments: call.arguments,
                            });
                        }
                        transcript.push(TraceMessage {
                            role: "assistant".into(),
                            text: a.text,
                        });
                    }
                } else {
                    let text = message
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    transcript.push(TraceMessage {
                        role: role.into(),
                        text,
                    });
                }
            }
        }
    }
    Trace {
        outcome: Some("completed".into()),
        final_text,
        transcript,
        tools,
        extras: std::collections::BTreeMap::default(),
        root: None,
    }
}
