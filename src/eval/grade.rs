//! Graders over a run trace.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::case::{GradeTarget, Grader, GraderSpec, TranscriptExpect};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Trace {
    pub outcome: Option<String>,
    pub final_text: String,
    pub transcript: Vec<TraceMessage>,
    pub tools: Vec<TraceTool>,
    pub extras: std::collections::BTreeMap<String, String>,
    #[serde(skip)]
    pub root: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceMessage {
    pub role: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceTool {
    pub name: String,
    pub arguments: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GradeResult {
    pub grader: String,
    pub passed: bool,
    pub detail: String,
    #[serde(default)]
    pub soft: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
}

pub fn apply(graders: &[GraderSpec], trace: &Trace) -> Vec<GradeResult> {
    graders
        .iter()
        .map(|g| {
            let mut r = grade_one(&g.kind, trace);
            r.soft = g.soft;
            r
        })
        .collect()
}

fn grade_one(grader: &Grader, trace: &Trace) -> GradeResult {
    match grader {
        Grader::Outcome { equals } => {
            let got = trace.outcome.as_deref().unwrap_or("");
            pass(
                "outcome",
                got.eq_ignore_ascii_case(equals),
                format!("want {equals}, got {got}"),
            )
        }
        Grader::Contains { text, target } => {
            let hay = target_text(trace, *target);
            let ok = hay.contains(text);
            pass(
                "contains",
                ok,
                if ok {
                    "ok".into()
                } else {
                    format!("{target:?} missing {text:?}")
                },
            )
        }
        Grader::NotContains { text, target } => {
            let hay = target_text(trace, *target);
            let ok = !hay.contains(text);
            pass(
                "not_contains",
                ok,
                if ok {
                    "ok".into()
                } else {
                    format!("{target:?} unexpectedly contains {text:?}")
                },
            )
        }
        Grader::Regex { pattern, target } => {
            let hay = target_text(trace, *target);
            let ok = regex::Regex::new(pattern).is_ok_and(|re| re.is_match(&hay));
            pass("regex", ok, format!("{pattern} vs {target:?}"))
        }
        Grader::Exact { text, target } => {
            let hay = target_text(trace, *target);
            pass(
                "exact",
                hay.trim() == text.trim(),
                format!("want {text:?}, got {:?}", hay.trim()),
            )
        }
        Grader::Equals { text } => {
            let got = trace.extras.get("equals").cloned().unwrap_or_default();
            pass(
                "equals",
                got == *text,
                format!("want {text:?}, got {got:?}"),
            )
        }
        Grader::UsedNoTools {} => {
            let ok = trace.tools.is_empty();
            pass(
                "used_no_tools",
                ok,
                if ok {
                    "ok".into()
                } else {
                    format!(
                        "tools: {:?}",
                        trace.tools.iter().map(|t| &t.name).collect::<Vec<_>>()
                    )
                },
            )
        }
        Grader::NotTools { names } => {
            let hit: Vec<&str> = trace
                .tools
                .iter()
                .filter(|t| names.iter().any(|n| n == &t.name))
                .map(|t| t.name.as_str())
                .collect();
            pass(
                "not_called_tool",
                hit.is_empty(),
                if hit.is_empty() {
                    "ok".into()
                } else {
                    format!("unexpected {hit:?}")
                },
            )
        }
        Grader::ToolOrder { names } => {
            let got: Vec<&str> = trace.tools.iter().map(|t| t.name.as_str()).collect();
            let mut gi = 0;
            let ok = names.iter().all(|want| {
                while gi < got.len() {
                    if got[gi] == want {
                        gi += 1;
                        return true;
                    }
                    gi += 1;
                }
                false
            });
            pass("tool_order", ok, format!("want {names:?}, got {got:?}"))
        }
        Grader::ClosedQa { criteria, at_least } => {
            let _ = (criteria, at_least);
            pass("closed_qa", true, "deferred to async judge".into())
        }
        Grader::Tools { names } => {
            let got: Vec<&str> = trace.tools.iter().map(|t| t.name.as_str()).collect();
            let ok = names.iter().all(|n| got.contains(&n.as_str()));
            pass("tools", ok, format!("want {names:?}, got {got:?}"))
        }
        Grader::ToolArgs { name, contains } => {
            let found = trace.tools.iter().any(|t| {
                t.name == *name && contains.iter().all(|(k, v)| t.arguments.get(k) == Some(v))
            });
            pass("tool_args", found, format!("{name} with {contains:?}"))
        }
        Grader::FileExists { path } => {
            let ok = resolve(trace, path).is_some_and(|p| p.exists());
            pass("file_exists", ok, path.clone())
        }
        Grader::FileNotExists { path } => {
            let ok = resolve(trace, path).is_none_or(|p| !p.exists());
            pass("file_not_exists", ok, path.clone())
        }
        Grader::FileContains { path, text } => {
            let ok = resolve(trace, path)
                .and_then(|p| std::fs::read_to_string(p).ok())
                .is_some_and(|body| body.contains(text));
            pass("file_contains", ok, format!("{path} has {text:?}"))
        }
        Grader::JsonPointer {
            path,
            pointer,
            equals,
        } => {
            let ok = resolve(trace, path)
                .and_then(|p| std::fs::read_to_string(p).ok())
                .and_then(|body| serde_json::from_str::<Value>(&body).ok())
                .and_then(|v| v.pointer(pointer).cloned())
                .is_some_and(|got| got == *equals);
            pass("json_pointer", ok, format!("{path} {pointer} == {equals}"))
        }
        Grader::Transcript { messages } => {
            let (ok, detail) = match_transcript(&trace.transcript, messages);
            pass("transcript", ok, detail)
        }
    }
}

fn target_text(trace: &Trace, target: GradeTarget) -> String {
    match target {
        GradeTarget::FinalText => trace.final_text.clone(),
        GradeTarget::Transcript => trace
            .transcript
            .iter()
            .map(|m| format!("{}: {}", m.role, m.text))
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn resolve(trace: &Trace, path: &str) -> Option<std::path::PathBuf> {
    let root = trace.root.as_ref()?;
    Some(root.join(path))
}

fn match_transcript(got: &[TraceMessage], want: &[TranscriptExpect]) -> (bool, String) {
    if got.len() < want.len() {
        return (
            false,
            format!("want {} messages, got {}", want.len(), got.len()),
        );
    }
    for (i, expect) in want.iter().enumerate() {
        let msg = &got[i];
        if !msg.role.eq_ignore_ascii_case(&expect.role) {
            return (
                false,
                format!("message {i}: want role {}, got {}", expect.role, msg.role),
            );
        }
        if let Some(exact) = &expect.exact
            && msg.text.trim() != exact.trim()
        {
            return (
                false,
                format!("message {i}: want exact {exact:?}, got {:?}", msg.text),
            );
        }
        if let Some(contains) = &expect.contains
            && !msg.text.contains(contains)
        {
            return (
                false,
                format!("message {i}: missing {contains:?} in {:?}", msg.text),
            );
        }
    }
    (true, "ok".into())
}

fn pass(name: &str, ok: bool, detail: String) -> GradeResult {
    GradeResult {
        grader: name.into(),
        passed: ok,
        detail,
        soft: false,
        score: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::case::{Grader, GraderSpec};

    #[test]
    fn contains_and_tools_grade() {
        let trace = Trace {
            outcome: Some("completed".into()),
            final_text: "hello Researcher".into(),
            tools: vec![TraceTool {
                name: "CreateAgent".into(),
                arguments: serde_json::Map::from_iter([(
                    "name".into(),
                    Value::String("Researcher".into()),
                )]),
            }],
            ..Default::default()
        };
        let results = apply(
            &[
                GraderSpec {
                    kind: Grader::Contains {
                        text: "Researcher".into(),
                        target: GradeTarget::FinalText,
                    },
                    soft: false,
                },
                GraderSpec {
                    kind: Grader::Tools {
                        names: vec!["CreateAgent".into()],
                    },
                    soft: false,
                },
                GraderSpec {
                    kind: Grader::NotContains {
                        text: "what should I be called".into(),
                        target: GradeTarget::FinalText,
                    },
                    soft: false,
                },
            ],
            &trace,
        );
        assert!(results.iter().all(|r| r.passed));
    }
}
