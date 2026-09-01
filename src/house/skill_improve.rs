//! Agent-managed skill improvement: create, patch, view.
//!
//! Ported from Hermes `skill_manage` / `skill_view` / `skills_list`. Writes are
//! planned as guest `Change`s. Delete archives (never hard-deletes).

use std::path::{Component, Path, PathBuf};

use serde::Deserialize;
use serde_json::{Map, Value};

use super::files::{Change, MAX_FILE};
use crate::curator::BUNDLED_SKILLS;
use crate::skills::{self, Skill};

const MAX_NAME: usize = 64;
const MAX_DESC: usize = 1024;
const ALLOWED_SUBDIRS: &[&str] = &["references", "templates", "scripts", "assets"];
const DEFAULT_NUDGE_INTERVAL: u32 = 15;

const NUDGE: &str = "\
[SAND_HIDDEN_PROMPT][skill-improve]
If this session produced a reusable class-level workflow, a user correction, \
or a missing pitfall, record it with skill_manage now. Patch an existing \
umbrella first (skill_view, then patch). Create only at class level — never \
a one-off task narrative, PR number, or 'fix-X-today' name. Bundled skills \
(create-skill, vm, routines, plugins, memory, secrets, browser, computer, \
curator, learn) are off-limits. Do not capture environment-dependent failures \
or 'tool X is broken'. If nothing is worth saving, ignore this.
[/SAND_HIDDEN_PROMPT]
";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Create,
    Patch,
    Edit,
    Delete,
    WriteFile,
    RemoveFile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    House,
    Bot,
}

#[derive(Debug, Clone)]
pub struct Request {
    pub action: Action,
    pub name: String,
    pub content: Option<String>,
    pub old_string: Option<String>,
    pub new_string: Option<String>,
    pub replace_all: bool,
    pub file_path: Option<String>,
    pub file_content: Option<String>,
    pub scope: Scope,
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub writes: Vec<Change>,
    pub remove: Vec<PathBuf>,
    pub archive: Option<String>,
    pub result: String,
    pub created: bool,
    pub patched: bool,
}

impl Request {
    pub fn parse(args: &Map<String, Value>) -> Result<Self, String> {
        for key in args.keys() {
            if ![
                "action",
                "name",
                "content",
                "old_string",
                "new_string",
                "replace_all",
                "file_path",
                "file_content",
                "scope",
            ]
            .contains(&key.as_str())
            {
                return Err(format!("unknown skill_manage field {key:?}"));
            }
        }
        let action: Action = serde_json::from_value(
            args.get("action")
                .cloned()
                .ok_or_else(|| "skill_manage action required".to_string())?,
        )
        .map_err(|e| format!("invalid action: {e}"))?;
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| "skill_manage name required".to_string())?
            .trim()
            .to_string();
        validate_name(&name)?;
        let scope = match args.get("scope") {
            None => Scope::House,
            Some(v) => {
                serde_json::from_value(v.clone()).map_err(|e| format!("invalid scope: {e}"))?
            }
        };
        Ok(Self {
            action,
            name,
            content: args
                .get("content")
                .and_then(Value::as_str)
                .map(str::to_string),
            old_string: args
                .get("old_string")
                .and_then(Value::as_str)
                .map(str::to_string),
            new_string: args
                .get("new_string")
                .and_then(Value::as_str)
                .map(str::to_string),
            replace_all: args
                .get("replace_all")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            file_path: args
                .get("file_path")
                .and_then(Value::as_str)
                .map(str::to_string),
            file_content: args
                .get("file_content")
                .and_then(Value::as_str)
                .map(str::to_string),
            scope,
        })
    }
}

pub fn nudge_interval(root: &Path) -> u32 {
    let Ok(text) = std::fs::read_to_string(root.join("config.yml")) else {
        return DEFAULT_NUDGE_INTERVAL;
    };
    let parsed: FileConfig = serde_yaml::from_str(&text).unwrap_or_default();
    parsed
        .skills
        .creation_nudge_interval
        .unwrap_or(DEFAULT_NUDGE_INTERVAL)
}

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    #[serde(default)]
    skills: FileSkills,
}

#[derive(Debug, Default, Deserialize)]
struct FileSkills {
    creation_nudge_interval: Option<u32>,
}

pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > MAX_NAME {
        return Err(format!(
            "skill name must be 1–{MAX_NAME} lowercase ascii letters, digits, '-' or '_'"
        ));
    }
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err("skill name is required".into());
    };
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return Err(format!("invalid skill name '{name}'"));
    }
    if !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_') {
        return Err(format!(
            "invalid skill name '{name}'. Use lowercase letters, numbers, hyphens, underscores."
        ));
    }
    Ok(())
}

