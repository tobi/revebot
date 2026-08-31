//! Each bot's home and persona. Never inherit a global SOUL/KNOWLEDGE file.
use super::profile::{Profile, validate_id};
use crate::sandbox::{ExecOptions, Sandbox};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

pub fn relative(id: &str) -> anyhow::Result<PathBuf> {
    validate_id(id)?;
    Ok(Path::new("workspace/agents").join(id))
}
pub fn guest(id: &str) -> anyhow::Result<String> {
    Ok(format!("/{}", relative(id)?.display()))
}

pub fn soul(profile: &Profile) -> String {
    format!(
        "# {}\n\nThis is your own voice and identity, not the house's personality.\n\nYour stable id is `{}`.\nYour role: {}\nYour remit: {}\n\nYou already have a name; do not ask what to be called.\nDevelop your own voice through your work and the user's preferences. Edit this file as you learn.\nDo not adopt another agent's projects or personality merely because they appear in the roster.\nBe honest about what you have and have not verified.\n",
        profile.name,
        profile.id,
        if profile.title.is_empty() {
            "not assigned yet"
        } else {
            &profile.title
        },
        if profile.description.is_empty() {
            "ask the user what they want your help with"
        } else {
            &profile.description
        }
    )
}

pub fn defaults(profile: &Profile) -> Vec<(&'static str, String)> {
    vec![("SOUL.md", soul(profile)), ("AGENTS.md", include_str!("../templates/AGENTS.md").into()),
        ("memory/profile.md", "# Enduring memory\n\nThis agent's foundational facts. Keep private work here; share only with explicit scope.\n".into())]
}

/// Missing-only installation for existing/created bots. Guest noclobber keeps
/// edited files, including an intentionally empty SOUL.md. No global copy.
pub async fn ensure(sandbox: &Sandbox, profile: &Profile) -> anyhow::Result<()> {
    let path = guest(&profile.id)?;
    let q = |s: &str| shell_words::quote(s).into_owned();
    let mut command = String::from("set -eu\n");
    for dir in [
        "",
        "workspace",
        "memory",
        "memory/log",
        "memory/notes",
        "knowledge",
        "notes",
        "skills",
        "sessions",
        "plugins",
        "routines",
    ] {
        let _ = writeln!(command, "mkdir -p -- {}", q(&format!("{path}/{dir}")));
    }
    for (name, body) in defaults(profile) {
        let file = q(&format!("{path}/{name}"));
        let _ = writeln!(
            command,
            "if [ ! -e {file} ] && [ ! -L {file} ]; then (set -C; printf %s {} > {file}) || exit 1; fi",
            q(&body)
        );
    }
    let result = sandbox.exec(&command, ExecOptions::default(), None).await?;
    if !result.success || result.cancelled {
        anyhow::bail!("could not initialize agent home: {}", result.stderr);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn souls_are_agent_specific_and_homes_reject_paths() {
        let miku = Profile::parse_for("miku", r#"{"name":"Miku","description":"Music and sound"}"#)
            .unwrap();
        let qmd = Profile::parse_for(
            "qmd-dev",
            r#"{"name":"QMD Dev","description":"QMD indexing"}"#,
        )
        .unwrap();
        assert!(soul(&miku).contains("Music and sound"));
        assert!(!soul(&miku).contains("QMD indexing"));
        assert_ne!(soul(&miku), soul(&qmd));
        assert_eq!(guest("miku").unwrap(), "/workspace/agents/miku");
        for id in ["../host", "/tmp", "a/b", "", ".."] {
            assert!(guest(id).is_err());
        }
    }
}
