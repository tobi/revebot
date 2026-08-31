//! Filesystem-backed memory. Explicit scope, exact facts, bounded projection.
//! Markdown is authoritative; ordinary user prose is preserved byte-for-byte.

use super::files::{Change, MAX_FILE, read_optional};
use super::profile::{Profile, validate_id};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

const OPEN: &str = "<!-- reve-memory ";
const CLOSE: &str = "\n<!-- /reve-memory -->";
const MAX_FACT: usize = 4096;
const MAX_FILES: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Profile,
    Log,
    Note,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Agent,
    User,
    Project,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Write,
    Forget,
}

#[derive(Clone, Debug)]
pub struct Request {
    pub action: Action,
    pub fact: String,
    pub tier: Tier,
    pub scope: Scope,
    pub project: Option<String>,
}

impl Request {
    pub fn parse(args: &Map<String, Value>) -> anyhow::Result<Self> {
        for key in args.keys() {
            if !["target", "action", "fact", "tier", "scope", "project"].contains(&key.as_str()) {
                anyhow::bail!("unknown memory field {key:?}");
            }
        }
        let action: Action = serde_json::from_value(
            args.get("action")
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("memory action required"))?,
        )?;
        let fact = args
            .get("fact")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("memory fact required"))?
            .to_string();
        if fact.trim().is_empty()
            || fact.len() > MAX_FACT
            || fact.contains(OPEN)
            || fact.contains("<!-- /reve-memory")
        {
            anyhow::bail!(
                "fact must be nonblank, at most 4096 bytes, and contain no memory delimiters"
            );
        }
        let tier = serde_json::from_value(
            args.get("tier")
                .cloned()
                .unwrap_or(Value::String("log".into())),
        )?;
        let scope = serde_json::from_value(
            args.get("scope")
                .cloned()
                .unwrap_or(Value::String("agent".into())),
        )?;
        let project = args
            .get("project")
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| anyhow::anyhow!("project must be a name"))
            })
            .transpose()?;
        if action == Action::Forget && args.contains_key("tier") {
            anyhow::bail!("forget uses exact fact text across tiers; omit tier");
        }
        if scope == Scope::Project {
            validate_id(
                project
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("project required for project scope"))?,
            )?;
        } else if project.is_some() {
            anyhow::bail!("project is only valid for project scope");
        }
        Ok(Self {
            action,
            fact,
            tier,
            scope,
            project,
        })
    }

    fn directory(&self, profile: &Profile) -> anyhow::Result<PathBuf> {
        validate_id(&profile.id)?;
        Ok(match self.scope {
            Scope::Agent => Path::new("workspace/agents")
                .join(&profile.id)
                .join("memory"),
            Scope::User => PathBuf::from("workspace/memory/user"),
            Scope::Project => {
                let project = self
                    .project
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("project required"))?;
                validate_id(project)?;
                if !profile.projects.iter().any(|p| p == project) {
                    anyhow::bail!(
                        "project {project:?} is not attached to this agent; explicitly add it to profile.projects first"
                    );
                }
                Path::new("workspace/projects")
                    .join(project)
                    .join("memory/agents")
                    .join(&profile.id)
            }
        })
    }
}

#[derive(Serialize, Deserialize)]
struct Metadata {
    at: DateTime<Utc>,
    tier: Tier,
}
struct Block {
    start: usize,
    end: usize,
    meta: Metadata,
    fact: String,
}
struct Document {
    relative: PathBuf,
    text: String,
    blocks: Vec<Block>,
}

fn blocks(text: &str) -> anyhow::Result<Vec<Block>> {
    let mut out = Vec::new();
    let mut offset = 0;
    while let Some(start) = text[offset..].find(OPEN).map(|i| i + offset) {
        let metadata_start = start + OPEN.len();
        let metadata_end = text[metadata_start..]
            .find(" -->\n")
            .map(|i| i + metadata_start)
            .ok_or_else(|| anyhow::anyhow!("malformed memory header"))?;
        let meta = serde_json::from_str(&text[metadata_start..metadata_end])?;
        let fact_start = metadata_end + " -->\n".len();
        let fact_end = text[fact_start..]
            .find(CLOSE)
            .map(|i| i + fact_start)
            .ok_or_else(|| anyhow::anyhow!("unterminated memory block"))?;
        let end = fact_end + CLOSE.len();
        out.push(Block {
            start,
            end,
            meta,
            fact: text[fact_start..fact_end].to_string(),
        });
        offset = end;
    }
    Ok(out)
}

