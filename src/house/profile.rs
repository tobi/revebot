//! File-based bot profile and roster scan.

use std::path::Path;

use serde::{Deserialize, Serialize};

pub const FIRST_BOT: &str = "chief-of-staff";
pub const BOT_CAP: usize = 50;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    #[serde(default)]
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
    /// Explicit project context. An empty list must not inherit other bots' work.
    #[serde(default)]
    pub projects: Vec<String>,
}

impl Profile {
    pub fn load(path: &Path) -> Option<Self> {
        let dir = path.parent()?;
        let id = dir.file_name()?.to_str()?;
        let root = dir.parent()?;
        let text =
            crate::script_fs::read_text(root, &Path::new(id).join("profile.json"), 131_072).ok()?;
        Self::parse_for(id, &text).ok()
    }

    pub fn parse_for(id: &str, text: &str) -> anyhow::Result<Self> {
        validate_id(id)?;
        let mut profile: Self = serde_json::from_str(text)?;
        if !profile.id.is_empty() && profile.id != id {
            anyhow::bail!(
                "profile id {:?} does not match its directory {id:?}",
                profile.id
            );
        }
        profile.id = id.into();
        if profile.name.trim().is_empty() {
            anyhow::bail!("profile name cannot be blank");
        }
        if profile
            .model
            .as_ref()
            .is_some_and(|model| model.trim().is_empty())
        {
            anyhow::bail!("model cannot be blank; use null for the house default");
        }
        for project in &profile.projects {
            validate_id(project)?;
        }
        let mut seen = std::collections::HashSet::new();
        profile
            .projects
            .retain(|project| seen.insert(project.clone()));
        Ok(profile)
    }

    pub fn load_for(root: &Path, id: &str) -> anyhow::Result<Self> {
        validate_id(id)?;
        let path = Path::new("workspace/agents").join(id).join("profile.json");
        let text = crate::script_fs::read_text(root, &path, 131_072)?;
        Self::parse_for(id, &text)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".into())
    }
}

/// A bot/project id is a single stable ASCII component, never a pathname.
pub fn validate_id(id: &str) -> anyhow::Result<()> {
    if id.is_empty()
        || id.len() > 80
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        || !id.as_bytes()[0].is_ascii_alphanumeric()
    {
        anyhow::bail!("invalid bot/project id {id:?}: use one ASCII name, not a path");
    }
    Ok(())
}

