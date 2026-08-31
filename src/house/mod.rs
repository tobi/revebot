//! A house: one microVM, many bots, one shared `/workspace`.

pub(crate) mod attach;
pub(crate) mod files;
pub mod fs;
pub mod home;
pub mod memory;
pub mod profile;
pub mod prompt;
pub mod resources;
pub mod secret;
pub mod serve;
pub mod tools;
pub mod usage;
pub mod wrap;

#[cfg(test)]
mod microvm_tests;

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

use profile::{BOT_CAP, Profile, scan_checked, slug_from_name, unique_slug};
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
    usage: usage::UsageLog,
    memory_edits: tokio::sync::Mutex<()>,
    profile_edits: tokio::sync::Mutex<()>,
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
    context: crate::working_directory::Context,
    profile_error: Option<String>,
}

#[derive(Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateSpec {
    pub name: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub soul: Option<String>,
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
            usage: usage::UsageLog::open(&project.state_dir()),
            memory_edits: tokio::sync::Mutex::new(()),
            profile_edits: tokio::sync::Mutex::new(()),
        });
        *inner.me.lock() = Arc::downgrade(&inner);

        let mut profiles = scan_checked(&project.root)?;
        if profiles.is_empty() {
            crate::project::init(&project.root)?;
            profiles = scan_checked(&project.root)?;
        }
        for profile in profiles {
            if let Err(error) = inner.spawn_ready(profile).await {
                let _ = Self {
                    inner: inner.clone(),
                }
                .shutdown()
                .await;
                return Err(error);
            }
        }
        // Resume + kick are the supervisor's first job. Awaiting
        // `resume_all` here would block the HTTP server until every
        // in-flight run finished (a hung tool looks like a stuck boot).

        spawn_routines(inner.clone());
        spawn_resource_observers(Arc::downgrade(&inner), inner.house_events.subscribe());

        Ok(Self { inner })
    }

    pub fn ready_profiles(&self) -> Vec<Profile> {
        self.inner.ready_profiles()
    }

    pub fn profile_views(&self) -> Vec<serde_json::Value> {
        self.inner.refresh_profiles();
        let mut views: Vec<_> = self
            .inner
            .snapshot
            .read()
            .values()
            .filter_map(|slot| {
                let BotSlot::Ready(rt) = slot else {
                    return None;
                };
                let mut value = serde_json::to_value(&rt.profile).expect("profile JSON");
                value["profile_error"] = serde_json::json!(rt.profile_error);
                value["cwd"] = serde_json::json!(rt.context.cwd());
                Some(value)
            })
            .collect();
        views.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        views
    }

    pub async fn bot_is_busy(&self, id: &str) -> bool {
        let Ok(harness) = self.inner.ready_harness(id) else {
            return false;
        };
        match harness.session().lane_state(MAIN_LANE).await {
            Ok(Some((state, _))) => state.current_operation_id.is_some(),
            _ => false,
        }
    }

    pub async fn bots_view(&self) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        for mut value in self.profile_views() {
            let id = value["id"].as_str().unwrap_or_default().to_string();
            value["busy"] = serde_json::json!(self.bot_is_busy(&id).await);
            out.push(value);
        }
        out
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

    pub fn bot_soul(&self, bot: &str) -> anyhow::Result<String> {
        self.inner.ready_harness(bot)?;
        Ok(files::read_optional(
            &self.inner.project.root,
            &home::relative(bot)?.join("SOUL.md"),
        )?
        .unwrap_or_default())
    }

    pub async fn set_bot_soul(&self, bot: &str, text: &str) -> anyhow::Result<()> {
        self.inner.ready_harness(bot)?;
        let _guard = self.inner.profile_edits.lock().await;
        let relative = home::relative(bot)?.join("SOUL.md");
        files::Change {
            before: files::read_optional(&self.inner.project.root, &relative)?,
            relative: relative.clone(),
            after: text.into(),
        }
        .apply(&self.inner.sandbox)
        .await?;
        self.inner
            .workspace_changed(bot, vec![format!("/{}", relative.display())], false)
            .await
    }

    pub fn save_attachment(
        &self,
        bot: &str,
        name: &str,
        bytes: &[u8],
        mime: Option<&str>,
    ) -> anyhow::Result<attach::Saved> {
        self.inner.ready_harness(bot)?;
        Ok(attach::save(&self.inner.project.root, name, bytes, mime)?)
    }

    pub fn read_attachment(
        &self,
        bot: &str,
        id: &str,
        name: &str,
    ) -> anyhow::Result<(attach::Saved, Vec<u8>)> {
        self.inner.ready_harness(bot)?;
        Ok(attach::read(&self.inner.project.root, id, name)?)
    }

    pub fn configured_models(&self) -> Vec<String> {
        crate::provider::config::Models::load(&self.inner.project.root.join("models.yml"))
            .map(|m| m.catalog())
            .unwrap_or_default()
    }

    pub fn list_workspace(&self, path: &str) -> Result<fs::FsList, String> {
        fs::list(self.inner.project.workspace().as_path(), path)
    }

    pub fn read_workspace(&self, path: &str) -> Result<fs::FsFile, String> {
        fs::read(self.inner.project.workspace().as_path(), path)
    }

    pub fn stat_workspace(&self, path: &str) -> Result<fs::FsStat, String> {
        fs::stat(self.inner.project.workspace().as_path(), path)
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

    /// Unscoped exec/tool clients can also write indirectly. No bot identity is
    /// invented: only global observers opting into unknown effects receive it.
    pub fn external_effect_finished(&self) {
        self.inner.refresh_profiles();
        let _ = self.inner.house_events.send(Event::new(
            "house",
            None,
            Kind::ResourcesChanged {
                bot: String::new(),
                cwd: self.inner.sandbox.workdir().into(),
                paths: Vec::new(),
                resources: Vec::new(),
                unknown: true,
            },
        ));
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
        let sessions: Vec<_> = self
            .inner
            .snapshot
            .read()
            .values()
            .filter_map(|slot| match slot {
                BotSlot::Ready(rt) => Some(rt.session.clone()),
                _ => None,
            })
            .collect();
        for session in sessions {
            session.close().await;
        }
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
    pub(crate) async fn workspace_changed(
        &self,
        bot: &str,
        paths: Vec<String>,
        unknown: bool,
    ) -> anyhow::Result<()> {
        self.refresh_profiles();
        let context = self.context(bot)?;
        let change = resources::Change::new(bot, context.cwd(), paths, unknown);
        if unknown || change.resources.iter().any(|r| r == "directory_rules") {
            // Explicit paths refresh every affected conversation. Unknown shell
            // paths refresh the originating conversation; others refresh on input.
            let contexts: Vec<_> = self
                .snapshot
                .read()
                .values()
                .filter_map(|slot| {
                    let BotSlot::Ready(rt) = slot else {
                        return None;
                    };
                    (rt.profile.id == bot || change.paths.iter().any(|p| rt.context.inherits(p)))
                        .then(|| (rt.context.clone(), rt.session.clone()))
                })
                .collect();
            for (context, session) in contexts {
                context
                    .change(&session, MAIN_LANE, &self.sandbox, ".")
                    .await?;
            }
        }
        if unknown || !change.paths.is_empty() {
            let _ = self.house_events.send(Event::new(
                "house",
                None,
                Kind::ResourcesChanged {
                    bot: change.bot,
                    cwd: change.cwd,
                    paths: change.paths,
                    resources: change.resources,
                    unknown,
                },
            ));
        }
        Ok(())
    }

    fn refresh_profiles(&self) {
        let mut changed = false;
        let mut snap = self.snapshot.write();
        for (id, slot) in snap.iter_mut() {
            let BotSlot::Ready(rt) = slot else {
                continue;
            };
            changed |= profile::refresh(
                &self.project.root,
                id,
                &mut rt.profile,
                &mut rt.profile_error,
            );
        }
        let ids = snap
            .iter()
            .filter_map(|(id, slot)| matches!(slot, BotSlot::Ready(_)).then_some(id.clone()))
            .collect();
        drop(snap);
        if changed {
            let _ = self
                .house_events
                .send(Event::new("house", None, Kind::RosterChanged { ids }));
        }
    }

    fn context(&self, bot: &str) -> anyhow::Result<crate::working_directory::Context> {
        profile::validate_id(bot)?;
        match self.snapshot.read().get(bot) {
            Some(BotSlot::Ready(rt)) => Ok(rt.context.clone()),
            _ => anyhow::bail!("unknown bot {bot}"),
        }
    }

    pub(crate) async fn change_directory(&self, bot: &str, path: &str) -> anyhow::Result<String> {
        let harness = self.ready_harness(bot)?;
        self.context(bot)?
            .change(harness.session(), MAIN_LANE, &self.sandbox, path)
            .await
    }

    pub(crate) async fn update_memory(
        &self,
        bot: &str,
        request: memory::Request,
    ) -> anyhow::Result<String> {
        self.ready_harness(bot)?;
        let _guard = self.memory_edits.lock().await;
        let profile = Profile::load_for(&self.project.root, bot)?;
        let planned = memory::plan(&self.project.root, &profile, &request, chrono::Utc::now())?;
        if let Some(change) = planned.change {
            change.apply(&self.sandbox).await?;
            self.workspace_changed(bot, vec![format!("/{}", change.relative.display())], false)
                .await?;
        }
        Ok(planned.result)
    }

    fn ready_profiles(&self) -> Vec<Profile> {
        self.refresh_profiles();
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
        profile::validate_id(id)?;
        let snap = self.snapshot.read();
        match snap.get(id) {
            Some(BotSlot::Ready(rt)) => Ok(rt.harness.clone()),
            _ => anyhow::bail!("unknown bot {id}"),
        }
    }

    async fn prompt(&self, bot: &str, text: &str) -> anyhow::Result<PromptAck> {
        let harness = self.ready_harness(bot)?;
        self.context(bot)?
            .change(harness.session(), MAIN_LANE, &self.sandbox, ".")
            .await?;
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
        for skill in wrap::skills_in(text, &skills) {
            let source = if skill.path.starts_with(self.project.bot_dir(bot)) {
                "bot"
            } else {
                "workspace"
            };
            self.usage
                .record(&usage::UsageEvent::skill(bot, &skill.name, source));
        }
        let wrapped = wrap::wrap_user_turn_at(
            text,
            &self.ready_profiles(),
            &skills,
            &updated,
            &removed,
            &wrap::timestamp_now(),
        );
        let wrapped = wrap::with_cwd(&wrapped, &self.context(bot)?.cwd());
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
        profile::validate_id(&id)?;
        home::ensure(&self.sandbox, &profile).await?;
        crate::script_fs::open_dir(&self.project.root, &home::relative(&id)?.join("sessions"))?;
        let session_path = self
            .project
            .latest_bot_session_checked(&id, MAIN_LANE)?
            .unwrap_or_else(|| self.project.bot_conversation_path(&id, MAIN_LANE));
        let storage = Storage::open_beneath(
            &self.project.root,
            session_path.strip_prefix(&self.project.root)?,
            &id,
            Some(home::guest(&id)?),
        )?;
        let session = Session::spawn(storage);
        let model = resolve_model(&self.project, profile.model.as_deref());
        let context = crate::working_directory::Context::new(&id)?;
        context.restore(&session, MAIN_LANE, &self.sandbox).await?;
        let toolbox = Toolbox::for_context(
            self.sandbox.clone(),
            self.project.runtime_arc(),
            context.clone(),
        );
        let house_tools = HouseTools {
            inner: toolbox,
            house: Arc::downgrade(self),
            bot_id: id.clone(),
        };
        let active_tool_names = house_tools.tool_names();
        let tools = Arc::new(house_tools);
        let prompt_profile = profile.clone();
        let prompt_context = context.clone();
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
                    format!(
                        "{}\n\nCurrent directory: {}\n{}",
                        system_prompt(&inner.project, &prompt_profile, &inner.ready_profiles()),
                        prompt_context.cwd(),
                        prompt_context.instructions()
                    )
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

        bind_profile_environment(
            &harness,
            self.project.clone(),
            id.clone(),
            tools.tool_names(),
        );
        let (cmds, cmd_rx) = mpsc::channel(32);
        let runtime = BotRuntime {
            profile: profile.clone(),
            harness: harness.clone(),
            session,
            cmds: cmds.clone(),
            context,
            profile_error: None,
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
        spawn_supervisor(harness, cmd_rx, Arc::downgrade(self), id.clone());
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
        if spec.name.trim().is_empty() {
            anyhow::bail!("agent name cannot be blank");
        }
        if spec.model.as_ref().is_some_and(|m| m.trim().is_empty()) {
            anyhow::bail!("model cannot be blank; omit it for the house default");
        }
        if let Some(model) = &spec.model {
            resolve_model_checked(&self.project, model).map_err(anyhow::Error::msg)?;
        }
        let mut snap = self.snapshot.write();
        if snap.len() >= BOT_CAP {
            anyhow::bail!("bot cap ({BOT_CAP}) reached");
        }
        let base = slug_from_name(&spec.name);
        let id = unique_slug(&base, |s| {
            snap.contains_key(s) || self.project.bot_dir(s).try_exists().unwrap_or(true)
        });
        let profile = Profile {
            id: id.clone(),
            name: spec.name.clone(),
            title: spec.title.clone(),
            description: spec.description.clone(),
            avatar: spec.avatar.clone(),
            group: String::new(),
            created_at: Some(chrono::Utc::now().to_rfc3339()),
            model: spec.model.clone(),
            projects: Vec::new(),
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
        let relative = home::relative(&profile.id)?;
        files::Change {
            relative: relative.join("profile.json"),
            before: None,
            after: profile.to_json(),
        }
        .apply(&self.sandbox)
        .await?;
        files::Change {
            relative: relative.join("SOUL.md"),
            before: None,
            after: spec.soul.clone().unwrap_or_else(|| home::soul(profile)),
        }
        .apply(&self.sandbox)
        .await?;
        Ok(())
    }

    async fn finish_create(self: &Arc<Self>, id: &str) -> anyhow::Result<()> {
        let profile = Profile::load_for(&self.project.root, id)?;
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
        self.ready_harness(id)?;
        let _guard = self.profile_edits.lock().await;
        let relative = home::relative(id)?.join("profile.json");
        let before = files::read_optional(&self.project.root, &relative)?
            .ok_or_else(|| anyhow::anyhow!("profile.json is missing"))?;
        let (profile, after) = profile::merge_patch(id, &before, &patch)?;
        if patch.get("model").is_some()
            && let Some(spec) = &profile.model
        {
            resolve_model_checked(&self.project, spec).map_err(|e| anyhow::anyhow!(e))?;
        }
        files::Change {
            relative,
            before: Some(before),
            after,
        }
        .apply(&self.sandbox)
        .await?;
        self.workspace_changed(
            id,
            vec![format!("/workspace/agents/{id}/profile.json")],
            false,
        )
        .await?;
        Ok(profile)
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
        let path = home::guest(id)?;
        let result = self
            .sandbox
            .exec(
                &format!("rm -rf -- {}", shell_words::quote(&path)),
                crate::sandbox::ExecOptions::default(),
                None,
            )
            .await?;
        if !result.success || result.cancelled {
            anyhow::bail!("could not delete bot home: {}", result.stderr);
        }
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
        self.refresh_profiles();
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
        let wake = wrap::with_cwd(
            &wrap_agent_arrival(&from_name, from, text),
            &self.context(to)?.cwd(),
        );
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

fn spawn_resource_observers(house: Weak<Inner>, mut events: broadcast::Receiver<Event>) {
    tokio::spawn(async move {
        loop {
            let event = match events.recv().await {
                Ok(event) => event,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            };
            let Kind::ResourcesChanged {
                bot,
                cwd,
                paths,
                resources,
                unknown,
            } = event.kind
            else {
                continue;
            };
            let Some(inner) = house.upgrade() else {
                break;
            };
            let change = resources::Change {
                bot,
                cwd,
                paths,
                resources,
                unknown,
            };
            let (sends, errors) = inner.project.runtime.run_changes(&change).await;
            for error in errors {
                eprintln!("revebot: change observer: {error}");
            }
            for (target, text) in sends {
                let result = async {
                    let harness = inner.ready_harness(&target)?;
                    let wrapped = wrap::wrap_user_turn(
                        &format!("[plugin] Resource-change notification\n\n{text}"),
                        &inner.ready_profiles(),
                        &[],
                    );
                    let content = wrap::with_cwd(&wrapped, &inner.context(&target)?.cwd());
                    harness.next_run(MAIN_LANE, &content).await?;
                    let cmds = match inner.snapshot.read().get(&target) {
                        Some(BotSlot::Ready(rt)) => rt.cmds.clone(),
                        _ => anyhow::bail!("unknown observer recipient"),
                    };
                    let _ = cmds.send(BotCmd::KickNow).await;
                    Ok::<_, anyhow::Error>(())
                }
                .await;
                if let Err(error) = result {
                    eprintln!("revebot: observer delivery: {error}");
                }
            }
        }
    });
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

fn publish_busy(house: &Weak<Inner>, bot_id: &str, busy: bool) {
    let Some(inner) = house.upgrade() else {
        return;
    };
    let _ = inner.house_events.send(Event::new(
        "house",
        None,
        Kind::BotBusy {
            bot_id: bot_id.into(),
            busy,
        },
    ));
}

fn spawn_supervisor(
    harness: Arc<Harness>,
    mut cmds: mpsc::Receiver<BotCmd>,
    house: Weak<Inner>,
    bot_id: String,
) {
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
                        Ok(ev) => {
                            match &ev.kind {
                                Kind::RunStart | Kind::RunResume { .. } => {
                                    publish_busy(&house, &bot_id, true);
                                }
                                Kind::RunEnd { .. } => {
                                    publish_busy(&house, &bot_id, false);
                                    let h = harness.clone();
                                    tokio::spawn(async move {
                                        let _ = h.kick(MAIN_LANE).await;
                                    });
                                }
                                _ => {}
                            }
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

pub(crate) fn bind_profile_environment(
    harness: &Harness,
    project: Arc<Project>,
    id: String,
    tool_names: Vec<String>,
) {
    let model_project = project.clone();
    harness.set_environment_sources(
        Arc::new(move || {
            let profile = Profile::load_for(&project.root, &id).map_err(|e| e.to_string())?;
            let spec = profile
                .model
                .or_else(|| project.runtime.agent.model.clone())
                .unwrap_or_else(|| "none".into());
            Ok(LaneConfiguration {
                model: ModelRef {
                    provider: spec.clone(),
                    model_id: spec,
                },
                thinking_level: project
                    .runtime
                    .agent
                    .thinking
                    .clone()
                    .unwrap_or_else(|| "default".into()),
                active_tool_names: tool_names.clone(),
            })
        }),
        Arc::new(move |configuration| {
            resolve_model_checked(&model_project, &configuration.model.model_id)
                .unwrap_or_else(|why| Arc::new(Unconfigured(why)))
        }),
    );
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
    use super::{CreateSpec, page_transcript};
    use crate::entry::Entry;
    use serde_json::json;

    fn msg(seq: u64) -> Entry {
        let mut entry = Entry::message(json!({"role": "user", "content": seq.to_string()}));
        entry.seq = seq;
        entry
    }

    #[test]
    fn creation_uses_one_closed_typed_contract() {
        let spec: CreateSpec =
            serde_json::from_value(serde_json::json!({"name":"Miku","soul":"Music remit"}))
                .unwrap();
        assert_eq!(spec.soul.as_deref(), Some("Music remit"));
        assert!(spec.title.is_empty());
        for value in [
            serde_json::json!({"name":"Miku","unexpected":true}),
            serde_json::json!({"name":"Miku","soul":42}),
        ] {
            assert!(serde_json::from_value::<CreateSpec>(value).is_err());
        }
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
