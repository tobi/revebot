//! Capability regression tests. No VM, no host shell, no fake shell transport.
use super::*;

fn source(root: &Path, relative: &str, body: impl AsRef<[u8]>) -> PathBuf {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, body).unwrap();
    path
}

#[test]
fn workspace_lua_has_no_ambient_host_authority() {
    let dir = tempfile::tempdir().unwrap();
    source(
        dir.path(),
        "workspace/plugins/check.lua",
        r"
        for _, name in ipairs({ 'io', 'os', 'package', 'require', 'debug',
            'load', 'loadfile', 'dofile', 'print', 'warn', 'collectgarbage',
            'agent', 'sandbox', 'HOST_SECRET', 'saved_open' }) do
            assert(rawget(_G, name) == nil, name .. ' leaked')
        end
        assert(string.dump == nil)
        local co = coroutine.create(function()
            assert(io == nil and os == nil and loadfile == nil)
        end)
        local ok, err = coroutine.resume(co)
        assert(ok, err)
        tool('pure', { run = function(args)
            local values = {3, 1, 2}; table.sort(values)
            return table.concat(values, ',') .. ':' .. utf8.len('hello')
        end })
    ",
    );
    let mut rt = Runtime::new().unwrap();
    rt.lua
        .load("HOST_SECRET = 'host'; saved_open = io.open")
        .exec()
        .unwrap();
    rt.load_workspace_tools(dir.path(), Path::new("workspace/plugins"), None)
        .unwrap();
    let def = rt.tool("pure").unwrap();
    let f: mlua::Function = def.lua.registry_value(&def.key).unwrap();
    assert_eq!(
        f.call::<String>(def.lua.create_table().unwrap()).unwrap(),
        "1,2,3:5"
    );
    assert!(
        rt.lua
            .load("return io.open == saved_open")
            .eval::<bool>()
            .unwrap()
    );
}

#[test]
fn workspace_scripts_cannot_read_or_write_host_sentinels() {
    let host = tempfile::tempdir().unwrap();
    let house = host.path().join("house");
    crate::project::init(&house).unwrap();
    let secret = host.path().join("sentinel");
    let dest = host.path().join("copied");
    std::fs::write(&secret, "HARMLESS_HOST_SENTINEL").unwrap();
    let secret = serde_json::to_string(&secret).unwrap();
    let dest_lua = serde_json::to_string(&dest).unwrap();
    for body in [
        format!(
            "local f = io.open({secret}, 'r'); local o = io.open({dest_lua}, 'w'); o:write(f:read('*a'))"
        ),
        format!("local o = io.open({dest_lua}, 'w'); o:write('changed')"),
        format!("dofile({secret})"),
        format!("loadfile({secret})()"),
        "local x = os.getenv('HOME')".into(),
        "require('io')".into(),
        "local x = package.loaded.io".into(),
        "load('return io')()".into(),
    ] {
        source(&house, "workspace/plugins/escape.lua", &body);
        let error = crate::project::Project::load(&house)
            .err()
            .expect("must reject host access");
        assert!(error.to_string().contains("escape.lua"), "{error}");
        assert!(!dest.exists(), "host output appeared for {body}");
    }
}

#[test]
fn workspace_mutation_cannot_change_the_host_lua_state() {
    let dir = tempfile::tempdir().unwrap();
    source(
        dir.path(),
        "workspace/plugins/change.lua",
        "table.sort = function() error('changed') end; tool('ok', {run=function() return 'ok' end})",
    );
    let mut rt = Runtime::new().unwrap();
    rt.load_workspace_tools(dir.path(), Path::new("workspace/plugins"), None)
        .unwrap();
    assert_eq!(
        rt.lua
            .load("local a={2,1}; table.sort(a); return a[1]")
            .eval::<i32>()
            .unwrap(),
        1
    );
    assert!(rt.lua.globals().get::<LuaValue>("tool").unwrap().is_nil());
}

#[tokio::test]
async fn workspace_guards_and_routines_execute_in_the_restricted_state() {
    let dir = tempfile::tempdir().unwrap();
    source(
        dir.path(),
        "workspace/plugins/guard.lua",
        r"
        guard('guard', { tools={'bash'}, run=function(event)
            assert(io == nil and os == nil)
            if event.args.command == 'blocked' then return {block='not allowed', terminate=true} end
        end })
        cron('cron', {cron='* * * * *', run=function(ctx)
            assert(io == nil and ctx.bot == 'reviewer')
            ctx.send(ctx.bot, 'tick')
        end})
    ",
    );
    source(
        dir.path(),
        "workspace/routines/check.lua",
        r"
        routine('routine', {cron='0 9 * * *', run=function(ctx)
            assert(io == nil and os == nil)
            ctx.send(ctx.bot, 'morning')
        end})
    ",
    );
    let mut rt = Runtime::new().unwrap();
    rt.load_workspace_tools(dir.path(), Path::new("workspace/plugins"), Some("reviewer"))
        .unwrap();
    rt.load_workspace_routines(
        dir.path(),
        Path::new("workspace/routines"),
        Some("reviewer"),
    )
    .unwrap();
    let event = BeforeToolEvent {
        lane: "main".into(),
        run_id: "run".into(),
        tool_call_id: "call".into(),
        tool_name: "bash".into(),
        args: serde_json::json!({"command":"blocked"})
            .as_object()
            .unwrap()
            .clone(),
    };
    let blocked = rt.run_guards(&event).await.unwrap().unwrap().block.unwrap();
    assert_eq!(blocked.reason, "not allowed");
    assert!(blocked.terminate);
    assert_eq!(
        rt.fire_routine("cron").await.unwrap(),
        vec![("reviewer".into(), "tick".into())]
    );
    assert_eq!(
        rt.fire_routine("routine").await.unwrap(),
        vec![("reviewer".into(), "morning".into())]
    );
}

