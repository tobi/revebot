//! House tools: `update_state`, `CreateAgent`, `UpdateAgent`,
//! `SendAgentMessage`, `SendUserMessage`, `AskUserForSecret`. Names win over Lua.

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
        "Update your own profile: name, title, description, avatar, model.",
        || {
            json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "title": {"type": "string"},
                    "description": {"type": "string"},
                    "avatar": {"type": ["string", "null"]},
                    "group": {"type": "string"},
                    "model": {"type": ["string", "null"]}
                },
                "additionalProperties": false
            })
        },
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
                    "instructions": {"type": "string"},
                    "model": {"type": "string"}
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
        "Post a user-visible chat bubble immediately. Always succeeds. Returns ok. The user already sees the text — this result is not a user reply and not a failure. Do not retry. Do not call twice with the same text this turn. Do not wait; they type in the chat on a later turn.",
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
        "AskUserForSecret",
        "Ask the user for a host-side secret. The microVM never holds the value — only a placeholder scoped to named HTTP hosts. Pass title, description, reason, and env (ENV_NAME). Do not ask them to paste a key in chat.",
        || {
            json!({
                "type": "object",
                "properties": {
                    "title": {"type": "string", "description": "Short heading for the form"},
                    "description": {"type": "string", "description": "What the secret is for"},
                    "reason": {"type": "string", "description": "Why you need it now"},
                    "env": {"type": "string", "description": "ENV_NAME, e.g. GITHUB_TOKEN"}
                },
                "required": ["title", "description", "reason", "env"],
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
                "CreateAgent" => self.create_agent(arguments).await,
                "UpdateAgent" => self.update_agent(arguments).await,
                "SendAgentMessage" => self.send_agent(arguments).await,
                "SendUserMessage" => self.send_user(arguments).await,
                "AskUserForSecret" => self.ask_secret(arguments, cancel).await,
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
                    self.inner.call_cancelled(other, arguments, cancel).await
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

    async fn update_state(&self, args: Map<String, Value>) -> Result<String, String> {
        let house = self.house()?;
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
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .ok_or("missing name")?
            .to_string();
        let spec = CreateSpec {
            name,
            title: args
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            description: args
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            instructions: args
                .get("instructions")
                .and_then(Value::as_str)
                .map(str::to_string),
            model: args
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string),
            avatar: args
                .get("avatar")
                .and_then(Value::as_str)
                .map(str::to_string),
        };
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
        // Persist-and-push best effort. The model must never see a failure
        // here: that is what produced the retry storm of identical bubbles.
        let _ = house.send_user_message(&self.bot_id, text).await;
        Ok("ok".into())
    }

    async fn ask_secret(
        &self,
        args: Map<String, Value>,
        cancel: Option<CancelRx>,
    ) -> Result<String, String> {
        let house = self.house()?;
        let env = args
            .get("env")
            .or_else(|| args.get("ENV_NAME"))
            .or_else(|| args.get("env_name"))
            .and_then(Value::as_str)
            .ok_or("missing env")?
            .trim()
            .to_string();
        super::secret::validate_env(&env)?;
        let _title = args.get("title").and_then(Value::as_str);
        let _description = args.get("description").and_then(Value::as_str);
        let _reason = args.get("reason").and_then(Value::as_str);
        match house.ask_secret(&self.bot_id, cancel).await? {
            super::SecretAskResult::Declined => Err("the user declined to add this secret".into()),
            super::SecretAskResult::Saved { env, hosts } => Ok(format!(
                "Saved {env} for hosts {}. The microVM never holds the value — only a placeholder. Use ${env} in requests to those hosts. Do not print it.",
                hosts.join(", ")
            )),
        }
    }
}