fn is_bundled(name: &str) -> bool {
    BUNDLED_SKILLS.contains(&name) || name == "learn"
}

pub fn plan(root: &Path, bot: &str, request: &Request) -> Result<Plan, String> {
    match request.action {
        Action::Create => plan_create(root, bot, request),
        Action::Edit => plan_edit(root, bot, request),
        Action::Patch => plan_patch(root, bot, request),
        Action::WriteFile => plan_write_file(root, bot, request),
        Action::RemoveFile => plan_remove_file(root, bot, request),
        Action::Delete => plan_delete(root, bot, request),
    }
}

fn plan_create(root: &Path, bot: &str, request: &Request) -> Result<Plan, String> {
    if is_bundled(&request.name) {
        return Err(format!(
            "skill '{}' is bundled; the improvement loop does not overwrite built-ins",
            request.name
        ));
    }
    if find_live(root, bot, &request.name).is_some() {
        return Err(format!(
            "a skill named '{}' already exists. Patch it, or pick another name.",
            request.name
        ));
    }
    let content = request
        .content
        .as_deref()
        .ok_or("content is required for create (full SKILL.md)")?;
    validate_skill_md(content, &request.name)?;
    let relative = skill_md_relative(bot, request.scope, &request.name);
    Ok(Plan {
        writes: vec![Change {
            relative,
            before: None,
            after: content.to_string(),
        }],
        remove: Vec::new(),
        archive: None,
        result: format!(
            "Skill '{}' created. Add support files with skill_manage action=write_file (references/, templates/, scripts/).",
            request.name
        ),
        created: true,
        patched: false,
    })
}

fn plan_edit(root: &Path, bot: &str, request: &Request) -> Result<Plan, String> {
    let content = request
        .content
        .as_deref()
        .ok_or("content is required for edit (full SKILL.md)")?;
    validate_skill_md(content, &request.name)?;
    replace_skill_md(root, bot, request, content, "replaced")
}

fn plan_patch(root: &Path, bot: &str, request: &Request) -> Result<Plan, String> {
    if let Some(content) = request.content.as_deref() {
        if request.old_string.is_some() || request.new_string.is_some() {
            return Err(
                "pass EITHER content (full rewrite) OR old_string/new_string, not both".into(),
            );
        }
        validate_skill_md(content, &request.name)?;
        return replace_skill_md(root, bot, request, content, "rewritten");
    }
    let old = request
        .old_string
        .as_deref()
        .ok_or("old_string is required for patch")?;
    let new = request
        .new_string
        .as_deref()
        .ok_or("new_string is required for patch")?;
    if old.is_empty() {
        return Err("old_string must not be empty".into());
    }
    let (skill, _) = find_live(root, bot, &request.name)
        .ok_or_else(|| format!("skill '{}' not found", request.name))?;
    refuse_bundled_delete(&request.name, "patch")?;
    let relative = match request.file_path.as_deref() {
        Some(path) => support_relative(root, &skill, path)?,
        None => workspace_relative(root, &skill.path)?,
    };
    let before = read_workspace(root, &relative)?;
    let after = apply_patch(&before, old, new, request.replace_all)?;
    if request.file_path.is_none() {
        validate_skill_md(&after, &request.name)?;
    }
    check_size(&after, "patched content")?;
    Ok(Plan {
        writes: vec![Change {
            relative,
            before: Some(before),
            after,
        }],
        remove: Vec::new(),
        archive: None,
        result: format!("Skill '{}' patched.", request.name),
        created: false,
        patched: true,
    })
}

fn plan_write_file(root: &Path, bot: &str, request: &Request) -> Result<Plan, String> {
    let path = request
        .file_path
        .as_deref()
        .ok_or("file_path is required for write_file (e.g. references/api.md)")?;
    let content = request
        .file_content
        .as_deref()
        .ok_or("file_content is required for write_file")?;
    check_size(content, path)?;
    let (skill, _) = find_live(root, bot, &request.name)
        .ok_or_else(|| format!("skill '{}' not found", request.name))?;
    refuse_bundled_delete(&request.name, "write_file")?;
    let relative = support_relative(root, &skill, path)?;
    let before = read_workspace(root, &relative).ok();
    Ok(Plan {
        writes: vec![Change {
            relative,
            before,
            after: content.to_string(),
        }],
        remove: Vec::new(),
        archive: None,
        result: format!("Wrote {path} on skill '{}'.", request.name),
        created: false,
        patched: true,
    })
}

