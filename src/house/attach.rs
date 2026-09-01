//! Chat attachments land in a unique guest folder and are named in the message.
//!
//! The UI uploads bytes; we write them under `workspace/tmp/{id}/{name}` so the
//! bind-mounted workspace (guest `/workspace`) keeps them across VM idle stops.
//! Guest `/tmp` is ephemeral. The message refers to `/workspace/tmp/{id}/{name}`
//! and the page renders that tag as a pill.

use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::Digest as _;

use crate::ids;
use crate::script_fs;

pub const MAX_BYTES: u64 = 25 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Saved {
    pub id: String,
    pub name: String,
    pub path: String,
    pub bytes: u64,
    pub mime: String,
}

#[derive(Debug, Clone)]
pub struct Prepared {
    pub saved: Saved,
    pub sha256: String,
}

pub fn sanitize_name(name: &str) -> String {
    let base = Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("file");
    let mut out = String::new();
    for c in base.chars() {
        if c.is_ascii_alphanumeric()
            || matches!(c, '.' | '-' | '_')
            || (!c.is_ascii() && !c.is_control())
        {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches('.').trim_matches('_');
    let mut name: String = trimmed.chars().take(120).collect();
    if name.is_empty() {
        name = "file".into();
    }
    name
}

pub fn valid_id(id: &str) -> bool {
    id.len() == 36 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

pub fn relative(id: &str, name: &str) -> PathBuf {
    PathBuf::from("workspace/tmp").join(id).join(name)
}
fn metadata_relative(id: &str) -> PathBuf {
    PathBuf::from("workspace/tmp")
        .join(id)
        .join(".attachment.json")
}

pub fn guest_path(id: &str, name: &str) -> String {
    format!("/workspace/tmp/{id}/{name}")
}

pub fn looks_like_mime(value: &str) -> bool {
    let mut parts = value.split('/');
    let (Some(typ), Some(sub), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !typ.is_empty()
        && !sub.is_empty()
        && typ
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b == b'+' || b == b'-')
        && sub
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'+' | b'-'))
        && value.len() < 80
}

pub fn mime_of(name: &str, hinted: Option<&str>) -> String {
    if let Some(h) = hinted.filter(|h| looks_like_mime(h)) {
        return h.to_string();
    }
    match Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "txt" | "md" | "csv" => "text/plain",
        "json" => "application/json",
        "html" => "text/html",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        _ => "application/octet-stream",
    }
    .into()
}

pub fn tag(saved: &Saved) -> String {
    format!(
        "<file path=\"{}\" name=\"{}\" mime=\"{}\" bytes=\"{}\"/>",
        saved.path, saved.name, saved.mime, saved.bytes
    )
}

pub fn save(root: &Path, name: &str, bytes: &[u8], mime: Option<&str>) -> io::Result<Saved> {
    if bytes.len() as u64 > MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file exceeds 25 MiB",
        ));
    }
    let name = sanitize_name(name);
    let id = ids::uuid_v7(ids::now_ms());
    let relative = relative(&id, &name);
    let saved = Saved {
        path: guest_path(&id, &name),
        mime: mime_of(&name, mime),
        bytes: bytes.len() as u64,
        id,
        name,
    };
    script_fs::write_new_bytes(root, &relative, bytes)?;
    let metadata = serde_json::to_vec(&saved).map_err(io::Error::other)?;
    script_fs::write_new_bytes(root, &metadata_relative(&saved.id), &metadata)?;
    Ok(saved)
}

pub fn prepare(root: &Path, workspace_relative: &Path, mime: &str) -> io::Result<Prepared> {
    if !looks_like_mime(mime) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid attachment mimetype",
        ));
    }
    let name = workspace_relative
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "missing attachment filename")
        })?;
    let bytes = script_fs::read_bytes(&root.join("workspace"), workspace_relative, MAX_BYTES)?;
    let sha256 =
        sha2::Sha256::digest(&bytes)
            .iter()
            .fold(String::with_capacity(64), |mut output, byte| {
                let _ = write!(output, "{byte:02x}");
                output
            });
    Ok(Prepared {
        saved: save(root, name, &bytes, Some(mime))?,
        sha256,
    })
}

