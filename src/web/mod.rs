//! Embedded local UI. One HTML file, no npm.

pub fn page(token: &str, bind: &str) -> String {
    include_str!("index.html")
        .replace("{{TOKEN}}", token)
        .replace("{{BIND}}", bind)
        .replace("/*{{BLOUB}}*/", include_str!("bloub.js"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_embeds_bloub_and_shadcn_bubbles() {
        let html = page("test-token", "127.0.0.1:7420");
        assert!(html.contains("test-token"));
        assert!(html.contains("const BLOUB"));
        assert!(html.contains("bubble-content"));
        assert!(html.contains("function appendInline"));
        assert!(html.contains("::-webkit-scrollbar"));
        assert!(html.contains("class=\"fence\"") || html.contains("\"fence\""));
        assert!(html.contains("contentParts"));
        assert!(html.contains("function addTool"));
        assert!(html.contains("function paintLog"));
        assert!(html.contains("function ensureStream"));
        assert!(html.contains("stickToBottom") || html.contains("nearBottom"));
        assert!(html.contains("scroll-fab"));
        assert!(html.contains("before="));
        assert!(html.contains("openSheet"));
        assert!(html.contains("swatches"));
        assert!(html.contains("function acUpdate"));
        assert!(html.contains("function routeBot"));
        assert!(html.contains("function setRoute"));
        assert!(html.contains("location.hash"));
        assert!(html.contains("function isUserNotice"));
        assert!(html.contains("function noticeText"));
        assert!(html.contains("sawTool"));
        assert!(html.contains("think-dot"));
        assert!(html.contains("--blink-dur"));
        assert!(html.contains("function setWorking"));
        assert!(html.contains("hiddenTool(name)"));
        assert!(html.contains("user_notice"));
        assert!(html.contains("AskUserForSecret"));
        assert!(html.contains("function addSecretAsk"));
        assert!(html.contains("function buildSecretForm"));
        assert!(html.contains("/api/bots/"));
        assert!(html.contains("/skills"));
        assert!(html.contains("id=\"ac\""));
        assert!(!html.contains("prompt(\"Name"));
        assert!(html.contains("tool-name"));
        assert!(html.contains("Plugins"));
        assert!(html.contains("Search"));
        assert!(
            !html.contains("JSON.stringify(content)"),
            "assistant content parts must be parsed, not stringified"
        );
        assert!(
            !html.contains("streamTarget"),
            "empty-bubble streamTarget dumped tokens onto the previous message"
        );
    }
}
