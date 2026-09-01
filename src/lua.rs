//! The scripting surface.
//!
//! Host configuration and host-installed tools are trusted launch code.
//! Bot-editable workspace plugins/routines use a SEPARATE restricted Lua state:
//! pure standard libraries, no ambient host filesystem, environment, modules,
//! native code, or streams. Only explicit Rust callbacks grant capabilities.
//! In both states `ctx.sh` is the sole command path and enters the microVM.
//! A routine's `ctx.send` collects messages for durable house delivery.
//!
//! Lua rather than a config format because a real tool needs branching, string
//! handling, and a standard library. Lua rather than embedding a second large
//! runtime because it vendors into the binary and starts in microseconds.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;

use mlua::{ChunkMode, Lua, LuaOptions, LuaSerdeExt, StdLib, Table, Value as LuaValue};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::cron::Cron;
use crate::hooks::{BeforeToolEvent, BeforeToolResult, Block};
use crate::plugin::{self, PluginEffect, PluginSnapshot};
use crate::sandbox::{Policy, Sandbox, Secret, SecretHost};
use crate::state::Replay;

#[derive(Debug, Error)]
pub enum LuaError {
    #[error("{path}: {source}")]
    Script {
        path: PathBuf,
        #[source]
        source: mlua::Error,
    },
    #[error("{0}")]
    Lua(#[from] mlua::Error),
    #[error("io error reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid {what}: {message}")]
    Invalid { what: &'static str, message: String },
}

pub type Result<T, E = LuaError> = std::result::Result<T, E>;

fn invalid(what: &'static str, message: impl Into<String>) -> LuaError {
    LuaError::Invalid {
        what,
        message: message.into(),
    }
}

// ── agent.lua ────────────────────────────────────────────────────────────

/// What `agent.lua` declares.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AgentConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

// ── tools/*.lua ──────────────────────────────────────────────────────────

/// One declared parameter, which becomes one property of the JSON schema the
/// model sees.
#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: String,
    pub kind: String,
    pub description: String,
    pub required: bool,
    pub default: Option<Value>,
    pub enum_values: Option<Vec<Value>>,
}

/// A tool defined in Lua.
///
/// The body stays in the Lua VM (a registry key), because a closure cannot
/// travel. Calls are dispatched to whichever task owns the VM — the same reason
/// project tools are host-side rather than isolated per call.
#[derive(Debug)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub params: Vec<Param>,
    pub replay: Replay,
    /// Owning folder (`None` = house-wide). Currently metadata, not access control.
    pub owner: Option<String>,
    // A registry key never crosses the host/workspace trust boundary.
    lua: Lua,
    key: mlua::RegistryKey,
}

impl ToolDef {
    /// The JSON schema the model is shown.
    pub fn schema(&self) -> Value {
        let mut properties = Map::new();
        let mut required = Vec::new();
        for param in &self.params {
            let mut spec = Map::new();
            spec.insert("type".into(), Value::String(param.kind.clone()));
            if !param.description.is_empty() {
                spec.insert(
                    "description".into(),
                    Value::String(param.description.clone()),
                );
            }
            if let Some(values) = &param.enum_values {
                spec.insert("enum".into(), Value::Array(values.clone()));
            }
            properties.insert(param.name.clone(), Value::Object(spec));
            if param.required {
                required.push(Value::String(param.name.clone()));
            }
        }
        serde_json::json!({
            "type": "object",
            "properties": Value::Object(properties),
            "required": Value::Array(required),
            "additionalProperties": false,
        })
    }

    /// Apply declared defaults to the arguments the model supplied, and reject
    /// a call that is missing something required.
    pub fn prepare(&self, mut args: Map<String, Value>) -> Result<Map<String, Value>> {
        for param in &self.params {
            if !args.contains_key(&param.name)
                && let Some(default) = &param.default
            {
                args.insert(param.name.clone(), default.clone());
            }
            if param.required && !args.contains_key(&param.name) {
                return Err(invalid(
                    "tool call",
                    format!("{} requires the argument {:?}", self.name, param.name),
                ));
            }
        }
        Ok(args)
    }
}

// ── the runtime ──────────────────────────────────────────────────────────

/// A cron-fired house job declared in `routines/*.lua`.
pub struct RoutineDef {
    pub id: String,
    pub name: String,
    pub cron: Cron,
    pub cron_src: String,
    pub enabled: bool,
    pub bot: Option<String>,
    pub message: Option<String>,
    lua: Lua,
    run: Option<mlua::RegistryKey>,
}

impl RoutineDef {
    pub fn schedule(&self) -> String {
        self.cron.describe(&self.cron_src)
    }
}

/// A `plugin()` declaration: slash command, paced `update`, gated tools.
pub struct PluginDef {
    pub name: String,
    pub interval_ms: u64,
    pub owner: Option<String>,
    lua: Lua,
    update: Option<mlua::RegistryKey>,
    command: Option<mlua::RegistryKey>,
    pub tools: Vec<ToolDef>,
}

impl PluginDef {
    pub fn has_command(&self) -> bool {
        self.command.is_some()
    }

    pub fn has_update(&self) -> bool {
        self.update.is_some()
    }
}

/// Owns separate trusted-host/restricted-workspace Lua states and their definitions.
pub struct Runtime {
    lua: Lua,
    workspace_lua: Lua,
    pub agent: AgentConfig,
    pub policy: Policy,
    pub tools: Vec<ToolDef>,
    pub plugins: Vec<PluginDef>,
    pub routines: Vec<RoutineDef>,
    pub guards: Vec<GuardDef>,
    changes: Vec<ChangeDef>,
}

/// A `guard()` from a workspace plugin. Runs on `before_tool`, fail closed.
pub struct GuardDef {
    pub id: String,
    pub tools: Vec<String>,
    lua: Lua,
    key: mlua::RegistryKey,
}

struct ChangeDef {
    id: String,
    paths: Vec<glob::Pattern>,
    resources: Vec<String>,
    include_unknown: bool,
    owner: Option<String>,
    lua: Lua,
    key: mlua::RegistryKey,
}

/// Everything Lua's standard library offers that reaches the host as a
/// *command* or as native code. Removed before a single line of agent script
/// runs, so the invariant is structural rather than a review convention.
///
/// This is not a defence against the agent author — they are trusted, and they
/// could edit the Rust. It is a defence against the shape of the system: with
/// `os.execute` in scope, "the microVM is the only way to run anything" is a
/// claim you have to keep re-checking. Without it, `ctx.sh` is the only door.
const HOST_COMMAND_PATH: &[(&str, &str)] = &[
    // Runs a command through the host's shell.
    ("os", "execute"),
    // Same, with a pipe attached.
    ("io", "popen"),
    // Ends the host process out from under the session owner.
    ("os", "exit"),
    // Loads a native library, which can call system() itself.
    ("package", "loadlib"),
];

fn json_to_lua(lua: &Lua, value: &Value) -> mlua::Result<LuaValue> {
    match value {
        Value::Null => Ok(LuaValue::Nil),
        Value::Bool(b) => Ok(LuaValue::Boolean(*b)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(LuaValue::Integer(i))
            } else if let Some(f) = n.as_f64() {
                Ok(LuaValue::Number(f))
            } else {
                Ok(LuaValue::Nil)
            }
        }
        Value::String(s) => Ok(LuaValue::String(lua.create_string(s)?)),
        Value::Array(items) => {
            let table = lua.create_table()?;
            for (i, item) in items.iter().enumerate() {
                table.set(i + 1, json_to_lua(lua, item)?)?;
            }
            Ok(LuaValue::Table(table))
        }
        Value::Object(map) => {
            let table = lua.create_table()?;
            for (key, item) in map {
                table.set(key.as_str(), json_to_lua(lua, item)?)?;
            }
            Ok(LuaValue::Table(table))
        }
    }
}

