//! Shared post-effect invalidation/notification vocabulary. No file content.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Change {
    pub bot: String,
    pub cwd: String,
    pub paths: Vec<String>,
    pub resources: Vec<String>,
    pub unknown: bool,
}

pub const KINDS: &[&str] = &[
    "profile",
    "soul",
    "memory",
    "directory_rules",
    "skills",
    "plugins",
    "routines",
    "vm",
];

pub fn classify(path: &str) -> Option<&'static str> {
    let path = std::path::Path::new(path);
    match path.file_name().and_then(|n| n.to_str()) {
        Some("profile.json") => Some("profile"),
        Some("SOUL.md") => Some("soul"),
        Some("AGENTS.md") => Some("directory_rules"),
        Some("VM.md") => Some("vm"),
        _ => {
            let parts: Vec<_> = path
                .components()
                .filter_map(|p| p.as_os_str().to_str())
                .collect();
            if parts.contains(&"memory") {
                Some("memory")
            } else if parts.contains(&"skills") {
                Some("skills")
            } else if parts.contains(&"plugins") {
                Some("plugins")
            } else if parts.contains(&"routines") {
                Some("routines")
            } else {
                None
            }
        }
    }
}

impl Change {
    pub fn new(bot: &str, cwd: String, paths: Vec<String>, unknown: bool) -> Self {
        let mut resources: Vec<_> = paths
            .iter()
            .filter_map(|p| classify(p).map(str::to_string))
            .collect();
        resources.sort();
        resources.dedup();
        Self {
            bot: bot.into(),
            cwd,
            paths,
            resources,
            unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resources_share_one_path_classifier() {
        for (path, kind) in [
            ("/workspace/agents/miku/SOUL.md", "soul"),
            ("/workspace/agents/miku/profile.json", "profile"),
            ("/workspace/agents/miku/memory/log/2026-08.md", "memory"),
            ("/repo/AGENTS.md", "directory_rules"),
            ("/workspace/plugins/new.lua", "plugins"),
        ] {
            assert_eq!(classify(path), Some(kind));
        }
        assert_eq!(classify("/workspace/project/main.rs"), None);
    }
}
