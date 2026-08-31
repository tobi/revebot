//! The plugins skill's guest-effect example, exercised only with a real VM.
use std::path::Path;
use std::sync::Arc;

use reve::lua::Runtime;
use reve::sandbox::{Policy, Sandbox, Silent};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a real microVM"]
async fn documented_workspace_tool_reads_a_note_inside_the_microvm() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(workspace.join("plugins")).unwrap();
    let skill = include_str!("../src/templates/plugins_skill.md");
    let example = skill
        .split("```lua\n")
        .nth(1)
        .unwrap()
        .split("```")
        .next()
        .unwrap();
    std::fs::write(workspace.join("plugins/read_note.lua"), example).unwrap();
    let filename = "a note with 'quotes'.md";
    std::fs::write(workspace.join(filename), "guest-visible note\n").unwrap();
    let mut runtime = Runtime::new().unwrap();
    runtime
        .load_workspace_tools(dir.path(), Path::new("workspace/plugins"), None)
        .unwrap();
    let name = format!("reve-it-workspace-lua-{}", rand::random::<u32>());
    let sandbox = Arc::new(
        Box::pin(Sandbox::start(
            Policy {
                name: Some(name.clone()),
                image: "alpine".into(),
                cpus: 1,
                memory: 512,
                provision: false,
                ..Policy::default()
            },
            &workspace,
            dir.path().join(".reve"),
            &Silent,
        ))
        .await
        .expect("mandatory VM boots"),
    );
    let result = runtime
        .call_tool(
            "read_note",
            serde_json::json!({"path": filename})
                .as_object()
                .unwrap()
                .clone(),
            sandbox.clone(),
        )
        .await;
    let stopped = sandbox.stop().await;
    let _ = microsandbox::Sandbox::remove(&name).await;
    assert_eq!(result.unwrap(), "guest-visible note\n");
    stopped.unwrap();
}