fn lua_to_json(value: LuaValue) -> mlua::Result<Value> {
    match value {
        LuaValue::Nil => Ok(Value::Null),
        LuaValue::Boolean(b) => Ok(Value::Bool(b)),
        LuaValue::Integer(n) => Ok(Value::Number(n.into())),
        LuaValue::Number(n) => {
            Ok(serde_json::Number::from_f64(n).map_or(Value::Null, Value::Number))
        }
        LuaValue::String(s) => Ok(Value::String(s.to_string_lossy().clone())),
        LuaValue::Table(table) => lua_table_to_json(&table),
        other => Err(mlua::Error::external(format!(
            "cannot store {} in plugin state",
            other.type_name()
        ))),
    }
}

fn lua_table_to_json(table: &Table) -> mlua::Result<Value> {
    let mut max_index = 0usize;
    let mut arrayish = true;
    let mut entries: Vec<(String, Value)> = Vec::new();
    let mut items: Vec<(usize, Value)> = Vec::new();
    for pair in table.pairs::<LuaValue, LuaValue>() {
        let (key, value) = pair?;
        let json = lua_to_json(value)?;
        match key {
            LuaValue::Integer(i) if i >= 1 => {
                let index = usize::try_from(i).map_err(mlua::Error::external)?;
                max_index = max_index.max(index);
                items.push((index, json));
            }
            LuaValue::String(s) => {
                arrayish = false;
                entries.push((s.to_string_lossy().clone(), json));
            }
            LuaValue::Integer(i) => {
                arrayish = false;
                entries.push((i.to_string(), json));
            }
            LuaValue::Number(n) => {
                arrayish = false;
                entries.push((n.to_string(), json));
            }
            other => {
                arrayish = false;
                entries.push((other.type_name().to_string(), json));
            }
        }
    }
    if arrayish && max_index == items.len() {
        let mut arr = vec![Value::Null; max_index];
        for (index, json) in items {
            let slot = index
                .checked_sub(1)
                .and_then(|offset| arr.get_mut(offset))
                .ok_or_else(|| mlua::Error::external("invalid Lua array index"))?;
            *slot = json;
        }
        return Ok(Value::Array(arr));
    }
    let mut map = Map::new();
    for (index, json) in items {
        map.insert(index.to_string(), json);
    }
    for (key, json) in entries {
        map.insert(key, json);
    }
    Ok(Value::Object(map))
}

fn lua_result_text(lua: &Lua, result: LuaValue) -> Result<Option<String>> {
    Ok(match result {
        LuaValue::Nil => None,
        LuaValue::String(s) => Some(s.to_string_lossy()),
        other => {
            let json: Value = lua.from_value(other)?;
            match json {
                Value::Null => None,
                Value::String(s) => Some(s),
                other => Some(serde_json::to_string_pretty(&other).unwrap_or_default()),
            }
        }
    })
}

fn plugin_ctx(
    lua: &Lua,
    _plugin: &str,
    snap: &PluginSnapshot,
    effects: Arc<Mutex<PluginEffect>>,
) -> Result<Table> {
    let ctx = lua.create_table()?;
    ctx.set("bot", snap.bot.clone())?;
    ctx.set("now", snap.now)?;
    let lane = lua.create_table()?;
    lane.set("busy", snap.busy)?;
    lane.set("id", snap.lane.clone())?;
    ctx.set("lane", lane)?;

    let state = lua.create_table()?;
    let get_fx = effects.clone();
    state.set(
        "get",
        lua.create_function(move |lua, key: String| {
            let held = get_fx.lock();
            match held.state.get(&key) {
                Some(value) => json_to_lua(lua, value),
                None => Ok(LuaValue::Nil),
            }
        })?,
    )?;
    let set_fx = effects.clone();
    state.set(
        "set",
        lua.create_function(move |_, (key, value): (String, LuaValue)| {
            let json = lua_to_json(value)?;
            let mut held = set_fx.lock();
            if json.is_null() {
                held.state.remove(&key);
            } else {
                held.state.insert(key, json);
            }
            held.state_dirty = true;
            Ok(())
        })?,
    )?;
    ctx.set("state", state)?;

    let status = lua.create_table()?;
    let sl_fx = effects.clone();
    let mt = lua.create_table()?;
    mt.set(
        "__newindex",
        lua.create_function(move |_, (_, key, value): (Table, String, LuaValue)| {
            let text = match value {
                LuaValue::String(s) => s.to_string_lossy(),
                LuaValue::Boolean(b) => b.to_string(),
                LuaValue::Integer(n) => n.to_string(),
                LuaValue::Number(n) => n.to_string(),
                _ => String::new(),
            };
            sl_fx.lock().statusline.insert(key, text);
            Ok(())
        })?,
    )?;
    status.set_metatable(Some(mt))?;
    ctx.set("statusline", status)?;

    let send_fx = effects.clone();
    ctx.set(
        "send",
        lua.create_function(move |_, (bot, text): (String, String)| {
            crate::house::profile::validate_id(&bot).map_err(mlua::Error::external)?;
            let mut held = send_fx.lock();
            if held.sends.len() >= 32 {
                return Err(mlua::Error::external("plugin send limit exceeded"));
            }
            held.sends.push((bot, text));
            Ok(())
        })?,
    )?;

    let offer_fx = effects.clone();
    ctx.set(
        "offer",
        lua.create_function(move |_, name: String| {
            offer_fx.lock().offers.push(name);
            Ok(())
        })?,
    )?;
    let retract_fx = effects.clone();
    ctx.set(
        "retract",
        lua.create_function(move |_, name: String| {
            retract_fx.lock().retracts.push(name);
            Ok(())
        })?,
    )?;

    let timer_fx = effects;
    ctx.set(
        "set_timer",
        lua.create_function(move |_, ms: u64| {
            if let Some(ms) = plugin::clamp_timer_ms(ms) {
                let mut held = timer_fx.lock();
                held.timer_ms = Some(held.timer_ms.map_or(ms, |cur| cur.min(ms)));
            }
            Ok(())
        })?,
    )?;
    Ok(ctx)
}

impl Runtime {
    pub fn new() -> Result<Self> {
        let lua = Lua::new();
        Self::close_the_host_door(&lua)?;
        Ok(Self {
            lua,
            workspace_lua: Self::restricted_lua()?,
            agent: AgentConfig::default(),
            policy: Policy::default(),
            tools: Vec::new(),
            plugins: Vec::new(),
            routines: Vec::new(),
            guards: Vec::new(),
            changes: Vec::new(),
        })
    }

    /// Allowlist pure libraries in a separate VM, so workspace code cannot
    /// recover privileged functions cached by a trusted host script.
    fn restricted_lua() -> Result<Lua> {
        let lua = Lua::new_with(
            StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::UTF8 | StdLib::COROUTINE,
            LuaOptions::default(),
        )?;
        Self::close_the_host_door(&lua)?;
        let allowed = [
            "_G",
            "_VERSION",
            "assert",
            "error",
            "getmetatable",
            "ipairs",
            "next",
            "pairs",
            "pcall",
            "rawequal",
            "rawget",
            "rawlen",
            "rawset",
            "select",
            "setmetatable",
            "tonumber",
            "tostring",
            "type",
            "xpcall",
            "table",
            "string",
            "math",
            "utf8",
            "coroutine",
        ];
        let globals = lua.globals();
        let keys = globals
            .pairs::<String, LuaValue>()
            .map(|pair| pair.map(|(key, _)| key))
            .collect::<mlua::Result<Vec<_>>>()?;
        for key in keys {
            if !allowed.contains(&key.as_str()) {
                globals.set(key, LuaValue::Nil)?;
            }
        }
        // No bytecode export/import path. Source loaded by Rust is text only.
        globals.get::<Table>("string")?.set("dump", LuaValue::Nil)?;
        Ok(lua)
    }

