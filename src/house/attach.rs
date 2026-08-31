//! Chat attachments land in a unique guest folder and are named in the message.
//!
//! The UI uploads bytes; we write them under `workspace/tmp/{id}/{name}` so the
//! bind-mounted workspace (guest `/workspace`) keeps them across VM idle stops.
//! Guest `/tmp` is ephemeral. The message refers to `/workspace/tmp/{id}/{name}`
//! and the page renders that tag as a pill.

use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::ids;
use crate::script_fs;

pub const MAX_BYTES: u64 = 25 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Saved {
    pub id: String,
    pub name: String,
    pub path: String,
    pub bytes: u64,
    pub mime: String,
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
    script_fs::write_new_bytes(root, &relative, bytes)?;
    Ok(Saved {
        path: guest_path(&id, &name),
        mime: mime_of(&name, mime),
        bytes: bytes.len() as u64,
        id,
        name,
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
    let relative = relative(id, &name);
    let bytes = script_fs::read_bytes(root, &relative, MAX_BYTES)?;
    let saved = Saved {
        id: id.into(),
        path: guest_path(id, &name),
        mime: mime_of(&name, None),
        bytes: bytes.len() as u64,
        name,
    };
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
