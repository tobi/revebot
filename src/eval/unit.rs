//! Pure unit runners: slug, roster, tools, session-path refuse.

use std::collections::BTreeMap;

use crate::house::profile::{slug_from_name, unique_slug};
use crate::house::tools as house_tools;
use crate::project;
use crate::tools::is_session_path;

use super::case::Case;
use super::grade::Trace;

pub fn run(case: &Case) -> anyhow::Result<Trace> {
    let unit = case
        .unit
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("{}: unit runner needs `unit:`", case.id))?;
    let mut extras = BTreeMap::new();
    match unit {
        "slug" => {
            let name = case
                .input
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("slug unit needs input.name"))?;
            extras.insert("equals".into(), slug_from_name(name));
        }
        "unique_slug" => {
            let base = case
                .input
                .get("base")
                .and_then(|v| v.as_str())
                .unwrap_or("agent");
            let taken: Vec<String> = case
                .input
                .get("taken")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            extras.insert(
                "equals".into(),
                unique_slug(base, |s| taken.iter().any(|t| t == s)),
            );
        }
        "session_path" => {
            let path = case
                .input
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            extras.insert(
                "equals".into(),
                if is_session_path(path) {
                    "true".into()
                } else {
                    "false".into()
                },
            );
        }
        "house_tools" => {
            let names = house_tools::names().join(",");
            extras.insert("equals".into(), names);
            extras.insert("house_tools".into(), extras["equals"].clone());
        }
        "cron_describe" => {
            let src = case
                .input
                .get("cron")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("cron_describe needs input.cron"))?;
            extras.insert(
                "equals".into(),
                crate::cron::Cron::parse(src)?.describe(src),
            );
        }
        "init_loads" => {
            let dir = tempfile::TempDir::new()?;
            project::init(dir.path())?;
            let project = project::Project::load(dir.path())?;
            extras.insert(
                "equals".into(),
                if project.runtime.agent.model.is_some() {
                    "ok".into()
                } else {
                    "no-model".into()
                },
            );
        }
        other => anyhow::bail!("{}: unknown unit {other}", case.id),
    }
    let final_text = extras.get("equals").cloned().unwrap_or_default();
    Ok(Trace {
        outcome: Some("completed".into()),
        final_text,
        extras,
        ..Default::default()
    })
}