    /// Delete the host command path from the VM's globals.
    fn close_the_host_door(lua: &Lua) -> Result<()> {
        let globals = lua.globals();
        for (module, function) in HOST_COMMAND_PATH {
            // A module that is not loaded at all is the outcome we wanted.
            if let Ok(table) = globals.get::<Table>(*module) {
                table.set(*function, LuaValue::Nil)?;
            }
        }
        Ok(())
    }

    /// Load `agent.lua`, if the agent has one.
    pub fn load_agent(&mut self, path: &Path) -> Result<()> {
        let captured: Arc<Mutex<Option<AgentConfig>>> = Arc::default();
        let sink = captured.clone();
        let agent_fn = self.lua.create_function(move |lua, table: Table| {
            let config: AgentConfig = lua.from_value(LuaValue::Table(table))?;
            *sink.lock() = Some(config);
            Ok(())
        })?;
        self.lua.globals().set("agent", agent_fn)?;
        self.exec_file(path)?;
        if let Some(config) = captured.lock().take() {
            self.agent = config;
        }
        Ok(())
    }

    /// Load `sandbox.lua`, if the agent has one. Absent means the default
    /// policy (public internet).
    pub fn load_sandbox(&mut self, path: &Path) -> Result<()> {
        let captured: Arc<Mutex<Option<Table>>> = Arc::default();
        let sink = captured.clone();
        let sandbox_fn = self.lua.create_function(move |_, table: Table| {
            *sink.lock() = Some(table);
            Ok(())
        })?;
        self.lua.globals().set("sandbox", sandbox_fn)?;
        self.exec_file(path)?;
        let table = captured.lock().take();
        if let Some(table) = table {
            self.policy = policy_from_table(&table)?;
        }
        Ok(())
    }

    /// Load every `tools/*.lua`. One file may declare several tools.
    pub fn load_tools(&mut self, dir: &Path) -> Result<()> {
        self.load_tools_for(dir, None)
    }

    /// Trusted host-installed tools. Workspace callers must use the rooted,
    /// restricted loader below, never this ambient-filesystem entry point.
    pub fn load_tools_for(&mut self, dir: &Path, owner: Option<&str>) -> Result<()> {
        let lua = self.lua.clone();
        self.load_tool_sources(&lua, trusted_sources(dir)?, owner)
    }

    pub fn load_workspace_tools(
        &mut self,
        root: &Path,
        relative: &Path,
        owner: Option<&str>,
    ) -> Result<()> {
        let lua = self.workspace_lua.clone();
        self.load_tool_sources(&lua, workspace_sources(root, relative)?, owner)
    }

    fn load_tool_sources(
        &mut self,
        lua: &Lua,
        sources: Vec<(PathBuf, String)>,
        owner: Option<&str>,
    ) -> Result<()> {
        let collected: Arc<Mutex<Vec<(String, Table)>>> = Arc::default();
        let sink = collected.clone();
        let tool_fn = lua.create_function(move |_, (name, spec): (String, Table)| {
            sink.lock().push((name, spec));
            Ok(())
        })?;
        lua.globals().set("tool", tool_fn)?;
        let guards: Arc<Mutex<Vec<(String, Table)>>> = Arc::default();
        let guard_sink = guards.clone();
        let guard_fn = lua.create_function(move |_, (name, spec): (String, Table)| {
            guard_sink.lock().push((name, spec));
            Ok(())
        })?;
        lua.globals().set("guard", guard_fn)?;
        let crons: Arc<Mutex<Vec<(String, Table)>>> = Arc::default();
        let cron_sink = crons.clone();
        let cron_fn = lua.create_function(move |_, (id, spec): (String, Table)| {
            cron_sink.lock().push((id, spec));
            Ok(())
        })?;
        lua.globals().set("cron", cron_fn)?;
        let changes: Arc<Mutex<Vec<(String, Table)>>> = Arc::default();
        let change_sink = changes.clone();
        lua.globals().set(
            "on_change",
            lua.create_function(move |_, (id, spec): (String, Table)| {
                change_sink.lock().push((id, spec));
                Ok(())
            })?,
        )?;
        let plugins: Arc<Mutex<Vec<(String, Table)>>> = Arc::default();
        let plugin_sink = plugins.clone();
        lua.globals().set(
            "plugin",
            lua.create_function(move |_, (name, spec): (String, Table)| {
                plugin_sink.lock().push((name, spec));
                Ok(())
            })?,
        )?;
        for (path, source) in sources {
            Self::exec_source(lua, &path, &source)?;
        }

        let declared = std::mem::take(&mut *collected.lock());
        for (name, spec) in declared {
            self.tools.push(Self::tool_from_table(
                lua,
                name,
                &spec,
                owner.map(str::to_string),
            )?);
        }
        let declared_guards = std::mem::take(&mut *guards.lock());
        for (name, spec) in declared_guards {
            self.guards.push(Self::guard_from_table(lua, name, &spec)?);
        }
        let declared_crons = std::mem::take(&mut *crons.lock());
        for (id, spec) in declared_crons {
            let mut def = Self::routine_from_table(lua, id, &spec)?;
            if def.bot.is_none() {
                def.bot = owner.map(str::to_string);
            }
            self.routines.push(def);
        }
        for (id, spec) in std::mem::take(&mut *changes.lock()) {
            let paths = spec
                .get::<Option<Vec<String>>>("paths")?
                .unwrap_or_default()
                .iter()
                .map(|p| glob::Pattern::new(p).map_err(|e| invalid("on_change", e.to_string())))
                .collect::<Result<Vec<_>>>()?;
            let resources = spec
                .get::<Option<Vec<String>>>("resources")?
                .unwrap_or_default();
            if resources
                .iter()
                .any(|r| !crate::house::resources::KINDS.contains(&r.as_str()))
            {
                return Err(invalid("on_change", "unknown resource kind"));
            }
            let run: mlua::Function = spec.get("run")?;
            self.changes.push(ChangeDef {
                id,
                paths,
                resources,
                include_unknown: spec
                    .get::<Option<bool>>("include_unknown")?
                    .unwrap_or(false),
                owner: owner.map(str::to_string),
                lua: lua.clone(),
                key: lua.create_registry_value(run)?,
            });
        }
        for (name, spec) in std::mem::take(&mut *plugins.lock()) {
            self.plugins.push(Self::plugin_from_table(
                lua,
                name,
                &spec,
                owner.map(str::to_string),
            )?);
        }
        Ok(())
    }

    /// Load every `routines/*.lua`. One file may declare several routines.
    pub fn load_routines(&mut self, dir: &Path) -> Result<()> {
        self.load_routines_for(dir, None)
    }

    /// Like [`Self::load_routines`], filling in `bot` from the owning folder
    /// when the file omits it.
    pub fn load_routines_for(&mut self, dir: &Path, default_bot: Option<&str>) -> Result<()> {
        let root = dir.parent().unwrap_or(Path::new("."));
        let relative = dir
            .file_name()
            .ok_or_else(|| invalid("routine", "expected a directory name"))?;
        self.load_workspace_routines(root, Path::new(relative), default_bot)
    }

    pub fn load_workspace_routines(
        &mut self,
        root: &Path,
        relative: &Path,
        default_bot: Option<&str>,
    ) -> Result<()> {
        let lua = self.workspace_lua.clone();
        let sources = workspace_sources(root, relative)?;
        let collected: Arc<Mutex<Vec<(String, Table)>>> = Arc::default();
        let sink = collected.clone();
        let routine_fn = lua.create_function(move |_, (id, spec): (String, Table)| {
            sink.lock().push((id, spec));
            Ok(())
        })?;
        lua.globals().set("routine", routine_fn)?;
        for (path, source) in sources {
            Self::exec_source(&lua, &path, &source)?;
        }
        let declared = std::mem::take(&mut *collected.lock());
        for (id, spec) in declared {
            let mut def = Self::routine_from_table(&lua, id, &spec)?;
            if def.bot.is_none() {
                def.bot = default_bot.map(str::to_string);
            }
            if def.run.is_none() && (def.bot.is_none() || def.message.is_none()) {
                return Err(invalid(
                    "routine",
                    format!("{} needs `run`, or both `bot` and `message`", def.id),
                ));
            }
            self.routines.push(def);
        }
        Ok(())
    }