fn plan_remove_file(root: &Path, bot: &str, request: &Request) -> Result<Plan, String> {
    let path = request
        .file_path
        .as_deref()
        .ok_or("file_path is required for remove_file")?;
    let (skill, _) = find_live(root, bot, &request.name)
        .ok_or_else(|| format!("skill '{}' not found", request.name))?;
    refuse_bundled_delete(&request.name, "remove_file")?;
    let relative = support_relative(root, &skill, path)?;
    if !root.join(&relative).is_file() {
        return Err(format!("{path} is not a file on skill '{}'", request.name));
    }
    Ok(Plan {
        writes: Vec::new(),
        remove: vec![relative],
        archive: None,
        result: format!("Removed {path} from skill '{}'.", request.name),
        created: false,
        patched: true,
    })
}

fn plan_delete(root: &Path, bot: &str, request: &Request) -> Result<Plan, String> {
    refuse_bundled_delete(&request.name, "delete")?;
    find_live(root, bot, &request.name)
        .ok_or_else(|| format!("skill '{}' not found", request.name))?;
    Ok(Plan {
        writes: Vec::new(),
        remove: Vec::new(),
        archive: Some(request.name.clone()),
        result: format!(
            "Skill '{}' archived (never deleted). Restore with `revebot curator restore {}`.",
            request.name, request.name
        ),
        created: false,
        patched: false,
    })
}

fn refuse_bundled_delete(name: &str, action: &str) -> Result<(), String> {
    if is_bundled(name) {
        return Err(format!("refusing {action} on bundled skill '{name}'"));
    }
    Ok(())
}

fn replace_skill_md(
    root: &Path,
    bot: &str,
    request: &Request,
    content: &str,
    verb: &str,
) -> Result<Plan, String> {
    let (skill, _) = find_live(root, bot, &request.name)
        .ok_or_else(|| format!("skill '{}' not found", request.name))?;
    refuse_bundled_delete(&request.name, "edit")?;
    let relative = workspace_relative(root, &skill.path)?;
    let before = read_workspace(root, &relative)?;
    Ok(Plan {
        writes: vec![Change {
            relative,
            before: Some(before),
            after: content.to_string(),
        }],
        remove: Vec::new(),
        archive: None,
        result: format!("Skill '{}' {verb}.", request.name),
        created: false,
        patched: true,
    })
}

fn find_live(root: &Path, bot: &str, name: &str) -> Option<(Skill, &'static str)> {
    let catalog = skills::catalog_for(
        &root.join("workspace"),
        &root.join("workspace/agents").join(bot),
    );
    catalog.into_iter().find(|s| s.name == name).map(|s| {
        let source = if s.path.starts_with(root.join("workspace/agents").join(bot)) {
            "bot"
        } else {
            "workspace"
        };
        (s, source)
    })
}

fn skill_md_relative(bot: &str, scope: Scope, name: &str) -> PathBuf {
    match scope {
        Scope::House => PathBuf::from(format!("workspace/skills/{name}/SKILL.md")),
        Scope::Bot => PathBuf::from(format!("workspace/agents/{bot}/skills/{name}/SKILL.md")),
    }
}

fn workspace_relative(root: &Path, path: &Path) -> Result<PathBuf, String> {
    path.strip_prefix(root)
        .map_err(|_| "skill path is outside the house".to_string())
        .map(Path::to_path_buf)
}

fn support_relative(root: &Path, skill: &Skill, file_path: &str) -> Result<PathBuf, String> {
    let path = Path::new(file_path);
    if path.is_absolute()
        || path.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err("file_path must be a relative support file (references/…, templates/…, scripts/…, assets/…)".into());
    }
    let mut parts = path.components();
    let Some(Component::Normal(first)) = parts.next() else {
        return Err("file_path is empty".into());
    };
    let first = first.to_string_lossy();
    if !ALLOWED_SUBDIRS.contains(&first.as_ref()) {
        return Err(format!(
            "file_path must start with one of: {}",
            ALLOWED_SUBDIRS.join(", ")
        ));
    }
    if parts.next().is_none() {
        return Err("file_path needs a file name under that directory".into());
    }
    let dir = skill.path.parent().ok_or("skill has no directory")?;
    to_workspace_relative(root, dir.join(file_path))
}

fn read_workspace(root: &Path, relative: &Path) -> Result<String, String> {
    let abs = if relative.is_absolute() {
        relative.to_path_buf()
    } else {
        root.join(relative)
    };
    let rel = abs
        .strip_prefix(root)
        .map_err(|_| "path is outside the house".to_string())?;
    crate::script_fs::read_text(root, rel, MAX_FILE).map_err(|e| e.to_string())
}

