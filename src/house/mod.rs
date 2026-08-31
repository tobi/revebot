//! A house: one microVM, many bots, one shared `/workspace`.

pub mod profile;
pub mod prompt;
pub mod secret;
pub mod serve;
pub mod tools;
pub mod wrap;

use std::collections::{BTreeMap, HashMap};
use std::fs::OpenOptions;
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use anyhow::Context;
use chrono::{Datelike, Timelike};
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use tokio::sync::{broadcast, mpsc};

use crate::entry::MAIN_LANE;
use crate::events::{Event, Kind};
use crate::harness::{Harness, HarnessConfig, HarnessError};
use crate::hooks::Hooks;
use crate::model::{Assistant, BoxFuture, Deltas, Model, ModelError, Request};
use crate::project::Project;
use crate::provider::HttpModel;
use crate::provider::config::Models;
use crate::sandbox::{Progress, Sandbox};
use crate::session::Session;
use crate::state::{LaneConfiguration, ModelRef, PendingEntry, RetryPolicy, RunSettings};
use crate::storage::Storage;
use crate::tools::Toolbox;

use profile::{BOT_CAP, FIRST_BOT, Profile, scan, slug_from_name, unique_slug};
use prompt::system_prompt;
use tools::HouseTools;
use wrap::wrap_agent_arrival;

struct Unconfigured(String);

impl Model for Unconfigured {
    fn respond<'a>(
        &'a self,
        _request: Request<'a>,
        _on_text: Deltas<'a>,
    ) -> BoxFuture<'a, crate::model::Result<Assistant>> {
        Box::pin(async move { Err(ModelError::terminal(format!("no model: {}", self.0))) })
    }
}

#[derive(Clone)]
pub struct House {
    inner: Arc<Inner>,
}

pub(crate) struct Inner {
    project: Arc<Project>,
    sandbox: Arc<Sandbox>,
    token: String,
    bind: String,
    sock: PathBuf,
    /// Held for process lifetime so the flock stays exclusive.
    #[allow(dead_code)]
    lock: std::fs::File,
    snapshot: RwLock<HashMap<String, BotSlot>>,
    house_events: broadcast::Sender<Event>,
    me: Mutex<Weak<Inner>>,
    /// Last minute a routine actually fired, so a tick cannot double-send.
    last_fired: Mutex<HashMap<String, MinuteStamp>>,
    /// Per-bot skill fingerprints so a created/edited SKILL.md is attached to
    /// the next user turn. Empty map = first snapshot, not "all new".
    skill_seen: Mutex<HashMap<String, BTreeMap<String, u64>>>,
    /// One in-flight AskUserForSecret per bot.
    secret_asks: Mutex<HashMap<String, tokio::sync::oneshot::Sender<SecretAskResult>>>,
}

#[derive(Debug, Clone)]
pub(crate) enum SecretAskResult {
    Declined,
    Saved { env: String, hosts: Vec<String> },
}

type MinuteStamp = (i32, u32, u32, u32, u32);

enum BotSlot {
    Creating {
        reserved_at: Instant,
        _spec: CreateSpec,
    },
    Ready(BotRuntime),
}

struct BotRuntime {
    profile: Profile,
    harness: Arc<Harness>,
    session: Session,
    cmds: mpsc::Sender<BotCmd>,
    model: Mutex<Arc<dyn Model>>,
}

#[derive(Clone)]
pub struct CreateSpec {
    pub name: String,
    pub title: String,
    pub description: String,
    pub instructions: Option<String>,
    pub model: Option<String>,
    pub avatar: Option<String>,
}

enum BotCmd {
    UserText {
        text: String,
        reply: tokio::sync::oneshot::Sender<Result<PromptAck, String>>,
    },
    Abort,
    KickNow,
}

#[derive(Debug, Clone)]
pub struct PromptAck {
    pub operation_id: String,
    pub entry_id: String,
    pub mode: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct RoutineInfo {
    pub id: String,
    pub name: String,
    pub cron: String,
    pub schedule: String,
    pub enabled: bool,
    pub bot: Option<String>,
}

impl House {
    pub fn project(&self) -> &Project {
        &self.inner.project
    }

    pub fn sandbox(&self) -> Arc<Sandbox> {
        self.inner.sandbox.clone()
    }

    pub fn token(&self) -> &str {
        &self.inner.token
    }

    pub fn bind(&self) -> &str {
        &self.inner.bind
    }

    pub fn sock(&self) -> &PathBuf {
        &self.inner.sock
    }