fn documents(root: &Path, directory: &Path) -> anyhow::Result<Vec<Document>> {
    let mut paths = vec![directory.join("profile.md")];
    for subdir in ["log", "notes"] {
        paths.extend(
            crate::script_fs::files(root, &directory.join(subdir))?
                .into_iter()
                .filter(|p| p.extension().is_some_and(|e| e == "md")),
        );
    }
    if paths.len() > MAX_FILES {
        anyhow::bail!(
            "too many memory files; archive old files outside profile.md/log/notes before updating"
        );
    }
    let mut docs = Vec::new();
    for relative in paths {
        if let Some(text) = read_optional(root, &relative)? {
            let blocks = blocks(&text)?;
            docs.push(Document {
                relative,
                text,
                blocks,
            });
        }
    }
    Ok(docs)
}

pub struct Planned {
    pub change: Option<Change>,
    pub result: String,
}

pub fn plan(
    root: &Path,
    profile: &Profile,
    request: &Request,
    now: DateTime<Utc>,
) -> anyhow::Result<Planned> {
    let directory = request.directory(profile)?;
    let docs = documents(root, &directory)?;
    let matches: Vec<&Document> = docs
        .iter()
        .filter(|d| d.blocks.iter().any(|b| b.fact == request.fact))
        .collect();
    if request.action == Action::Forget {
        if matches.len() > 1 {
            anyhow::bail!(
                "fact occurs in multiple manually edited files; remove duplicates explicitly before forgetting"
            );
        }
        let Some(doc) = matches.first() else {
            return Ok(Planned {
                change: None,
                result: "Fact not found; no files changed.".into(),
            });
        };
        let mut next = doc.text.clone();
        for block in doc.blocks.iter().rev().filter(|b| b.fact == request.fact) {
            next.replace_range(block.start..block.end, "");
        }
        return Ok(Planned {
            change: Some(Change {
                relative: doc.relative.clone(),
                before: Some(doc.text.clone()),
                after: next,
            }),
            result: "Forgot the exact recorded fact.".into(),
        });
    }
    if !matches.is_empty() {
        return Ok(Planned {
            change: None,
            result: "Already remembered in this scope; no duplicate written.".into(),
        });
    }
    let relative = directory.join(match request.tier {
        Tier::Profile => PathBuf::from("profile.md"),
        Tier::Log => PathBuf::from(format!("log/{}.md", now.format("%Y-%m"))),
        Tier::Note => PathBuf::from(format!("notes/{}.md", now.format("%Y-%m"))),
    });
    let before = read_optional(root, &relative)?;
    let mut after = before.clone().unwrap_or_default();
    if !after.is_empty() && !after.ends_with('\n') {
        after.push('\n');
    }
    let metadata = serde_json::to_string(&Metadata {
        at: now,
        tier: request.tier,
    })?;
    after.push_str(&format!(
        "\n{OPEN}{metadata} -->\n{}{CLOSE}\n",
        request.fact
    ));
    if after.len() as u64 > MAX_FILE {
        anyhow::bail!("memory file is full; archive older material rather than overwriting it");
    }
    Ok(Planned {
        change: Some(Change {
            relative,
            before,
            after,
        }),
        result: format!("Remembered ({:?}, {:?}).", request.scope, request.tier),
    })
}

/// UTF-8-safe, explicit truncation. Limits count bytes to bound provider input.
pub fn bounded(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.into();
    }
    const NOTICE: &str = "\n[truncated; more is on disk]";
    if budget <= NOTICE.len() {
        return String::new();
    }
    let mut end = budget - NOTICE.len();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{NOTICE}", &text[..end])
}