fn apply_patch(before: &str, old: &str, new: &str, replace_all: bool) -> Result<String, String> {
    let count = before.matches(old).count();
    if count == 0 {
        return Err("old_string not found in the target file".into());
    }
    if count > 1 && !replace_all {
        return Err(format!(
            "old_string matched {count} times; pass replace_all=true or a unique snippet"
        ));
    }
    Ok(before.replace(old, new))
}

fn check_size(content: &str, label: &str) -> Result<(), String> {
    if content.len() as u64 > MAX_FILE {
        return Err(format!(
            "{label} exceeds {MAX_FILE} bytes; split into references/ files"
        ));
    }
    Ok(())
}

fn validate_skill_md(content: &str, expect_name: &str) -> Result<(), String> {
    check_size(content, "SKILL.md")?;
    let text = content.trim_start_matches('\u{feff}');
    let Some(rest) = text.strip_prefix("---") else {
        return Err("SKILL.md must start with YAML frontmatter (---)".into());
    };
    let Some((yaml, after)) = rest.split_once("\n---") else {
        return Err("SKILL.md frontmatter is not closed".into());
    };
    let body = after.trim_start_matches('-').trim();
    if body.is_empty() {
        return Err("SKILL.md must have a body after frontmatter".into());
    }
    let mut name = None;
    let mut desc = None;
    for line in yaml.lines() {
        if let Some(v) = line.strip_prefix("name:") {
            name = Some(v.trim().trim_matches('"').trim_matches('\'').to_string());
        } else if let Some(v) = line.strip_prefix("description:") {
            let rest = v.trim();
            if !rest.is_empty()
                && rest != ">"
                && rest != ">|"
                && !rest.starts_with('>')
                && !rest.starts_with('|')
            {
                desc = Some(rest.trim_matches('"').trim_matches('\'').to_string());
            } else {
                desc = Some(String::new());
            }
        } else if desc.as_deref() == Some("") && (line.starts_with(' ') || line.starts_with('\t')) {
            let bit = line.trim();
            if !bit.is_empty() {
                desc = Some(bit.to_string());
            }
        }
    }
    let name = name
        .filter(|n| !n.is_empty())
        .ok_or("frontmatter must include name:")?;
    if name != expect_name {
        return Err(format!(
            "frontmatter name '{name}' does not match skill_manage name '{expect_name}'"
        ));
    }
    validate_name(&name)?;
    let desc = desc
        .filter(|d| !d.is_empty())
        .ok_or("frontmatter must include description:")?;
    if desc.len() > MAX_DESC {
        return Err(format!("description exceeds {MAX_DESC} characters"));
    }
    Ok(())
}

/// Host-absolute support path → workspace-relative for Change / rm.
pub fn to_workspace_relative(root: &Path, host: PathBuf) -> Result<PathBuf, String> {
    if host.starts_with("workspace") {
        return Ok(host);
    }
    host.strip_prefix(root)
        .map(Path::to_path_buf)
        .map_err(|_| "support path is outside the house".to_string())
}

pub fn list_text(root: &Path, bot: &str) -> String {
    let listed = skills::listings_for(
        &root.join("workspace"),
        &root.join("workspace/agents").join(bot),
    );
    if listed.is_empty() {
        return "no skills".into();
    }
    let mut lines = Vec::new();
    for skill in listed {
        lines.push(format!(
            "- {} ({}) — {}",
            skill.name,
            skill.source,
            skill.description.replace('\n', " ")
        ));
    }
    lines.join("\n")
}

pub fn view_text(
    root: &Path,
    bot: &str,
    name: &str,
    file_path: Option<&str>,
) -> Result<String, String> {
    validate_name(name)?;
    let (skill, source) =
        find_live(root, bot, name).ok_or_else(|| format!("skill '{name}' not found"))?;
    match file_path {
        None => Ok(format!(
            "# /{name} ({source})\npath: {}\n\n{}",
            skill.path.display(),
            std::fs::read_to_string(&skill.path).map_err(|e| e.to_string())?
        )),
        Some(path) => {
            let relative = support_relative(root, &skill, path)?;
            let text = std::fs::read_to_string(root.join(&relative))
                .map_err(|e| format!("cannot read {path}: {e}"))?;
            Ok(text)
        }
    }
}

