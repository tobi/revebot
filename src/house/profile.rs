//! File-based bot profile and roster scan.

use std::path::Path;

use serde::{Deserialize, Serialize};

pub const FIRST_BOT: &str = "chief-of-staff";
pub const BOT_CAP: usize = 50;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub avatar: Option<String>,
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

impl Profile {
    pub fn load(path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        let mut profile: Self = serde_json::from_str(&text).ok()?;
        if profile.id.trim().is_empty() {
            profile.id = path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("agent")
                .to_string();
        }
        Some(profile)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".into())
    }
}

pub fn scan(agents_dir: &Path) -> Vec<Profile> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(agents_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if let Some(profile) = Profile::load(&path.join("profile.json")) {
            out.push(profile);
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// ASCII slug: non-alnum → `-`, collapse, trim, empty → `agent`, suffix later.
pub fn slug_from_name(name: &str) -> String {
    let mut slug = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        return "agent".into();
    }
    if slug.len() > 64 {
        slug.truncate(64);
        while slug.ends_with('-') {
            slug.pop();
        }
        if slug.is_empty() {
            return "agent".into();
        }
    }
    slug
}

pub fn unique_slug(base: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(base) {
        return base.to_string();
    }
    for n in 2..10_000 {
        let candidate = format!("{base}-{n}");
        if !taken(&candidate) {
            return candidate;
        }
    }
    format!("{base}-x")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_collapses_and_falls_back() {
        assert_eq!(slug_from_name("Chief of Staff"), "chief-of-staff");
        assert_eq!(slug_from_name("Researcher"), "researcher");
        assert_eq!(slug_from_name("!!!"), "agent");
        assert_eq!(slug_from_name("Ada"), "ada");
    }

    #[test]
    fn unique_slug_suffixes() {
        let taken = |s: &str| s == "researcher" || s == "researcher-2";
        assert_eq!(unique_slug("researcher", taken), "researcher-3");
    }
}