#[tokio::test]
async fn deferred_workspace_callback_cannot_use_host_io() {
    let dir = tempfile::tempdir().unwrap();
    source(
        dir.path(),
        "workspace/routines/late.lua",
        "routine('late', {cron='* * * * *', run=function() return io.open('/not-read', 'r') end})",
    );
    let mut rt = Runtime::new().unwrap();
    rt.load_workspace_routines(dir.path(), Path::new("workspace/routines"), None)
        .unwrap();
    assert!(
        rt.fire_routine("late")
            .await
            .unwrap_err()
            .to_string()
            .contains("nil")
    );
}

#[test]
fn all_workspace_script_locations_use_the_restricted_loader() {
    for relative in [
        "workspace/plugins/probe.lua",
        "workspace/routines/probe.lua",
        "workspace/agents/chief-of-staff/plugins/probe.lua",
        "workspace/agents/chief-of-staff/routines/probe.lua",
    ] {
        let dir = tempfile::tempdir().unwrap();
        crate::project::init(dir.path()).unwrap();
        source(
            dir.path(),
            relative,
            "assert(io == nil and os == nil and require == nil and loadfile == nil)",
        );
        crate::project::Project::load(dir.path()).unwrap();
    }
}

#[test]
fn symlinked_script_files_and_bot_directories_are_rejected_before_load() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    crate::project::init(dir.path()).unwrap();
    let secret = source(outside.path(), "host.lua", "error('HOST SOURCE EXECUTED')");
    let path = dir.path().join("workspace/plugins/linked.lua");
    symlink(secret, &path).unwrap();
    let error = crate::project::Project::load(dir.path())
        .err()
        .unwrap()
        .to_string();
    assert!(!error.contains("HOST SOURCE EXECUTED"));
    std::fs::remove_file(&path).unwrap();
    symlink(outside.path(), dir.path().join("workspace/agents/linked")).unwrap();
    assert!(crate::project::Project::load(dir.path()).is_err());
}

#[tokio::test]
async fn plugins_skill_examples_load_and_pure_callbacks_execute() {
    let skill = include_str!("templates/plugins_skill.md");
    let dir = tempfile::tempdir().unwrap();
    let mut rt = Runtime::new().unwrap();
    let mut examples = 0;
    for rest in skill.split("```lua\n").skip(1) {
        let snippet = rest.split("```").next().unwrap();
        let relative = format!("example-{examples}");
        source(dir.path(), &format!("{relative}/example.lua"), snippet);
        if snippet.starts_with("-- example: routine") {
            rt.load_workspace_routines(dir.path(), Path::new(&relative), None)
                .unwrap();
        } else {
            assert!(snippet.starts_with("-- example: plugin"));
            rt.load_workspace_tools(dir.path(), Path::new(&relative), None)
                .unwrap();
        }
        examples += 1;
    }
    assert_eq!(examples, 6, "every executable example must be exercised");
    let event = crate::house::resources::Change::new(
        "miku",
        "/repo".into(),
        vec!["/workspace/agents/miku/memory/profile.md".into()],
        false,
    );
    let (sends, errors) = rt.run_changes(&event).await;
    assert!(sends.is_empty() && errors.is_empty());
    assert!(rt.tool("read_note").is_some()); // Guest effect exercised by ignored microVM test.
    assert_eq!(
        rt.fire_routine("chief_weekday_briefing").await.unwrap(),
        vec![(
            "chief-of-staff".into(),
            "Summarize what needs my attention.".into()
        )]
    );
    assert_eq!(
        rt.fire_routine("reviewer_morning").await.unwrap(),
        vec![("chief-of-staff".into(), "What needs attention?".into())]
    );
    let event = BeforeToolEvent {
        lane: "main".into(),
        run_id: "run".into(),
        tool_call_id: "call".into(),
        tool_name: "bash".into(),
        args: serde_json::json!({"command":"git push --force"})
            .as_object()
            .unwrap()
            .clone(),
    };
    assert!(
        rt.run_guards(&event)
            .await
            .unwrap()
            .unwrap()
            .block
            .is_some()
    );
}

