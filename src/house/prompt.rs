//! Per-bot system prompt, rebuilt each turn from files the bot can edit.

use std::sync::Arc;

use parking_lot::RwLock;

use crate::project::Project;
use crate::skills;

use super::profile::Profile;
use super::wrap::house_kernel;

pub fn system_prompt(project: &Project, bot: &Profile, teammates: &[Profile]) -> String {
    let mut parts = Vec::new();
    parts.push(house_kernel(bot, teammates));
    let bot_dir = project.bot_dir(&bot.id);
    if let Ok(text) = std::fs::read_to_string(bot_dir.join("instructions.md")) {
        parts.push(text.trim().to_string());
    }
    let workspace = project.workspace();
    for (file, limit) in [
        ("AGENTS.md", usize::MAX),
        ("SOUL.md", usize::MAX),
        ("KNOWLEDGE.md", 100),
    ] {
        let path = if file == "SOUL.md" {
            let own = bot_dir.join("SOUL.md");
            if own.is_file() {
                own
            } else {
                workspace.join(file)
            }
        } else {
            workspace.join(file)
        };
        if let Ok(text) = std::fs::read_to_string(path) {
            let body: String = text.lines().take(limit).collect::<Vec<_>>().join("\n");
            if !body.trim().is_empty() {
                parts.push(format!("# {file}\n\n{}", body.trim()));
            }
        }
    }
    let listed = skills::listings_for(&workspace, &bot_dir);
    if !listed.is_empty() {
        let mut lines = vec![
            "The user invokes a skill with `/name` — its full body is then attached to that turn.".into(),
            "House skills live in `/workspace/skills/`; yours in `/workspace/agents/<your-id>/skills/` and win on the same name.".into(),
            "A skill that is created or edited is attached to the next user message automatically. Follow the latest body.".into(),
            String::new(),
        ];
        for skill in &listed {
            let where_ = if skill.source == "bot" {
                "yours"
            } else {
                "house"
            };
            let desc = skill.description.replace('\n', " ");
            lines.push(format!(
                "- `/{name}` ({where_}) — {desc}",
                name = skill.name
            ));
        }
        parts.push(format!("# Available skills\n\n{}", lines.join("\n")));
    }

    parts.push(environment_prompt(project));
    parts.join("\n\n")
}

fn environment_prompt(project: &Project) -> String {
    let internet = project.runtime.policy.internet_prompt();
    format!(
        "<env>\n\
         You are running inside a microVM. The workspace is mounted at /workspace and is the \
         working directory; paths are relative to it. Every tool runs in that VM.\n\
         mise is installed. Use it to install missing language runtimes and development tools.\n\
         {internet}\n\
         Your files are under /workspace/agents/<your-id>/.\n\
         </env>"
    )
}

/// Snapshot of ready profiles for the system-prompt closure.
pub type ProfileCache = Arc<RwLock<Vec<Profile>>>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::house::profile::{FIRST_BOT, scan};
    use crate::project::{self, Project};

    #[test]
    fn system_prompt_reads_the_bot_not_a_file_at_the_house_root() {
        let dir = tempfile::tempdir().unwrap();
        project::init(dir.path()).unwrap();
        std::fs::write(dir.path().join("instructions.md"), "LEGACY ROOT IDENTITY").unwrap();
        let project = Project::load(dir.path()).unwrap();
        let teammates = scan(&project.agents_dir());
        let bot = teammates.iter().find(|p| p.id == FIRST_BOT).unwrap();
        let prompt = system_prompt(&project, bot, &teammates);
        assert!(
            prompt.contains("Chief of Staff"),
            "prompt should include bot standing orders"
        );
        assert!(
            prompt.contains("# House"),
            "kernel is applied before instructions.md"
        );
        let kernel_at = prompt.find("# House").unwrap();
        let orders_at = prompt.find("You coordinate this house").unwrap();
        assert!(
            kernel_at < orders_at,
            "house kernel must precede instructions.md"
        );
        assert!(
            !prompt.contains("LEGACY ROOT IDENTITY"),
            "a file at the house root is not identity"
        );
    }

    #[test]
    fn system_prompt_lists_workspace_and_bot_skills_with_bot_shadow() {
        let dir = tempfile::tempdir().unwrap();
        project::init(dir.path()).unwrap();
        let project = Project::load(dir.path()).unwrap();
        let teammates = scan(&project.agents_dir());
        let bot = teammates.iter().find(|p| p.id == FIRST_BOT).unwrap();
        let shared = project.workspace().join("skills/review/SKILL.md");
        std::fs::create_dir_all(shared.parent().unwrap()).unwrap();
        std::fs::write(
            &shared,
            "---\nname: review\ndescription: workspace review\n---\nWS",
        )
        .unwrap();
        let own = project.bot_dir(&bot.id).join("skills/review/SKILL.md");
        std::fs::create_dir_all(own.parent().unwrap()).unwrap();
        std::fs::write(
            &own,
            "---\nname: review\ndescription: bot-local review\n---\nBOT",
        )
        .unwrap();
        let prompt = system_prompt(&project, bot, &teammates);
        assert!(prompt.contains("`/review` (yours) — bot-local review"));
        assert!(!prompt.contains("workspace review"));
        assert!(prompt.contains("`/plugins` (house)"));
        assert!(prompt.contains("`/create-skill` (house)"));
        assert!(prompt.contains("`/secrets` (house)"));
        assert!(
            prompt.contains("Create a new Reve skill"),
            "folded YAML descriptions must list as real text, not `>`"
        );
        assert!(prompt.contains("AskUserForSecret"));
    }
}
