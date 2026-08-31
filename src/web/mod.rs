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
        assert!(html.contains("sawTool = false"));
        assert!(html.contains("think-dot"));
        assert!(html.contains("--blink-dur"));
        assert!(html.contains("function setBusy"));
        assert!(html.contains("function stickBottom"));
        assert!(
            html.contains("if (pin) stickBottom()"),
            "message_update/user_notice must keep if (pin) or the page script is invalid"
        );
        assert!(html.contains("height: 36px"));
        assert!(html.contains("function openCtx"));
        assert!(html.contains("function loadTree"));
        assert!(html.contains("function previewFile"));
        assert!(html.contains("function treeBranch"));
        assert!(html.contains("function expandRow"));
        assert!(html.contains("function collapseTree"));
        assert!(html.contains("function showSide"));
        assert!(html.contains("function revealFile"));
        assert!(html.contains("function inspectHover"));
        assert!(html.contains("function unwrapFileRef"));
        assert!(html.contains("function asWorkspacePath"));
        assert!(html.contains("/api/fs"));
        assert!(html.contains("/api/fs/file"));
        assert!(html.contains("/api/fs/stat"));
        assert!(html.contains("id=\"tree\""));
        assert!(html.contains("role=\"tree\""));
        assert!(html.contains("id=\"tree-collapse\""));
        assert!(
            html.contains(".tree-row.open + .tree-kids") && html.contains(".tree-kids[hidden]"),
            "collapsed folders must actually hide their children"
        );
        assert!(html.contains("data-pane=\"files\""));
        assert!(html.contains("id=\"pane-files\""));
        assert!(html.contains("file-ref"));
        assert!(html.contains("ctx-edit"));
        assert!(html.contains("optgroup"));
        assert!(html.contains("function paintBusy"));
        assert!(html.contains("bot_busy"));
        assert!(html.contains("/api/events"));
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