fn projection(
    root: &Path,
    directory: &Path,
    now: DateTime<Utc>,
    budget: usize,
) -> anyhow::Result<String> {
    let docs = documents(root, directory)?;
    let mut profile = Vec::new();
    let mut recent = Vec::new();
    let mut omitted = 0;
    for doc in &docs {
        let mut manual = doc.text.clone();
        for block in doc.blocks.iter().rev() {
            manual.replace_range(block.start..block.end, "");
        }
        // Only profile.md has timeless manually maintained prose. Unstructured
        // log/notes remain readable on demand, without inventing timestamps.
        if doc.relative.file_name().is_some_and(|n| n == "profile.md") && !manual.trim().is_empty()
        {
            profile.push(manual.trim().to_string());
        }
        for block in &doc.blocks {
            let age = now.signed_duration_since(block.meta.at);
            match block.meta.tier {
                Tier::Profile => profile.push(block.fact.clone()),
                Tier::Log if age <= Duration::days(30) => {
                    recent.push((block.meta.at, block.fact.clone()))
                }
                Tier::Note if age <= Duration::days(2) => {
                    recent.push((block.meta.at, block.fact.clone()))
                }
                _ => omitted += 1,
            }
        }
    }
    recent.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    let mut text = format!("Memory source: /{}\n", directory.display());
    if !profile.is_empty() {
        text.push_str(&format!(
            "Enduring facts:\n{}\n",
            bounded(&profile.join("\n"), budget / 2)
        ));
    }
    if !recent.is_empty() {
        text.push_str("Recently (newest first):\n");
    }
    for (at, fact) in recent {
        text.push_str(&format!("- [{}] {fact}\n", at.format("%Y-%m-%d")));
    }
    if omitted > 0 {
        text.push_str(&format!(
            "{omitted} older facts remain on disk. Read the files when needed.\n"
        ));
    }
    if docs.is_empty() {
        return Ok(String::new());
    }
    Ok(bounded(&text, budget))
}

