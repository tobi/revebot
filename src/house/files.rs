//! Planned, compare-before-replace workspace writes. Effects are guest-only.
use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};

use crate::sandbox::{ExecOptions, Sandbox};
use sha2::{Digest, Sha256};

pub const MAX_FILE: u64 = 65536;

pub fn read_optional(root: &Path, relative: &Path) -> anyhow::Result<Option<String>> {
    match crate::script_fs::read_text(root, relative, MAX_FILE) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[derive(Debug, Clone)]
pub struct Change {
    pub relative: PathBuf,
    pub before: Option<String>,
    pub after: String,
}

impl Change {
    pub fn command(&self) -> anyhow::Result<String> {
        if self.after.len() as u64 > MAX_FILE {
            anyhow::bail!("memory/profile file exceeds 64 KiB; archive old material first");
        }
        if !self.relative.starts_with("workspace")
            || self
                .relative
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            anyhow::bail!("write is not a workspace-relative path");
        }
        let path = format!("/{}", self.relative.display());
        let quote = |s: &str| shell_words::quote(s).into_owned();
        let mut ancestors = PathBuf::new();
        let mut checks = String::new();
        for component in self.relative.components() {
            ancestors.push(component.as_os_str());
            let _ = writeln!(
                checks,
                "[ ! -L {} ] || {{ echo 'symlink refused' >&2; exit 73; }}",
                quote(&format!("/{}", ancestors.display()))
            );
        }
        let compare = match &self.before {
            Some(text) => format!(
                "[ -f \"$p\" ] && [ \"$(sha256sum -- \"$p\" | cut -d ' ' -f 1)\" = {} ]",
                quote(
                    &Sha256::digest(text.as_bytes())
                        .iter()
                        .fold(String::new(), |mut out, b| {
                            let _ = write!(out, "{b:02x}");
                            out
                        })
                )
            ),
            None => "[ ! -e \"$p\" ] && [ ! -L \"$p\" ]".into(),
        };
        Ok(format!(
            "set -eu\np={}\n{checks}d=${{p%/*}}\nmkdir -p -- \"$d\"\ntmp=$(mktemp \"$d/.reve-write.XXXXXX\")\ntrap 'rm -f -- \"$tmp\"' EXIT\nprintf %s {} > \"$tmp\"\n{compare} || {{ echo 'file changed since it was read; retry' >&2; exit 73; }}\nmv -f -- \"$tmp\" \"$p\"\n",
            quote(&path),
            quote(&self.after)
        ))
    }

    pub async fn apply(&self, sandbox: &Sandbox) -> anyhow::Result<()> {
        let output = sandbox
            .exec(&self.command()?, ExecOptions::default(), None)
            .await?;
        if !output.success || output.cancelled {
            anyhow::bail!("workspace write failed: {}", output.stderr.trim());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn writes_are_guest_scoped_quoted_and_compared() {
        let change = Change {
            relative: "workspace/agents/miku/memory/profile.md".into(),
            before: Some("old".into()),
            after: "a 'quote' and $(not a command)".into(),
        };
        let command = change.command().unwrap();
        assert!(command.contains("sha256sum"));
        assert!(command.contains("mktemp"));
        assert!(command.contains("mv -f"));
        assert!(command.contains(&shell_words::quote(&change.after).to_string()));
        for path in [
            "../host",
            "workspace/../host",
            "/workspace/file",
            "host/file",
        ] {
            assert!(
                Change {
                    relative: path.into(),
                    before: None,
                    after: "x".into()
                }
                .command()
                .is_err()
            );
        }
    }
}
