//! Per-bot system prompt, rebuilt each turn from files the bot can edit.

use std::sync::Arc;

use parking_lot::RwLock;

use crate::project::Project;
use crate::skills;

use super::profile::Profile;
use super::wrap::house_kernel;

pub fn system_prompt(project: &Project, bot: &Profile, teammates: &[Profile]) -> String {
    let current = match Profile::load_for(&project.root, &bot.id) {
        Ok(profile) => profile,
        Err(error) => {
            return format!(
                "Agent profile unavailable: {error}. Do not substitute another bot's identity."
            );
        }
    };
    let bot = &current;
    let mut parts = vec![house_kernel(bot, teammates)];
    parts.push(format!(
        "# Your profile\nName: {}\nTitle: {}\nRemit: {}\nProjects explicitly attached: {}",
        bot.name,
        bot.title,
        bot.description,
        bot.projects.join(", ")
    ));
    let bot_dir = project.bot_dir(&bot.id);
    match super::home::relative(&bot.id)
        .and_then(|relative| super::files::read_optional(&project.root, &relative.join("SOUL.md")))
    {
        Ok(Some(text)) => parts.push(format!("# Your SOUL.md\n{text}")),
        Ok(None) => parts.push(super::home::soul(bot)),
        Err(error) => parts.push(format!(
            "Your SOUL.md could not be read: {error}. No global soul was substituted."
        )),
    }
    let workspace = project.workspace();
    match super::files::read_optional(&project.root, std::path::Path::new("workspace/VM.md")) {
        Ok(Some(text)) => parts.push(format!(
            "# Shared machine facts (not an assignment)\n{}",
            super::memory::bounded(&text, 8000)
        )),
        Ok(None) => {}
        Err(error) => parts.push(format!("VM.md could not be read: {error}")),
    }
    parts.push(super::memory::prompt(
        &project.root,
        bot,
        chrono::Utc::now(),
    ));
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

    parts.push(environment_prompt(project, bot));
    parts.join("\n\n")
}

fn environment_prompt(project: &Project, bot: &Profile) -> String {
    let internet = project.runtime.policy.internet_prompt();
    format!(
        "<env>\n\
         You are running inside a microVM. The workspace is mounted at /workspace and is the \
         shared filesystem root. Every tool runs in that VM.\n\
         mise is installed. Use it to install missing language runtimes and development tools.\n\
         {internet}\n\
         Your agent home is /workspace/agents/{id}/. Your initial cwd is that home's workspace/; \
         cd changes cwd, not your identity.\n\
         The guest Unix account is `user` (HOME=/home/user, passwordless sudo). That is not your agent home.\n\
         A desktop (XFCE on DISPLAY=:1) is shared with the user via the Screen panel. \
         Steer Chrome with agent-browser (`/browser`); other GUI with xdotool (`/computer`).\n\
         Relative paths resolve against the current conversation cwd, shown in message headers.\n\
         </env>",
        id = bot.id
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
    fn souls_profiles_and_memory_do_not_bleed_across_agents() {
        let dir = tempfile::tempdir().unwrap();
        project::init(dir.path()).unwrap();
        let project = Project::load(dir.path()).unwrap();
        for (id, name, soul, fact) in [
            ("miku", "Miku", "MUSIC_SOUL", "MUSIC_MEMORY"),
            ("qmd-dev", "QMD Dev", "QMD_SOUL", "DONT_AUTO_EMBED"),
        ] {
            let home = project.bot_dir(id);
            std::fs::create_dir_all(home.join("memory")).unwrap();
            std::fs::write(
                home.join("profile.json"),
                serde_json::json!({"id":id,"name":name}).to_string(),
            )
            .unwrap();
            std::fs::write(home.join("SOUL.md"), soul).unwrap();
            std::fs::write(home.join("memory/profile.md"), fact).unwrap();
        }
        for filename in ["SOUL.md", "KNOWLEDGE.md", "AGENTS.md"] {
            std::fs::write(project.workspace().join(filename), "GLOBAL_QMD_BIAS").unwrap();
        }
        std::fs::write(project.workspace().join("VM.md"), "SHARED_VM_TOOLCHAIN").unwrap();
        let roster = scan(&project.agents_dir());
        let miku = roster.iter().find(|p| p.id == "miku").unwrap();
        let prompt = system_prompt(&project, miku, &roster);
        for expected in ["MUSIC_SOUL", "MUSIC_MEMORY", "SHARED_VM_TOOLCHAIN"] {
            assert!(prompt.contains(expected));
        }
        for forbidden in ["GLOBAL_QMD_BIAS", "QMD_SOUL", "DONT_AUTO_EMBED"] {
            assert!(!prompt.contains(forbidden), "{forbidden}");
        }
        std::fs::write(
            project.bot_dir("miku").join("profile.json"),
            r#"{"name":"Miku Renamed","description":"Own music"}"#,
        )
        .unwrap();
        let refreshed = system_prompt(&project, miku, &roster);
        assert!(refreshed.contains("display name is Miku Renamed"));
        assert!(refreshed.contains("Own music"));
    }

    #[test]
    fn system_prompt_uses_the_bot_soul() {
        let dir = tempfile::tempdir().unwrap();
        project::init(dir.path()).unwrap();
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
            "kernel is applied before SOUL.md"
        );
        let kernel_at = prompt.find("# House").unwrap();
        let orders_at = prompt.find("# Your SOUL.md").unwrap();
        assert!(kernel_at < orders_at, "house kernel must precede SOUL.md");
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
