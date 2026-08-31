//! Conversation-local cwd. HOME is identity; cwd is durable working state.
use crate::entry::Namespace;
use crate::sandbox::{ExecOptions, Sandbox};
use crate::session::Session;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub path: String,
    pub instructions: Vec<Instruction>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instruction {
    pub path: String,
    pub text: String,
}

#[derive(Clone)]
pub struct Context {
    pub bot: String,
    pub home: String,
    state: Arc<RwLock<State>>,
    // One mutation line per conversation, never a VM-global chdir.
    mutation: Arc<tokio::sync::Mutex<()>>,
}

impl Context {
    pub fn new(bot: &str) -> anyhow::Result<Self> {
        let home = crate::house::home::guest(bot)?;
        Ok(Self {
            bot: bot.into(),
            home: home.clone(),
            state: Arc::new(RwLock::new(State {
                path: format!("{home}/workspace"),
                instructions: Vec::new(),
            })),
            mutation: Arc::new(tokio::sync::Mutex::new(())),
        })
    }
    pub fn cwd(&self) -> String {
        self.state.read().path.clone()
    }
    pub fn inherits(&self, path: &str) -> bool {
        ancestor_files(&self.cwd()).iter().any(|p| p == path)
    }
    pub fn resolve(&self, path: &str) -> String {
        normalize(&self.cwd(), path)
    }
    pub fn options(&self) -> ExecOptions {
        ExecOptions {
            cwd: Some(self.cwd()),
            env: [("HOME".into(), self.home.clone())].into(),
            ..Default::default()
        }
    }
    pub fn instructions(&self) -> String {
        format_instructions(&self.state.read().instructions)
    }
    pub async fn restore(
        &self,
        session: &Session,
        lane: &str,
        sandbox: &Sandbox,
    ) -> anyhow::Result<()> {
        let key = format!("cwd/{lane}");
        let path = session
            .register::<State>(Namespace::FactCustom, &key)
            .await?
            .map(|(s, _)| s.path)
            .unwrap_or_else(|| format!("{}/workspace", self.home));
        self.change(session, lane, sandbox, &path).await?;
        Ok(())
    }
    pub async fn change(
        &self,
        session: &Session,
        lane: &str,
        sandbox: &Sandbox,
        path: &str,
    ) -> anyhow::Result<String> {
        let _guard = self.mutation.lock().await;
        if path.is_empty() || path.contains(['\0', '\n', '\r']) {
            anyhow::bail!("cd needs a nonempty directory path without control characters");
        }
        let requested = self.resolve(path);
        let result = sandbox
            .exec(
                &format!("cd -- {} && pwd -P", shell_words::quote(&requested)),
                ExecOptions {
                    cwd: Some(self.home.clone()),
                    env: [("HOME".into(), self.home.clone())].into(),
                    ..Default::default()
                },
                None,
            )
            .await?;
        if !result.success || result.cancelled {
            anyhow::bail!("cannot change directory: {}", result.stderr.trim());
        }
        let canonical = result.stdout.trim_end_matches('\n');
        if !canonical.starts_with('/') || canonical.contains(['\0', '\n', '\r']) {
            anyhow::bail!("guest returned an invalid directory");
        }
        let mut instructions = Vec::new();
        for file in ancestor_files(canonical) {
            let result = sandbox
                .exec(
                    &format!(
                        "if [ -f {p} ]; then cat -- {p}; else exit 3; fi",
                        p = shell_words::quote(&file)
                    ),
                    ExecOptions {
                        cwd: Some(self.home.clone()),
                        ..Default::default()
                    },
                    None,
                )
                .await?;
            if result.exit_code == 3 {
                continue;
            }
            if !result.success || result.cancelled {
                anyhow::bail!("cannot read {file}: {}", result.stderr.trim());
            }
            instructions.push(Instruction {
                path: file,
                text: result.stdout,
            });
        }
        let next = State {
            path: canonical.into(),
            instructions,
        };
        let key = format!("cwd/{lane}");
        if session
            .register::<State>(Namespace::FactCustom, &key)
            .await?
            .map(|(s, _)| s)
            != Some(next.clone())
        {
            session
                .set_fact(
                    Namespace::FactCustom,
                    &key,
                    Some(serde_json::to_value(&next)?),
                )
                .await?;
        }
        let text = format!(
            "Working directory: {}\n{}",
            next.path,
            format_instructions(&next.instructions)
        );
        *self.state.write() = next;
        Ok(text)
    }
}

pub fn normalize(cwd: &str, path: &str) -> String {
    let path = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        Path::new(cwd).join(path)
    };
    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir => {
                normalized.pop();
            }
            _ => {}
        }
    }
    normalized.display().to_string()
}

fn ancestor_files(path: &str) -> Vec<String> {
    let mut ancestors: Vec<_> = Path::new(path)
        .ancestors()
        .map(|p| p.join("AGENTS.md").display().to_string())
        .collect();
    ancestors.reverse();
    ancestors
}

fn format_instructions(instructions: &[Instruction]) -> String {
    if instructions.is_empty() {
        return "No AGENTS.md files in this directory's ancestor chain.".into();
    }
    let mut text =
        String::from("Directory instructions (root to leaf; nearest takes precedence):\n");
    for instruction in instructions {
        text.push_str(&format!(
            "\n## {}\n{}\n",
            instruction.path, instruction.text
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn homes_stay_fixed_and_directory_contexts_do_not_leak() {
        let a = Context::new("miku").unwrap();
        let b = Context::new("qmd-dev").unwrap();
        a.state.write().path = "/workspace/projects/qmd".into();
        assert_eq!(a.options().env["HOME"], "/workspace/agents/miku");
        assert_eq!(
            a.resolve("../other/./file"),
            "/workspace/projects/other/file"
        );
        assert_eq!(b.cwd(), "/workspace/agents/qmd-dev/workspace");
        assert_eq!(normalize("/", "../../tmp"), "/tmp");
    }
    #[test]
    fn ancestor_rules_are_full_and_ordered_root_to_leaf() {
        assert_eq!(
            ancestor_files("/workspace/projects/qmd"),
            [
                "/AGENTS.md",
                "/workspace/AGENTS.md",
                "/workspace/projects/AGENTS.md",
                "/workspace/projects/qmd/AGENTS.md"
            ]
        );
        let body = "rule\n".repeat(10000);
        let text = format_instructions(&[
            Instruction {
                path: "/AGENTS.md".into(),
                text: "root".into(),
            },
            Instruction {
                path: "/a/AGENTS.md".into(),
                text: body.clone(),
            },
        ]);
        assert!(text.contains(&body));
        assert!(text.find("root\n").unwrap() < text.find("/a/AGENTS.md").unwrap());
    }
}