    pub async fn boot(
        project: Project,
        bind: String,
        progress: &dyn Progress,
    ) -> anyhow::Result<Self> {
        let project = Arc::new(project);
        std::fs::create_dir_all(project.state_dir()).context("create .reve")?;
        let lock_path = project.state_dir().join("house.lock");
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("open {}", lock_path.display()))?;
        lock.try_lock()
            .map_err(|_| anyhow::anyhow!("a house is already running in this directory"))?;

        let name =
            crate::sandbox::Sandbox::sandbox_name_for(&project.runtime.policy, project.workspace());
        Sandbox::reclaim_namesake(&name).await.ok();

        let sandbox = Sandbox::start(
            project.runtime.policy.clone(),
            project.workspace(),
            project.state_dir(),
            progress,
        )
        .await?;
        let sandbox = Arc::new(sandbox);
        sandbox.hold().await?;

        let token = hex_token();
        let sock = project.state_dir().join("house.sock");
        let (house_events, _) = broadcast::channel(256);
        let inner = Arc::new(Inner {
            project: project.clone(),
            sandbox: sandbox.clone(),
            token,
            bind,
            sock,
            lock,
            snapshot: RwLock::new(HashMap::new()),
            house_events,
            me: Mutex::new(Weak::new()),
            last_fired: Mutex::new(HashMap::new()),
            skill_seen: Mutex::new(HashMap::new()),
            secret_asks: Mutex::new(HashMap::new()),
        });
        *inner.me.lock() = Arc::downgrade(&inner);

        let mut profiles = scan(&project.agents_dir());
        if !profiles.iter().any(|p| p.id == FIRST_BOT) {
            crate::project::init(&project.root)?;
            profiles = scan(&project.agents_dir());
        }
        for profile in profiles {
            inner.spawn_ready(profile).await?;
        }
        // Resume + kick are the supervisor's first job. Awaiting
        // `resume_all` here would block the HTTP server until every
        // in-flight run finished (a hung tool looks like a stuck boot).

        spawn_routines(inner.clone());

        Ok(Self { inner })
    }

    pub fn ready_profiles(&self) -> Vec<Profile> {
        self.inner.ready_profiles()
    }

    pub async fn prompt(&self, bot: &str, text: &str) -> anyhow::Result<PromptAck> {
        self.inner.prompt(bot, text).await
    }

    pub async fn abort(&self, bot: &str) -> anyhow::Result<()> {
        self.inner.abort_bot(bot).await
    }

    pub async fn transcript(&self, bot: &str) -> anyhow::Result<Vec<crate::entry::Entry>> {
        let harness = self.inner.ready_harness(bot)?;
        Ok(harness.session().transcript(MAIN_LANE).await?)
    }

    pub async fn transcript_page(
        &self,
        bot: &str,
        before: Option<u64>,
        limit: usize,
    ) -> anyhow::Result<(Vec<crate::entry::Entry>, bool)> {
        let entries = self.transcript(bot).await?;
        Ok(page_transcript(entries, before, limit))
    }