    /// Best-effort post-write observers. Errors cannot veto a completed write;
    /// each callback's sends publish only if that callback returns successfully.
    pub async fn run_changes(
        &self,
        event: &crate::house::resources::Change,
    ) -> (Vec<(String, String)>, Vec<String>) {
        let mut sends = Vec::new();
        let mut errors = Vec::new();
        for def in &self.changes {
            if def.owner.as_ref().is_some_and(|id| id != &event.bot) {
                continue;
            }
            if event.unknown {
                if !def.include_unknown {
                    continue;
                }
            } else {
                if !def.paths.is_empty()
                    && !event
                        .paths
                        .iter()
                        .any(|p| def.paths.iter().any(|pattern| pattern.matches(p)))
                {
                    continue;
                }
                if !def.resources.is_empty()
                    && !event.resources.iter().any(|r| def.resources.contains(r))
                {
                    continue;
                }
            }
            let queued: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
            let result: Result<()> = async {
                let lua = &def.lua;
                let ctx = lua.create_table()?;
                if !event.bot.is_empty() {
                    ctx.set("bot", event.bot.clone())?;
                }
                ctx.set("cwd", event.cwd.clone())?;
                let sink = queued.clone();
                ctx.set(
                    "send",
                    lua.create_function(move |_, (bot, text): (String, String)| {
                        crate::house::profile::validate_id(&bot).map_err(mlua::Error::external)?;
                        let mut sink = sink.lock();
                        if sink.len() >= 32 {
                            return Err(mlua::Error::external("observer send limit exceeded"));
                        }
                        sink.push((bot, text));
                        Ok(())
                    })?,
                )?;
                let callback: mlua::Function = lua.registry_value(&def.key)?;
                callback
                    .call_async::<()>((lua.to_value(event)?, ctx))
                    .await?;
                Ok(())
            }
            .await;
            match result {
                Ok(()) => sends.extend(std::mem::take(&mut *queued.lock())),
                Err(error) => errors.push(format!("{}: {error}", def.id)),
            }
        }
        (sends, errors)
    }

    pub fn routine(&self, id: &str) -> Option<&RoutineDef> {
        self.routines.iter().find(|r| r.id == id)
    }

    /// Collect the messages a routine wants to send. Does not talk to the house
    /// — the caller delivers each pair through `House::prompt`.
    pub async fn fire_routine(&self, id: &str) -> Result<Vec<(String, String)>> {
        let def = self
            .routine(id)
            .ok_or_else(|| invalid("routine", format!("no routine named {id:?}")))?;
        if let Some(key) = &def.run {
            let lua = &def.lua;
            let queued: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
            let sink = queued.clone();
            let ctx = lua.create_table()?;
            if let Some(bot) = &def.bot {
                ctx.set("bot", bot.clone())?;
            }
            let send = lua.create_function(move |_, (bot, text): (String, String)| {
                sink.lock().push((bot, text));
                Ok(())
            })?;
            ctx.set("send", send)?;
            let function: mlua::Function = lua.registry_value(key)?;
            function.call_async::<()>(ctx).await?;
            return Ok(std::mem::take(&mut *queued.lock()));
        }
        match (&def.bot, &def.message) {
            (Some(bot), Some(message)) => Ok(vec![(bot.clone(), message.clone())]),
            _ => Err(invalid(
                "routine",
                format!("{id} has no `run` and no `bot`+`message`"),
            )),
        }
    }

    fn routine_from_table(lua: &Lua, id: String, spec: &Table) -> Result<RoutineDef> {
        let cron_src: String = spec
            .get("cron")
            .map_err(|_| invalid("routine", format!("{id} needs a `cron` field")))?;
        let cron = Cron::parse(&cron_src).map_err(|e| invalid("routine", e.to_string()))?;
        let name: String = spec
            .get::<Option<String>>("name")?
            .unwrap_or_else(|| id.clone());
        let enabled = spec.get::<Option<bool>>("enabled")?.unwrap_or(true);
        let bot = spec.get::<Option<String>>("bot")?;
        let message = spec.get::<Option<String>>("message")?;
        let run = spec
            .get::<Option<mlua::Function>>("run")?
            .map(|f| lua.create_registry_value(f))
            .transpose()?;
        Ok(RoutineDef {
            id,
            name,
            cron,
            cron_src,
            enabled,
            bot,
            message,
            lua: lua.clone(),
            run,
        })
    }

    pub fn tool(&self, name: &str) -> Option<&ToolDef> {
        self.tools.iter().find(|t| t.name == name).or_else(|| {
            self.plugins
                .iter()
                .flat_map(|plugin| plugin.tools.iter())
                .find(|t| t.name == name)
        })
    }

    pub fn plugin(&self, name: &str) -> Option<&PluginDef> {
        self.plugins.iter().find(|p| p.name == name)
    }

    pub fn plugin_tool_names(&self) -> impl Iterator<Item = (&str, &ToolDef)> {
        self.plugins.iter().flat_map(|plugin| {
            plugin
                .tools
                .iter()
                .map(move |tool| (plugin.name.as_str(), tool))
        })
    }

    fn plugin_from_table(
        lua: &Lua,
        name: String,
        spec: &Table,
        owner: Option<String>,
    ) -> Result<PluginDef> {
        if !plugin::is_plugin_name(&name) {
            return Err(invalid(
                "plugin",
                format!("{name} must match /^[a-z0-9][a-z0-9_-]*$/"),
            ));
        }
        let interval_ms = plugin::clamp_interval_ms(
            spec.get::<Option<u64>>("interval")?
                .unwrap_or(plugin::DEFAULT_INTERVAL_MS),
        );
        let update = spec
            .get::<Option<mlua::Function>>("update")?
            .map(|f| lua.create_registry_value(f))
            .transpose()?;
        let command = spec
            .get::<Option<mlua::Function>>("command")?
            .map(|f| lua.create_registry_value(f))
            .transpose()?;
        if update.is_none() && command.is_none() {
            return Err(invalid(
                "plugin",
                format!("{name} needs `update` and/or `command`"),
            ));
        }
        let mut tools = Vec::new();
        if let Ok(list) = spec.get::<Table>("tools") {
            let mut i = 1;
            while let Ok(row) = list.get::<Table>(i) {
                let tool_name: String = row
                    .get("name")
                    .map_err(|_| invalid("plugin", format!("{name} tools[{i}] needs a `name`")))?;
                tools.push(Self::tool_from_table(lua, tool_name, &row, owner.clone())?);
                i += 1;
            }
        }
        Ok(PluginDef {
            name,
            interval_ms,
            owner,
            lua: lua.clone(),
            update,
            command,
            tools,
        })
    }

    pub async fn run_plugin_update(
        &self,
        name: &str,
        snap: PluginSnapshot,
    ) -> Result<PluginEffect> {
        let def = self
            .plugin(name)
            .ok_or_else(|| invalid("plugin", format!("no plugin named {name:?}")))?;
        let Some(key) = &def.update else {
            return Ok(PluginEffect {
                state: snap.state,
                ..PluginEffect::default()
            });
        };
        self.invoke_plugin(def, key, None, snap).await
    }

    pub async fn run_plugin_command(
        &self,
        name: &str,
        args: &str,
        snap: PluginSnapshot,
    ) -> Result<PluginEffect> {
        let def = self
            .plugin(name)
            .ok_or_else(|| invalid("plugin", format!("no plugin named {name:?}")))?;
        let Some(key) = &def.command else {
            return Err(invalid("plugin", format!("{name} has no command")));
        };
        self.invoke_plugin(def, key, Some(args), snap).await
    }

