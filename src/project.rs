//! The house directory.
//!
//! A house is a directory, and the files in it are its definition. There is no
//! machine-wide profile, no home-directory config, and no global session store:
//! copy the directory and you copy the house. Bot identity lives under
//! `workspace/agents/<id>/`, never as a file at the house root.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use thiserror::Error;

use crate::lua::Runtime;

#[derive(Debug, Error)]
pub enum ProjectError {
    #[error(
        "not a house directory: {0}\n\n  a house needs:\n    config.yml (or agent.lua)              house configuration\n    workspace/agents/<id>/SOUL.md          identity and standing remit for each bot\n\n  run `revebot init` to scaffold one here"
    )]
    NotAHouse(PathBuf),
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Lua(#[from] crate::lua::LuaError),
    #[error("invalid agent profile: {0}")]
    Profile(String),
}

pub type Result<T, E = ProjectError> = std::result::Result<T, E>;

/// The files `revebot init` writes. Kept as one table so `init` is idempotent and
/// can report created / unchanged / changed-and-kept per file.
const TEMPLATES: &[(&str, &str)] = &[
    ("config.yml", include_str!("templates/config.yml")),
    (
        "tools/example.lua",
        include_str!("templates/example_tool.lua"),
    ),
    (
        "workspace/agents/chief-of-staff/routines/example.lua",
        include_str!("templates/example_routine.lua"),
    ),
    (
        "workspace/plugins/web_fetch.lua",
        include_str!("templates/web_fetch.lua"),
    ),
    (
        "workspace/skills/create-skill/SKILL.md",
        include_str!("templates/create_skill.md"),
    ),
    (
        "workspace/skills/vm/SKILL.md",
        include_str!("templates/vm_skill.md"),
    ),
    (
        "workspace/skills/routines/SKILL.md",
        include_str!("templates/routines_skill.md"),
    ),
    (
        "workspace/skills/plugins/SKILL.md",
        include_str!("templates/plugins_skill.md"),
    ),
    (
        "workspace/skills/memory/SKILL.md",
        include_str!("templates/memory_skill.md"),
    ),
    (
        "workspace/skills/secrets/SKILL.md",
        include_str!("templates/secrets_skill.md"),
    ),
    (
        "workspace/skills/browser/SKILL.md",
        include_str!("templates/browser_skill.md"),
    ),
    (
        "workspace/skills/computer/SKILL.md",
        include_str!("templates/computer_skill.md"),
    ),
    (
        "workspace/skills/curator/SKILL.md",
        include_str!("templates/curator_skill.md"),
    ),
    (
        "workspace/skills/learn/SKILL.md",
        include_str!("templates/learn_skill.md"),
    ),
    ("models.yml", include_str!("templates/models.yml")),
    ("workspace/VM.md", include_str!("templates/VM.md")),
    (
        "workspace/HEARTBEAT.yml",
        include_str!("templates/HEARTBEAT.yml"),
    ),
    (
        "workspace/agents/chief-of-staff/profile.json",
        include_str!("templates/profile.json"),
    ),
    (".gitignore", include_str!("templates/gitignore")),
];

const KEEP_DIRS: &[&str] = &[
    "tools",
    "plugins",
    "channels",
    "workspace/projects",
    "workspace/memory/user",
    "workspace/skills",
    "workspace/plugins",
    "workspace/routines",
    "workspace/tmp",
    "workspace/agents",
    "workspace/agents/chief-of-staff/skills",
    "workspace/agents/chief-of-staff/sessions",
    "workspace/agents/chief-of-staff/memory",
    "workspace/agents/chief-of-staff/memory/log",
    "workspace/agents/chief-of-staff/memory/notes",
    "workspace/agents/chief-of-staff/workspace",
    "workspace/agents/chief-of-staff/knowledge",
    "workspace/agents/chief-of-staff/notes",
    "workspace/agents/chief-of-staff/routines",
    "workspace/agents/chief-of-staff/plugins",
];

