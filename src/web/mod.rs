//! Embedded browser assets generated from `public/` at build time.

include!(concat!(env!("OUT_DIR"), "/public_assets.rs"));

#[must_use]
pub fn page(token: &str, bind: &str) -> String {
    include_str!("../../public/index.html")
        .replace("{{TOKEN}}", &escape_attribute(token))
        .replace("{{BIND}}", &escape_attribute(bind))
}

fn escape_attribute(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

#[must_use]
pub fn asset(path: &str) -> Option<(&'static [u8], &'static str)> {
    if path == "/asset-manifest.json" {
        return Some((
            include_bytes!(concat!(env!("OUT_DIR"), "/asset-manifest.json")),
            "application/json; charset=utf-8",
        ));
    }
    PUBLIC_ASSETS
        .iter()
        .find(|(route, _, _)| *route == path)
        .map(|(_, bytes, content_type)| (*bytes, *content_type))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_is_a_small_shell_over_public_modules() {
        let html = page("test-token", "127.0.0.1:7420");
        assert!(html.contains("test-token"));
        assert!(html.contains("/css/app.css"));
        assert!(html.contains("/js/app.mjs"));
        assert!(html.contains("<reve-feed"));
        assert!(html.contains("<reve-composer"));
        assert!(html.contains("<reve-autocomplete"));
        assert!(html.contains("/js/register-service-worker.mjs"));
    }

    #[test]
    fn page_escapes_runtime_metadata() {
        let html = page("token\"<", "bind&>");
        assert!(html.contains("token&quot;&lt;"));
        assert!(html.contains("bind&amp;&gt;"));
        assert!(!html.contains("token\"<"));
    }

    #[test]
    fn generated_manifest_covers_authored_assets_but_not_rules() {
        assert!(asset("/js/app.mjs").is_some());
        assert!(asset("/css/app.css").is_some());
        assert!(asset("/AGENTS.md").is_none());
        let (manifest, content_type) = asset("/asset-manifest.json").expect("generated manifest");
        assert_eq!(content_type, "application/json; charset=utf-8");
        let manifest = std::str::from_utf8(manifest).expect("UTF-8 manifest");
        assert!(manifest.contains("/js/app.mjs"));
        assert!(!manifest.contains("AGENTS.md"));
        let (service_worker, _) = asset("/sw.js").expect("service worker");
        let service_worker = std::str::from_utf8(service_worker).expect("UTF-8 service worker");
        assert!(service_worker.contains("/js/conversation-commands.mjs"));
        assert!(service_worker.contains("/js/register-service-worker.mjs"));
        assert!(!manifest.contains("/index.html"));
    }
}