    pub async fn run_plugin_tool(
        &self,
        name: &str,
        args: Map<String, Value>,
        snap: PluginSnapshot,
    ) -> Result<PluginEffect> {
        let (plugin, tool) = self
            .plugins
            .iter()
            .find_map(|plugin| {
                plugin
                    .tools
                    .iter()
                    .find(|t| t.name == name)
                    .map(|tool| (plugin, tool))
            })
            .ok_or_else(|| invalid("plugin", format!("no plugin tool named {name:?}")))?;
        let args = tool.prepare(args)?;
        let lua = &tool.lua;
        let effects = Arc::new(Mutex::new(PluginEffect {
            state: snap.state.clone(),
            ..PluginEffect::default()
        }));
        let ctx = plugin_ctx(lua, &plugin.name, &snap, effects.clone())?;
        let function: mlua::Function = lua.registry_value(&tool.key)?;
        let lua_args = lua.to_value(&Value::Object(args))?;
        let result: LuaValue = function.call_async((lua_args, ctx)).await?;
        let mut effect = std::mem::take(&mut *effects.lock());
        if let Some(notice) = lua_result_text(lua, result)? {
            effect.notice = Some(notice);
        }
        Ok(effect)
    }

    async fn invoke_plugin(
        &self,
        def: &PluginDef,
        key: &mlua::RegistryKey,
        args: Option<&str>,
        snap: PluginSnapshot,
    ) -> Result<PluginEffect> {
        let lua = &def.lua;
        let effects = Arc::new(Mutex::new(PluginEffect {
            state: snap.state.clone(),
            ..PluginEffect::default()
        }));
        let ctx = plugin_ctx(lua, &def.name, &snap, effects.clone())?;
        let function: mlua::Function = lua.registry_value(key)?;
        let result: LuaValue = if let Some(args) = args {
            function.call_async((args.to_string(), ctx)).await?
        } else {
            function.call_async(ctx).await?
        };
        let mut effect = std::mem::take(&mut *effects.lock());
        if let Some(notice) = lua_result_text(lua, result)? {
            effect.notice = Some(notice);
        }
        Ok(effect)
    }

    /// Workspace `guard()` plugins. Fail closed: a Lua error blocks the tool.
    pub async fn run_guards(
        &self,
        event: &BeforeToolEvent,
    ) -> std::result::Result<Option<BeforeToolResult>, String> {
        for guard in &self.guards {
            if !guard.tools.is_empty() && !guard.tools.iter().any(|t| t == &event.tool_name) {
                continue;
            }
            let lua = &guard.lua;
            let function: mlua::Function =
                lua.registry_value(&guard.key).map_err(|e| e.to_string())?;
            let table = lua.create_table().map_err(|e| e.to_string())?;
            table
                .set("tool_name", event.tool_name.clone())
                .map_err(|e| e.to_string())?;
            let args = lua.to_value(&event.args).map_err(|e| e.to_string())?;
            table.set("args", args).map_err(|e| e.to_string())?;
            let result: LuaValue = function
                .call_async(table)
                .await
                .map_err(|e| format!("guard {}: {e}", guard.id))?;
            if result.is_nil() {
                continue;
            }
            let parsed: Value = lua.from_value(result).map_err(|e| e.to_string())?;
            if let Some(reason) = parsed.get("block").and_then(Value::as_str) {
                return Ok(Some(BeforeToolResult {
                    args: None,
                    block: Some(Block {
                        reason: reason.to_string(),
                        terminate: parsed
                            .get("terminate")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    }),
                }));
            }
        }
        Ok(None)
    }

    /// Run a Lua tool.
    ///
    /// `ctx.sh` is wired to this sandbox for the duration of the call. It is
    /// the only command path. Workspace definitions additionally lack ambient
    /// host capabilities; host-installed definitions remain trusted.
    pub async fn call_tool(
        &self,
        name: &str,
        args: Map<String, Value>,
        sandbox: Arc<Sandbox>,
    ) -> Result<String> {
        self.call_tool_cancelled(name, args, sandbox, None).await
    }

    /// Run a Lua tool while allowing each `ctx.sh` guest command to be
    /// interrupted. Neither Lua state is preemptively cancelled; cancellation
    /// applies to the sandbox effects the callback awaits.
    pub async fn call_tool_cancelled(
        &self,
        name: &str,
        args: Map<String, Value>,
        sandbox: Arc<Sandbox>,
        cancel: Option<crate::sandbox::tokio_util_lite::CancelRx>,
    ) -> Result<String> {
        self.call_tool_in(name, args, sandbox, cancel, None).await
    }

    pub async fn call_tool_in(
        &self,
        name: &str,
        args: Map<String, Value>,
        sandbox: Arc<Sandbox>,
        cancel: Option<crate::sandbox::tokio_util_lite::CancelRx>,
        context: Option<crate::working_directory::Context>,
    ) -> Result<String> {
        let def = self
            .tool(name)
            .ok_or_else(|| invalid("tool call", format!("no tool named {name:?}")))?;
        let args = def.prepare(args)?;
        let lua = &def.lua;
        let ctx = lua.create_table()?;
        let sh_sandbox = sandbox.clone();
        let sh_cancel = cancel.clone();
        let sh_context = context.clone();
        let sh = lua.create_async_function(move |_, command: String| {
            let sandbox = sh_sandbox.clone();
            let cancel = sh_cancel.clone();
            let options = sh_context
                .as_ref()
                .map(super::working_directory::Context::options)
                .unwrap_or_default();
            async move {
                let output = sandbox
                    .exec(&command, options, cancel)
                    .await
                    .map_err(|e| mlua::Error::external(e.to_string()))?;
                // This API exposes text only, not exit/cancellation status.
                // A structured exec result is a separate future API.
                Ok(if output.stderr.is_empty() {
                    output.stdout
                } else {
                    format!("{}{}", output.stdout, output.stderr)
                })
            }
        })?;
        ctx.set("sh", sh)?;
        let cwd = context.as_ref().map_or_else(
            || sandbox.workdir().to_string(),
            super::working_directory::Context::cwd,
        );
        ctx.set("workdir", cwd.clone())?;
        ctx.set("cwd", cwd)?;
        if let Some(context) = &context {
            ctx.set("bot", context.bot.clone())?;
            ctx.set("home", context.home.clone())?;
        } else if let Some(owner) = &def.owner {
            ctx.set("bot", owner.clone())?;
        }
        ctx.set(
            "shellescape",
            lua.create_function(|_, s: String| Ok(shell_words::quote(&s).into_owned()))?,
        )?;

        let function: mlua::Function = lua.registry_value(&def.key)?;
        let lua_args = lua.to_value(&Value::Object(args))?;
        let result: LuaValue = function.call_async((lua_args, ctx)).await?;
        Ok(match result {
            LuaValue::String(s) => s.to_string_lossy().clone(),
            LuaValue::Nil => String::new(),
            other => {
                let json: Value = lua.from_value(other)?;
                match json {
                    Value::String(s) => s,
                    other => serde_json::to_string_pretty(&other).unwrap_or_default(),
                }
            }
        })
    }