    pub fn bot_instructions(&self, bot: &str) -> anyhow::Result<String> {
        let path = self.inner.project.bot_dir(bot).join("instructions.md");
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn set_bot_instructions(&self, bot: &str, text: &str) -> anyhow::Result<()> {
        self.inner.ready_harness(bot)?;
        self.inner
            .sandbox
            .write_file(&format!("agents/{bot}/instructions.md"), text)
            .await?;
        Ok(())
    }

    pub fn configured_models(&self) -> Vec<String> {
        crate::provider::config::Models::load(&self.inner.project.root.join("models.yml"))
            .map(|m| m.catalog())
            .unwrap_or_default()
    }

    pub fn skills_for(&self, bot: &str) -> Vec<crate::skills::SkillListing> {
        crate::skills::listings_for(
            &self.inner.project.workspace(),
            &self.inner.project.bot_dir(bot),
        )
    }

    pub async fn complete_secret(
        &self,
        bot: &str,
        decision: secret::SecretDecision,
    ) -> anyhow::Result<String> {
        self.inner.complete_secret(bot, decision).await
    }

    pub fn subscribe(&self, bot: &str) -> anyhow::Result<broadcast::Receiver<Event>> {
        Ok(self.inner.ready_harness(bot)?.subscribe())
    }

    pub fn subscribe_house(&self) -> broadcast::Receiver<Event> {
        self.inner.house_events.subscribe()
    }

    pub async fn create_bot(&self, spec: CreateSpec) -> anyhow::Result<Profile> {
        self.inner.create_bot(spec).await
    }

    pub async fn patch_bot(&self, id: &str, patch: serde_json::Value) -> anyhow::Result<Profile> {
        self.inner.patch_profile(id, patch).await
    }

    pub async fn delete_bot(&self, id: &str) -> anyhow::Result<()> {
        self.inner.delete_bot(id).await
    }

    pub fn routines(&self) -> Vec<RoutineInfo> {
        self.inner.routine_info()
    }

    pub async fn run_routine(&self, id: &str) -> anyhow::Result<Vec<String>> {
        self.inner.run_routine(id).await
    }

    pub async fn shutdown(&self) -> anyhow::Result<()> {
        self.inner.sandbox.release_hold().await;
        self.inner.sandbox.stop().await?;
        let _ = std::fs::remove_file(self.inner.project.state_dir().join("house.json"));
        let _ = std::fs::remove_file(&self.inner.sock);
        Ok(())
    }

    pub fn write_house_json(&self) -> anyhow::Result<()> {
        let path = self.inner.project.state_dir().join("house.json");
        let body = serde_json::json!({
            "pid": std::process::id(),
            "bind": self.inner.bind,
            "sock": self.inner.sock,
            "token": self.inner.token,
            "status": "ready",
            "started_at": chrono::Utc::now().to_rfc3339(),
        });
        std::fs::write(&path, serde_json::to_vec_pretty(&body)?)?;
        Ok(())
    }
}

impl Inner {
    fn ready_profiles(&self) -> Vec<Profile> {
        let snap = self.snapshot.read();
        let mut out: Vec<Profile> = snap
            .values()
            .filter_map(|slot| match slot {
                BotSlot::Ready(rt) => Some(rt.profile.clone()),
                BotSlot::Creating { .. } => None,
            })
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    fn ready_harness(&self, id: &str) -> anyhow::Result<Arc<Harness>> {
        let snap = self.snapshot.read();
        match snap.get(id) {
            Some(BotSlot::Ready(rt)) => Ok(rt.harness.clone()),
            _ => anyhow::bail!("unknown bot {id}"),
        }
    }

    async fn prompt(&self, bot: &str, text: &str) -> anyhow::Result<PromptAck> {
        let tx = {
            let snap = self.snapshot.read();
            match snap.get(bot) {
                Some(BotSlot::Ready(rt)) => rt.cmds.clone(),
                _ => anyhow::bail!("unknown bot {bot}"),
            }
        };
        let skills =
            crate::skills::catalog_for(&self.project.workspace(), &self.project.bot_dir(bot));
        let (updated, removed) = {
            let mut seen = self.skill_seen.lock();
            let prev = seen.get(bot).cloned().unwrap_or_default();
            let (changed, removed, next) = crate::skills::diff(&skills, &prev);
            seen.insert(bot.to_string(), next);
            (changed, removed)
        };
        let wrapped = wrap::wrap_user_turn_at(
            text,
            &self.ready_profiles(),
            &skills,
            &updated,
            &removed,
            &wrap::timestamp_now(),
        );
        let (reply, rx) = tokio::sync::oneshot::channel();
        tx.send(BotCmd::UserText {
            text: wrapped,
            reply,
        })
        .await
        .map_err(|_| anyhow::anyhow!("bot supervisor gone"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("bot supervisor dropped"))?
            .map_err(|e| anyhow::anyhow!(e))
    }

    async fn abort_bot(&self, bot: &str) -> anyhow::Result<()> {
        let (harness, cmds) = {
            let snap = self.snapshot.read();
            match snap.get(bot) {
                Some(BotSlot::Ready(rt)) => (rt.harness.clone(), rt.cmds.clone()),
                _ => anyhow::bail!("unknown bot {bot}"),
            }
        };
        let _ = cmds.send(BotCmd::Abort).await;
        match harness.abort(MAIN_LANE).await {
            Ok(()) | Err(HarnessError::Idle(_)) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    pub(crate) async fn ask_secret(
        &self,
        bot: &str,
        cancel: Option<crate::sandbox::tokio_util_lite::CancelRx>,
    ) -> Result<SecretAskResult, String> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut asks = self.secret_asks.lock();
            if asks.contains_key(bot) {
                return Err("a secret prompt is already waiting for this bot".into());
            }
            asks.insert(bot.to_string(), tx);
        }
        let cancelled = async {
            if let Some(mut rx) = cancel {
                rx.cancelled().await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            result = rx => {
                result.map_err(|_| "secret prompt was dropped".to_string())
            }
            _ = cancelled => {
                self.secret_asks.lock().remove(bot);
                Err("cancelled".into())
            }
        }
    }

    pub(crate) async fn complete_secret(
        &self,
        bot: &str,
        decision: secret::SecretDecision,
    ) -> anyhow::Result<String> {
        self.ready_harness(bot)?;
        if !self.secret_asks.lock().contains_key(bot) {
            anyhow::bail!("no pending secret prompt");
        }
        if !decision.accept {
            let tx = self
                .secret_asks
                .lock()
                .remove(bot)
                .ok_or_else(|| anyhow::anyhow!("no pending secret prompt"))?;
            let _ = tx.send(SecretAskResult::Declined);
            return Ok("declined".into());
        }
        let secret =
            secret::to_secret(&self.project.root, &decision).map_err(|e| anyhow::anyhow!(e))?;
        let path = self.project.root.join("config.yml");
        let text = std::fs::read_to_string(&path).unwrap_or_else(|_| "sandbox: {}\n".into());
        let next = secret::upsert_config_yml(&text, &secret).map_err(|e| anyhow::anyhow!(e))?;
        std::fs::write(&path, next)?;
        self.sandbox.upsert_secret(secret.clone()).await?;
        let tx = self
            .secret_asks
            .lock()
            .remove(bot)
            .ok_or_else(|| anyhow::anyhow!("no pending secret prompt"))?;
        let _ = tx.send(SecretAskResult::Saved {
            env: secret.env.clone(),
            hosts: secret.hosts.clone(),
        });
        Ok(format!("saved {}", secret.env))
    }

    async fn spawn_ready(self: &Arc<Self>, profile: Profile) -> anyhow::Result<()> {
        let id = profile.id.clone();
        std::fs::create_dir_all(self.project.bot_sessions_dir(&id))?;
        let session_path = self
            .project
            .latest_bot_session(&id, MAIN_LANE)
            .unwrap_or_else(|| self.project.bot_conversation_path(&id, MAIN_LANE));
        let storage = Storage::open(&session_path, &id, Some("workspace".into()))?;
        let session = Session::spawn(storage);
        let model = resolve_model(&self.project, profile.model.as_deref());
        let toolbox = Toolbox::new(self.sandbox.clone(), self.project.runtime_arc());
        let house_tools = HouseTools {
            inner: toolbox,
            house: Arc::downgrade(self),
            bot_id: id.clone(),
        };
        let active_tool_names = house_tools.tool_names();
        let tools = Arc::new(house_tools);
        let prompt_profile = profile.clone();
        let prompt_house = Arc::downgrade(self);
        let runtime = self.project.runtime.clone();
        let hooks = Hooks::new().on_before_tool(Arc::new(move |event| {
            let runtime = runtime.clone();
            Box::pin(async move { runtime.run_guards(&event).await })
        }));
        let harness = Harness::new(
            session.clone(),
            HarnessConfig {
                model: model.clone(),
                tools: tools.clone(),
                hooks,
                system_prompt: Arc::new(move || {
                    let Some(inner) = prompt_house.upgrade() else {
                        return String::new();
                    };
                    system_prompt(&inner.project, &prompt_profile, &inner.ready_profiles())
                }),
                settings: RunSettings::default(),
                retry: RetryPolicy::default(),
                configuration: LaneConfiguration {
                    model: ModelRef {
                        provider: profile.model.clone().unwrap_or_else(|| {
                            self.project
                                .runtime
                                .agent
                                .model
                                .clone()
                                .unwrap_or_else(|| "none".into())
                        }),
                        model_id: profile.model.clone().unwrap_or_else(|| {
                            self.project
                                .runtime
                                .agent
                                .model
                                .clone()
                                .unwrap_or_else(|| "none".into())
                        }),
                    },
                    thinking_level: self
                        .project
                        .runtime
                        .agent
                        .thinking
                        .clone()
                        .unwrap_or_else(|| "default".into()),
                    active_tool_names,
                },
                event_capacity: 1024,
            },
        );

        let (cmds, cmd_rx) = mpsc::channel(32);
        let runtime = BotRuntime {
            profile: profile.clone(),
            harness: harness.clone(),
            session,
            cmds: cmds.clone(),
            model: Mutex::new(model),
        };
        {
            let mut snap = self.snapshot.write();
            snap.insert(id.clone(), BotSlot::Ready(runtime));
        }
        {
            let catalog =
                crate::skills::catalog_for(&self.project.workspace(), &self.project.bot_dir(&id));
            let snap: BTreeMap<String, u64> = catalog
                .iter()
                .map(|s| (s.name.clone(), crate::skills::fingerprint(s)))
                .collect();
            self.skill_seen.lock().insert(id.clone(), snap);
        }
        spawn_supervisor(harness, cmd_rx);
        let _ = self.house_events.send(Event::new(
            "house",
            None,
            Kind::RosterChanged {
                ids: self.ready_profiles().into_iter().map(|p| p.id).collect(),
            },
        ));
        Ok(())
    }

    pub(crate) async fn create_bot(self: &Arc<Self>, spec: CreateSpec) -> anyhow::Result<Profile> {
        let reserved = self.reserve_create(spec.clone())?;
        let guard = CreateGuard {
            inner: Arc::clone(self),
            id: reserved.id.clone(),
            finished: false,
        };
        self.write_bot_files(&reserved, &spec).await?;
        self.finish_create(&reserved.id).await?;
        let mut guard = guard;
        guard.finished = true;
        Ok(reserved)
    }

    fn reserve_create(&self, spec: CreateSpec) -> anyhow::Result<Profile> {
        let mut snap = self.snapshot.write();
        if snap.len() >= BOT_CAP {
            anyhow::bail!("bot cap ({BOT_CAP}) reached");
        }
        let base = slug_from_name(&spec.name);
        let id = unique_slug(&base, |s| snap.contains_key(s));
        let profile = Profile {
            id: id.clone(),
            name: spec.name.clone(),
            title: spec.title.clone(),
            description: spec.description.clone(),
            avatar: spec.avatar.clone(),
            group: String::new(),
            created_at: Some(chrono::Utc::now().to_rfc3339()),
            model: spec.model.clone(),
        };
        snap.insert(
            id,
            BotSlot::Creating {
                reserved_at: Instant::now(),
                _spec: spec,
            },
        );
        Ok(profile)
    }

    async fn write_bot_files(&self, profile: &Profile, spec: &CreateSpec) -> anyhow::Result<()> {
        let guest = format!("/workspace/agents/{}", profile.id);
        let mkdir = format!(
            "mkdir -p {guest}/skills {guest}/sessions {guest}/memory {guest}/routines {guest}/plugins"
        );
        let out = self
            .sandbox
            .exec(&mkdir, crate::sandbox::ExecOptions::default(), None)
            .await?;
        if !out.success || out.cancelled {
            anyhow::bail!("could not create bot directory");
        }
        self.sandbox
            .write_file(
                &format!("agents/{}/profile.json", profile.id),
                &profile.to_json(),
            )
            .await?;
        let instructions = spec.instructions.clone().unwrap_or_else(|| {
            include_str!("../templates/specialist_instructions.md")
                .replace("{name}", &profile.name)
                .replace("{title}", &profile.title)
                .replace("{description}", &profile.description)
        });
        self.sandbox
            .write_file(
                &format!("agents/{}/instructions.md", profile.id),
                &instructions,
            )
            .await?;
        Ok(())
    }

    async fn finish_create(self: &Arc<Self>, id: &str) -> anyhow::Result<()> {
        let path = self.project.bot_dir(id).join("profile.json");
        let profile = Profile::load(&path)
            .ok_or_else(|| anyhow::anyhow!("finish create: {id} has no parseable profile.json"))?;
        {
            let snap = self.snapshot.read();
            match snap.get(id) {
                Some(BotSlot::Creating { .. }) => {}
                _ => anyhow::bail!("finish create: {id} is not reserved"),
            }
        }
        self.spawn_ready(profile).await?;
        Ok(())
    }

    fn abort_create(&self, id: &str) {
        let mut snap = self.snapshot.write();
        if matches!(snap.get(id), Some(BotSlot::Creating { .. })) {
            snap.remove(id);
        }
    }

    fn sweep_creating(&self) {
        let mut snap = self.snapshot.write();
        snap.retain(|_, slot| match slot {
            BotSlot::Creating { reserved_at, .. } => reserved_at.elapsed().as_secs() < 30,
            BotSlot::Ready(_) => true,
        });
    }

    pub(crate) async fn patch_profile(
        self: &Arc<Self>,
        id: &str,
        patch: serde_json::Value,
    ) -> anyhow::Result<Profile> {
        self.sweep_creating();
        let mut profile = {
            let snap = self.snapshot.read();
            match snap.get(id) {
                Some(BotSlot::Ready(rt)) => rt.profile.clone(),
                _ => anyhow::bail!("unknown bot {id}"),
            }
        };
        if let Some(name) = patch.get("name").and_then(|v| v.as_str()) {
            if name.trim().is_empty() {
                anyhow::bail!("name cannot be blank");
            }
            profile.name = name.to_string();
        }
        if let Some(title) = patch.get("title").and_then(|v| v.as_str()) {
            profile.title = title.to_string();
        }
        if let Some(description) = patch.get("description").and_then(|v| v.as_str()) {
            profile.description = description.to_string();
        }
        if patch.get("avatar").is_some() {
            profile.avatar = patch
                .get("avatar")
                .and_then(|v| v.as_str())
                .map(str::to_string);
        }
        if let Some(group) = patch.get("group").and_then(|v| v.as_str()) {
            profile.group = group.to_string();
        }
        if patch.get("model").is_some() {
            profile.model = patch
                .get("model")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            if let Some(spec) = &profile.model {
                resolve_model_checked(&self.project, spec).map_err(|e| anyhow::anyhow!(e))?;
            }
        }
        self.sandbox
            .write_file(&format!("agents/{id}/profile.json"), &profile.to_json())
            .await?;
        self.reload_profile(id).await?;
        Ok(profile)
    }

    async fn reload_profile(self: &Arc<Self>, id: &str) -> anyhow::Result<()> {
        let path = self.project.bot_dir(id).join("profile.json");
        let profile = Profile::load(&path).ok_or_else(|| anyhow::anyhow!("unreadable profile"))?;
        let model = resolve_model(&self.project, profile.model.as_deref());
        let mut snap = self.snapshot.write();
        if let Some(BotSlot::Ready(rt)) = snap.get_mut(id) {
            rt.profile = profile;
            *rt.model.lock() = model;
        }
        Ok(())
    }

    async fn delete_bot(&self, id: &str) -> anyhow::Result<()> {
        if self.ready_profiles().len() <= 1 {
            anyhow::bail!("cannot delete the last bot");
        }
        let (harness, session, cmds) = {
            let snap = self.snapshot.read();
            match snap.get(id) {
                Some(BotSlot::Ready(rt)) => {
                    (rt.harness.clone(), rt.session.clone(), rt.cmds.clone())
                }
                _ => anyhow::bail!("unknown bot {id}"),
            }
        };
        let _ = cmds.send(BotCmd::Abort).await;
        let _ = harness.abort(MAIN_LANE).await;
        session.close().await;
        std::fs::remove_dir_all(self.project.bot_dir(id))?;
        self.snapshot.write().remove(id);
        Ok(())
    }

    pub(crate) async fn send_agent_message(
        &self,
        from: &str,
        to: &str,
        text: &str,
    ) -> anyhow::Result<String> {
        self.sweep_creating();
        let (target_harness, target_cmds, from_name, to_name) = {
            let snap = self.snapshot.read();
            let from_rt = match snap.get(from) {
                Some(BotSlot::Ready(rt)) => rt,
                _ => anyhow::bail!("unknown to {from}"),
            };
            let to_rt = match snap.get(to) {
                Some(BotSlot::Ready(rt)) => rt,
                _ => anyhow::bail!("unknown to {to}"),
            };
            (
                to_rt.harness.clone(),
                to_rt.cmds.clone(),
                from_rt.profile.name.clone(),
                to_rt.profile.name.clone(),
            )
        };
        let wake = wrap_agent_arrival(&from_name, from, text);
        let payload = serde_json::json!({
            "role": "user",
            "content": wake,
            "from_bot": from,
            "from_name": from_name,
        });
        target_harness
            .next_run_entry(MAIN_LANE, PendingEntry::message(payload))
            .await?;
        let _ = target_cmds.send(BotCmd::KickNow).await;
        Ok(format!("sent to {to_name}"))
    }

    pub(crate) async fn send_user_message(&self, bot: &str, text: &str) -> anyhow::Result<()> {
        let harness = self.ready_harness(bot)?;
        let notice = PendingEntry::custom(
            "user_notice",
            serde_json::json!({ "text": text, "bot": bot }),
        );
        let persist = match harness.write_entry(MAIN_LANE, notice.clone()).await {
            Ok(_) => Ok(()),
            Err(HarnessError::Idle(_)) => harness.place_idle(MAIN_LANE, notice).await.map(|_| ()),
            Err(e) => Err(e),
        };
        let event = Kind::UserNotice {
            bot_id: bot.into(),
            text: text.into(),
        };
        // The UI websocket is the bot harness stream. house_events is a
        // house-wide bus and is not what the chat page is subscribed to —
        // emitting only there meant the bubble waited for run_end reload,
        // which never comes if the model keeps retrying this tool.
        harness.emit_now(MAIN_LANE, event.clone());
        let _ = self.house_events.send(Event::new(bot, None, event));
        persist.map_err(Into::into)
    }

    fn routine_info(&self) -> Vec<RoutineInfo> {
        self.project
            .runtime
            .routines
            .iter()
            .map(|r| RoutineInfo {
                id: r.id.clone(),
                name: r.name.clone(),
                cron: r.cron_src.clone(),
                schedule: r.schedule(),
                enabled: r.enabled,
                bot: r.bot.clone(),
            })
            .collect()
    }

    async fn run_routine(self: &Arc<Self>, id: &str) -> anyhow::Result<Vec<String>> {
        let name = self
            .project
            .runtime
            .routine(id)
            .map(|r| r.name.clone())
            .ok_or_else(|| anyhow::anyhow!("unknown routine {id}"))?;
        let sends = self
            .project
            .runtime
            .fire_routine(id)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut delivered = Vec::new();
        for (bot, text) in sends {
            let body = format!("[routine] {name}\n\n{text}");
            self.prompt(&bot, &body).await?;
            delivered.push(bot);
        }
        Ok(delivered)
    }

    async fn fire_due_routines(self: &Arc<Self>) {
        let now = chrono::Local::now().naive_local();
        let stamp = (now.year(), now.month(), now.day(), now.hour(), now.minute());
        let due: Vec<String> = {
            let fired = self.last_fired.lock();
            self.project
                .runtime
                .routines
                .iter()
                .filter(|r| r.enabled && r.cron.matches(now) && fired.get(&r.id) != Some(&stamp))
                .map(|r| r.id.clone())
                .collect()
        };
        for id in due {
            self.last_fired.lock().insert(id.clone(), stamp);
            if let Err(e) = self.run_routine(&id).await {
                eprintln!("revebot: routine {id}: {e}");
            }
        }
    }
}

fn spawn_routines(inner: Arc<Inner>) {
    tokio::spawn(async move {
        inner.fire_due_routines().await;
        loop {
            let secs = 60u64.saturating_sub(chrono::Local::now().timestamp() as u64 % 60);
            tokio::time::sleep(Duration::from_secs(secs.max(1))).await;
            inner.fire_due_routines().await;
        }
    });
}

fn spawn_supervisor(harness: Arc<Harness>, mut cmds: mpsc::Receiver<BotCmd>) {
    tokio::spawn(async move {
        let mut events = harness.subscribe();
        {
            let h = harness.clone();
            tokio::spawn(async move {
                let _ = h.resume_all().await;
                match h.kick(MAIN_LANE).await {
                    Ok(_) | Err(HarnessError::Idle(_)) | Err(HarnessError::Busy(_)) => {}
                    Err(_) => {}
                }
            });
        }
        loop {
            tokio::select! {
                cmd = cmds.recv() => {
                    let Some(cmd) = cmd else { break };
                    match cmd {
                        BotCmd::UserText { text, reply } => {
                            let ack = claim_then_drive(&harness, &text).await;
                            let _ = reply.send(ack);
                        }
                        BotCmd::Abort => {
                            let _ = harness.abort(MAIN_LANE).await;
                        }
                        BotCmd::KickNow => {
                            let h = harness.clone();
                            tokio::spawn(async move {
                                match h.kick(MAIN_LANE).await {
                                    Ok(_) | Err(HarnessError::Idle(_)) | Err(HarnessError::Busy(_)) => {}
                                    Err(_) => {}
                                }
                            });
                        }
                    }
                }
                event = events.recv() => {
                    match event {
                        Ok(ev) if matches!(ev.kind, Kind::RunEnd { .. }) => {
                            let h = harness.clone();
                            tokio::spawn(async move {
                                let _ = h.kick(MAIN_LANE).await;
                            });
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            if let Ok(Some((state, _))) = harness.session().lane_state(MAIN_LANE).await
                                && !state.pending_next_run.is_empty()
                                && state.current_operation_id.is_none()
                            {
                                let h = harness.clone();
                                tokio::spawn(async move {
                                    let _ = h.kick(MAIN_LANE).await;
                                });
                            }
                        }
                        Err(_) => break,
                        Ok(_) => {}
                    }
                }
            }
        }
    });
}

async fn claim_then_drive(harness: &Arc<Harness>, text: &str) -> Result<PromptAck, String> {
    match harness.steer_claimed(MAIN_LANE, text).await {
        Ok((entry_id, op)) => {
            return Ok(PromptAck {
                operation_id: op.to_string(),
                entry_id: entry_id.to_string(),
                mode: "steer",
            });
        }
        Err(HarnessError::Idle(_)) => {}
        Err(e) => return Err(e.to_string()),
    }
    match harness.begin_run(MAIN_LANE, text).await {
        Ok(current) => {
            let ack = PromptAck {
                operation_id: current.operation.operation_id.to_string(),
                entry_id: String::new(),
                mode: "prompt",
            };
            let harness = harness.clone();
            tokio::spawn(async move {
                let _ = harness.drive(current).await;
            });
            Ok(ack)
        }
        Err(HarnessError::Busy(_)) => match harness.steer_claimed(MAIN_LANE, text).await {
            Ok((entry_id, op)) => Ok(PromptAck {
                operation_id: op.to_string(),
                entry_id: entry_id.to_string(),
                mode: "steer",
            }),
            Err(e) => Err(e.to_string()),
        },
        Err(e) => Err(e.to_string()),
    }
}

struct CreateGuard {
    inner: Arc<Inner>,
    id: String,
    finished: bool,
}

impl Drop for CreateGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.inner.abort_create(&self.id);
        }
    }
}

/// Oldest-first transcript, then the last `limit` entries before `before`.
pub(crate) fn page_transcript(
    entries: Vec<crate::entry::Entry>,
    before: Option<u64>,
    limit: usize,
) -> (Vec<crate::entry::Entry>, bool) {
    let limit = limit.clamp(1, 200);
    let filtered: Vec<crate::entry::Entry> = match before {
        Some(seq) => entries.into_iter().filter(|e| e.seq < seq).collect(),
        None => entries,
    };
    let has_more = filtered.len() > limit;
    if !has_more {
        return (filtered, false);
    }
    let skip = filtered.len() - limit;
    (filtered.into_iter().skip(skip).collect(), true)
}

fn hex_token() -> String {
    use sha2::{Digest, Sha256};
    let bytes: [u8; 16] = rand::random();
    let digest = Sha256::digest(bytes);
    digest.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

fn resolve_model(project: &Project, spec: Option<&str>) -> Arc<dyn Model> {
    match resolve_model_checked(project, spec.unwrap_or("")) {
        Ok(model) => model,
        Err(why) => {
            let fallback = project.runtime.agent.model.as_deref().unwrap_or("");
            if spec.is_some() && spec != Some(fallback) {
                return resolve_model(project, project.runtime.agent.model.as_deref());
            }
            Arc::new(Unconfigured(why))
        }
    }
}

fn resolve_model_checked(project: &Project, spec: &str) -> Result<Arc<dyn Model>, String> {
    let spec = if spec.is_empty() {
        project
            .runtime
            .agent
            .model
            .clone()
            .ok_or_else(|| "agent.lua does not set a model".to_string())?
    } else {
        spec.to_string()
    };
    let models = Models::load(&project.root.join("models.yml")).map_err(|e| e.to_string())?;
    let resolved = models.resolve(&spec).map_err(|e| e.to_string())?;
    Ok(Arc::new(HttpModel::new(resolved)))
}

#[cfg(test)]
mod tests {
    use super::page_transcript;
    use crate::entry::Entry;
    use serde_json::json;

    fn msg(seq: u64) -> Entry {
        let mut entry = Entry::message(json!({"role": "user", "content": seq.to_string()}));
        entry.seq = seq;
        entry
    }

    #[test]
    fn transcript_page_returns_the_tail() {
        let entries: Vec<_> = (1..=10).map(msg).collect();
        let (page, more) = page_transcript(entries, None, 3);
        assert!(more);
        assert_eq!(
            page.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![8, 9, 10]
        );
    }

    #[test]
    fn transcript_page_before_is_strictly_older() {
        let entries: Vec<_> = (1..=10).map(msg).collect();
        let (page, more) = page_transcript(entries, Some(8), 3);
        assert!(more);
        assert_eq!(
            page.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![5, 6, 7]
        );
    }

    #[test]
    fn a_short_transcript_is_not_paged() {
        let entries: Vec<_> = (1..=2).map(msg).collect();
        let (page, more) = page_transcript(entries, None, 80);
        assert!(!more);
        assert_eq!(page.len(), 2);
    }
}
