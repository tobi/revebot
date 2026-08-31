//! House tools: `update_state`, `CreateAgent`, `UpdateAgent`,
//! `SendAgentMessage`, `SendUserMessage`, `AskUserSandboxPolicyChange`. Names win over Lua.

use std::sync::{Arc, Weak};

use serde_json::{Map, Value, json};

use crate::model::{BoxFuture, ToolSchema};
use crate::sandbox::tokio_util_lite::CancelRx;
use crate::state::Replay;
use crate::tools::{Toolbox, Tools};

use super::{CreateSpec, Inner};

type HouseTool = (&'static str, &'static str, fn() -> Value);
const HOUSE_TOOLS: &[HouseTool] = &[
    (
        "update_state",
        "Update your profile (target=profile, default) or remember/forget an exact fact (target=memory). Memory defaults to agent scope and log tier; user scope is explicitly shared, project scope requires profile.projects membership.",
        || {
            json!({
                "type": "object",
                "properties": {
                    "target": {"type": "string", "enum": ["profile", "memory"]},
                    "name": {"type": "string"},
                    "title": {"type": "string"},
                    "description": {"type": "string"},
                    "avatar": {"type": ["string", "null"]},
                    "group": {"type": "string"},
                    "model": {"type": ["string", "null"]},
                    "projects": {"type": "array", "items": {"type": "string"}},
                    "action": {"type": "string", "enum": ["write", "forget"]},
                    "fact": {"type": "string"},
                    "tier": {"type": "string", "enum": ["profile", "log", "note"]},
                    "scope": {"type": "string", "enum": ["agent", "user", "project"]},
                    "project": {"type": "string"}
                },
                "additionalProperties": false
            })
        },
    ),
    (
        "cd",
        "Change this conversation's guest working directory. HOME stays fixed. Returns the canonical path and full root-to-leaf ancestor AGENTS.md instructions; saved across restarts.",
        || json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}),
    ),
    (
        "CreateAgent",
        "Create a sibling bot under /workspace/agents/. Returns its id. Then message it.",
        || {
            json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "title": {"type": "string"},
                    "description": {"type": "string"},
                    "soul": {"type": "string", "description": "Identity, personality and standing remit for the new bot's SOUL.md"},
                    "avatar": {"type": ["string", "null"]},
                    "model": {"type": ["string", "null"]}
                },
                "required": ["name"],
                "additionalProperties": false
            })
        },
    ),
    (
        "UpdateAgent",
        "Merge-patch another bot's profile. Cannot blank it. Cannot delete.",
        || {
            json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "name": {"type": "string"},
                    "title": {"type": "string"},
                    "description": {"type": "string"},
                    "model": {"type": ["string", "null"]}
                },
                "required": ["id"],
                "additionalProperties": false
            })
        },
    ),
    (
        "SendAgentMessage",
        "Send an asynchronous message to another bot. Like texting: you get an ack, not a reply this turn. Their reply arrives later as a wrapped [agent] user message. Use their id (folder name).",
        || {
            json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "Target bot id (folder name)"},
                    "text": {"type": "string", "description": "The message they will receive"},
                    "priority": {"type": "boolean", "description": "Wake them ahead of other queued work"}
                },
                "required": ["id", "text"],
                "additionalProperties": false
            })
        },
    ),
    (
        "SendUserMessage",
        "Durably accept a user-visible message and return its entry id. Repeating identical text in the same run returns the same id, not a second bubble. An acknowledgment is not a user reply. Acceptance failures are errors; do not claim the user received a failed write.",
        || {
            json!({
                "type": "object",
                "properties": {
                    "text": {"type": "string"}
                },
                "required": ["text"],
                "additionalProperties": false
            })
        },
    ),
    (
        crate::sandbox::POLICY_TOOL,
        "Ask the user to change the sandbox policy: allow egress to `hosts` (a gateway 403 told you a host is not in the allowed-host list), and/or add a host-scoped `secret` (an API key, token, or password). The user answers an inline form; the microVM never holds a secret value, only a placeholder for the secret's hosts. Pass `title` and `reason`, then `hosts` and/or `secret`. Do not ask the user to paste a key in chat. Do not retry or route around a denied host before they answer.",
        || {
            json!({
                "type": "object",
                "properties": {
                    "title": {"type": "string", "description": "Short heading for the form"},
                    "reason": {"type": "string", "description": "Why you need this now, in one or two sentences"},
                    "hosts": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Hostnames to allow egress to, e.g. [\"pypi.org\", \"files.pythonhosted.org\"]"
                    },
                    "secret": {
                        "type": "object",
                        "description": "A secret to add. The value is entered by the user, never by you.",
                        "properties": {
                            "env": {"type": "string", "description": "ENV_NAME, e.g. GITHUB_TOKEN"},
                            "hosts": {"type": "array", "items": {"type": "string"}, "description": "Hosts that may receive the real value"},
                            "header": {"type": "string", "description": "Request header to inject it into, default Authorization"},
                            "prefix": {"type": "string", "description": "Header value prefix, default Bearer"}
                        },
                        "required": ["env"],
                        "additionalProperties": false
                    }
                },
                "required": ["title", "reason"],
                "additionalProperties": false
            })
        },
    ),
];

