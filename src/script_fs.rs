//! Read bot-authored source beneath a caller-chosen house directory.
//!
//! Walk directory descriptors with O_NOFOLLOW, not canonicalize-then-open:
//! checking a pathname and later reading it leaves a symlink-swap race. This
//! host helper is not exposed to Lua and never executes commands.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use rustix::fs::{Dir, Mode, OFlags, openat};

const MAX_SCRIPT_BYTES: u64 = 1024 * 1024;

pub(crate) fn open_dir(root: &Path, relative: &Path) -> io::Result<File> {
    let names = relative
        .components()
        .map(|component| match component {
            Component::Normal(name) => Ok(name),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected a relative path without traversal",
            )),
        })
        .collect::<io::Result<Vec<_>>>()?;
    let mut dir = File::open(root)?;
    for name in names {
        dir = openat(
            &dir,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?
        .into();
    }
    Ok(dir)
}

/// Host scaffold/session setup only, not a Lua capability. Every component is
/// created/opened relative to a held directory descriptor without symlinks.
pub(crate) fn ensure_dir(root: &Path, relative: &Path) -> io::Result<File> {
    let mut dir = File::open(root)?;
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsafe relative directory",
            ));
        };
        match rustix::fs::mkdirat(&dir, name, Mode::from_bits_truncate(0o755)) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => {}
            Err(error) => return Err(error.into()),
        }
        dir = openat(
            &dir,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?
        .into();
    }
    Ok(dir)
}

pub(crate) fn create_new(root: &Path, relative: &Path) -> io::Result<File> {
    let dir = ensure_dir(
        root,
        relative
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing parent"))?,
    )?;
    let name = relative
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing name"))?;
    Ok(openat(
        &dir,
        name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_bits_truncate(0o644),
    )?
    .into())
}

/// List actual child directories, refusing symlinks rather than following them
/// into a host tree. Used when discovering per-bot script directories.
pub(crate) fn child_dirs(root: &Path, relative: &Path) -> io::Result<Vec<PathBuf>> {
    let dir = match open_dir(root, relative) {
        Ok(dir) => dir,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut dirs = Vec::new();
    for entry in Dir::read_from(&dir)? {
        let entry = entry?;
        let name = OsStr::from_bytes(entry.file_name().to_bytes());
        if name == "." || name == ".." {
            continue;
        }
        let stat = rustix::fs::statat(&dir, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)?;
        match rustix::fs::FileType::from_raw_mode(stat.st_mode) {
            rustix::fs::FileType::Symlink => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "symlinked bot directories are not allowed",
                ));
            }
            rustix::fs::FileType::Directory => dirs.push(relative.join(name)),
            _ => {}
        }
    }
    dirs.sort();
    Ok(dirs)
}

/// Read a regular file without following any workspace symlink.
pub(crate) fn read_text(root: &Path, relative: &Path, limit: u64) -> io::Result<String> {
    let parent = relative
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing parent"))?;
    let name = relative
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing filename"))?;
    let dir = open_dir(root, parent)?;
    let file: File = openat(
        &dir,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )?
    .into();
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected regular file",
        ));
    }
    let mut text = String::new();
    file.take(limit + 1).read_to_string(&mut text)?;
    if text.len() as u64 > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file exceeds read limit",
        ));
    }
    Ok(text)
}

pub(crate) fn files(root: &Path, relative: &Path) -> io::Result<Vec<PathBuf>> {
    let dir = match open_dir(root, relative) {
        Ok(dir) => dir,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut files = Vec::new();
    for entry in Dir::read_from(&dir)? {
        let entry = entry?;
        let name = OsStr::from_bytes(entry.file_name().to_bytes());
        if name == "." || name == ".." {
            continue;
        }
        let stat = rustix::fs::statat(&dir, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)?;
        match rustix::fs::FileType::from_raw_mode(stat.st_mode) {
            rustix::fs::FileType::RegularFile => files.push(relative.join(name)),
            rustix::fs::FileType::Symlink => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "symlinked file is not allowed",
                ));
            }
            _ => {}
        }
    }
    files.sort();
    Ok(files)
}

pub(crate) fn scripts(root: &Path, relative: &Path) -> io::Result<Vec<(PathBuf, String)>> {
    let dir = match open_dir(root, relative) {
        Ok(dir) => dir,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut names = Vec::new();
    for entry in Dir::read_from(&dir)? {
        let entry = entry?;
        let name = OsStr::from_bytes(entry.file_name().to_bytes());
        if Path::new(name).extension().is_some_and(|ext| ext == "lua") {
            names.push(name.to_owned());
        }
    }
    names.sort();
    names
        .into_iter()
        .map(|name| {
            // NONBLOCK makes a FIFO/device fail validation rather than hanging the
            // host loader. Symlinks are refused even when they stay in the house.
            let file: File = openat(
                &dir,
                &name,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
                Mode::empty(),
            )?
            .into();
            if !file.metadata()?.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Lua source must be a regular file",
                ));
            }
            let mut source = String::new();
            file.take(MAX_SCRIPT_BYTES + 1)
                .read_to_string(&mut source)?;
            if source.len() as u64 > MAX_SCRIPT_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Lua source exceeds 1 MiB",
                ));
            }
            Ok((root.join(relative).join(name), source))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn scripts_are_sorted_and_missing_directories_are_empty() {
        let root = tempfile::tempdir().unwrap();
        assert!(
            scripts(root.path(), Path::new("workspace/plugins"))
                .unwrap()
                .is_empty()
        );
        let dir = root.path().join("workspace/plugins");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("b.lua"), "b").unwrap();
        std::fs::write(dir.join("a.lua"), "a").unwrap();
        std::fs::write(dir.join("ignored.md"), "ignored").unwrap();
        let scripts = scripts(root.path(), Path::new("workspace/plugins")).unwrap();
        assert_eq!(
            scripts
                .iter()
                .map(|(_, body)| body.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
    }

    #[test]
    fn script_and_ancestor_symlinks_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let dir = root.path().join("workspace/plugins");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(outside.path().join("secret.lua"), "HOST_ONLY").unwrap();
        symlink(outside.path().join("secret.lua"), dir.join("escape.lua")).unwrap();
        assert!(scripts(root.path(), Path::new("workspace/plugins")).is_err());
        std::fs::remove_file(dir.join("escape.lua")).unwrap();
        std::fs::remove_dir(&dir).unwrap();
        symlink(outside.path(), &dir).unwrap();
        assert!(scripts(root.path(), Path::new("workspace/plugins")).is_err());
        // Missing child does not hide a symlinked ancestor.
        assert!(scripts(root.path(), Path::new("workspace/plugins/missing")).is_err());
    }

    #[test]
    fn traversal_nonfiles_and_oversized_sources_are_refused() {
        let root = tempfile::tempdir().unwrap();
        for path in ["../escape", "/absolute", "workspace/../plugins"] {
            // Make the normal prefix exist so traversal cannot hide behind ENOENT.
            std::fs::create_dir_all(root.path().join("workspace")).unwrap();
            assert!(scripts(root.path(), Path::new(path)).is_err());
        }
        let dir = root.path().join("plugins");
        std::fs::create_dir_all(dir.join("directory.lua")).unwrap();
        assert!(scripts(root.path(), Path::new("plugins")).is_err());
        std::fs::remove_dir(dir.join("directory.lua")).unwrap();
        std::fs::write(
            dir.join("huge.lua"),
            vec![b' '; MAX_SCRIPT_BYTES as usize + 1],
        )
        .unwrap();
        assert!(scripts(root.path(), Path::new("plugins")).is_err());
    }
}
