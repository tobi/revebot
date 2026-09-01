//! End-to-end house effects: opt-in, always a real microVM, never host shells.
use super::*;
use crate::tools::Tools;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a real microVM"]
async fn homes_cwd_memory_and_profile_notifications_work_in_the_guest() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    crate::project::init(dir.path())?;
    let mut project = Project::load(dir.path())?;
    let runtime = Arc::get_mut(&mut project.runtime).expect("unshared runtime");
    let name = format!("reve-it-homes-{}", rand::random::<u32>());
    runtime.policy.name = Some(name.clone());
    runtime.policy.image = "alpine".into();
    runtime.policy.cpus = 1;
    runtime.policy.memory = 512;
    runtime.policy.provision = false;
    runtime.policy.secrets.clear();
    runtime.agent.model = None;
    runtime.policy.root_disk = 1024;
    println!("isolated VM integration instance: {name}");
    let policy = runtime.policy.clone();
    let mut remaining_ids = Vec::new();
    let house = Box::pin(House::boot(
        project,
        "127.0.0.1:0".into(),
        &crate::sandbox::Silent,
    ))
    .await?;
    let result: anyhow::Result<()> = Box::pin(async {
        let first = "reve";
        let second = house.create_bot(CreateSpec { name:"Miku".into(), title:"Music".into(), description:"Compose music".into(), soul:Some("MUSIC_ONLY_SOUL".into()), model:None, avatar:None }).await?;
        anyhow::ensure!(house.bot_soul(&second.id)?.contains("MUSIC_ONLY_SOUL"));
        let context = house.inner.context(first)?;
        let other = house.inner.context(&second.id)?;
        anyhow::ensure!(context.cwd() == format!("{}/workspace", context.home));
        let sandbox = house.sandbox();
        let setup = sandbox.exec("mkdir -p /workspace/projects/qmd/sub; printf ROOT_RULE > /AGENTS.md; printf SHARED_RULE > /workspace/AGENTS.md; printf QMD_RULE > /workspace/projects/qmd/AGENTS.md; printf SUB_RULE > /workspace/projects/qmd/sub/AGENTS.md", crate::sandbox::ExecOptions::default(), None).await?;
        anyhow::ensure!(setup.success);
        let changed = house.inner.change_directory(first, "/workspace/projects/qmd/sub").await?;
        for word in ["ROOT_RULE", "SHARED_RULE", "QMD_RULE", "SUB_RULE"] { anyhow::ensure!(changed.contains(word)); }
        anyhow::ensure!(changed.find("ROOT_RULE") < changed.find("SUB_RULE"));
        let tools = HouseTools { inner:Toolbox::for_context(sandbox.clone(), house.project().runtime_arc(), context.clone()), house:Arc::downgrade(&house.inner), bot_id:first.into() };
        let output = tools.invoke("bash", serde_json::json!({"command":"printf '%s\\n' \"$HOME\"; pwd"}).as_object().unwrap().clone(), None).await.map_err(anyhow::Error::msg)?;
        anyhow::ensure!(output.contains("/workspace/agents/reve\n/workspace/projects/qmd/sub"));
        tools.invoke("write", serde_json::json!({"path":"note.txt","content":"working output"}).as_object().unwrap().clone(), None).await.map_err(anyhow::Error::msg)?;
        anyhow::ensure!(std::fs::read_to_string(dir.path().join("workspace/projects/qmd/sub/note.txt"))? == "working output");
        anyhow::ensure!(other.cwd() == "/workspace/agents/miku/workspace");
        anyhow::ensure!(house.inner.change_directory(first, "/directory-that-does-not-exist").await.is_err());
        anyhow::ensure!(context.cwd() == "/workspace/projects/qmd/sub");
        let harness = house.inner.ready_harness(first)?;
        let restored = crate::working_directory::Context::new(first)?;
        restored.restore(harness.session(), MAIN_LANE, &sandbox).await?;
        anyhow::ensure!(restored.cwd() == context.cwd());
        house.inner.change_directory(first, &context.home).await?;
        anyhow::ensure!(!context.instructions().contains("QMD_RULE"));
        let request = memory::Request::parse(serde_json::json!({"target":"memory","action":"write","fact":"PRIVATE_MUSIC_FACT","tier":"profile"}).as_object().unwrap())?;
        house.inner.update_memory(&second.id, request.clone()).await?;
        let duplicate = house.inner.update_memory(&second.id, request).await?;
        anyhow::ensure!(duplicate.contains("Already remembered"));
        let roster = house.ready_profiles();
        anyhow::ensure!(!system_prompt(house.project(), &roster.iter().find(|p| p.id == first).unwrap().clone(), &roster).contains("PRIVATE_MUSIC_FACT"));
        let mut events = house.subscribe_house();
        let profile_path = format!("/workspace/agents/{}/profile.json", second.id);
        tools.invoke("write", serde_json::json!({"path":profile_path,"content":serde_json::json!({"id":second.id,"name":"Miku Renamed","title":"Composer","avatar":"blue:blob"}).to_string()}).as_object().unwrap().clone(), None).await.map_err(anyhow::Error::msg)?;
        let view = house.profile_views().into_iter().find(|p| p["id"] == second.id).unwrap();
        anyhow::ensure!(view["name"] == "Miku Renamed");
        anyhow::ensure!(matches!(events.recv().await?.kind, Kind::RosterChanged { .. }));
        // A stale plan must not overwrite a newer user edit.
        let relative = std::path::PathBuf::from("workspace/projects/qmd/sub/note.txt");
        let stale = files::Change { relative:relative.clone(), before:Some("older bytes".into()), after:"bad overwrite".into() };
        anyhow::ensure!(stale.apply(&sandbox).await.is_err());
        anyhow::ensure!(std::fs::read_to_string(dir.path().join(relative))? == "working output");
        let (name_patch, role_patch) = tokio::join!(
            house.patch_bot(&second.id, serde_json::json!({"title":"Composer"})),
            house.patch_bot(&second.id, serde_json::json!({"description":"Music only"})),
        );
        name_patch?; role_patch?;
        let profile = Profile::load_for(dir.path(), &second.id)?;
        anyhow::ensure!(profile.title == "Composer" && profile.description == "Music only");
        let source = house.inner.ready_harness(&second.id)?;
        let root = crate::entry::Entry::message(serde_json::json!({"role":"user","content":"root"}));
        let selected = crate::entry::Entry::message(serde_json::json!({"role":"user","content":"fork here"}))
            .with_parent(Some(root.id.clone()));
        let later = crate::entry::Entry::message(serde_json::json!({"role":"user","content":"do not copy"}))
            .with_parent(Some(selected.id.clone()));
        source.session().commit(
            crate::entry::Transaction::new()
                .with(crate::entry::Write::entry(root.clone()))
                .with(crate::entry::Write::entry(selected.clone()))
                .with(crate::entry::Write::entry(later.clone()))
                .with(crate::entry::Write::set(
                    crate::entry::Namespace::LaneLeaf,
                    MAIN_LANE,
                    Some(later.id.clone()),
                )),
        ).await?;
        let source_log = source.session().id().to_string();
        let forked = house.fork_chat(&second.id, &source_log, Some(selected.id.clone())).await?;
        anyhow::ensure!(forked.log_id != source_log && forked.previous_log_id == source_log);
        let fork_entries = house.transcript(&second.id).await?;
        anyhow::ensure!(fork_entries.iter().map(|entry| &entry.id).eq([&root.id, &selected.id]));
        anyhow::ensure!(house.prompt(&second.id, "stale source", Some(&source_log)).await.is_err());
        let fresh = house.new_chat(&second.id, &forked.log_id).await?;
        anyhow::ensure!(fresh.log_id != forked.log_id && fresh.previous_log_id == forked.log_id);
        anyhow::ensure!(house.transcript(&second.id).await?.is_empty());
        anyhow::ensure!(house.prompt(&second.id, "stale fork", Some(&forked.log_id)).await.is_err());
        let old_log = house.inner.ready_harness(&second.id)?.session().id().to_string();
        house.delete_bot(&second.id).await?;
        let make = |name: &str| CreateSpec {name:name.into(),title:String::new(),description:String::new(),soul:None,model:None,avatar:None};
        let replacement = house.create_bot(make("Miku")).await?;
        anyhow::ensure!(replacement.id == second.id);
        let new_log = house.inner.ready_harness(&replacement.id)?.session().id().to_string();
        anyhow::ensure!(new_log != old_log, "recreated bots must not reuse an old log identity");
        anyhow::ensure!(house.prompt(&replacement.id, "stale client", Some(&old_log)).await.is_err());
        let (a,b) = tokio::join!(house.create_bot(make("Worker")), house.create_bot(make("Worker")));
        let a=a?; let b=b?;
        anyhow::ensure!(a.id != b.id);
        house.delete_bot(first).await?;
        house.delete_bot(&b.id).await?;
        let (a_deleted,miku_deleted) = tokio::join!(house.delete_bot(&a.id), house.delete_bot(&replacement.id));
        anyhow::ensure!(a_deleted.is_ok() != miku_deleted.is_ok(), "only one concurrent deletion may pass the last-bot floor");
        remaining_ids = house.ready_profiles().into_iter().map(|p|p.id).collect();
        anyhow::ensure!(remaining_ids.len() == 1 && remaining_ids[0] != first);
        Ok(())
    }).await;
    let stopped = house.shutdown().await;
    drop(house);
    if let Err(error) = result {
        let _ = microsandbox::Sandbox::remove(&name).await;
        return Err(error);
    }
    stopped?;
    let mut project = Project::load(dir.path())?;
    let runtime = Arc::get_mut(&mut project.runtime).unwrap();
    runtime.policy = policy;
    runtime.agent.model = None;
    let reopened = Box::pin(House::boot(
        project,
        "127.0.0.1:0".into(),
        &crate::sandbox::Silent,
    ))
    .await?;
    let actual_ids: Vec<_> = reopened
        .ready_profiles()
        .into_iter()
        .map(|p| p.id)
        .collect();
    let stopped = reopened.shutdown().await;
    let removed = microsandbox::Sandbox::remove(&name).await;
    anyhow::ensure!(
        actual_ids == remaining_ids,
        "restart must not resurrect the deleted reve"
    );
    stopped?;
    removed?;
    Ok(())
}
