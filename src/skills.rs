//! Workspace skill discovery and frontmatter validation.

use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SkillError {
    #[error("{path}: {message}")]
    Invalid { path: PathBuf, message: String },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub body: String,
}

pub fn discover(root: &Path) -> Result<Vec<Skill>, SkillError> {
    let mut files = Vec::new();
    collect(&root.join("skills"), &mut files)?;
    files.sort();
    let mut out = Vec::new();
    for path in files {
        let text = std::fs::read_to_string(&path)?;
        let (head, body) = parse(&text, &path)?;
        out.push(Skill {
            name: head.0,
            description: head.1,
            path,
            body,
        });
    }
    Ok(out)
}

/// Autocomplete / wrap listing. `source` is `"workspace"` or `"bot"`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SkillListing {
    pub name: String,
    pub description: String,
    pub source: String,
}

/// Shared `workspace/skills/` plus `bot_dir/skills/`. Bot-local last shadows
/// a workspace skill of the same `name`. A broken tree is skipped, not fatal.
pub fn catalog_for(workspace: &Path, bot_dir: &Path) -> Vec<Skill> {
    listings(workspace, bot_dir)
        .into_iter()
        .map(|(skill, _)| skill)
        .collect()
}

pub fn listings_for(workspace: &Path, bot_dir: &Path) -> Vec<SkillListing> {
    listings(workspace, bot_dir)
        .into_iter()
        .map(|(skill, source)| SkillListing {
            name: skill.name,
            description: skill.description,
            source: source.to_string(),
        })
        .collect()
}

fn listings(workspace: &Path, bot_dir: &Path) -> Vec<(Skill, &'static str)> {
    let mut map: BTreeMap<String, (Skill, &'static str)> = BTreeMap::new();
    for skill in discover_lenient(workspace) {
        map.insert(skill.name.clone(), (skill, "workspace"));
    }
    for skill in discover_lenient(bot_dir) {
        map.insert(skill.name.clone(), (skill, "bot"));
    }
    map.into_values().collect()
}

/// Like [`discover`], but a broken SKILL.md is skipped so one bad file cannot
/// hide the rest of the catalog from the system prompt or `/` menu.
pub fn discover_lenient(root: &Path) -> Vec<Skill> {
    let mut files = Vec::new();
    let _ = collect(&root.join("skills"), &mut files);
    files.sort();
    let mut out = Vec::new();
    for path in files {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok((head, body)) = parse(&text, &path) else {
            continue;
        };
        out.push(Skill {
            name: head.0,
            description: head.1,
            path,
            body,
        });
    }
    out
}

pub fn fingerprint(skill: &Skill) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    skill.name.hash(&mut hasher);
    skill.description.hash(&mut hasher);
    skill.body.hash(&mut hasher);
    hasher.finish()
}

/// Skills created or edited since `prev`. An empty `prev` is the first
/// snapshot: nothing is reported as new (the system prompt already lists them).
pub fn diff(
    catalog: &[Skill],
    prev: &BTreeMap<String, u64>,
) -> (Vec<Skill>, Vec<String>, BTreeMap<String, u64>) {
    let mut next = BTreeMap::new();
    let mut changed = Vec::new();
    for skill in catalog {
        let fp = fingerprint(skill);
        next.insert(skill.name.clone(), fp);
        if prev.is_empty() {
            continue;
        }
        match prev.get(&skill.name) {
            None => changed.push(skill.clone()),
            Some(old) if *old != fp => changed.push(skill.clone()),
            _ => {}
        }
    }
    let removed: Vec<String> = prev
        .keys()
        .filter(|name| !next.contains_key(*name))
        .cloned()
        .collect();
    (changed, removed, next)
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), SkillError> {
    if !dir.is_dir() {
        return Ok(());
    }
    for e in std::fs::read_dir(dir)? {
        let p = e?.path();
        if p.is_dir() {
            collect(&p, out)?;
        } else if p.file_name().is_some_and(|n| n == "SKILL.md") {
            out.push(p);
        }
    }
    Ok(())
}
fn parse(text: &str, path: &Path) -> Result<((String, String), String), SkillError> {
    let mut lines = text.lines();
    if lines.next() != Some("---") {
        return Err(SkillError::Invalid {
            path: path.into(),
            message: "missing frontmatter".into(),
        });
    }
    let mut name = None;
    let mut desc = None;
    let mut body = Vec::new();
    let mut closed = false;
    let mut folding_desc = false;
    let mut fold_join_nl = false;
    for line in lines {
        if !closed && line == "---" {
            closed = true;
            folding_desc = false;
            continue;
        }
        if closed {
            body.push(line);
        } else {
            if folding_desc {
                if line.starts_with(' ') || line.starts_with('\t') {
                    let bit = line.trim();
                    if bit.is_empty() {
                        continue;
                    }
                    let slot = desc.get_or_insert_with(String::new);
                    if !slot.is_empty() {
                        slot.push(if fold_join_nl { '\n' } else { ' ' });
                    }
                    slot.push_str(bit);
                    continue;
                }
                folding_desc = false;
            }
            if let Some(v) = line.strip_prefix("name:") {
                name = Some(unquote(v.trim()));
            } else if let Some(v) = line.strip_prefix("description:") {
                let rest = v.trim();
                if rest.is_empty()
                    || rest == ">"
                    || rest == ">-"
                    || rest == ">+"
                    || rest == "|"
                    || rest == "|-"
                    || rest == "|+"
                {
                    folding_desc = true;
                    fold_join_nl = rest.starts_with('|');
                    desc = Some(String::new());
                } else {
                    desc = Some(unquote(rest));
                }
            }
        }
    }
    let name = name
        .filter(|n| !n.is_empty())
        .ok_or_else(|| SkillError::Invalid {
            path: path.into(),
            message: "missing name".into(),
        })?;
    let description = desc
        .filter(|d| !d.is_empty())
        .ok_or_else(|| SkillError::Invalid {
            path: path.into(),
            message: "missing description".into(),
        })?;
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        return Err(SkillError::Invalid {
            path: path.into(),
            message: "name must be lowercase ascii".into(),
        });
    }
    Ok(((name, description), body.join("\n")))
}