pub fn names() -> Vec<&'static str> {
    HOUSE_TOOLS.iter().map(|(name, _, _)| *name).collect()
}

pub struct HouseTools {
    pub(crate) inner: Toolbox,
    pub(crate) house: Weak<Inner>,
    pub(crate) bot_id: String,
}

impl HouseTools {
    pub fn tool_names(&self) -> Vec<String> {
        let mut names = self.inner.tool_names();
        for (name, _, _) in HOUSE_TOOLS {
            names.retain(|n| n != name);
            names.push((*name).to_string());
        }
        names
    }
}

impl Tools for HouseTools {
    fn replay(&self, name: &str) -> Option<Replay> {
        if HOUSE_TOOLS.iter().any(|(n, _, _)| *n == name) {
            return Some(Replay::Never);
        }
        self.inner.replay(name)
    }

    fn schemas(&self) -> Vec<ToolSchema> {
        let mut schemas = self.inner.schemas();
        for (name, description, schema) in HOUSE_TOOLS {
            schemas.retain(|s| s.name != *name);
            schemas.push(ToolSchema {
                name: (*name).to_string(),
                description: (*description).to_string(),
                schema: schema(),
            });
        }
        schemas
    }

    fn invoke<'a>(
        &'a self,
        name: &'a str,
        arguments: Map<String, Value>,
        cancel: Option<CancelRx>,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            match name {
                "update_state" => self.update_state(arguments).await,
                "cd" => {
                    let path = arguments
                        .get("path")
                        .and_then(Value::as_str)
                        .ok_or("cd requires path")?;
                    self.house()?
                        .change_directory(&self.bot_id, path)
                        .await
                        .map_err(|e| e.to_string())
                }
                "CreateAgent" => self.create_agent(arguments).await,
                "UpdateAgent" => self.update_agent(arguments).await,
                "SendAgentMessage" => self.send_agent(arguments).await,
                "SendUserMessage" => self.send_user(arguments).await,
                name if name == crate::sandbox::POLICY_TOOL => {
                    self.ask_policy_change(arguments, cancel).await
                }
                other => {
                    if let Some(house) = self.house.upgrade()
                        && let Some(def) = house.project.runtime.tool(other)
                    {
                        let source = match def.owner.as_deref() {
                            Some(_) => Some("bot"),
                            None => Some("workspace"),
                        };
                        house.usage.record(&super::usage::UsageEvent::plugin(
                            &self.bot_id,
                            other,
                            source,
                        ));
                    }
                    let path = arguments
                        .get("path")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let mut result = self.inner.call_cancelled(other, arguments, cancel).await;
                    if !matches!(other, "read" | "ls" | "glob" | "grep")
                        && (result.is_ok() || !matches!(other, "write" | "edit"))
                        && let Some(house) = self.house.upgrade()
                    {
                        let known = matches!(other, "write" | "edit");
                        let paths = if known {
                            match (path, house.context(&self.bot_id)) {
                                (Some(path), Ok(context)) => vec![context.resolve(&path)],
                                _ => Vec::new(),
                            }
                        } else {
                            Vec::new()
                        };
                        if let Err(error) =
                            house.workspace_changed(&self.bot_id, paths, !known).await
                            && let Ok(text) = &mut result
                        {
                            text.push_str(&format!("\nSpecial-file refresh failed: {error}. Use cd to refresh directory rules."));
                        }
                    }
                    result
                }
            }
        })
    }
}

impl HouseTools {
    fn house(&self) -> Result<Arc<Inner>, String> {
        self.house
            .upgrade()
            .ok_or_else(|| "house is shutting down".into())
    }