#[tokio::test]
async fn change_observers_are_filtered_scoped_and_errors_do_not_veto_other_observers() {
    let dir = tempfile::tempdir().unwrap();
    source(
        dir.path(),
        "workspace/plugins/changes.lua",
        r"
        on_change('bad', {resources={'memory'}, run=function(event, ctx)
            ctx.send(ctx.bot, 'must not leak'); error('broken observer')
        end})
        on_change('good', {paths={'/workspace/agents/*/memory/*'}, resources={'memory'}, run=function(event, ctx)
            assert(io == nil and ctx.bot == 'miku' and ctx.cwd == '/repo')
            ctx.send(ctx.bot, event.resources[1])
        end})
    ",
    );
    source(
        dir.path(),
        "workspace/agents/qmd/plugins/private.lua",
        r"
        on_change('private', {include_unknown=true, run=function() error('wrong owner called') end})
    ",
    );
    let mut rt = Runtime::new().unwrap();
    rt.load_workspace_tools(dir.path(), Path::new("workspace/plugins"), None)
        .unwrap();
    rt.load_workspace_tools(
        dir.path(),
        Path::new("workspace/agents/qmd/plugins"),
        Some("qmd"),
    )
    .unwrap();
    let event = crate::house::resources::Change::new(
        "miku",
        "/repo".into(),
        vec!["/workspace/agents/miku/memory/profile.md".into()],
        false,
    );
    let (sends, errors) = rt.run_changes(&event).await;
    assert_eq!(sends, vec![("miku".into(), "memory".into())]);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("broken observer"));
    let unknown = crate::house::resources::Change::new("miku", "/repo".into(), Vec::new(), true);
    let (sends, errors) = rt.run_changes(&unknown).await;
    assert!(sends.is_empty() && errors.is_empty());
}

#[test]
fn workspace_bytecode_is_never_loaded() {
    let dir = tempfile::tempdir().unwrap();
    let rt = Runtime::new().unwrap();
    let f = rt.lua.load("return 42").into_function().unwrap();
    source(dir.path(), "workspace/plugins/binary.lua", f.dump(false));
    let mut rt = Runtime::new().unwrap();
    assert!(
        rt.load_workspace_tools(dir.path(), Path::new("workspace/plugins"), None)
            .is_err()
    );
}

#[tokio::test]
async fn plugin_command_update_and_gated_tools_use_durable_state() {
    let dir = tempfile::tempdir().unwrap();
    source(
        dir.path(),
        "workspace/plugins/loop.lua",
        r#"
        plugin("loop", {
          interval = 1000,
          command = function(args, ctx)
            assert(ctx.now > 0 and ctx.lane.busy == false)
            ctx.state.set("prompt", args)
            ctx.offer("LoopUpdate")
            ctx.statusline["loop"] = "1 loop"
            ctx.set_timer(5000)
            return "created " .. args
          end,
          update = function(ctx)
            if ctx.state.get("prompt") then
              ctx.offer("LoopUpdate")
              ctx.statusline["loop"] = "running"
            end
          end,
          tools = {
            { name = "LoopUpdate", description = "Update the running loop",
              params = { { name = "status", type = "string", required = true } },
              run = function(args, ctx)
                ctx.state.set("status", args.status)
                if args.status == "completed" then
                  ctx.retract("LoopUpdate")
                  ctx.statusline["loop"] = ""
                end
                return args.status
              end },
          },
        })
        "#,
    );
    let mut rt = Runtime::new().unwrap();
    rt.load_workspace_tools(dir.path(), Path::new("workspace/plugins"), None)
        .unwrap();
    let plugin = rt.plugin("loop").unwrap();
    assert!(plugin.has_command() && plugin.has_update());
    assert_eq!(plugin.tools.len(), 1);
    assert!(rt.tools.iter().all(|t| t.name != "LoopUpdate"));

    let snap = crate::plugin::PluginSnapshot {
        bot: "chief-of-staff".into(),
        now: 10,
        busy: false,
        lane: "main".into(),
        state: serde_json::Map::new(),
    };
    let created = rt
        .run_plugin_command("loop", "5m check", snap.clone())
        .await
        .unwrap();
    assert_eq!(created.notice.as_deref(), Some("created 5m check"));
    assert_eq!(created.offers, vec!["LoopUpdate".to_string()]);
    assert_eq!(
        created.statusline.get("loop").map(String::as_str),
        Some("1 loop")
    );
    assert_eq!(created.timer_ms, Some(5000));
    assert_eq!(created.state["prompt"], "5m check");

    let mut snap = snap;
    snap.state = created.state;
    let updated = rt.run_plugin_update("loop", snap.clone()).await.unwrap();
    assert_eq!(updated.offers, vec!["LoopUpdate".to_string()]);
    assert_eq!(
        updated.statusline.get("loop").map(String::as_str),
        Some("running")
    );

    let mut args = serde_json::Map::new();
    args.insert(
        "status".into(),
        serde_json::Value::String("completed".into()),
    );
    let tool = rt.run_plugin_tool("LoopUpdate", args, snap).await.unwrap();
    assert_eq!(tool.notice.as_deref(), Some("completed"));
    assert_eq!(tool.retracts, vec!["LoopUpdate".to_string()]);
    assert_eq!(crate::plugin::join_statusline(&tool.statusline), "");
}