#[derive(Debug, Default)]
pub struct InitReport {
    pub root: PathBuf,
    pub created: Vec<String>,
    pub unchanged: Vec<String>,
    /// Present but different from the current template. Left alone.
    pub changed: Vec<String>,
}

/// Create (or top up) an agent directory. Idempotent, and it never writes
/// outside `root`.
pub fn init(root: impl AsRef<Path>) -> Result<InitReport> {
    let root = root.as_ref().to_path_buf();
    let mut report = InitReport {
        root: root.clone(),
        ..Default::default()
    };

    std::fs::create_dir_all(&root).map_err(|source| ProjectError::Io {
        path: root.clone(),
        source,
    })?;
    for dir in KEEP_DIRS {
        crate::script_fs::ensure_dir(&root, Path::new(dir)).map_err(|source| ProjectError::Io {
            path: root.join(dir),
            source,
        })?;
    }
    // `.reve` is durable state, not scaffold; it is created on first launch.
    for (name, body) in TEMPLATES {
        install_scaffold(&root, name, body, &mut report)?;
    }
    // Existing bots receive missing home files, never global persona/project
    // copies. create_new preserves even intentionally empty edited files.
    let profiles = crate::house::profile::scan_checked(&root)
        .map_err(|e| ProjectError::Profile(e.to_string()))?;
    for profile in profiles {
        for directory in [
            "workspace",
            "memory/log",
            "memory/notes",
            "knowledge",
            "notes",
        ] {
            let relative = Path::new("workspace/agents")
                .join(&profile.id)
                .join(directory);
            crate::script_fs::ensure_dir(&root, &relative).map_err(|source| ProjectError::Io {
                path: root.join(relative),
                source,
            })?;
        }
        for (name, body) in crate::house::home::defaults(&profile) {
            install_scaffold(
                &root,
                &format!("workspace/agents/{}/{name}", profile.id),
                &body,
                &mut report,
            )?;
        }
    }
    Ok(report)
}

fn install_scaffold(
    root: &Path,
    relative: &str,
    body: &str,
    report: &mut InitReport,
) -> Result<()> {
    let path = root.join(relative);
    match crate::script_fs::create_new(root, Path::new(relative)) {
        Ok(mut file) => {
            use std::io::Write;
            file.write_all(body.as_bytes())
                .map_err(|source| ProjectError::Io { path, source })?;
            report.created.push(relative.into());
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            match crate::script_fs::read_text(root, Path::new(relative), 1024 * 1024) {
                Ok(existing) if existing == body => report.unchanged.push(relative.into()),
                Ok(_) => report.changed.push(relative.into()),
                Err(source) => return Err(ProjectError::Io { path, source }),
            }
        }
        Err(source) => return Err(ProjectError::Io { path, source }),
    }
    Ok(())
}

/// A loaded agent directory.
pub struct Project {
    pub root: PathBuf,
    /// Shared, because tool calls need it alongside the sandbox and a Lua VM
    /// cannot be cloned.
    pub runtime: Arc<Runtime>,
}

impl Project {
    /// Is this a directory revebot is willing to run?
    ///
    /// The check exists so a house cannot silently attach itself to an
    /// arbitrary checkout and start acting like it belongs there. Identity
    /// lives under `workspace/agents/<id>/`, not a file at the house root.
    pub fn is_house_dir(root: &Path) -> bool {
        root.join("config.yml").is_file() || root.join("agent.lua").is_file()
    }