    async fn update_state(&self, mut args: Map<String, Value>) -> Result<String, String> {
        let house = self.house()?;
        let target = match args.get("target") {
            None => "profile",
            Some(Value::String(target)) => target,
            Some(_) => return Err("update_state target must be a string".into()),
        };
        match target {
            "memory" => {
                let request = super::memory::Request::parse(&args).map_err(|e| e.to_string())?;
                return house
                    .update_memory(&self.bot_id, request)
                    .await
                    .map_err(|e| e.to_string());
            }
            "profile" => {
                args.remove("target");
            }
            other => return Err(format!("unknown update_state target {other:?}")),
        }
        let patch = Value::Object(args);
        let profile = house
            .patch_profile(&self.bot_id, patch)
            .await
            .map_err(|e| e.to_string())?;
        Ok(format!(
            "updated profile: {} ({})",
            profile.name, profile.id
        ))
    }

    async fn create_agent(&self, args: Map<String, Value>) -> Result<String, String> {
        let house = self.house()?;
        let spec: CreateSpec =
            serde_json::from_value(Value::Object(args)).map_err(|e| e.to_string())?;
        let profile = house.create_bot(spec).await.map_err(|e| e.to_string())?;
        Ok(format!("created {} (id: {})", profile.name, profile.id))
    }

    async fn update_agent(&self, args: Map<String, Value>) -> Result<String, String> {
        let house = self.house()?;
        let id = args
            .get("id")
            .and_then(Value::as_str)
            .ok_or("missing id")?
            .to_string();
        if id == self.bot_id {
            return Err("use update_state to change your own profile".into());
        }
        let mut patch = args.clone();
        patch.remove("id");
        let profile = house
            .patch_profile(&id, Value::Object(patch))
            .await
            .map_err(|e| e.to_string())?;
        Ok(format!("updated {} ({})", profile.name, profile.id))
    }

    async fn send_agent(&self, args: Map<String, Value>) -> Result<String, String> {
        let house = self.house()?;
        let id = args
            .get("id")
            .or_else(|| args.get("target_id"))
            .and_then(Value::as_str)
            .ok_or("missing id")?;
        let text = args
            .get("text")
            .or_else(|| args.get("message"))
            .and_then(Value::as_str)
            .ok_or("missing text")?;
        let _priority = args
            .get("priority")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        house
            .send_agent_message(&self.bot_id, id, text)
            .await
            .map_err(|e| e.to_string())
    }

    async fn send_user(&self, args: Map<String, Value>) -> Result<String, String> {
        let house = self.house()?;
        let text = args
            .get("text")
            .or_else(|| args.get("message"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or("missing text")?;
        let id = house
            .send_user_message(&self.bot_id, text)
            .await
            .map_err(|e| e.to_string())?;
        Ok(format!("Message accepted: {id}"))
    }

    async fn ask_policy_change(
        &self,
        args: Map<String, Value>,
        cancel: Option<CancelRx>,
    ) -> Result<String, String> {
        let house = self.house()?;
        // Validate the request before the user sees it, so a malformed call
        // is the model's error, not a broken form.
        let hosts: Vec<String> = args
            .get("hosts")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let hosts = super::policy::normalize_hosts(&hosts)?;
        let secret_env = match args.get("secret") {
            Some(Value::Object(secret)) => {
                let env = secret
                    .get("env")
                    .and_then(Value::as_str)
                    .ok_or("secret.env is required")?
                    .trim()
                    .to_string();
                super::policy::validate_env(&env)?;
                Some(env)
            }
            Some(Value::Null) | None => None,
            Some(_) => return Err("secret must be an object with env".into()),
        };
        if hosts.is_empty() && secret_env.is_none() {
            return Err("nothing requested: pass hosts and/or secret".into());
        }
        match house.ask_policy_change(&self.bot_id, cancel).await? {
            super::PolicyAskResult::Declined => {
                Err("the user declined this sandbox policy change".into())
            }
            super::PolicyAskResult::Applied { hosts, secret } => {
                let mut lines = Vec::new();
                match hosts {
                    Some(crate::sandbox::AllowOutcome::Rebuilt { added }) => lines.push(format!(
                        "Allowed egress to {}. The sandbox was restarted with the new policy; \
                         re-run any command that was interrupted.",
                        added.join(", ")
                    )),
                    Some(crate::sandbox::AllowOutcome::OpenPolicy { added }) => lines.push(format!(
                        "Recorded {} in the allow list; egress is open, so they were already reachable.",
                        added.join(", ")
                    )),
                    Some(crate::sandbox::AllowOutcome::AlreadyAllowed) => {
                        lines.push("Those hosts were already allowed.".to_string());
                    }
                    None => {}
                }
                if let Some((env, hosts)) = secret {
                    lines.push(format!(
                        "Saved {env} for hosts {}. The microVM never holds the value — only a \
                         placeholder. Use ${env} in requests to those hosts. Do not print it.",
                        hosts.join(", ")
                    ));
                }
                Ok(lines.join("\n"))
            }
        }
    }
}