pub fn read(root: &Path, id: &str, name: &str) -> io::Result<(Saved, Vec<u8>)> {
    if !valid_id(id) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "bad id"));
    }
    let name = name.to_string();
    if sanitize_name(&name) != name {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "bad name"));
    }
    let metadata = script_fs::read_bytes(root, &metadata_relative(id), 4096)?;
    let saved: Saved = serde_json::from_slice(&metadata).map_err(io::Error::other)?;
    if saved.id != id || saved.name != name || saved.path != guest_path(id, &name) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "attachment metadata does not match route",
        ));
    }
    let bytes = script_fs::read_bytes(root, &relative(id, &name), MAX_BYTES)?;
    if saved.bytes != bytes.len() as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "attachment metadata size does not match file",
        ));
    }
    Ok((saved, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_basenames_without_traversal() {
        assert_eq!(sanitize_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_name("/tmp/x y.png"), "x_y.png");
        assert_eq!(sanitize_name(""), "file");
        assert_eq!(sanitize_name("..."), "file");
        assert_eq!(sanitize_name("ok-file_1.PNG"), "ok-file_1.PNG");
    }

    #[test]
    fn save_writes_under_workspace_tmp_id_and_tags_the_guest_path() {
        let root = tempfile::tempdir().unwrap();
        let saved = save(root.path(), "photo.png", b"hello", Some("image/png")).unwrap();
        assert!(valid_id(&saved.id));
        assert_eq!(saved.name, "photo.png");
        assert_eq!(saved.mime, "image/png");
        assert_eq!(saved.bytes, 5);
        assert_eq!(saved.path, guest_path(&saved.id, "photo.png"));
        assert!(saved.path.starts_with("/workspace/tmp/"));
        let disk = root
            .path()
            .join("workspace/tmp")
            .join(&saved.id)
            .join("photo.png");
        assert_eq!(std::fs::read(&disk).unwrap(), b"hello");
        assert_eq!(
            tag(&saved),
            format!(
                "<file path=\"{}\" name=\"photo.png\" mime=\"image/png\" bytes=\"5\"/>",
                saved.path
            )
        );
        let (again, bytes) = read(root.path(), &saved.id, "photo.png").unwrap();
        assert_eq!(again, saved);
        assert_eq!(bytes, b"hello");
    }

    #[test]
    fn prepare_copies_workspace_files_with_persisted_mimetype() {
        let root = tempfile::tempdir().unwrap();
        let source = Path::new("agents/miku/workspace/report.data");
        std::fs::create_dir_all(root.path().join("workspace/agents/miku/workspace")).unwrap();
        std::fs::write(root.path().join("workspace").join(source), b"# Report\n").unwrap();
        let prepared = prepare(root.path(), source, "text/markdown").unwrap();
        assert_eq!(prepared.saved.name, "report.data");
        assert_eq!(prepared.saved.mime, "text/markdown");
        assert_eq!(prepared.sha256.len(), 64);
        let (saved, bytes) = read(root.path(), &prepared.saved.id, &prepared.saved.name).unwrap();
        assert_eq!(saved, prepared.saved);
        assert_eq!(bytes, b"# Report\n");
    }

    #[test]
    fn read_refuses_traversal_and_bad_ids() {
        let root = tempfile::tempdir().unwrap();
        assert!(read(root.path(), "not-a-uuid", "x").is_err());
        let saved = save(root.path(), "a.txt", b"a", None).unwrap();
        assert!(read(root.path(), &saved.id, "../a.txt").is_err());
    }

    #[test]
    fn oversize_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let big = vec![
            0u8;
            usize::try_from(MAX_BYTES)
                .unwrap_or(usize::MAX)
                .saturating_add(1)
        ];
        assert!(save(root.path(), "big.bin", &big, None).is_err());
    }
}
