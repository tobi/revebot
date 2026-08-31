//! Host listing of the workspace mount (`/workspace` in the guest).

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

const LIST_CAP: usize = 400;
const READ_CAP: usize = 64 * 1024;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FsEntry {
    pub name: String,
    pub dir: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct FsList {
    pub cwd: String,
    pub path: String,
    pub entries: Vec<FsEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FsFile {
    pub path: String,
    pub binary: bool,
    pub truncated: bool,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FsStat {
    pub path: String,
    pub exists: bool,
    pub dir: bool,
}

/// Guest-relative path: strip a leading `/workspace/` component only.
fn guest_rel(rel: &str) -> &str {
    let rel = rel.trim().trim_start_matches('/');
    if let Some(rest) = rel.strip_prefix("workspace/") {
        rest.trim_start_matches('/')
    } else if rel == "workspace" {
        ""
    } else {
        rel
    }
}

pub fn join_under(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let rel = guest_rel(rel);
    if rel.is_empty() {
        return Ok(root.to_path_buf());
    }
    for part in rel.split(['/', '\\']) {
        if part.is_empty() || part == "." || part == ".." {
            return Err("invalid path".into());
        }
    }
    Ok(root.join(rel))
}

fn contained(root: &Path, path: &Path) -> Result<PathBuf, String> {
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let path = path.canonicalize().map_err(|e| e.to_string())?;
    if !path.starts_with(&root) {
        return Err("invalid path".into());
    }
    Ok(path)
}

/// JSONL transcripts under `agents/<id>/sessions/` — skip in the browser.
fn skip_name(parent: &str, name: &str, is_dir: bool) -> bool {
    if name.starts_with('.') {
        return true;
    }
    if is_dir && name == "sessions" {
        let mut parts = parent.split(['/', '\\']).filter(|s| !s.is_empty());
        return matches!(
            (parts.next(), parts.next(), parts.next()),
            (Some("agents"), Some(id), None) if !id.is_empty()
        );
    }
    false
}

pub fn list(root: &Path, rel: &str) -> Result<FsList, String> {
    let dir = contained(root, &join_under(root, rel)?)?;
    let rd = fs::read_dir(&dir).map_err(|e| e.to_string())?;
    let path = guest_rel(rel).trim_end_matches('/').to_string();
    let mut entries = Vec::new();
    for ent in rd {
        let ent = ent.map_err(|e| e.to_string())?;
        let name = ent.file_name().to_string_lossy().into_owned();
        let is_dir = ent.metadata().map(|m| m.is_dir()).unwrap_or(false);
        if skip_name(&path, &name, is_dir) {
            continue;
        }
        entries.push(FsEntry { name, dir: is_dir });
        if entries.len() >= LIST_CAP {
            break;
        }
    }
    entries.sort_by(|a, b| b.dir.cmp(&a.dir).then(a.name.cmp(&b.name)));
    Ok(FsList {
        cwd: "/workspace".into(),
        path,
        entries,
    })
}

pub fn stat(root: &Path, rel: &str) -> Result<FsStat, String> {
    let joined = join_under(root, rel)?;
    let path = guest_rel(rel).to_string();
    if !joined.exists() {
        return Ok(FsStat {
            path,
            exists: false,
            dir: false,
        });
    }
    let path_c = contained(root, &joined)?;
    Ok(FsStat {
        path,
        exists: true,
        dir: path_c.is_dir(),
    })
}

pub fn read(root: &Path, rel: &str) -> Result<FsFile, String> {
    let path = contained(root, &join_under(root, rel)?)?;
    let meta = fs::metadata(&path).map_err(|e| e.to_string())?;
    if meta.is_dir() {
        return Err("is a directory".into());
    }
    let mut bytes = fs::read(&path).map_err(|e| e.to_string())?;
    let truncated = bytes.len() > READ_CAP;
    if truncated {
        bytes.truncate(READ_CAP);
    }
    let binary = bytes.iter().take(800).filter(|b| **b == 0).count() > 4
        || bytes
            .iter()
            .take(200)
            .any(|b| *b < 9 && *b != b'\t' && *b != b'\n' && *b != b'\r');
    let text = if binary {
        String::new()
    } else {
        String::from_utf8_lossy(&bytes).into_owned()
    };
    Ok(FsFile {
        path: guest_rel(rel).to_string(),
        binary,
        truncated,
        text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_under_rejects_dotdot() {
        let root = Path::new("/tmp/ws");
        assert!(join_under(root, "../etc/passwd").is_err());
        assert!(join_under(root, "agents/../../etc").is_err());
        assert_eq!(join_under(root, "").unwrap(), root);
        assert_eq!(
            join_under(root, "agents/rune").unwrap(),
            root.join("agents/rune")
        );
        assert_eq!(
            join_under(root, "/workspace/agents/rune").unwrap(),
            root.join("agents/rune")
        );
        assert_eq!(
            join_under(root, "workspace_notes").unwrap(),
            root.join("workspace_notes")
        );
        assert_eq!(join_under(root, "workspace").unwrap(), root);
    }

    #[test]
    fn list_sorts_dirs_first_and_skips_dotfiles() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("agents")).unwrap();
        fs::write(dir.path().join("AGENTS.md"), "x").unwrap();
        fs::write(dir.path().join(".secret"), "no").unwrap();
        let listed = list(dir.path(), "").unwrap();
        assert_eq!(listed.cwd, "/workspace");
        assert_eq!(listed.entries[0].name, "agents");
        assert!(listed.entries[0].dir);
        assert!(listed.entries.iter().any(|e| e.name == "AGENTS.md"));
        assert!(!listed.entries.iter().any(|e| e.name == ".secret"));
    }

    #[test]
    fn list_omits_bot_session_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let bot = dir.path().join("agents/rune");
        fs::create_dir_all(bot.join("sessions")).unwrap();
        fs::create_dir_all(bot.join("skills")).unwrap();
        fs::write(bot.join("sessions/main.jsonl"), "{}").unwrap();
        fs::create_dir(dir.path().join("sessions")).unwrap();
        let listed = list(dir.path(), "agents/rune").unwrap();
        assert!(listed.entries.iter().any(|e| e.name == "skills"));
        assert!(!listed.entries.iter().any(|e| e.name == "sessions"));
        let root = list(dir.path(), "").unwrap();
        assert!(root.entries.iter().any(|e| e.name == "sessions"));
    }

    #[test]
    fn stat_reports_missing_and_dirs() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("KNOWLEDGE.md"), "k").unwrap();
        fs::create_dir(dir.path().join("agents")).unwrap();
        let file = stat(dir.path(), "KNOWLEDGE.md").unwrap();
        assert!(file.exists && !file.dir);
        let folder = stat(dir.path(), "agents").unwrap();
        assert!(folder.exists && folder.dir);
        let missing = stat(dir.path(), "nope.md").unwrap();
        assert!(!missing.exists);
        assert!(stat(dir.path(), "../etc").is_err());
    }

    #[test]
    fn read_text_and_rejects_dir() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "hello").unwrap();
        fs::create_dir(dir.path().join("agents")).unwrap();
        let file = read(dir.path(), "AGENTS.md").unwrap();
        assert_eq!(file.text, "hello");
        assert!(!file.binary);
        assert!(!file.truncated);
        assert!(read(dir.path(), "agents").is_err());
    }
}