    pub fn load(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        if !Self::is_house_dir(&root) {
            return Err(ProjectError::NotAHouse(root));
        }
        crate::house::profile::scan_checked(&root)
            .map_err(|e| ProjectError::Profile(e.to_string()))?;
        let mut runtime = Runtime::new()?;
        load_host_config(&root, &mut runtime)?;
        runtime.load_tools(&root.join("tools"))?;
        runtime.load_tools(&root.join("plugins"))?;
        runtime.load_workspace_tools(&root, Path::new("workspace/plugins"), None)?;
        runtime.load_workspace_routines(&root, Path::new("workspace/routines"), None)?;
        let dirs = crate::script_fs::child_dirs(&root, Path::new("workspace/agents")).map_err(
            |source| ProjectError::Io {
                path: root.join("workspace/agents"),
                source,
            },
        )?;
        for dir in dirs {
            let id = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
            runtime.load_workspace_tools(&root, &dir.join("plugins"), Some(id))?;
            runtime.load_workspace_routines(&root, &dir.join("routines"), Some(id))?;
        }
        Ok(Self {
            root,
            runtime: Arc::new(runtime),
        })
    }

    pub fn runtime_arc(&self) -> Arc<Runtime> {
        self.runtime.clone()
    }

    pub fn workspace(&self) -> PathBuf {
        self.root.join("workspace")
    }

    pub fn state_dir(&self) -> PathBuf {
        self.root.join(".reve")
    }

    pub fn first_bot_id() -> &'static str {
        "chief-of-staff"
    }

    pub fn agents_dir(&self) -> PathBuf {
        self.workspace().join("agents")
    }

    pub fn bot_dir(&self, id: &str) -> PathBuf {
        self.agents_dir().join(id)
    }

    pub fn bot_sessions_dir(&self, id: &str) -> PathBuf {
        self.bot_dir(id).join("sessions")
    }

    /// The durable session file for a named conversation of a bot.
    pub fn conversation_path(&self, name: &str) -> PathBuf {
        self.bot_conversation_path(Self::first_bot_id(), name)
    }

    pub fn bot_conversation_path(&self, bot: &str, name: &str) -> PathBuf {
        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.6f");
        let unique = crate::ids::uuid_v7(crate::ids::now_ms());
        self.bot_sessions_dir(bot)
            .join(format!("{name}-{stamp}-{unique}.jsonl"))
    }

    /// The newest existing session for a conversation, if there is one.
    pub fn latest_session(&self, name: &str) -> Option<PathBuf> {
        self.latest_bot_session(Self::first_bot_id(), name)
    }

    pub fn latest_bot_session(&self, bot: &str, name: &str) -> Option<PathBuf> {
        self.latest_bot_session_checked(bot, name).ok().flatten()
    }

    pub fn latest_bot_session_checked(&self, bot: &str, name: &str) -> Result<Option<PathBuf>> {
        crate::house::profile::validate_id(bot)
            .and_then(|()| crate::house::profile::validate_id(name))
            .map_err(|e| ProjectError::Profile(e.to_string()))?;
        let relative = Path::new("workspace/agents").join(bot).join("sessions");
        let mut files: Vec<_> = crate::script_fs::files(&self.root, &relative)
            .map_err(|source| ProjectError::Io {
                path: self.root.join(&relative),
                source,
            })?
            .into_iter()
            .filter(|path| {
                path.extension().is_some_and(|e| e == "jsonl")
                    && path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with(&format!("{name}-")))
            })
            .collect();
        files.sort();
        Ok(files.pop().map(|p| self.root.join(p)))
    }
}

fn load_host_config(root: &Path, runtime: &mut Runtime) -> Result<()> {
    let yml = root.join("config.yml");
    if yml.is_file() {
        return apply_config_yml(runtime, &yml);
    }
    runtime.load_agent(&root.join("agent.lua"))?;
    runtime.load_sandbox(&root.join("sandbox.lua"))?;
    Ok(())
}

#[derive(serde::Deserialize, Default)]
struct FileConfig {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    sandbox: Option<FileSandbox>,
}

#[derive(serde::Deserialize, Default)]
struct FileSandbox {
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    cpus: Option<u8>,
    #[serde(default)]
    memory: Option<u32>,
    #[serde(default)]
    root_disk: Option<u32>,
    #[serde(default)]
    open: Option<bool>,
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    secrets: Vec<crate::sandbox::Secret>,
}