pub fn scan_checked(root: &Path) -> anyhow::Result<Vec<Profile>> {
    let mut profiles = Vec::new();
    for relative in crate::script_fs::child_dirs(root, Path::new("workspace/agents"))? {
        let id = relative
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow::anyhow!("bot directory is not UTF-8"))?;
        validate_id(id)?;
        let path = relative.join("profile.json");
        match crate::script_fs::read_text(root, &path, 131_072) {
            Ok(text) => profiles.push(Profile::parse_for(id, &text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    profiles.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(profiles)
}

/// Refresh a live view from disk. Invalid edits keep a labelled last-good
/// display, never silently masquerade as current valid metadata.
pub(crate) fn refresh(
    root: &Path,
    id: &str,
    current: &mut Profile,
    error: &mut Option<String>,
) -> bool {
    match Profile::load_for(root, id) {
        Ok(profile) => {
            let changed = *current != profile || error.is_some();
            *current = profile;
            *error = None;
            changed
        }
        Err(why) => {
            let why = why.to_string();
            let changed = error.as_ref() != Some(&why);
            *error = Some(why);
            changed
        }
    }
}

/// Merge only supported metadata fields, preserving unrecognized user-owned
/// fields already on disk. Return bytes for a compare-before-replace write.
pub fn merge_patch(
    id: &str,
    before: &str,
    patch: &serde_json::Value,
) -> anyhow::Result<(Profile, String)> {
    let mut value: serde_json::Value = serde_json::from_str(before)?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("profile must be an object"))?;
    let patch = patch
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("profile patch must be an object"))?;
    for (key, field) in patch {
        let valid = match key.as_str() {
            "name" | "title" | "description" | "group" => field.is_string(),
            "avatar" | "model" => field.is_string() || field.is_null(),
            "projects" => field
                .as_array()
                .is_some_and(|v| v.iter().all(serde_json::Value::is_string)),
            _ => anyhow::bail!("unknown or immutable profile field {key:?}"),
        };
        if !valid {
            anyhow::bail!("invalid type for profile field {key:?}");
        }
        object.insert(key.clone(), field.clone());
    }
    let after = serde_json::to_string_pretty(&value)?;
    let profile = Profile::parse_for(id, &after)?;
    Ok((profile, after))
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
    fn ids_are_directory_owned_and_paths_or_mismatches_are_rejected() {
        for id in ["../host", "/tmp", "a/b", "..", "", "a\\b"] {
            assert!(validate_id(id).is_err());
        }
        assert_eq!(
            Profile::parse_for("miku", r#"{"name":"Miku"}"#).unwrap().id,
            "miku"
        );
        assert!(Profile::parse_for("miku", r#"{"id":"other","name":"Miku"}"#).is_err());
        assert!(Profile::parse_for("miku", r#"{"name":"Miku","projects":["../qmd"]}"#).is_err());
    }

    #[test]
    fn direct_profile_edits_refresh_metadata_and_report_invalid_files() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("workspace/agents/miku");
        std::fs::create_dir_all(&home).unwrap();
        let file = home.join("profile.json");
        std::fs::write(&file, r#"{"name":"Miku"}"#).unwrap();
        let mut current = Profile::load_for(root.path(), "miku").unwrap();
        let mut error = None;
        assert!(!refresh(root.path(), "miku", &mut current, &mut error));
        std::fs::write(
            &file,
            r#"{"name":"Music","title":"Composer","avatar":"blue:blob"}"#,
        )
        .unwrap();
        assert!(refresh(root.path(), "miku", &mut current, &mut error));
        assert_eq!(current.name, "Music");
        assert_eq!(current.title, "Composer");
        std::fs::write(&file, "{broken").unwrap();
        assert!(refresh(root.path(), "miku", &mut current, &mut error));
        assert!(error.is_some());
        assert_eq!(current.name, "Music");
        std::fs::write(&file, r#"{"name":"Fixed"}"#).unwrap();
        assert!(refresh(root.path(), "miku", &mut current, &mut error));
        assert!(error.is_none());
    }

    #[test]
    fn profile_updates_preserve_user_fields_and_reject_silent_noops() {
        let before = r#"{"id":"miku","name":"Miku","custom":{"keep":true}}"#;
        let (profile, after) = merge_patch(
            "miku",
            before,
            &serde_json::json!({"name":"Miku Music","projects":["music"]}),
        )
        .unwrap();
        assert_eq!(profile.name, "Miku Music");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&after).unwrap()["custom"]["keep"],
            true
        );
        for patch in [
            serde_json::json!({"id":"other"}),
            serde_json::json!({"name":null}),
            serde_json::json!({"name":""}),
            serde_json::json!({"project":"qmd"}),
        ] {
            assert!(merge_patch("miku", before, &patch).is_err());
        }
    }

    #[test]
    fn profiles_refuse_file_and_parent_symlinks() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("workspace/agents/miku")).unwrap();
        std::fs::write(outside.path().join("profile.json"), r#"{"name":"Outside"}"#).unwrap();
        symlink(
            outside.path().join("profile.json"),
            dir.path().join("workspace/agents/miku/profile.json"),
        )
        .unwrap();
        assert!(scan_checked(dir.path()).is_err());
        std::fs::remove_file(dir.path().join("workspace/agents/miku/profile.json")).unwrap();
        std::fs::remove_dir(dir.path().join("workspace/agents/miku")).unwrap();
        symlink(outside.path(), dir.path().join("workspace/agents/miku")).unwrap();
        assert!(scan_checked(dir.path()).is_err());
    }

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
