//! YAML case schema. One file is one case, or a document with `cases:`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Offline,
    Live,
    Microvm,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerKind {
    #[default]
    Harness,
    Files,
    Unit,
    LiveHouse,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Case {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub suite: String,
    #[serde(default)]
    pub mode: Mode,
    #[serde(default)]
    pub runner: RunnerKind,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Default bot when driving a live house (`--url`).
    #[serde(default)]
    pub bot: Option<String>,
    /// Imperative drive+assert script. Compiled into `actions` + `graders`.
    #[serde(default)]
    pub test: Vec<Value>,
    #[serde(default)]
    pub prompt: Option<String>,
    /// Ordered harness actions. If empty, a single `prompt` is run.
    #[serde(default)]
    pub actions: Vec<Action>,
    /// Scripted assistant turns (offline harness). Absent ⇒ live model.
    #[serde(default)]
    pub script: Vec<ScriptTurn>,
    /// Tool stubs for the recording toolbox: name → result text.
    #[serde(default)]
    pub tools: BTreeMap<String, ToolStub>,
    /// Extra system prompt for the harness runner.
    #[serde(default)]
    pub system: Option<String>,
    /// Files-runner: write these into the temp house before grading.
    #[serde(default)]
    pub setup_files: BTreeMap<String, String>,
    /// Unit runner selector (`slug`, `unique_slug`, `session_path`, `house_tools`).
    #[serde(default)]
    pub unit: Option<String>,
    #[serde(default)]
    pub input: Value,
    #[serde(default)]
    pub graders: Vec<GraderSpec>,
    /// If set, the case is skipped with this reason (from a `skip` test step).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip: Option<String>,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    #[serde(skip)]
    pub source: PathBuf,
}

fn default_timeout() -> u64 {
    30
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Action {
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub next_run: Option<String>,
    #[serde(default)]
    pub kick: bool,
    #[serde(default)]
    pub place_idle: Option<PlaceIdle>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaceIdle {
    #[serde(default)]
    pub custom_type: String,
    #[serde(default)]
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptTurn {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub tool_calls: Vec<ScriptToolCall>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptToolCall {
    pub name: String,
    #[serde(default)]
    pub arguments: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolStub {
    #[serde(default)]
    pub result: String,
    #[serde(default)]
    pub replay: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GradeTarget {
    #[default]
    FinalText,
    Transcript,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Grader {
    Outcome {
        equals: String,
    },
    Contains {
        text: String,
        #[serde(default, rename = "in")]
        target: GradeTarget,
    },
    NotContains {
        text: String,
        #[serde(default, rename = "in")]
        target: GradeTarget,
    },
    Regex {
        pattern: String,
        #[serde(default, rename = "in")]
        target: GradeTarget,
    },
    Exact {
        text: String,
        #[serde(default, rename = "in")]
        target: GradeTarget,
    },
    Tools {
        names: Vec<String>,
    },
    ToolArgs {
        name: String,
        contains: Map<String, Value>,
    },
    FileExists {
        path: String,
    },
    FileNotExists {
        path: String,
    },
    FileContains {
        path: String,
        text: String,
    },
    JsonPointer {
        path: String,
        pointer: String,
        equals: Value,
    },
    Transcript {
        messages: Vec<TranscriptExpect>,
    },
    Equals {
        text: String,
    },
    UsedNoTools {},
    NotTools {
        names: Vec<String>,
    },
    ToolOrder {
        names: Vec<String>,
    },
    ClosedQa {
        criteria: String,
        #[serde(default)]
        at_least: Option<f64>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraderSpec {
    #[serde(flatten)]
    pub kind: Grader,
    /// Tracked, not fatal, unless `--strict`.
    #[serde(default)]
    pub soft: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptExpect {
    pub role: String,
    #[serde(default)]
    pub contains: Option<String>,
    #[serde(default)]
    pub exact: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Many {
    cases: Vec<Case>,
}

pub fn load_file(path: &Path) -> anyhow::Result<Vec<Case>> {
    let text = std::fs::read_to_string(path)?;
    let mut loaded = if let Ok(many) = serde_yaml::from_str::<Many>(&text) {
        many.cases
    } else {
        let case: Case =
            serde_yaml::from_str(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        vec![case]
    };
    for case in &mut loaded {
        case.source = path.to_path_buf();
        if case.suite.is_empty() {
            case.suite = suite_from_path(path);
        }
        compile_test(case)?;
    }
    Ok(loaded)
}

/// Fill identity from the file path when `id` is omitted.
pub fn assign_path_id(cases_dir: &Path, path: &Path, case: &mut Case) {
    if !case.id.is_empty() {
        return;
    }
    let rel = path.strip_prefix(cases_dir).unwrap_or(path);
    case.id = rel
        .with_extension("")
        .to_string_lossy()
        .replace('\\', "/")
        .to_string();
}

fn compile_test(case: &mut Case) -> anyhow::Result<()> {
    if case.test.is_empty() {
        return Ok(());
    }
    let steps = case.test.clone();
    for step in &steps {
        match step {
            Value::String(word) => match word.as_str() {
                "succeeded" => case.graders.push(spec(Grader::Outcome {
                    equals: "completed".into(),
                })),
                "used_no_tools" => case.graders.push(spec(Grader::UsedNoTools {})),
                other => anyhow::bail!("{}: unknown test step {other}", case.id),
            },
            Value::Object(_) => {
                let map = step.as_object().ok_or_else(|| {
                    anyhow::anyhow!("{}: test step must be a map or a word", case.id)
                })?;
                if let Some(text) = map.get("send").and_then(Value::as_str) {
                    case.actions.push(Action {
                        prompt: Some(text.into()),
                        next_run: None,
                        kick: false,
                        place_idle: None,
                    });
                    if case.prompt.is_none() {
                        case.prompt = Some(text.into());
                    }
                } else if let Some(reason) = map.get("skip").and_then(Value::as_str) {
                    case.skip = Some(reason.into());
                } else if let Some(text) = map.get("reply_includes").and_then(Value::as_str) {
                    case.graders.push(spec(Grader::Contains {
                        text: text.into(),
                        target: GradeTarget::FinalText,
                    }));
                } else if let Some(text) = map.get("reply_not_includes").and_then(Value::as_str) {
                    case.graders.push(spec(Grader::NotContains {
                        text: text.into(),
                        target: GradeTarget::FinalText,
                    }));
                } else if let Some(pat) = map.get("reply_matches").and_then(Value::as_str) {
                    case.graders.push(spec(Grader::Regex {
                        pattern: pat.into(),
                        target: GradeTarget::FinalText,
                    }));
                } else if let Some(name) = map.get("called_tool") {
                    compile_called_tool(case, name)?;
                } else if let Some(name) = map.get("not_called_tool").and_then(Value::as_str) {
                    case.graders.push(spec(Grader::NotTools {
                        names: vec![name.into()],
                    }));
                } else if let Some(names) = map.get("tool_order").and_then(Value::as_array) {
                    case.graders.push(spec(Grader::ToolOrder {
                        names: names
                            .iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect(),
                    }));
                } else if let Some(criteria) = map.get("closed_qa").and_then(Value::as_str) {
                    let mut g = spec(Grader::ClosedQa {
                        criteria: criteria.into(),
                        at_least: map.get("at_least").and_then(Value::as_f64),
                    });
                    g.soft = map.get("soft").and_then(Value::as_bool).unwrap_or(true);
                    case.graders.push(g);
                } else {
                    anyhow::bail!("{}: unrecognized test step {step}", case.id);
                }
            }
            other => anyhow::bail!("{}: bad test step {other}", case.id),
        }
    }
    Ok(())
}

fn spec(kind: Grader) -> GraderSpec {
    GraderSpec { kind, soft: false }
}

fn compile_called_tool(case: &mut Case, name: &Value) -> anyhow::Result<()> {
    match name {
        Value::String(n) => case.graders.push(spec(Grader::Tools {
            names: vec![n.clone()],
        })),
        Value::Object(map) => {
            let n = map
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("{}: called_tool needs name", case.id))?;
            case.graders.push(spec(Grader::Tools {
                names: vec![n.into()],
            }));
            if let Some(Value::Object(args)) = map.get("args") {
                case.graders.push(spec(Grader::ToolArgs {
                    name: n.into(),
                    contains: args.clone(),
                }));
            }
        }
        _ => anyhow::bail!("{}: called_tool must be a name or a map", case.id),
    }
    Ok(())
}

fn suite_from_path(path: &Path) -> String {
    path.parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("misc")
        .to_string()
}

pub fn discover(root: &Path) -> anyhow::Result<Vec<Case>> {
    let cases_dir = root.join("cases");
    if !cases_dir.is_dir() {
        anyhow::bail!("no cases directory at {}", cases_dir.display());
    }
    let mut paths = Vec::new();
    for pattern in ["**/*.yaml", "**/*.yml"] {
        let glob = cases_dir.join(pattern);
        let glob = glob.to_string_lossy().replace('\\', "/");
        for entry in glob::glob(&glob)? {
            paths.push(entry?);
        }
    }
    paths.sort();
    paths.dedup();
    let mut cases = Vec::new();
    for path in paths {
        let mut loaded = load_file(&path)?;
        for case in &mut loaded {
            assign_path_id(&cases_dir, &path, case);
        }
        cases.extend(loaded);
    }
    Ok(cases)
}