fn apply_config_yml(runtime: &mut Runtime, path: &Path) -> Result<()> {
    let text = std::fs::read_to_string(path).map_err(|source| ProjectError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let parsed: FileConfig = serde_yaml::from_str(&text).map_err(|e| ProjectError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, e),
    })?;
    runtime.agent.model = parsed.model.or(runtime.agent.model.take());
    runtime.agent.thinking = parsed.thinking.or(runtime.agent.thinking.take());
    if let Some(s) = parsed.sandbox {
        if let Some(image) = s.image {
            runtime.policy.image = image;
        }
        if let Some(cpus) = s.cpus {
            runtime.policy.cpus = cpus;
        }
        if let Some(memory) = s.memory {
            runtime.policy.memory = memory;
        }
        if let Some(root_disk) = s.root_disk {
            runtime.policy.root_disk = root_disk;
        }
        if let Some(open) = s.open {
            runtime.policy.open = open;
        }
        if !s.allow.is_empty() {
            runtime.policy.allow_hosts = s.allow;
        }
        if !s.secrets.is_empty() {
            runtime.policy.secrets = s.secrets;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_scaffolds_a_directory_that_loads() {
        let dir = tempfile::tempdir().unwrap();
        let report = init(dir.path()).unwrap();
        assert!(report.created.contains(&"config.yml".to_string()));
        assert!(report.changed.is_empty());

        // The scaffold must actually be a runnable agent, not just files.
        let project = Project::load(dir.path()).expect("the scaffold loads");
        assert!(
            project.runtime.agent.model.is_some(),
            "config.yml sets a model"
        );
        assert!(
            project.runtime.tool("example").is_some(),
            "the example tool registered"
        );
        assert!(
            project.runtime.tool("web_fetch").is_some(),
            "workspace web_fetch plugin registered"
        );
        assert!(
            project.runtime.policy.mount_workspace,
            "workspace is mounted"
        );
        let hosts = project.runtime.policy.egress_hosts();
        assert!(
            hosts.iter().any(|h| h == "github.com"),
            "secret hosts join the allow list: {hosts:?}"
        );
        assert!(hosts.iter().any(|h| h == "api.github.com"));
        assert!(hosts.iter().any(|h| h == "openrouter.ai"));
        let github = project
            .runtime
            .policy
            .secrets
            .iter()
            .find(|s| s.env == "GITHUB_TOKEN")
            .expect("GITHUB_TOKEN secret");
        assert!(github.hosts["github.com"].allow);
        assert!(github.hosts["github.com"].headers.is_empty());
        let openrouter = project
            .runtime
            .policy
            .secrets
            .iter()
            .find(|secret| secret.env == "OPENROUTER_API_KEY")
            .expect("OPENROUTER_API_KEY secret");
        assert_eq!(openrouter.source, "$OPENROUTER_API_KEY");
    }

    #[test]
    fn init_scaffolds_house_and_first_bot() {
        let dir = tempfile::tempdir().unwrap();
        let report = init(dir.path()).unwrap();
        assert!(
            report
                .created
                .iter()
                .any(|n| n == "workspace/agents/chief-of-staff/SOUL.md")
        );
        assert!(
            report
                .created
                .iter()
                .any(|n| n == "workspace/agents/chief-of-staff/profile.json")
        );
        let gitignore = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(gitignore.contains("workspace/agents/*/sessions/"));
        assert!(
            dir.path()
                .join("workspace/agents/chief-of-staff/skills")
                .is_dir()
        );
        assert!(
            !dir.path().join("SOUL.md").exists(),
            "bot identity lives under workspace/agents/, not the house root"
        );
        assert!(
            dir.path()
                .join("workspace/agents/chief-of-staff/routines/example.lua")
                .is_file()
        );
        assert!(dir.path().join("workspace/plugins/web_fetch.lua").is_file());
        for skill in [
            "create-skill",
            "vm",
            "routines",
            "plugins",
            "secrets",
            "browser",
            "computer",
            "curator",
            "learn",
        ] {
            assert!(
                dir.path()
                    .join(format!("workspace/skills/{skill}/SKILL.md"))
                    .is_file(),
                "{skill} skill"
            );
        }
        let edited = dir.path().join("workspace/agents/chief-of-staff/SOUL.md");
        std::fs::write(&edited, "# mine\n").unwrap();
        let again = init(dir.path()).unwrap();
        assert!(
            again
                .changed
                .iter()
                .any(|n| n == "workspace/agents/chief-of-staff/SOUL.md")
        );
        assert_eq!(std::fs::read_to_string(&edited).unwrap(), "# mine\n");
    }

    #[test]
    fn each_agent_gets_its_own_home_and_edited_souls_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("workspace/agents/miku");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("profile.json"), r#"{"name":"Miku"}"#).unwrap();
        std::fs::write(dir.path().join("workspace/SOUL.md"), "GLOBAL PERSONALITY").unwrap();
        std::fs::write(dir.path().join("workspace/KNOWLEDGE.md"), "QMD KNOWLEDGE").unwrap();
        init(dir.path()).unwrap();
        let soul = std::fs::read_to_string(home.join("SOUL.md")).unwrap();
        assert!(soul.contains("Miku"));
        assert!(!soul.contains("GLOBAL PERSONALITY"));
        assert!(!soul.contains("QMD KNOWLEDGE"));
        assert!(home.join("workspace").is_dir());
        assert!(home.join("memory/log").is_dir());
        std::fs::write(home.join("SOUL.md"), "").unwrap();
        init(dir.path()).unwrap();
        assert_eq!(std::fs::read_to_string(home.join("SOUL.md")).unwrap(), "");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("workspace/KNOWLEDGE.md")).unwrap(),
            "QMD KNOWLEDGE"
        );
    }

    #[test]
    fn init_does_not_follow_a_symlinked_home_subdirectory() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        init(dir.path()).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let home = dir.path().join("workspace/agents/chief-of-staff");
        std::fs::remove_dir_all(home.join("memory")).unwrap();
        symlink(outside.path(), home.join("memory")).unwrap();
        assert!(init(dir.path()).is_err());
        assert!(std::fs::read_dir(outside.path()).unwrap().next().is_none());
    }

    #[test]
    fn init_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        init(dir.path()).unwrap();
        let second = init(dir.path()).unwrap();
        assert!(second.created.is_empty(), "nothing is rewritten");
        assert!(!second.unchanged.is_empty());
    }

    #[test]
    fn init_never_clobbers_an_edited_file() {
        let dir = tempfile::tempdir().unwrap();
        init(dir.path()).unwrap();
        std::fs::write(dir.path().join("config.yml"), "# mine\n").unwrap();
        let report = init(dir.path()).unwrap();
        assert!(report.changed.contains(&"config.yml".to_string()));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("config.yml")).unwrap(),
            "# mine\n"
        );
    }

    #[test]
    fn an_arbitrary_checkout_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("README.md"), "someone else's project").unwrap();
        assert!(!Project::is_house_dir(dir.path()));
        let Err(err) = Project::load(dir.path()) else {
            panic!("an arbitrary checkout must not load as an agent")
        };
        assert!(
            err.to_string().contains("not a house directory"),
            "got {err}"
        );
        assert!(
            err.to_string().contains("revebot init"),
            "and it says how to fix that"
        );
    }

    #[test]
    fn durable_paths_stay_under_the_agent_root() {
        let dir = tempfile::tempdir().unwrap();
        init(dir.path()).unwrap();
        let project = Project::load(dir.path()).unwrap();
        assert!(
            project
                .bot_sessions_dir("chief-of-staff")
                .starts_with(dir.path())
        );
        assert!(
            project
                .conversation_path("main")
                .starts_with(project.bot_sessions_dir("chief-of-staff"))
        );
        assert!(project.workspace().starts_with(dir.path()));
    }
}