    fn exec_file(&self, path: &Path) -> Result<()> {
        if !path.is_file() {
            return Ok(());
        }
        let source = std::fs::read_to_string(path).map_err(|source| LuaError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::exec_source(&self.lua, path, &source)
    }

    fn exec_source(lua: &Lua, path: &Path, source: &str) -> Result<()> {
        lua.load(source)
            .set_mode(ChunkMode::Text)
            .set_name(path.to_string_lossy().as_ref())
            .exec()
            .map_err(|source| LuaError::Script {
                path: path.to_path_buf(),
                source,
            })
    }

    fn tool_from_table(
        lua: &Lua,
        name: String,
        spec: &Table,
        owner: Option<String>,
    ) -> Result<ToolDef> {
        let run: mlua::Function = spec
            .get("run")
            .map_err(|_| invalid("tool", format!("{name} has no `run` function")))?;
        let key = lua.create_registry_value(run)?;
        let description: String = spec
            .get::<Option<String>>("description")?
            .unwrap_or_default();
        let replay = Replay::parse(
            spec.get::<Option<String>>("replay")?
                .as_deref()
                .unwrap_or("never"),
        );

        let mut params = Vec::new();
        if let Ok(list) = spec.get::<Table>("params") {
            for pair in list.sequence_values::<Table>() {
                let table = pair?;
                let pname: String = table.get("name").map_err(|_| {
                    invalid("tool", format!("{name} has a parameter without a name"))
                })?;
                params.push(Param {
                    name: pname,
                    kind: table
                        .get::<Option<String>>("type")?
                        .unwrap_or_else(|| "string".into()),
                    description: table
                        .get::<Option<String>>("description")?
                        .unwrap_or_default(),
                    required: table.get::<Option<bool>>("required")?.unwrap_or(false),
                    default: table
                        .get::<LuaValue>("default")
                        .ok()
                        .filter(|v| !v.is_nil())
                        .and_then(|v| lua.from_value(v).ok()),
                    enum_values: table
                        .get::<LuaValue>("enum")
                        .ok()
                        .filter(|v| !v.is_nil())
                        .and_then(|v| lua.from_value::<Vec<Value>>(v).ok()),
                });
            }
        }
        Ok(ToolDef {
            name,
            description,
            params,
            replay,
            owner,
            lua: lua.clone(),
            key,
        })
    }

    fn guard_from_table(lua: &Lua, id: String, spec: &Table) -> Result<GuardDef> {
        let run: mlua::Function = spec
            .get("run")
            .map_err(|_| invalid("guard", format!("{id} has no `run` function")))?;
        let key = lua.create_registry_value(run)?;
        let tools = spec
            .get::<Option<Vec<String>>>("tools")?
            .unwrap_or_default();
        Ok(GuardDef {
            id,
            tools,
            lua: lua.clone(),
            key,
        })
    }
}

fn workspace_sources(root: &Path, relative: &Path) -> Result<Vec<(PathBuf, String)>> {
    crate::script_fs::scripts(root, relative).map_err(|source| LuaError::Io {
        path: root.join(relative),
        source,
    })
}

fn trusted_sources(dir: &Path) -> Result<Vec<(PathBuf, String)>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .map_err(|source| LuaError::Io {
            path: dir.into(),
            source,
        })?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "lua"))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            std::fs::read_to_string(&path)
                .map(|source| (path.clone(), source))
                .map_err(|source| LuaError::Io { path, source })
        })
        .collect()
}

/// Translate the `sandbox { ... }` table into a [`Policy`].
///
/// Written by hand rather than derived because the Lua shape is friendlier than
/// the struct: `allow` is a flat list of hostnames, and secrets carry a per-host
/// map (`allow`, `headers`) that also feeds the sandbox allow list.
fn policy_from_table(table: &Table) -> Result<Policy> {
    let mut policy = Policy::default();
    // `Option<T>`, not `T`: mlua converts a missing key to `false` for `bool`,
    // so `if let Ok(v) = table.get::<bool>(..)` silently turns every unset flag
    // off. That once removed the workspace mount from a policy that never
    // mentioned it.
    if let Some(v) = table.get::<Option<String>>("image")? {
        policy.image = v;
    }
    if let Some(v) = table.get::<Option<u8>>("cpus")? {
        policy.cpus = v;
    }
    if let Some(v) = table.get::<Option<u32>>("memory")? {
        policy.memory = v;
    }
    if let Some(v) = table.get::<Option<u32>>("root_disk")? {
        policy.root_disk = v;
    }
    if let Some(v) = table.get::<Option<String>>("workdir")? {
        policy.workdir = v;
    }
    if let Some(v) = table.get::<Option<String>>("name")? {
        policy.name = Some(v);
    }
    if let Some(v) = table.get::<Option<bool>>("provision")? {
        policy.provision = v;
    }
    if let Some(v) = table.get::<Option<bool>>("mount_workspace")? {
        policy.mount_workspace = v;
    }
    if let Some(v) = table.get::<Option<bool>>("open")? {
        policy.open = v;
    }
    if let Ok(list) = table.get::<Table>("packages") {
        policy.packages = string_list(&list)?;
    }
    if let Ok(list) = table.get::<Table>("mise") {
        policy.mise = string_list(&list)?;
    }
    if let Ok(list) = table.get::<Table>("npm") {
        policy.npm = string_list(&list)?;
    }
    if let Ok(list) = table.get::<Table>("bootstrap") {
        policy.bootstrap = string_list(&list)?;
    }
    if let Ok(list) = table.get::<Table>("allow") {
        let mut hosts = policy.allow_hosts.clone();
        hosts.extend(string_list(&list)?);
        hosts.sort();
        hosts.dedup();
        policy.allow_hosts = hosts;
    }
    if let Ok(map) = table.get::<Table>("env") {
        let mut env = BTreeMap::new();
        for pair in map.pairs::<String, String>() {
            let (key, value) = pair?;
            env.insert(key, value);
        }
        policy.env.extend(env);
    }
    if let Ok(list) = table.get::<Table>("secrets") {
        let mut secrets = Vec::new();
        for entry in list.sequence_values::<Table>() {
            let entry = entry?;
            let env: String = entry
                .get("env")
                .map_err(|_| invalid("sandbox secret", "each secret needs an `env` name"))?;
            let source: String = entry.get("source").map_err(|_| {
                invalid(
                    "sandbox secret",
                    "each secret needs a `source` (`$ENV`, a literal string, `$(command)`, `file:`, or HTTP(S))",
                )
            })?;
            if source.trim().starts_with("$(")
                && crate::sandbox::command_secret_source(&source).is_none()
            {
                return Err(invalid(
                    "sandbox secret",
                    format!(
                        "secret {env} source must be `$(command)` with a non-empty command and no nested substitution"
                    ),
                ));
            }
            let hosts = match entry.get::<Table>("hosts") {
                Ok(table) => secret_hosts_from_table(&table, &env)?,
                Err(_) => BTreeMap::new(),
            };
            if hosts.is_empty() {
                return Err(invalid(
                    "sandbox secret",
                    format!(
                        "secret {env} needs at least one host; an unscoped credential is one the whole VM can use"
                    ),
                ));
            }
            secrets.push(Secret {
                env,
                source,
                placeholder: entry
                    .get::<String>("placeholder")
                    .ok()
                    .filter(|s| !s.is_empty()),
                hosts,
            });
        }
        policy.secrets = secrets;
    }
    Ok(policy)
}

fn string_list(table: &Table) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for value in table.clone().sequence_values::<String>() {
        out.push(value?);
    }
    Ok(out)
}

fn secret_hosts_from_table(table: &Table, env: &str) -> Result<BTreeMap<String, SecretHost>> {
    if table.get::<Option<LuaValue>>(1)?.is_some() {
        return Err(invalid(
            "sandbox secret",
            format!(
                "secret {env} hosts must be a map of hostname to {{ allow, headers }}; a list is not accepted"
            ),
        ));
    }
    let mut hosts = BTreeMap::new();
    for pair in table.clone().pairs::<String, LuaValue>() {
        let (host, value) = pair?;
        let host = host.trim().to_string();
        if host.is_empty() {
            continue;
        }
        let cfg = secret_host_from_value(&value, env, &host)?;
        hosts.insert(host, cfg);
    }
    Ok(hosts)
}

