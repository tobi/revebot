use std::error::Error;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=public");

    let root = PathBuf::from("public");
    let mut files = Vec::new();
    collect_assets(&root, &root, &mut files)?;
    files.sort();

    let output_dir = PathBuf::from(std::env::var("OUT_DIR")?);
    let mut rust = String::from("pub static PUBLIC_ASSETS: &[(&str, &[u8], &str)] = &[\n");
    let mut manifest = String::from("{\n");

    for (position, path) in files.iter().enumerate() {
        let relative = path.strip_prefix(&root)?;
        let relative = relative
            .to_str()
            .ok_or_else(|| std::io::Error::other("public asset path is not UTF-8"))?;
        if !relative.is_ascii() {
            return Err(std::io::Error::other("public asset paths must be ASCII").into());
        }
        let route = format!("/{}", relative.replace('\\', "/"));
        let mime = content_type(path);
        let bytes = fs::read(path)?;
        let revision = fnv1a(&bytes);

        writeln!(
            rust,
            "    ({route:?}, include_bytes!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/public\", {route:?})), {mime:?}),"
        )?;
        if position > 0 {
            manifest.push_str(",\n");
        }
        write!(
            manifest,
            "  {route:?}: {{\"bytes\":{},\"contentType\":{mime:?},\"revision\":\"{revision:016x}\"}}",
            bytes.len()
        )?;
    }

    rust.push_str("];\n");
    manifest.push_str("\n}\n");
    fs::write(output_dir.join("public_assets.rs"), rust)?;
    fs::write(output_dir.join("asset-manifest.json"), manifest)?;
    Ok(())
}

fn collect_assets(
    root: &Path,
    directory: &Path,
    files: &mut Vec<PathBuf>,
) -> Result<(), Box<dyn Error>> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let relative = path.strip_prefix(root)?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_assets(root, &path, files)?;
        } else if file_type.is_file()
            && path.file_name().and_then(|name| name.to_str()) != Some("AGENTS.md")
            && relative != Path::new("index.html")
        {
            files.push(path);
        }
    }
    Ok(())
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("css") => "text/css; charset=utf-8",
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("webmanifest") => "application/manifest+json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}