fn unquote(s: &str) -> String {
    if (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
        || (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
    {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovers_and_validates_skills() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("skills/review/SKILL.md");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(
            &p,
            "---\nname: review\ndescription: review changes\n---\nRead the diff.",
        )
        .unwrap();
        let found = discover(d.path()).unwrap();
        assert_eq!(found[0].name, "review");
        assert!(found[0].body.contains("Read"));
    }
    #[test]
    fn rejects_missing_frontmatter() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("skills/x/SKILL.md");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "# nope").unwrap();
        assert!(discover(d.path()).is_err());
    }

    fn write_skill(root: &Path, name: &str, desc: &str, body: &str) {
        let p = root.join("skills").join(name).join("SKILL.md");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(
            p,
            format!("---\nname: {name}\ndescription: {desc}\n---\n{body}"),
        )
        .unwrap();
    }

    #[test]
    fn folded_yaml_description_is_joined() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("skills/create-skill/SKILL.md");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(
            &p,
            "---\nname: create-skill\ndescription: >\n  Create a new skill.\n  Use when the user runs /create-skill.\n---\nBody.\n",
        )
        .unwrap();
        let found = discover(d.path()).unwrap();
        assert_eq!(found[0].name, "create-skill");
        assert!(
            found[0].description.contains("Create a new skill."),
            "{}",
            found[0].description
        );
        assert!(found[0].description.contains("/create-skill"));
        assert!(!found[0].description.starts_with('>'));
    }

    #[test]
    fn a_broken_skill_does_not_hide_the_catalog() {
        let d = tempfile::tempdir().unwrap();
        write_skill(d.path(), "ok", "fine", "body");
        let bad = d.path().join("skills/bad/SKILL.md");
        std::fs::create_dir_all(bad.parent().unwrap()).unwrap();
        std::fs::write(&bad, "# nope").unwrap();
        assert!(discover(d.path()).is_err());
        let listed = discover_lenient(d.path());
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "ok");
    }

    #[test]
    fn catalog_unions_and_bot_skills_shadow_workspace() {
        let d = tempfile::tempdir().unwrap();
        let ws = d.path();
        let bot = ws.join("agents/x");
        write_skill(ws, "review", "workspace review", "WS");
        write_skill(ws, "lua-plugins", "lua", "L");
        write_skill(&bot, "review", "bot review", "BOT");
        let cat = catalog_for(ws, &bot);
        assert_eq!(cat.len(), 2);
        let review = cat.iter().find(|s| s.name == "review").unwrap();
        assert!(review.body.contains("BOT"));
        let listed = listings_for(ws, &bot);
        assert_eq!(
            listed.iter().find(|s| s.name == "review").unwrap().source,
            "bot"
        );
        assert_eq!(
            listed
                .iter()
                .find(|s| s.name == "lua-plugins")
                .unwrap()
                .source,
            "workspace"
        );
    }
}