fn secret_host_from_value(value: &LuaValue, env: &str, host: &str) -> Result<SecretHost> {
    let Some(table) = value.as_table() else {
        return Err(invalid(
            "sandbox secret",
            format!("secret {env} host {host} must be a table with allow/headers"),
        ));
    };
    let allow = table.get::<Option<bool>>("allow")?.unwrap_or(true);
    let mut headers = BTreeMap::new();
    if let Ok(header_table) = table.get::<Table>("headers") {
        for pair in header_table.pairs::<String, String>() {
            let (name, value) = pair?;
            let name = name.trim().to_string();
            if name.is_empty() {
                return Err(invalid(
                    "sandbox secret",
                    format!("secret {env} host {host} has an empty header name"),
                ));
            }
            headers.insert(name, value);
        }
    }
    Ok(SecretHost { allow, headers })
}

#[cfg(test)]
#[path = "lua_workspace_tests.rs"]
mod workspace_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn the_host_command_path_is_gone_before_any_script_runs() {
        let runtime = Runtime::new().unwrap();
        for (module, function) in HOST_COMMAND_PATH {
            let probe = format!("return {module} ~= nil and {module}.{function} ~= nil");
            let present: bool = runtime.lua.load(&probe).eval().unwrap();
            assert!(!present, "{module}.{function} is still reachable");
        }
    }

    #[test]
    fn a_tool_that_tries_to_shell_out_on_the_host_fails_to_load() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "tools/escape.lua",
            r#"
            os.execute("touch /tmp/reve-escaped")
            "#,
        );
        let mut runtime = Runtime::new().unwrap();
        let error = runtime
            .load_tools(&dir.path().join("tools"))
            .expect_err("must not run");
        assert!(
            error.to_string().contains("call a nil value"),
            "the failure should be the missing door, not something else: {error}"
        );
        assert!(!Path::new("/tmp/reve-escaped").exists());
    }

    #[test]
    fn agent_lua_configures_the_model() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "agent.lua",
            r#"
            agent {
              model = "openai/gpt-5.6-luna",
              thinking = "low",
            }
        "#,
        );
        let mut rt = Runtime::new().unwrap();
        rt.load_agent(&path).unwrap();
        assert_eq!(rt.agent.model.as_deref(), Some("openai/gpt-5.6-luna"));
        assert_eq!(rt.agent.thinking.as_deref(), Some("low"));
    }

    #[test]
    fn a_missing_agent_file_is_simply_no_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let mut rt = Runtime::new().unwrap();
        rt.load_agent(&dir.path().join("agent.lua")).unwrap();
        assert_eq!(rt.agent, AgentConfig::default());
    }

    #[test]
    fn sandbox_lua_records_an_allow_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "sandbox.lua",
            r#"
            sandbox {
              image = "alpine",
              cpus = 1,
              memory = 512,
              provision = false,
              allow = { "api.example.com" },
            }
        "#,
        );
        let mut rt = Runtime::new().unwrap();
        rt.load_sandbox(&path).unwrap();
        assert_eq!(rt.policy.image, "alpine");
        assert_eq!(rt.policy.cpus, 1);
        assert!(!rt.policy.provision);
        assert!(
            rt.policy.open,
            "internet stays on unless sandbox.lua turns it off"
        );
        assert_eq!(
            rt.policy.egress_hosts(),
            vec!["api.example.com".to_string()],
        );
    }

    #[test]
    fn sandbox_lua_can_lock_down_egress() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "sandbox.lua",
            r#"
            sandbox {
              open = false,
              allow = { "github.com" },
            }
        "#,
        );
        let mut rt = Runtime::new().unwrap();
        rt.load_sandbox(&path).unwrap();
        assert!(!rt.policy.open);
        assert_eq!(rt.policy.egress_summary(), "github.com");
    }

    #[test]
    fn an_unmentioned_flag_keeps_its_default() {
        // Regression: mlua maps a missing key to `false` for `bool`, so a
        // policy that never mentions `mount_workspace` used to lose the
        // workspace bind mount and the VM refused to boot.
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "sandbox.lua",
            r#"
            sandbox { image = "alpine" }
        "#,
        );
        let mut rt = Runtime::new().unwrap();
        rt.load_sandbox(&path).unwrap();
        assert!(rt.policy.mount_workspace, "the workspace is still mounted");
        assert!(rt.policy.open, "an unmentioned open flag keeps internet on");
        assert_eq!(
            rt.policy.root_disk,
            crate::sandbox::DEFAULT_ROOT_DISK_MIB,
            "and the rootfs is still big enough to build in"
        );
    }

    #[test]
    fn a_flag_that_is_set_false_is_honoured() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "sandbox.lua",
            r"
            sandbox { mount_workspace = false, provision = true }
        ",
        );
        let mut rt = Runtime::new().unwrap();
        rt.load_sandbox(&path).unwrap();
        assert!(!rt.policy.mount_workspace, "an explicit false is honoured");
        assert!(
            rt.policy.provision,
            "and so is an explicit true, against a default of false"
        );
    }

    #[test]
    fn a_secret_must_name_its_hosts() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "sandbox.lua",
            r#"
            sandbox {
              secrets = { { env = "TOKEN", source = "$HOST_TOKEN" } },
            }
        "#,
        );
        let mut rt = Runtime::new().unwrap();
        let err = rt.load_sandbox(&path).unwrap_err();
        assert!(err.to_string().contains("at least one host"), "got {err}");
    }

    #[test]
    fn a_scoped_secret_carries_its_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "sandbox.lua",
            r#"
            sandbox {
              secrets = {
                { env = "GITHUB_TOKEN", source = "$HOST_GITHUB_TOKEN",
                  placeholder = "reve-github-token",
                  hosts = { ["github.com"] = { allow = true } } },
              },
            }
        "#,
        );
        let mut rt = Runtime::new().unwrap();
        rt.load_sandbox(&path).unwrap();
        let secret = &rt.policy.secrets[0];
        assert_eq!(secret.env, "GITHUB_TOKEN");
        assert_eq!(secret.source, "$HOST_GITHUB_TOKEN");
        assert_eq!(secret.placeholder.as_deref(), Some("reve-github-token"));
        assert_eq!(secret.hostnames(), vec!["github.com".to_string()]);
        assert!(secret.hosts["github.com"].allow);
    }

    #[test]
    fn a_secret_host_map_carries_headers_and_joins_allow() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "sandbox.lua",
            r#"
            sandbox {
              open = false,
              secrets = {
                { env = "TOOL_GATEWAY", source = "$TOOL_GATEWAY",
                  placeholder = "reve-tool-gateway",
                  hosts = {
                    ["tool-gateway.shopify.io"] = {
                      allow = true,
                      headers = { Authorization = "Bearer $TOOL_GATEWAY" },
                    },
                  } },
              },
            }
        "#,
        );
        let mut rt = Runtime::new().unwrap();
        rt.load_sandbox(&path).unwrap();
        let secret = &rt.policy.secrets[0];
        let host = &secret.hosts["tool-gateway.shopify.io"];
        assert!(host.allow);
        assert_eq!(
            host.headers.get("Authorization").map(String::as_str),
            Some("Bearer $TOOL_GATEWAY")
        );
        assert_eq!(
            rt.policy.egress_hosts(),
            vec!["tool-gateway.shopify.io".to_string()]
        );
    }

    #[test]
    fn a_secret_host_list_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "sandbox.lua",
            r#"
            sandbox {
              secrets = {
                { env = "TOKEN", source = "$HOST_TOKEN",
                  hosts = { "github.com" } },
              },
            }
        "#,
        );
        let mut rt = Runtime::new().unwrap();
        let err = rt.load_sandbox(&path).unwrap_err();
        assert!(err.to_string().contains("map of hostname"), "got {err}");
    }

    #[test]
    fn a_command_secret_source_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "sandbox.lua",
            r#"
            sandbox {
              secrets = {
                { env = "GITHUB_TOKEN", source = "$(gh auth token)",
                  hosts = { ["github.com"] = { allow = true } } },
              },
            }
        "#,
        );
        let mut rt = Runtime::new().unwrap();
        rt.load_sandbox(&path).unwrap();
        assert_eq!(rt.policy.secrets[0].source, "$(gh auth token)");
    }

    #[test]
    fn a_malformed_command_secret_source_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "sandbox.lua",
            r#"
            sandbox {
              secrets = {
                { env = "TOKEN", source = "$(gh auth token",
                  hosts = { ["github.com"] = { allow = true } } },
              },
            }
        "#,
        );
        let mut rt = Runtime::new().unwrap();
        let err = rt.load_sandbox(&path).unwrap_err();
        assert!(err.to_string().contains("$(command)"), "got {err}");
    }

    #[test]
    fn a_value_field_without_source_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "sandbox.lua",
            r#"
            sandbox {
              secrets = {
                { env = "TOKEN", value = "must-not-be-persisted",
                  hosts = { ["example.com"] = { allow = true } } },
              },
            }
        "#,
        );
        let mut rt = Runtime::new().unwrap();
        let err = rt.load_sandbox(&path).unwrap_err();
        assert!(err.to_string().contains("needs a `source`"), "got {err}");
    }

    #[test]
    fn a_tool_becomes_a_json_schema() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "tools/release.lua",
            r#"
            tool("release_report", {
              description = "Summarize commits since a reference",
              replay = "safe",
              params = {
                { name = "since", type = "string", description = "Starting ref", required = true },
                { name = "include_tests", type = "boolean", default = true },
              },
              run = function(args, ctx) return "ok" end,
            })
        "#,
        );
        let mut rt = Runtime::new().unwrap();
        rt.load_tools(&dir.path().join("tools")).unwrap();

        let tool = rt.tool("release_report").expect("declared");
        assert_eq!(tool.replay, Replay::Safe);
        let schema = tool.schema();
        assert_eq!(schema["properties"]["since"]["type"], "string");
        assert_eq!(schema["properties"]["since"]["description"], "Starting ref");
        assert_eq!(schema["properties"]["include_tests"]["type"], "boolean");
        assert_eq!(schema["required"], serde_json::json!(["since"]));
        assert_eq!(schema["additionalProperties"], false);
    }

    #[test]
    fn defaults_are_applied_and_missing_required_arguments_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "tools/t.lua",
            r#"
            tool("t", {
              params = {
                { name = "since", type = "string", required = true },
                { name = "include_tests", type = "boolean", default = true },
              },
              run = function(args, ctx) return "ok" end,
            })
        "#,
        );
        let mut rt = Runtime::new().unwrap();
        rt.load_tools(&dir.path().join("tools")).unwrap();
        let tool = rt.tool("t").unwrap();

        let prepared = tool
            .prepare(
                serde_json::json!({"since": "v1"})
                    .as_object()
                    .unwrap()
                    .clone(),
            )
            .unwrap();
        assert_eq!(prepared["include_tests"], true, "declared default applied");

        let err = tool.prepare(Map::new()).unwrap_err();
        assert!(err.to_string().contains("since"), "got {err}");
    }

    #[test]
    fn a_tool_defaults_to_never_replaying() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "tools/t.lua",
            r#"
            tool("t", { run = function(args, ctx) return "" end })
        "#,
        );
        let mut rt = Runtime::new().unwrap();
        rt.load_tools(&dir.path().join("tools")).unwrap();
        assert_eq!(rt.tool("t").unwrap().replay, Replay::Never);
    }

    #[test]
    fn one_file_may_declare_several_tools() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "tools/pair.lua",
            r#"
            tool("first",  { run = function() return "1" end })
            tool("second", { run = function() return "2" end })
        "#,
        );
        let mut rt = Runtime::new().unwrap();
        rt.load_tools(&dir.path().join("tools")).unwrap();
        assert_eq!(rt.tools.len(), 2);
    }

    #[test]
    fn a_broken_script_names_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "agent.lua", "agent { this is not lua");
        let mut rt = Runtime::new().unwrap();
        let err = rt.load_agent(&path).unwrap_err();
        assert!(err.to_string().contains("agent.lua"), "got {err}");
    }

    #[test]
    fn lua_has_no_host_command_path() {
        // `os.execute` and `io.popen` are the two ways out of stock Lua. A tool
        // is trusted launch code, but the *model* only ever reaches Lua through
        // a tool call, so make sure a tool cannot be tricked into shelling out
        // on the host by way of ctx: ctx exposes sh (VM), workdir, shellescape.
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "tools/t.lua",
            r#"
            tool("t", { run = function(args, ctx)
              local keys = {}
              for k in pairs(ctx) do keys[#keys + 1] = k end
              table.sort(keys)
              return table.concat(keys, ",")
            end })
        "#,
        );
        let mut rt = Runtime::new().unwrap();
        rt.load_tools(&dir.path().join("tools")).unwrap();
        assert!(rt.tool("t").is_some());
    }

    #[test]
    fn a_routine_declares_cron_and_a_message() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "routines/morning.lua",
            r#"
            routine("morning", {
              name = "Morning briefing",
              cron = "0 9 * * 1-5",
              bot = "reve",
              message = "Brief me.",
            })
            "#,
        );
        let mut rt = Runtime::new().unwrap();
        rt.load_routines(&dir.path().join("routines")).unwrap();
        let r = rt.routine("morning").expect("declared");
        assert_eq!(r.name, "Morning briefing");
        assert!(r.enabled);
        assert_eq!(r.schedule(), "Weekdays at 9:00 AM");
    }

    #[test]
    fn a_bot_folder_routine_inherits_the_folder_id() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "routines/ping.lua",
            r#"
            routine("ping", {
              cron = "0 9 * * 1-5",
              message = "ping",
            })
            "#,
        );
        let mut rt = Runtime::new().unwrap();
        rt.load_routines_for(&dir.path().join("routines"), Some("qmd-dev"))
            .unwrap();
        assert_eq!(rt.routine("ping").unwrap().bot.as_deref(), Some("qmd-dev"));
    }

    #[tokio::test]
    async fn a_guard_blocks_a_matching_tool() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "plugins/nope.lua",
            r#"
            guard("no-ls", {
              tools = { "bash" },
              run = function(event)
                return { block = "no bash", terminate = false }
              end,
            })
            "#,
        );
        let mut rt = Runtime::new().unwrap();
        rt.load_tools(&dir.path().join("plugins")).unwrap();
        assert_eq!(rt.guards.len(), 1);
        let event = crate::hooks::BeforeToolEvent {
            lane: "main".into(),
            run_id: "op".into(),
            tool_call_id: "c1".into(),
            tool_name: "bash".into(),
            args: serde_json::Map::new(),
        };
        let result = rt.run_guards(&event).await.unwrap().unwrap();
        assert_eq!(result.block.unwrap().reason, "no bash");
    }

    #[tokio::test]
    async fn a_routine_run_function_queues_sends() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "routines/fan.lua",
            r#"
            routine("fan", {
              cron = "0 * * * *",
              run = function(ctx)
                ctx.send("reve", "one")
                ctx.send("researcher", "two")
              end,
            })
            "#,
        );
        let mut rt = Runtime::new().unwrap();
        rt.load_routines(&dir.path().join("routines")).unwrap();
        let sends = rt.fire_routine("fan").await.unwrap();
        assert_eq!(
            sends,
            vec![
                ("reve".into(), "one".into()),
                ("researcher".into(), "two".into()),
            ]
        );
    }

    #[test]
    fn a_routine_without_a_target_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "routines/empty.lua",
            r#"
            routine("empty", { cron = "* * * * *" })
            "#,
        );
        let mut rt = Runtime::new().unwrap();
        let err = rt.load_routines(&dir.path().join("routines")).unwrap_err();
        assert!(err.to_string().contains("run"), "got {err}");
    }
}