pub fn with_nudge(wrapped: &str) -> String {
    const TAG: &str = "</user_query>";
    let Some((before, after)) = wrapped.rsplit_once(TAG) else {
        return format!("{wrapped}\n{NUDGE}");
    };
    format!("{before}\n{NUDGE}{TAG}{after}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, rel: &str, name: &str, body: &str) {
        let path = root.join(rel).join(name).join("SKILL.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            format!("---\nname: {name}\ndescription: d\n---\n{body}\n"),
        )
        .unwrap();
    }

    fn req(action: Action, name: &str) -> Request {
        Request {
            action,
            name: name.into(),
            content: None,
            old_string: None,
            new_string: None,
            replace_all: false,
            file_path: None,
            file_content: None,
            scope: Scope::House,
        }
    }

    #[test]
    fn create_plans_a_house_skill_and_marks_new() {
        let dir = tempfile::tempdir().unwrap();
        let mut request = req(Action::Create, "deploy-k8s");
        request.content =
            Some("---\nname: deploy-k8s\ndescription: Deploy the k8s app.\n---\n1. apply\n".into());
        let plan = plan(dir.path(), "chief-of-staff", &request).unwrap();
        assert!(plan.created);
        assert_eq!(
            plan.writes[0].relative,
            PathBuf::from("workspace/skills/deploy-k8s/SKILL.md")
        );
    }

    #[test]
    fn create_refuses_bundled_and_collision() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "workspace/skills", "mine", "body");
        let mut request = req(Action::Create, "create-skill");
        request.content = Some("---\nname: create-skill\ndescription: no\n---\nnope\n".into());
        assert!(plan(dir.path(), "chief-of-staff", &request).is_err());
        request.name = "mine".into();
        request.content = Some("---\nname: mine\ndescription: d\n---\nx\n".into());
        assert!(plan(dir.path(), "chief-of-staff", &request).is_err());
    }

    #[test]
    fn patch_replaces_unique_snippet() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "workspace/skills", "mine", "step one\nstep two");
        let mut request = req(Action::Patch, "mine");
        request.old_string = Some("step two".into());
        request.new_string = Some("step two carefully".into());
        let plan = plan(dir.path(), "chief-of-staff", &request).unwrap();
        assert!(plan.patched);
        assert!(plan.writes[0].after.contains("carefully"));
    }

    #[test]
    fn patch_refuses_ambiguous_without_replace_all() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "workspace/skills", "mine", "aa aa");
        let mut request = req(Action::Patch, "mine");
        request.old_string = Some("aa".into());
        request.new_string = Some("bb".into());
        assert!(plan(dir.path(), "chief-of-staff", &request).is_err());
        request.replace_all = true;
        assert!(plan(dir.path(), "chief-of-staff", &request).is_ok());
    }

    #[test]
    fn write_file_must_stay_in_support_dirs() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "workspace/skills", "mine", "body");
        let mut request = req(Action::WriteFile, "mine");
        request.file_path = Some("../host".into());
        request.file_content = Some("x".into());
        assert!(plan(dir.path(), "chief-of-staff", &request).is_err());
        request.file_path = Some("references/api.md".into());
        let plan = plan(dir.path(), "chief-of-staff", &request).unwrap();
        assert!(plan.patched);
    }

    #[test]
    fn delete_archives_instead_of_unlinking() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "workspace/skills", "mine", "body");
        let plan = plan(dir.path(), "chief-of-staff", &req(Action::Delete, "mine")).unwrap();
        assert_eq!(plan.archive.as_deref(), Some("mine"));
        assert!(plan.writes.is_empty());
    }

    #[test]
    fn frontmatter_name_must_match() {
        let dir = tempfile::tempdir().unwrap();
        let mut request = req(Action::Create, "alpha");
        request.content = Some("---\nname: beta\ndescription: d\n---\nbody\n".into());
        let err = plan(dir.path(), "chief-of-staff", &request).unwrap_err();
        assert!(err.contains("does not match"));
    }

    #[test]
    fn list_and_view_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "workspace/skills", "mine", "hello");
        let list = list_text(dir.path(), "chief-of-staff");
        assert!(list.contains("mine"));
        let view = view_text(dir.path(), "chief-of-staff", "mine", None).unwrap();
        assert!(view.contains("hello"));
    }

    #[test]
    fn nudge_injects_before_user_query_close() {
        let wrapped = "<timestamp>t</timestamp>\n<user_query>\nhi\n</user_query>";
        let out = with_nudge(wrapped);
        assert!(out.contains("[skill-improve]"));
        assert!(out.contains("skill_manage"));
        assert!(out.ends_with("</user_query>"));
        assert!(out.contains("hi"));
    }

    #[test]
    fn default_nudge_interval_is_fifteen() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(nudge_interval(dir.path()), 15);
        std::fs::write(
            dir.path().join("config.yml"),
            "skills:\n  creation_nudge_interval: 0\n",
        )
        .unwrap();
        assert_eq!(nudge_interval(dir.path()), 0);
    }
}