/// Private first, intentionally shared user facts second, explicitly selected
/// projects last. Never enumerate sibling private memory or all global projects.
pub fn prompt(root: &Path, profile: &Profile, now: DateTime<Utc>) -> String {
    let mut sections = Vec::new();
    let mut add = |label: &str, path: PathBuf, budget| match projection(root, &path, now, budget) {
        Ok(text) if !text.is_empty() => sections.push(format!("## {label}\n{text}")),
        Ok(_) => {}
        Err(error) => sections.push(format!(
            "## {label}\nMemory unavailable: {error}. No substitute memory was loaded."
        )),
    };
    if validate_id(&profile.id).is_err() {
        return "Invalid agent id; memory was not loaded.".into();
    }
    add(
        "Your memory",
        Path::new("workspace/agents")
            .join(&profile.id)
            .join("memory"),
        8000,
    );
    add(
        "Explicitly shared user memory",
        PathBuf::from("workspace/memory/user"),
        4000,
    );
    for project in profile.projects.iter().take(4) {
        if validate_id(project).is_err() {
            continue;
        }
        let base = Path::new("workspace/projects")
            .join(project)
            .join("memory/agents");
        match crate::script_fs::child_dirs(root, &base) {
            Ok(shards) => {
                for shard in shards.into_iter().take(50) {
                    add(&format!("Project {project} (shared shard)"), shard, 2000);
                }
            }
            Err(error) => add(&format!("Project {project}: {error}"), base, 2000),
        }
    }
    bounded(&sections.join("\n\n"), 16000)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn profile(id: &str) -> Profile {
        Profile::parse_for(id, &format!(r#"{{"name":"{id}"}}"#)).unwrap()
    }
    fn request(
        action: &str,
        fact: &str,
        tier: Option<&str>,
        scope: Option<&str>,
        project: Option<&str>,
    ) -> Request {
        let mut args = serde_json::json!({"target":"memory","action":action,"fact":fact})
            .as_object()
            .unwrap()
            .clone();
        if let Some(t) = tier {
            args.insert("tier".into(), t.into());
        }
        if let Some(s) = scope {
            args.insert("scope".into(), s.into());
        }
        if let Some(p) = project {
            args.insert("project".into(), p.into());
        }
        Request::parse(&args).unwrap()
    }
    fn apply_test(root: &Path, planned: Planned) {
        // Exercise pure planning with fixture IO, never a host shell fallback.
        if let Some(change) = planned.change {
            let path = root.join(change.relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            assert_eq!(std::fs::read_to_string(&path).ok(), change.before);
            std::fs::write(path, change.after).unwrap();
        }
    }
    #[test]
    fn private_memory_is_deduped_exactly_forgotten_and_preserves_manual_prose() {
        let dir = tempfile::tempdir().unwrap();
        let p = profile("miku");
        let now = Utc::now();
        let path = dir.path().join("workspace/agents/miku/memory/profile.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "User-edited heading\n").unwrap();
        let write = request(
            "write",
            "QMD belongs to someone else",
            Some("profile"),
            None,
            None,
        );
        apply_test(dir.path(), plan(dir.path(), &p, &write, now).unwrap());
        assert!(plan(dir.path(), &p, &write, now).unwrap().change.is_none());
        assert!(prompt(dir.path(), &p, now).contains(&write.fact));
        assert!(!prompt(dir.path(), &profile("monad"), now).contains(&write.fact));
        let forget = request("forget", &write.fact, None, None, None);
        apply_test(dir.path(), plan(dir.path(), &p, &forget, now).unwrap());
        assert!(!prompt(dir.path(), &p, now).contains(&write.fact));
        assert!(
            std::fs::read_to_string(path)
                .unwrap()
                .starts_with("User-edited heading\n")
        );
    }
    #[test]
    fn scopes_are_explicit_and_project_shards_require_membership() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = profile("qmd-dev");
        let now = Utc::now();
        let qmd = request(
            "write",
            "QMD_EMBED_RULE",
            None,
            Some("project"),
            Some("qmd"),
        );
        assert!(plan(dir.path(), &p, &qmd, now).is_err());
        p.projects.push("qmd".into());
        apply_test(dir.path(), plan(dir.path(), &p, &qmd, now).unwrap());
        assert!(prompt(dir.path(), &p, now).contains("QMD_EMBED_RULE"));
        assert!(!prompt(dir.path(), &profile("miku"), now).contains("QMD_EMBED_RULE"));
        let shared = request(
            "write",
            "USER_TIMEZONE",
            Some("profile"),
            Some("user"),
            None,
        );
        apply_test(dir.path(), plan(dir.path(), &p, &shared, now).unwrap());
        assert!(prompt(dir.path(), &profile("miku"), now).contains("USER_TIMEZONE"));
    }
    #[test]
    fn notes_expire_before_logs_and_old_facts_stay_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let p = profile("miku");
        let now = Utc::now();
        for (fact, tier, age) in [
            ("ENDURING", "profile", 365),
            ("RECENT_LOG", "log", 3),
            ("OLD_LOG", "log", 40),
            ("EXPIRED_NOTE", "note", 3),
            ("FRESH_NOTE", "note", 0),
        ] {
            apply_test(
                dir.path(),
                plan(
                    dir.path(),
                    &p,
                    &request("write", fact, Some(tier), None, None),
                    now - Duration::days(age),
                )
                .unwrap(),
            );
        }
        let text = prompt(dir.path(), &p, now);
        for fact in ["ENDURING", "RECENT_LOG", "FRESH_NOTE"] {
            assert!(text.contains(fact));
        }
        for fact in ["OLD_LOG", "EXPIRED_NOTE"] {
            assert!(!text.contains(fact));
        }
        assert!(text.contains("2 older facts"));
        assert!(
            plan(
                dir.path(),
                &p,
                &request("write", "OLD_LOG", None, None, None),
                now
            )
            .unwrap()
            .change
            .is_none()
        );
    }
    #[test]
    fn facts_are_exact_and_projection_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let p = profile("miku");
        let now = Utc::now();
        apply_test(
            dir.path(),
            plan(
                dir.path(),
                &p,
                &request("write", "  exact\ntext  ", None, None, None),
                now,
            )
            .unwrap(),
        );
        assert!(
            plan(
                dir.path(),
                &p,
                &request("forget", "exact\ntext", None, None, None),
                now
            )
            .unwrap()
            .change
            .is_none()
        );
        for n in 0..12 {
            apply_test(
                dir.path(),
                plan(
                    dir.path(),
                    &p,
                    &request(
                        "write",
                        &format!("{n} {}", "日".repeat(1000)),
                        None,
                        None,
                        None,
                    ),
                    now,
                )
                .unwrap(),
            );
        }
        let text = prompt(dir.path(), &p, now);
        assert!(text.len() <= 16000);
        assert!(text.contains("truncated"));
    }
}
