//! Files runner: `revebot init` a temp house and grade the tree.

use std::collections::BTreeMap;

use tempfile::TempDir;

use crate::project;

use super::case::Case;
use super::grade::Trace;

pub fn run_keep(case: &Case) -> anyhow::Result<(Trace, TempDir)> {
    let dir = TempDir::new()?;
    project::init(dir.path())?;
    for (rel, body) in &case.setup_files {
        let path = dir.path().join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, body)?;
    }
    let trace = Trace {
        outcome: Some("completed".into()),
        extras: BTreeMap::new(),
        root: Some(dir.path().to_path_buf()),
        ..Default::default()
    };
    Ok((trace, dir))
}
