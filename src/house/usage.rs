//! Append-only invocation log for later stats.
//!
//! Lives in the house state dir (`.reve/usage.jsonl`). One JSON object per
//! line, flush every append. A write failure is ignored: usage is telemetry,
//! not a control path.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use parking_lot::Mutex;
use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct UsageEvent {
    pub ts: String,
    pub kind: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bot: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl UsageEvent {
    pub fn skill(bot: &str, name: &str, source: &str) -> Self {
        Self {
            ts: now(),
            kind: "skill".into(),
            name: name.into(),
            bot: Some(bot.into()),
            source: Some(source.into()),
        }
    }

    pub fn plugin(bot: &str, name: &str, source: Option<&str>) -> Self {
        Self {
            ts: now(),
            kind: "plugin".into(),
            name: name.into(),
            bot: Some(bot.into()),
            source: source.map(str::to_string),
        }
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub struct UsageLog {
    path: PathBuf,
    lock: Mutex<()>,
}

impl UsageLog {
    pub fn open(state_dir: &Path) -> Self {
        Self {
            path: state_dir.join("usage.jsonl"),
            lock: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn record(&self, event: &UsageEvent) {
        let Ok(line) = serde_json::to_string(event) else {
            return;
        };
        let _guard = self.lock.lock();
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        else {
            return;
        };
        let _ = file.write_all(line.as_bytes());
        let _ = file.write_all(b"\n");
        let _ = file.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_one_json_object_per_line() {
        let dir = tempfile::tempdir().unwrap();
        let log = UsageLog::open(dir.path());
        log.record(&UsageEvent::skill("rune", "create-skill", "workspace"));
        log.record(&UsageEvent::plugin("rune", "web_fetch", Some("workspace")));
        let text = std::fs::read_to_string(log.path()).unwrap();
        let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 2);
        let a: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(a["kind"], "skill");
        assert_eq!(a["name"], "create-skill");
        assert_eq!(a["bot"], "rune");
        assert_eq!(a["source"], "workspace");
        let b: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(b["kind"], "plugin");
        assert_eq!(b["name"], "web_fetch");
    }
}
