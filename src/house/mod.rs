//! A house: one microVM, many bots, one shared `/workspace`.

pub(crate) mod attach;
pub mod client;
pub(crate) mod files;
pub mod fs;
pub mod home;
pub mod memory;
pub mod profile;
pub mod prompt;
pub mod resources;
mod roster;
pub mod secret;
pub mod serve;
pub(crate) mod skill_improve;
pub mod tailnet;
pub mod tools;
pub mod usage;
pub mod wrap;

#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
mod microvm_tests;

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Duration;

use anyhow::Context;
use chrono::{Datelike, Timelike};
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use sha2::Digest as _;
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
    snapshot: RwLock<roster::Roster<BotRuntime>>,
    jobs: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    controllers: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    sessions: Mutex<HashMap<String, Session>>,
    shutdown_lock: tokio::sync::Mutex<()>,
    house_events: broadcast::Sender<Event>,
    me: Mutex<Weak<Inner>>,
    /// Last minute a routine actually fired, so a tick cannot double-send.
    last_fired: Mutex<HashMap<String, MinuteStamp>>,
    /// Per-bot skill fingerprints so a created/edited SKILL.md is attached to
    /// the next user turn. Empty map = first snapshot, not "all new".
    skill_seen: Mutex<HashMap<String, BTreeMap<String, u64>>>,
    /// One in-flight `AskUserForSecret` per bot.
    secret_asks: Mutex<HashMap<String, tokio::sync::oneshot::Sender<SecretAskResult>>>,
    /// Per-bot plugin tool names currently offered to the model.
    plugin_offers: Mutex<HashMap<String, std::collections::BTreeSet<String>>>,
    /// Per-bot statusline slots (`loop` → `2 loops`).
    plugin_status: Mutex<HashMap<String, BTreeMap<String, String>>>,
    /// Extra plugin wakes from `ctx.set_timer`.
    plugin_wake: Mutex<HashMap<String, std::time::Instant>>,
    /// Last `update` fire per plugin.
    plugin_last: Mutex<HashMap<String, std::time::Instant>>,
    usage: usage::UsageLog,
    memory_edits: tokio::sync::Mutex<()>,
    profile_edits: tokio::sync::Mutex<()>,
    skill_edits: tokio::sync::Mutex<()>,
    /// User turns since the last skill-improve nudge, per bot.
    skill_turns: Mutex<HashMap<String, u32>>,
}

#[derive(Debug, Clone)]
pub(crate) enum SecretAskResult {
    Declined,
    Saved { env: String, hosts: Vec<String> },
}

type MinuteStamp = (i32, u32, u32, u32, u32);

type BotSlot = roster::Slot<BotRuntime>;

struct BotRuntime {
    profile: Profile,
    harness: Arc<Harness>,
    session: Session,
    cmds: mpsc::Sender<BotCmd>,
    context: crate::working_directory::Context,
    profile_error: Option<String>,
    session_key: String,
    supervisor: Option<tokio::task::AbortHandle>,
}

impl Drop for BotRuntime {
    fn drop(&mut self) {
        if let Some(supervisor) = &self.supervisor {
            supervisor.abort();
        }
    }
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
    KickNow,
    Stop(tokio::sync::oneshot::Sender<()>),
    Compact {
        instructions: Option<String>,
        reply: tokio::sync::oneshot::Sender<Result<CompactAck, String>>,
    },
    Quiesce(tokio::sync::oneshot::Sender<Result<(), String>>),
}

#[derive(Debug, Clone)]
pub struct PromptAck {
    pub log_id: String,
    pub record: Option<crate::log::Record>,
    pub operation_id: String,
    pub entry_id: String,
    pub mode: &'static str,
}

#[derive(Debug, Clone)]
pub struct CompactAck {
    pub log_id: String,
    pub operation_id: String,
    pub outcome: &'static str,
}

#[derive(Debug, Clone)]
pub struct SwitchAck {
    pub log_id: String,
    pub previous_log_id: String,
}

#[derive(Debug, Clone)]
enum SwitchKind {
    New,
    Fork(Option<crate::ids::EntryId>),
}

#[derive(Serialize)]
pub struct LogPage {
    pub log_id: String,
    pub records: Vec<crate::log::Record>,
    pub operation_id: Option<String>,
    pub oldest_seq: Option<u64>,
    pub has_more: bool,
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

        let preflight = scan_checked(&project.root)?;
        if preflight.len() > BOT_CAP {
            anyhow::bail!("bot cap ({BOT_CAP}) exceeded");
        }
        let name =
            crate::sandbox::Sandbox::sandbox_name_for(&project.runtime.policy, project.workspace());
        Sandbox::reclaim_namesake(&name).await.ok();

        let sandbox = Box::pin(Sandbox::start(
            project.runtime.policy.clone(),
            project.workspace(),
            project.state_dir(),
            progress,
        ))
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
            snapshot: RwLock::new(roster::Roster::default()),
            jobs: Mutex::new(Vec::new()),
            controllers: Mutex::new(Vec::new()),
            sessions: Mutex::new(HashMap::new()),
            shutdown_lock: tokio::sync::Mutex::new(()),
            house_events,
            me: Mutex::new(Weak::new()),
            last_fired: Mutex::new(HashMap::new()),
            skill_seen: Mutex::new(HashMap::new()),
            secret_asks: Mutex::new(HashMap::new()),
            plugin_offers: Mutex::new(HashMap::new()),
            plugin_status: Mutex::new(HashMap::new()),
            plugin_wake: Mutex::new(HashMap::new()),
            plugin_last: Mutex::new(HashMap::new()),
            usage: usage::UsageLog::open(&project.state_dir()),
            memory_edits: tokio::sync::Mutex::new(()),
            profile_edits: tokio::sync::Mutex::new(()),
            skill_edits: tokio::sync::Mutex::new(()),
            skill_turns: Mutex::new(HashMap::new()),
        });
        *inner.me.lock() = Arc::downgrade(&inner);

        let mut profiles = preflight;
        if profiles.is_empty() {
            crate::project::init(&project.root)?;
            profiles = scan_checked(&project.root)?;
        }
        for profile in profiles {
            if let Err(error) = Box::pin(inner.spawn_ready(profile)).await {
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

        inner
            .controllers
            .lock()
            .push(spawn_routines(Arc::downgrade(&inner)));
        inner
            .controllers
            .lock()
            .push(spawn_plugins(Arc::downgrade(&inner)));
        inner
            .controllers
            .lock()
            .push(spawn_curator(Arc::downgrade(&inner)));
        inner.controllers.lock().push(spawn_resource_observers(
            Arc::downgrade(&inner),
            inner.house_events.subscribe(),
        ));

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
            .filter_map(|slot| match slot {
                BotSlot::Ready(rt) => {
                    let mut value = serde_json::to_value(&rt.profile).ok()?;
                    let obj = value.as_object_mut()?;
                    obj.insert("profile_error".into(), serde_json::json!(rt.profile_error));
                    obj.insert("cwd".into(), serde_json::json!(rt.context.cwd()));
                    obj.insert("log_id".into(), serde_json::json!(rt.session.id()));
                    obj.insert("status".into(), "ready".into());
                    Some(value)
                }
                BotSlot::Replacing { profile, .. } => {
                    let mut value = serde_json::to_value(profile).ok()?;
                    let obj = value.as_object_mut()?;
                    obj.insert(
                        "profile_error".into(),
                        serde_json::json!("Switching conversation"),
                    );
                    obj.insert("status".into(), "switching".into());
                    Some(value)
                }
                BotSlot::Deleting {
                    profile,
                    error,
                    retryable,
                    ..
                } => {
                    let mut value = serde_json::to_value(profile).ok()?;
                    let obj = value.as_object_mut()?;
                    obj.insert(
                        "profile_error".into(),
                        serde_json::json!(
                            error
                                .as_deref()
                                .unwrap_or("Deleting: waiting for owned work to stop")
                        ),
                    );
                    obj.insert(
                        "status".into(),
                        if error.is_some() {
                            "delete_failed"
                        } else {
                            "deleting"
                        }
                        .into(),
                    );
                    obj.insert("delete_retryable".into(), (*retryable).into());
                    Some(value)
                }
                BotSlot::Creating { .. } => None,
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
            let id = value
                .get("id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            let busy = self.bot_is_busy(&id).await;
            if let Some(obj) = value.as_object_mut() {
                obj.insert("busy".into(), serde_json::json!(busy));
            }
            out.push(value);
        }
        out
    }

    pub async fn prompt(
        &self,
        bot: &str,
        text: &str,
        log_id: Option<&str>,
    ) -> anyhow::Result<PromptAck> {
        self.inner.prompt(bot, text, log_id).await
    }

    pub async fn compact(
        &self,
        bot: &str,
        instructions: Option<String>,
        log_id: &str,
    ) -> anyhow::Result<CompactAck> {
        self.inner.compact(bot, instructions, log_id).await
    }

    pub async fn new_chat(&self, bot: &str, log_id: &str) -> anyhow::Result<SwitchAck> {
        Box::pin(self.inner.clone().switch_chat(bot, log_id, SwitchKind::New)).await
    }

    pub async fn fork_chat(
        &self,
        bot: &str,
        log_id: &str,
        entry_id: Option<crate::ids::EntryId>,
    ) -> anyhow::Result<SwitchAck> {
        Box::pin(
            self.inner
                .clone()
                .switch_chat(bot, log_id, SwitchKind::Fork(entry_id)),
        )
        .await
    }

    pub async fn abort(&self, bot: &str) -> anyhow::Result<()> {
        self.inner.abort_bot(bot).await
    }

    pub async fn transcript(&self, bot: &str) -> anyhow::Result<Vec<crate::entry::Entry>> {
        let harness = self.inner.ready_harness(bot)?;
        Ok(harness.session().transcript(MAIN_LANE).await?)
    }

    pub async fn log_record(
        &self,
        bot: &str,
        entry: crate::ids::EntryId,
    ) -> anyhow::Result<Option<crate::log::Record>> {
        Ok(self
            .inner
            .ready_harness(bot)?
            .log_record(MAIN_LANE, entry)
            .await?)
    }

    pub async fn log_page(
        &self,
        bot: &str,
        before: Option<u64>,
        limit: usize,
    ) -> anyhow::Result<LogPage> {
        use crate::log::Status;
        let harness = self.inner.ready_harness(bot)?;
        let rows = harness.log_snapshot(MAIN_LANE).await?;
        let operation_id = harness
            .session()
            .lane_state(MAIN_LANE)
            .await?
            .and_then(|(state, _)| state.current_operation_id.map(|id| id.to_string()));
        let mut committed: Vec<_> = rows
            .iter()
            .filter(|r| r.status == Status::Committed && before.is_none_or(|seq| r.order < seq))
            .cloned()
            .collect();
        let limit = limit.clamp(1, 200);
        let has_more = committed.len() > limit;
        if has_more {
            committed.drain(..committed.len() - limit);
        }
        if before.is_none() {
            committed.extend(rows.into_iter().filter(|r| r.status != Status::Committed));
        }
        committed.sort_by_key(|r| r.order);
        let oldest_seq = committed
            .iter()
            .filter(|r| r.status == Status::Committed)
            .map(|r| r.order)
            .min();
        Ok(LogPage {
            records: committed,
            has_more,
            oldest_seq,
            operation_id,
            log_id: harness.session().id().into(),
        })
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
        let expected = self.inner.ready_harness(bot)?.session().id().to_string();
        let _guard = self.inner.profile_edits.lock().await;
        if self.inner.ready_harness(bot)?.session().id() != expected {
            anyhow::bail!("bot was replaced during soul update");
        }
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
        let mut listings = crate::skills::listings_for(
            &self.inner.project.workspace(),
            &self.inner.project.bot_dir(bot),
        );
        for plugin in &self.inner.project.runtime.plugins {
            if !plugin.has_command() {
                continue;
            }
            if plugin.owner.as_deref().is_some_and(|id| id != bot) {
                continue;
            }
            if listings.iter().any(|s| s.name == plugin.name) {
                continue;
            }
            listings.push(crate::skills::SkillListing {
                name: plugin.name.clone(),
                description: format!("/{0} plugin command", plugin.name),
                source: "plugin".into(),
            });
        }
        listings
    }

    pub async fn complete_secret(
        &self,
        bot: &str,
        decision: secret::SecretDecision,
    ) -> anyhow::Result<String> {
        self.inner.complete_secret(bot, decision).await
    }

    pub fn subscribe(&self, bot: &str) -> anyhow::Result<(String, broadcast::Receiver<Event>)> {
        let harness = self.inner.ready_harness(bot)?;
        Ok((harness.session().id().into(), harness.subscribe()))
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
        let _shutdown = self.inner.shutdown_lock.lock().await;
        let runtimes = self.inner.snapshot.write().close();
        let jobs = std::mem::take(&mut *self.inner.jobs.lock());
        for job in &jobs {
            job.abort();
        }
        for job in jobs {
            let _ = job.await;
        }
        for runtime in &runtimes {
            runtime.harness.close().await;
        }
        let sessions = std::mem::take(&mut *self.inner.sessions.lock());
        for (_, session) in sessions {
            session.close().await;
        }
        let controllers = std::mem::take(&mut *self.inner.controllers.lock());
        for controller in &controllers {
            controller.abort();
        }
        for controller in controllers {
            let _ = controller.await;
        }
        drop(runtimes);
        self.inner.sandbox.release_hold().await;
        self.inner.sandbox.stop().await?;
        let _ = std::fs::remove_file(self.inner.project.state_dir().join("house.json"));
        let _ = std::fs::remove_file(&self.inner.sock);
        Ok(())
    }

    pub fn write_house_json(&self, tailnet: Option<&str>) -> anyhow::Result<()> {
        let path = self.inner.project.state_dir().join("house.json");
        let mut body = serde_json::Map::new();
        body.insert("pid".into(), serde_json::json!(std::process::id()));
        body.insert("bind".into(), serde_json::json!(self.inner.bind));
        body.insert("sock".into(), serde_json::json!(self.inner.sock));
        body.insert("token".into(), serde_json::json!(self.inner.token));
        body.insert("status".into(), serde_json::json!("ready"));
        body.insert(
            "started_at".into(),
            serde_json::json!(chrono::Utc::now().to_rfc3339()),
        );
        if let Some(tailnet) = tailnet {
            body.insert("tailnet".into(), serde_json::json!(tailnet));
        }
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&serde_json::Value::Object(body))?,
        )?;
        Ok(())
    }
}

impl Inner {
    /// Mutation jobs belong to the house, not a disconnecting HTTP request or
    /// an aborted caller tool. Registration linearizes with house shutdown.
    async fn owned<T, F>(self: &Arc<Self>, work: F) -> anyhow::Result<T>
    where
        T: Send + 'static,
        F: std::future::Future<Output = anyhow::Result<T>> + Send + 'static,
    {
        let rx = {
            let roster = self.snapshot.read();
            if roster.is_closed() {
                anyhow::bail!("house is closing");
            }
            roster::spawn_job(&self.jobs, work)
        };
        rx.await
            .map_err(|_| anyhow::anyhow!("house mutation stopped"))?
            .map_err(anyhow::Error::msg)
    }

    fn announce_roster(&self) {
        let ids = self
            .snapshot
            .read()
            .iter()
            .filter_map(|(id, slot)| {
                (!matches!(slot, BotSlot::Creating { .. })).then_some(id.clone())
            })
            .collect();
        let _ = self
            .house_events
            .send(Event::new("house", None, Kind::RosterChanged { ids }));
    }

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
        for (id, rt) in snap.ready_mut() {
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
        let expected = self.ready_harness(bot)?.session().id().to_string();
        let _guard = self.memory_edits.lock().await;
        if self.ready_harness(bot)?.session().id() != expected {
            anyhow::bail!("bot was replaced during memory update");
        }
        let profile = Profile::load_for(&self.project.root, bot)?;
        let planned = memory::plan(&self.project.root, &profile, &request, chrono::Utc::now())?;
        if let Some(change) = planned.change {
            change.apply(&self.sandbox).await?;
            self.workspace_changed(bot, vec![format!("/{}", change.relative.display())], false)
                .await?;
        }
        Ok(planned.result)
    }

    fn take_skill_nudge(&self, bot: &str) -> bool {
        let interval = skill_improve::nudge_interval(&self.project.root);
        if interval == 0 {
            return false;
        }
        let mut turns = self.skill_turns.lock();
        let slot = turns.entry(bot.to_string()).or_insert(0);
        *slot = slot.saturating_add(1);
        if *slot >= interval {
            *slot = 0;
            true
        } else {
            false
        }
    }

    pub(crate) async fn skill_manage(
        &self,
        bot: &str,
        args: serde_json::Map<String, serde_json::Value>,
    ) -> Result<String, String> {
        let request = skill_improve::Request::parse(&args)?;
        let expected = self
            .ready_harness(bot)
            .map_err(|e| e.to_string())?
            .session()
            .id()
            .to_string();
        let _guard = self.skill_edits.lock().await;
        if self
            .ready_harness(bot)
            .map_err(|e| e.to_string())?
            .session()
            .id()
            != expected
        {
            return Err("bot was replaced during skill_manage".into());
        }
        let plan = skill_improve::plan(&self.project.root, bot, &request)?;
        for change in &plan.writes {
            change
                .apply(&self.sandbox)
                .await
                .map_err(|e| e.to_string())?;
            self.workspace_changed(bot, vec![format!("/{}", change.relative.display())], false)
                .await
                .map_err(|e| e.to_string())?;
        }
        for relative in &plan.remove {
            let path = format!("/{}", relative.display());
            let cmd = format!("rm -f -- {}", shell_words::quote(&path));
            let output = self
                .sandbox
                .exec(&cmd, crate::sandbox::ExecOptions::default(), None)
                .await
                .map_err(|e| e.to_string())?;
            if !output.success || output.cancelled {
                return Err(format!("failed to remove {path}: {}", output.stderr.trim()));
            }
            self.workspace_changed(bot, vec![path], false)
                .await
                .map_err(|e| e.to_string())?;
        }
        if let Some(name) = &plan.archive {
            let curator = crate::curator::Curator::open(&self.project.root);
            let _ = curator.adopt(name);
            curator.archive(name).map_err(|e| e.to_string())?;
        }
        let curator = crate::curator::Curator::open(&self.project.root);
        if plan.created {
            curator.mark_agent_created(&request.name);
        }
        if plan.patched {
            curator.record_patch(&request.name);
        }
        Ok(plan.result)
    }

    pub(crate) fn skills_list(&self, bot: &str) -> String {
        skill_improve::list_text(&self.project.root, bot)
    }

    pub(crate) fn skill_view(
        &self,
        bot: &str,
        name: &str,
        file_path: Option<&str>,
    ) -> Result<String, String> {
        let text = skill_improve::view_text(&self.project.root, bot, name, file_path)?;
        crate::curator::Curator::open(&self.project.root).record_view(name);
        Ok(text)
    }

    fn ready_profiles(&self) -> Vec<Profile> {
        self.refresh_profiles();
        let snap = self.snapshot.read();
        let mut out: Vec<Profile> = snap
            .values()
            .filter_map(|slot| match slot {
                BotSlot::Ready(rt) => Some(rt.profile.clone()),
                _ => None,
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

    async fn prompt(
        &self,
        bot: &str,
        text: &str,
        expected: Option<&str>,
    ) -> anyhow::Result<PromptAck> {
        let (harness, context, tx) = {
            let roster = self.snapshot.read();
            match roster.get(bot) {
                Some(BotSlot::Ready(rt)) => {
                    if expected.is_some_and(|id| id != rt.session.id()) {
                        anyhow::bail!("bot was replaced; refresh before sending");
                    }
                    (rt.harness.clone(), rt.context.clone(), rt.cmds.clone())
                }
                _ => anyhow::bail!("unknown or unavailable bot {bot}"),
            }
        };
        context
            .change(harness.session(), MAIN_LANE, &self.sandbox, ".")
            .await?;
        if let Some(ack) = self.try_plugin_command(bot, text, &harness).await? {
            return Ok(ack);
        }
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
            crate::curator::record_use(&self.project.root, &skill.name);
        }
        let mut wrapped = wrap::wrap_user_turn_at(
            text,
            &self.ready_profiles(),
            &skills,
            &updated,
            &removed,
            &wrap::timestamp_now(),
        );
        if self.take_skill_nudge(bot) {
            wrapped = skill_improve::with_nudge(&wrapped);
        }
        let wrapped = wrap::with_cwd(&wrapped, &context.cwd());
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

    async fn dispatch_user_text(
        &self,
        bot: &str,
        text: &str,
        expected: Option<&str>,
    ) -> anyhow::Result<PromptAck> {
        let (harness, context, tx) = {
            let roster = self.snapshot.read();
            match roster.get(bot) {
                Some(BotSlot::Ready(rt)) => {
                    if expected.is_some_and(|id| id != rt.session.id()) {
                        anyhow::bail!("bot was replaced; refresh before sending");
                    }
                    (rt.harness.clone(), rt.context.clone(), rt.cmds.clone())
                }
                _ => anyhow::bail!("unknown or unavailable bot {bot}"),
            }
        };
        context
            .change(harness.session(), MAIN_LANE, &self.sandbox, ".")
            .await?;
        let skills =
            crate::skills::catalog_for(&self.project.workspace(), &self.project.bot_dir(bot));
        let wrapped = wrap::wrap_user_turn_at(
            text,
            &self.ready_profiles(),
            &skills,
            &[],
            &[],
            &wrap::timestamp_now(),
        );
        let wrapped = wrap::with_cwd(&wrapped, &context.cwd());
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

    async fn compact(
        &self,
        bot: &str,
        instructions: Option<String>,
        expected: &str,
    ) -> anyhow::Result<CompactAck> {
        let tx = {
            let roster = self.snapshot.read();
            match roster.get(bot) {
                Some(BotSlot::Ready(runtime)) if runtime.session.id() == expected => {
                    runtime.cmds.clone()
                }
                Some(BotSlot::Ready(_)) => {
                    anyhow::bail!("bot was replaced; refresh before compacting")
                }
                _ => anyhow::bail!("unknown or unavailable bot {bot}"),
            }
        };
        let (reply, result) = tokio::sync::oneshot::channel();
        tx.send(BotCmd::Compact {
            instructions,
            reply,
        })
        .await
        .map_err(|_| anyhow::anyhow!("bot supervisor gone"))?;
        result
            .await
            .map_err(|_| anyhow::anyhow!("bot supervisor dropped"))?
            .map_err(anyhow::Error::msg)
    }

    async fn abort_bot(&self, bot: &str) -> anyhow::Result<()> {
        let harness = self.ready_harness(bot)?;
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
            () = cancelled => {
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
            hosts: secret.hostnames(),
        });
        Ok(format!("saved {}", secret.env))
    }

    async fn spawn_ready(self: &Arc<Self>, profile: Profile) -> anyhow::Result<()> {
        let token = self.snapshot.write().reserve(&profile.id)?;
        let mut guard = CreateGuard {
            inner: self.clone(),
            id: profile.id.clone(),
            token: token.clone(),
            finished: false,
        };
        Box::pin(self.prepare_ready(profile, &token)).await?;
        guard.finished = true;
        Ok(())
    }

    async fn prepare_ready(self: &Arc<Self>, profile: Profile, token: &str) -> anyhow::Result<()> {
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
            format!("{id}:{token}"),
            Some(home::guest(&id)?),
        )?;
        let (runtime, cmd_rx) = self.make_runtime(profile, token, storage).await?;
        let session = runtime.session.clone();
        let harness = runtime.harness.clone();
        self.sessions.lock().insert(token.into(), session.clone());
        let published = {
            let mut snapshot = self.snapshot.write();
            match snapshot.publish(&id, token, runtime) {
                Ok(()) => {
                    let handle =
                        spawn_supervisor(harness, cmd_rx, Arc::downgrade(self), id.clone());
                    let abort = handle.abort_handle();
                    let mut controllers = self.controllers.lock();
                    controllers.retain(|task| !task.is_finished());
                    controllers.push(handle);
                    for (key, runtime) in snapshot.ready_mut() {
                        if key == &id {
                            runtime.supervisor = Some(abort.clone());
                        }
                    }
                    Ok(())
                }
                Err((error, runtime)) => Err((error, runtime)),
            }
        };
        if let Err((error, _runtime)) = published {
            session.close().await;
            self.sessions.lock().remove(token);
            return Err(error.into());
        }
        {
            let catalog =
                crate::skills::catalog_for(&self.project.workspace(), &self.project.bot_dir(&id));
            let skill_snapshot: BTreeMap<String, u64> = catalog
                .iter()
                .map(|skill| (skill.name.clone(), crate::skills::fingerprint(skill)))
                .collect();
            self.skill_seen.lock().insert(id.clone(), skill_snapshot);
        }
        let _ = self.house_events.send(Event::new(
            "house",
            None,
            Kind::RosterChanged {
                ids: self
                    .ready_profiles()
                    .into_iter()
                    .map(|profile| profile.id)
                    .collect(),
            },
        ));
        Ok(())
    }

    async fn make_runtime(
        self: &Arc<Self>,
        profile: Profile,
        token: &str,
        storage: Storage,
    ) -> anyhow::Result<(BotRuntime, mpsc::Receiver<BotCmd>)> {
        let id = profile.id.clone();
        profile::validate_id(&id)?;
        let context = crate::working_directory::Context::new(&id)?;
        let session = Session::spawn(storage);
        if let Err(error) = context.restore(&session, MAIN_LANE, &self.sandbox).await {
            session.close().await;
            return Err(error);
        }
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
        let model = resolve_model(&self.project, profile.model.as_deref());
        let prompt_profile = profile.clone();
        let prompt_context = context.clone();
        let prompt_house = Arc::downgrade(self);
        let lua = self.project.runtime.clone();
        let hooks = Hooks::new().on_before_tool(Arc::new(move |event| {
            let lua = lua.clone();
            Box::pin(async move { lua.run_guards(&event).await })
        }));
        let configured_model = profile
            .model
            .clone()
            .or_else(|| self.project.runtime.agent.model.clone())
            .unwrap_or_else(|| "none".into());
        let harness = Harness::new(
            session.clone(),
            HarnessConfig {
                model,
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
                        provider: configured_model.clone(),
                        model_id: configured_model,
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
        bind_profile_environment(&harness, self.project.clone(), id, {
            let tools = tools.clone();
            Arc::new(move || tools.tool_names())
        });
        let (cmds, cmd_rx) = mpsc::channel(32);
        Ok((
            BotRuntime {
                profile,
                harness,
                session,
                cmds,
                context,
                profile_error: None,
                session_key: token.into(),
                supervisor: None,
            },
            cmd_rx,
        ))
    }

    async fn switch_chat(
        self: Arc<Self>,
        bot: &str,
        expected: &str,
        kind: SwitchKind,
    ) -> anyhow::Result<SwitchAck> {
        let bot = bot.to_string();
        let expected = expected.to_string();
        let inner = self.clone();
        Box::pin(
            self.owned(
                async move { Box::pin(inner.switch_chat_inner(&bot, &expected, kind)).await },
            ),
        )
        .await
    }

    async fn switch_chat_inner(
        self: &Arc<Self>,
        bot: &str,
        expected: &str,
        kind: SwitchKind,
    ) -> anyhow::Result<SwitchAck> {
        let _profiles = self.profile_edits.lock().await;
        let guest_home = home::guest(bot)?;
        let path = self.project.bot_conversation_path(bot, MAIN_LANE);
        let relative = path.strip_prefix(&self.project.root)?.to_path_buf();
        let (token, previous) = {
            let mut roster = self.snapshot.write();
            let profile = match roster.get(bot) {
                Some(BotSlot::Ready(runtime)) if runtime.session.id() == expected => {
                    runtime.profile.clone()
                }
                Some(BotSlot::Ready(_)) => {
                    anyhow::bail!("bot was replaced; refresh before switching conversations")
                }
                _ => anyhow::bail!("unknown or unavailable bot {bot}"),
            };
            roster.begin_replace(bot, profile)?
        };
        self.announce_roster();

        let (reply, quiesced) = tokio::sync::oneshot::channel();
        let quiesce_result = match previous.cmds.send(BotCmd::Quiesce(reply)).await {
            Ok(()) => quiesced
                .await
                .map_err(|_| anyhow::anyhow!("bot supervisor dropped while switching")),
            Err(_) => Err(anyhow::anyhow!(
                "bot supervisor unavailable while switching"
            )),
        };
        match quiesce_result {
            Ok(Ok(())) => {}
            Ok(Err(message)) => {
                self.rollback_replace(bot, &token, previous, false)?;
                return Err(anyhow::anyhow!(message));
            }
            Err(error) => {
                self.rollback_replace(bot, &token, previous, true)?;
                return Err(error);
            }
        }

        let fork = match kind {
            SwitchKind::New => None,
            SwitchKind::Fork(target) => {
                match previous.session.fork_snapshot(MAIN_LANE, target).await {
                    Ok(snapshot) => Some(snapshot),
                    Err(error) => {
                        self.rollback_replace(bot, &token, previous, true)?;
                        return Err(error.into());
                    }
                }
            }
        };
        let storage_result = if let Some(snapshot) = fork {
            let parent = snapshot.parent_session_id().to_string();
            Storage::create_fork_beneath(
                &self.project.root,
                &relative,
                format!("{bot}:{token}"),
                Some(guest_home.clone()),
                parent,
            )
            .and_then(|mut storage| snapshot.apply(&mut storage).map(|_| storage))
        } else {
            Storage::open_beneath(
                &self.project.root,
                &relative,
                format!("{bot}:{token}"),
                Some(guest_home),
            )
        };
        let storage = match storage_result {
            Ok(storage) => storage,
            Err(error) => {
                let cleanup = crate::script_fs::remove_file(&self.project.root, &relative);
                self.rollback_replace(bot, &token, previous, true)?;
                if let Err(cleanup) = cleanup
                    && cleanup.kind() != std::io::ErrorKind::NotFound
                {
                    return Err(anyhow::anyhow!(
                        "create conversation: {error}; remove failed session: {cleanup}"
                    ));
                }
                return Err(error.into());
            }
        };
        let profile = previous.profile.clone();
        let (runtime, cmd_rx) = match self.make_runtime(profile, &token, storage).await {
            Ok(runtime) => runtime,
            Err(error) => {
                let cleanup = crate::script_fs::remove_file(&self.project.root, &relative);
                self.rollback_replace(bot, &token, previous, true)?;
                if let Err(cleanup) = cleanup
                    && cleanup.kind() != std::io::ErrorKind::NotFound
                {
                    return Err(anyhow::anyhow!(
                        "open conversation: {error}; remove failed session: {cleanup}"
                    ));
                }
                return Err(error);
            }
        };
        let next_session = runtime.session.clone();
        let next_harness = runtime.harness.clone();
        self.sessions
            .lock()
            .insert(token.clone(), next_session.clone());
        let published = {
            let mut roster = self.snapshot.write();
            match roster.finish_replace(bot, &token, runtime) {
                Ok(()) => {
                    let handle = spawn_supervisor(
                        next_harness,
                        cmd_rx,
                        Arc::downgrade(self),
                        bot.to_string(),
                    );
                    let abort = handle.abort_handle();
                    let mut controllers = self.controllers.lock();
                    controllers.retain(|task| !task.is_finished());
                    controllers.push(handle);
                    for (id, runtime) in roster.ready_mut() {
                        if id == bot {
                            runtime.supervisor = Some(abort.clone());
                        }
                    }
                    Ok(())
                }
                Err((error, runtime)) => Err((error, runtime)),
            }
        };
        if let Err((error, runtime)) = published {
            runtime.harness.close().await;
            self.sessions.lock().remove(&token);
            let _ = crate::script_fs::remove_file(&self.project.root, &relative);
            previous.harness.close().await;
            return Err(error.into());
        }
        let previous_log_id = previous.session.id().to_string();
        let previous_key = previous.session_key.clone();
        previous.harness.close().await;
        self.sessions.lock().remove(&previous_key);
        drop(previous);
        self.announce_roster();
        Ok(SwitchAck {
            log_id: next_session.id().to_string(),
            previous_log_id,
        })
    }

    fn rollback_replace(
        self: &Arc<Self>,
        bot: &str,
        token: &str,
        mut runtime: BotRuntime,
        restart_supervisor: bool,
    ) -> anyhow::Result<()> {
        if restart_supervisor {
            let (cmds, cmd_rx) = mpsc::channel(32);
            runtime.cmds = cmds;
            let handle = spawn_supervisor(
                runtime.harness.clone(),
                cmd_rx,
                Arc::downgrade(self),
                bot.to_string(),
            );
            runtime.supervisor = Some(handle.abort_handle());
            let mut controllers = self.controllers.lock();
            controllers.retain(|task| !task.is_finished());
            controllers.push(handle);
        }
        self.snapshot
            .write()
            .cancel_replace(bot, token, runtime)
            .map_err(|(error, _runtime)| anyhow::Error::from(error))?;
        self.announce_roster();
        Ok(())
    }

    pub(crate) async fn create_bot(self: &Arc<Self>, spec: CreateSpec) -> anyhow::Result<Profile> {
        let inner = self.clone();
        self.owned(async move { Box::pin(inner.create_inner(spec)).await })
            .await
    }

    async fn create_inner(self: &Arc<Self>, spec: CreateSpec) -> anyhow::Result<Profile> {
        let (reserved, token) = self.reserve_create(&spec)?;
        let guard = CreateGuard {
            inner: Arc::clone(self),
            id: reserved.id.clone(),
            token: token.clone(),
            finished: false,
        };
        self.write_bot_files(&reserved, &spec).await?;
        Box::pin(self.finish_create(&reserved.id, &token)).await?;
        let mut guard = guard;
        guard.finished = true;
        Ok(reserved)
    }

    fn reserve_create(&self, spec: &CreateSpec) -> anyhow::Result<(Profile, String)> {
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
        let base = slug_from_name(&spec.name);
        let id = unique_slug(&base, |s| {
            snap.contains(s) || self.project.bot_dir(s).try_exists().unwrap_or(true)
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
        let token = snap.reserve(&id)?;
        Ok((profile, token))
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

    async fn finish_create(self: &Arc<Self>, id: &str, token: &str) -> anyhow::Result<()> {
        if !self.snapshot.read().is_reservation(id, token) {
            anyhow::bail!("stale create reservation");
        }
        let profile = Profile::load_for(&self.project.root, id)?;
        Box::pin(self.prepare_ready(profile, token)).await
    }

    fn abort_create(&self, id: &str, token: &str) {
        let changed = self.snapshot.write().abandon(id, token);
        if changed {
            self.announce_roster();
        }
    }

    pub(crate) async fn patch_profile(
        self: &Arc<Self>,
        id: &str,
        patch: serde_json::Value,
    ) -> anyhow::Result<Profile> {
        let expected = self.ready_harness(id)?.session().id().to_string();
        let _guard = self.profile_edits.lock().await;
        if self.ready_harness(id)?.session().id() != expected {
            anyhow::bail!("bot was replaced during profile update");
        }
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

    async fn delete_bot(self: &Arc<Self>, id: &str) -> anyhow::Result<()> {
        let inner = self.clone();
        let id = id.to_string();
        self.owned(async move { inner.delete_inner(&id).await })
            .await
    }

    async fn delete_inner(self: &Arc<Self>, id: &str) -> anyhow::Result<()> {
        let _files = self.profile_edits.lock().await;
        let (token, runtime) = {
            let mut roster = self.snapshot.write();
            let profile = match roster.get(id) {
                Some(BotSlot::Ready(rt)) => rt.profile.clone(),
                Some(BotSlot::Deleting { profile, .. }) => (**profile).clone(),
                Some(_) => anyhow::bail!("bot is still creating"),
                None => anyhow::bail!("unknown bot {id}"),
            };
            roster.begin_delete(id, profile)?
        };
        self.announce_roster();
        if let Some(runtime) = runtime {
            let stopped = async {
                let (reply, rx) = tokio::sync::oneshot::channel();
                runtime.cmds.send(BotCmd::Stop(reply)).await.map_err(|_| {
                    anyhow::anyhow!("supervisor unavailable; restart the house before deleting")
                })?;
                rx.await.map_err(|_| {
                    anyhow::anyhow!(
                        "supervisor failed to quiesce; restart the house before deleting"
                    )
                })?;
                Ok::<_, anyhow::Error>(())
            }
            .await;
            if let Err(error) = stopped {
                runtime.harness.close().await;
                self.snapshot
                    .write()
                    .fail_delete(id, &token, error.to_string(), false);
                self.announce_roster();
                return Err(error);
            }
            self.sessions.lock().remove(&runtime.session_key);
        }
        let removed = async {
            let relative = home::relative(id)?;
            let output = self
                .sandbox
                .exec(
                    &format!("cd /workspace/agents && [ \"$(pwd -P)\" = /workspace/agents ] && rm -rf -- {}", shell_words::quote(id)),
                    crate::sandbox::ExecOptions::default(),
                    None,
                )
                .await?;
            if !output.success || output.cancelled {
                anyhow::bail!("could not delete bot home: {}", output.stderr);
            }
            match crate::script_fs::open_dir(&self.project.root, &relative) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
                Err(error) => return Err(error.into()),
                Ok(_) => anyhow::bail!("home still exists after guest removal"),
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        match removed {
            Ok(()) => {
                self.snapshot.write().finish_delete(id, &token)?;
                self.skill_seen.lock().remove(id);
                self.secret_asks.lock().remove(id);
            }
            Err(error) => {
                self.snapshot
                    .write()
                    .fail_delete(id, &token, error.to_string(), true);
                self.announce_roster();
                return Err(error);
            }
        }
        self.announce_roster();
        Ok(())
    }

    pub(crate) async fn send_agent_message(
        &self,
        from: &str,
        to: &str,
        text: &str,
    ) -> anyhow::Result<String> {
        self.refresh_profiles();
        let (target_harness, target_cmds, from_name, to_name) = {
            let snap = self.snapshot.read();
            let Some(BotSlot::Ready(from_rt)) = snap.get(from) else {
                anyhow::bail!("unknown to {from}")
            };
            let Some(BotSlot::Ready(to_rt)) = snap.get(to) else {
                anyhow::bail!("unknown to {to}")
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

    pub(crate) fn prepare_user_attachment(
        &self,
        bot: &str,
        file: &str,
        mimetype: &str,
    ) -> anyhow::Result<attach::Prepared> {
        let resolved = self.context(bot)?.resolve(file);
        let relative = Path::new(&resolved)
            .strip_prefix("/workspace")
            .map_err(|_| anyhow::anyhow!("attachment must be inside /workspace"))?;
        if relative.as_os_str().is_empty() {
            anyhow::bail!("attachment path must name a file");
        }
        attach::prepare(&self.project.root, relative, mimetype).map_err(Into::into)
    }

    fn user_notice(
        bot: &str,
        text: &str,
        attachments: Vec<attach::Prepared>,
    ) -> anyhow::Result<(PendingEntry, String)> {
        let fingerprints: Vec<_> = attachments
            .iter()
            .map(|attachment| {
                serde_json::json!({
                    "name": attachment.saved.name,
                    "mime": attachment.saved.mime,
                    "sha256": attachment.sha256,
                })
            })
            .collect();
        let digest_source = serde_json::to_vec(&(text, fingerprints))?;
        let saved: Vec<_> = attachments
            .into_iter()
            .map(|attachment| attachment.saved)
            .collect();
        let notice = PendingEntry::custom(
            "user_notice",
            serde_json::json!({ "text": text, "bot": bot, "attachments": saved }),
        );
        let mut digest = String::with_capacity(64);
        for byte in sha2::Sha256::digest(digest_source) {
            let _ = write!(digest, "{byte:02x}");
        }
        Ok((notice, digest))
    }

    pub(crate) async fn send_user_message(
        &self,
        bot: &str,
        text: &str,
        attachments: Vec<attach::Prepared>,
    ) -> anyhow::Result<crate::ids::EntryId> {
        let harness = self.ready_harness(bot)?;
        let (notice, digest) = Self::user_notice(bot, text, attachments)?;
        match harness
            .write_once(MAIN_LANE, &format!("user_notice/{digest}"), notice.clone())
            .await
        {
            Ok(id) => Ok(id),
            Err(HarnessError::Idle(_)) => Ok(harness.place_idle(MAIN_LANE, notice).await?),
            Err(error) => Err(error.into()),
        }
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
            self.prompt(&bot, &body, None).await?;
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

    pub(crate) async fn invoke_plugin_tool(
        &self,
        bot: &str,
        name: &str,
        args: serde_json::Map<String, serde_json::Value>,
    ) -> Result<String, String> {
        let plugin = self
            .project
            .runtime
            .plugins
            .iter()
            .find(|p| p.tools.iter().any(|t| t.name == name))
            .map(|p| p.name.clone())
            .ok_or_else(|| format!("no plugin tool named {name}"))?;
        let snap = self
            .plugin_snapshot(bot, &plugin)
            .await
            .map_err(|e| e.to_string())?;
        let effect = self
            .project
            .runtime
            .run_plugin_tool(name, args, snap)
            .await
            .map_err(|e| e.to_string())?;
        let notice = effect.notice.clone();
        let (_, sends) = self
            .apply_plugin_effect(bot, &plugin, effect)
            .await
            .map_err(|e| e.to_string())?;
        self.deliver_plugin_sends(&plugin, sends).await;
        Ok(notice.unwrap_or_default())
    }

    pub(crate) fn offered_plugin_tools(&self, bot: &str) -> Vec<String> {
        self.plugin_offers
            .lock()
            .get(bot)
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default()
    }

    async fn plugin_snapshot(
        &self,
        bot: &str,
        plugin: &str,
    ) -> anyhow::Result<crate::plugin::PluginSnapshot> {
        let harness = self.ready_harness(bot)?;
        let busy = match harness.session().lane_state(MAIN_LANE).await {
            Ok(Some((state, _))) => state.current_operation_id.is_some(),
            _ => false,
        };
        let key = crate::plugin::PluginSnapshot::fact_key(plugin);
        let state = match harness
            .session()
            .register_json(crate::entry::Namespace::FactCustom, &key)
            .await?
        {
            Some((serde_json::Value::Object(map), _)) => map,
            _ => serde_json::Map::new(),
        };
        Ok(crate::plugin::PluginSnapshot {
            bot: bot.to_string(),
            now: chrono::Utc::now().timestamp(),
            busy,
            lane: MAIN_LANE.to_string(),
            state,
        })
    }

    async fn apply_plugin_effect(
        &self,
        bot: &str,
        plugin: &str,
        effect: crate::plugin::PluginEffect,
    ) -> anyhow::Result<(Option<crate::ids::EntryId>, Vec<(String, String)>)> {
        if effect.state_dirty {
            let harness = self.ready_harness(bot)?;
            let key = crate::plugin::PluginSnapshot::fact_key(plugin);
            harness
                .session()
                .set_fact(
                    crate::entry::Namespace::FactCustom,
                    &key,
                    Some(serde_json::Value::Object(effect.state)),
                )
                .await?;
        }
        {
            let mut offers = self.plugin_offers.lock();
            let slot = offers.entry(bot.to_string()).or_default();
            for name in effect.offers {
                slot.insert(name);
            }
            for name in effect.retracts {
                slot.remove(&name);
            }
        }
        if !effect.statusline.is_empty() {
            let text = {
                let mut status = self.plugin_status.lock();
                let slot = status.entry(bot.to_string()).or_default();
                for (key, value) in effect.statusline {
                    if value.trim().is_empty() {
                        slot.remove(&key);
                    } else {
                        slot.insert(key, value);
                    }
                }
                crate::plugin::join_statusline(slot)
            };
            let _ = self.house_events.send(Event::new(
                "house",
                None,
                Kind::Statusline {
                    bot_id: bot.to_string(),
                    text,
                },
            ));
        }
        if let Some(ms) = effect.timer_ms {
            self.plugin_wake.lock().insert(
                plugin.to_string(),
                std::time::Instant::now() + Duration::from_millis(ms),
            );
        }
        let mut notice_id = None;
        if let Some(notice) = effect.notice.filter(|s| !s.trim().is_empty()) {
            notice_id = Some(self.send_user_message(bot, &notice, Vec::new()).await?);
        }
        Ok((notice_id, effect.sends))
    }

    async fn deliver_plugin_sends(&self, plugin: &str, sends: Vec<(String, String)>) {
        for (target, text) in sends {
            let body = format!("[plugin:{plugin}] {text}");
            if let Err(e) = self.dispatch_user_text(&target, &body, None).await {
                eprintln!("revebot: plugin {plugin} send {target}: {e}");
            }
        }
    }

    async fn try_plugin_command(
        &self,
        bot: &str,
        text: &str,
        harness: &crate::harness::Harness,
    ) -> anyhow::Result<Option<PromptAck>> {
        let Some((name, args)) = crate::plugin::slash_command(text) else {
            return Ok(None);
        };
        let Some(def) = self.project.runtime.plugin(name) else {
            return Ok(None);
        };
        if !def.has_command() {
            return Ok(None);
        }
        if def.owner.as_deref().is_some_and(|id| id != bot) {
            return Ok(None);
        }
        let snap = self.plugin_snapshot(bot, name).await?;
        let effect = self
            .project
            .runtime
            .run_plugin_command(name, args, snap)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let (notice_id, sends) = self.apply_plugin_effect(bot, name, effect).await?;
        self.deliver_plugin_sends(name, sends).await;
        let entry_id = notice_id.map(|id| id.to_string()).unwrap_or_default();
        Ok(Some(PromptAck {
            log_id: harness.session().id().into(),
            record: if entry_id.is_empty() {
                None
            } else {
                harness
                    .session()
                    .log_record(crate::ids::EntryId::from(entry_id.clone()))
                    .await
                    .ok()
                    .flatten()
            },
            operation_id: String::new(),
            entry_id,
            mode: "plugin",
        }))
    }

    async fn tick_plugins(self: &Arc<Self>) {
        let now = std::time::Instant::now();
        let plugins: Vec<(String, u64, Option<String>)> = self
            .project
            .runtime
            .plugins
            .iter()
            .filter(|p| p.has_update())
            .map(|p| (p.name.clone(), p.interval_ms, p.owner.clone()))
            .collect();
        let bots: Vec<String> = {
            let snap = self.snapshot.read();
            snap.iter()
                .filter_map(|(id, slot)| match slot {
                    BotSlot::Ready(_) => Some(id.clone()),
                    _ => None,
                })
                .collect()
        };
        for (name, interval_ms, owner) in plugins {
            let extra_due = self
                .plugin_wake
                .lock()
                .get(&name)
                .copied()
                .is_some_and(|at| at <= now);
            let interval_due = self
                .plugin_last
                .lock()
                .get(&name)
                .copied()
                .is_none_or(|last| {
                    u64::try_from(now.saturating_duration_since(last).as_millis())
                        .unwrap_or(u64::MAX)
                        >= interval_ms
                });
            if !extra_due && !interval_due {
                continue;
            }
            self.plugin_wake.lock().remove(&name);
            self.plugin_last.lock().insert(name.clone(), now);
            for bot in &bots {
                if owner.as_deref().is_some_and(|id| id != bot) {
                    continue;
                }
                let snap = match self.plugin_snapshot(bot, &name).await {
                    Ok(snap) => snap,
                    Err(e) => {
                        eprintln!("revebot: plugin {name} snapshot {bot}: {e}");
                        continue;
                    }
                };
                match self.project.runtime.run_plugin_update(&name, snap).await {
                    Ok(effect) => match self.apply_plugin_effect(bot, &name, effect).await {
                        Ok((_, sends)) => self.deliver_plugin_sends(&name, sends).await,
                        Err(e) => eprintln!("revebot: plugin {name} {bot}: {e}"),
                    },
                    Err(e) => eprintln!("revebot: plugin {name} update {bot}: {e}"),
                }
            }
        }
    }
}

fn spawn_resource_observers(
    house: Weak<Inner>,
    mut events: broadcast::Receiver<Event>,
) -> tokio::task::JoinHandle<()> {
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
    })
}

fn spawn_routines(house: Weak<Inner>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let Some(inner) = house.upgrade() else {
                break;
            };
            if inner.snapshot.read().is_closed() {
                break;
            }
            inner.fire_due_routines().await;
            drop(inner);
            let secs = 60u64
                .saturating_sub(u64::try_from(chrono::Local::now().timestamp()).unwrap_or(0) % 60);
            tokio::time::sleep(Duration::from_secs(secs.max(1))).await;
        }
    })
}

fn spawn_plugins(house: Weak<Inner>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let Some(inner) = house.upgrade() else {
                break;
            };
            if inner.snapshot.read().is_closed() {
                break;
            }
            inner.tick_plugins().await;
            drop(inner);
            tokio::time::sleep(Duration::from_millis(crate::plugin::MIN_INTERVAL_MS)).await;
        }
    })
}

fn spawn_curator(house: Weak<Inner>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let Some(inner) = house.upgrade() else {
                break;
            };
            if inner.snapshot.read().is_closed() {
                break;
            }
            let root = inner.project.root.clone();
            drop(inner);
            let _ = tokio::task::spawn_blocking(move || {
                let _ = crate::curator::Curator::open(&root).maybe_run();
            })
            .await;
            tokio::time::sleep(Duration::from_hours(1)).await;
        }
    })
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
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut drives = tokio::task::JoinSet::new();
        let mut events = harness.subscribe();
        {
            let h = harness.clone();
            drives.spawn(async move {
                let _ = h.resume_all().await;
                let _ = h.kick(MAIN_LANE).await;
            });
        }
        loop {
            tokio::select! {
                cmd = cmds.recv() => {
                    let Some(cmd) = cmd else { break };
                    match cmd {
                        BotCmd::UserText { text, reply } => {
                            let ack = claim_then_drive(&harness, &text, &mut drives).await;
                            let _ = reply.send(ack);
                        }
                        BotCmd::Stop(reply) => {
                            cmds.close();
                            while let Ok(cmd) = cmds.try_recv() {
                                reject_bot_cmd(cmd, "bot is stopping");
                            }
                            let _ = harness.abort(MAIN_LANE).await;
                            harness.close().await;
                            publish_busy(&house, &bot_id, false);
                            // Do not unlink a session/home while any owned effect
                            // still runs. Close prevents late kicks from starting.
                            while drives.join_next().await.is_some() {}
                            let _ = reply.send(());
                            break;
                        }
                        BotCmd::Compact {
                            instructions,
                            reply,
                        } => {
                            let h = harness.clone();
                            drives.spawn(async move {
                                let result = h
                                    .compact(MAIN_LANE, instructions)
                                    .await
                                    .map(|result| CompactAck {
                                        log_id: h.session().id().into(),
                                        operation_id: result.operation_id.to_string(),
                                        outcome: result.outcome.as_str(),
                                    })
                                    .map_err(|error| error.to_string());
                                let _ = reply.send(result);
                            });
                        }
                        BotCmd::Quiesce(reply) => {
                            while drives.try_join_next().is_some() {}
                            let lane_idle = harness
                                .session()
                                .lane_state(MAIN_LANE)
                                .await
                                .is_ok_and(|state| {
                                    state.is_none_or(|(state, _)| {
                                        state.current_operation_id.is_none()
                                            && state.pending_next_run.is_empty()
                                    })
                                });
                            // A drive may have made its terminal commit but not returned
                            // yet. Once the durable lane is idle it has no remaining
                            // effect, so drain it rather than spuriously rejecting a
                            // conversation switch.
                            if lane_idle {
                                while drives.join_next().await.is_some() {}
                            }
                            if !lane_idle || !drives.is_empty() {
                                let _ = reply.send(Err(
                                    "bot is busy; wait for its current and queued work to finish"
                                        .into(),
                                ));
                                continue;
                            }
                            cmds.close();
                            while let Ok(cmd) = cmds.try_recv() {
                                reject_bot_cmd(cmd, "bot conversation changed before command ran");
                            }
                            let _ = reply.send(Ok(()));
                            break;
                        }
                        BotCmd::KickNow => {
                            let h = harness.clone();
                            drives.spawn(async move {
                                let _ = h.kick(MAIN_LANE).await;
                            });
                        }
                    }
                }
                event = events.recv() => {
                    match event {
                        Ok(ev) => {
                            match &ev.kind {
                                Kind::RunStart
                                | Kind::RunResume { .. }
                                | Kind::CompactionStart { .. } => {
                                    publish_busy(&house, &bot_id, true);
                                }
                                Kind::RunEnd { .. } | Kind::CompactionEnd { .. } => {
                                    publish_busy(&house, &bot_id, false);
                                    let h = harness.clone();
                                    drives.spawn(async move {
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
                                drives.spawn(async move {
                                    let _ = h.kick(MAIN_LANE).await;
                                });
                            }
                        }
                        Err(_) => break,
                    }
                }
                _ = drives.join_next(), if !drives.is_empty() => {}
            }
        }
    })
}

fn reject_bot_cmd(cmd: BotCmd, reason: &str) {
    match cmd {
        BotCmd::UserText { reply, .. } => {
            let _ = reply.send(Err(reason.into()));
        }
        BotCmd::Compact { reply, .. } => {
            let _ = reply.send(Err(reason.into()));
        }
        BotCmd::Quiesce(reply) => {
            let _ = reply.send(Err(reason.into()));
        }
        BotCmd::KickNow | BotCmd::Stop(_) => {}
    }
}

async fn claim_then_drive(
    harness: &Arc<Harness>,
    text: &str,
    drives: &mut tokio::task::JoinSet<()>,
) -> Result<PromptAck, String> {
    match harness.steer_claimed(MAIN_LANE, text).await {
        Ok((entry_id, op)) => {
            return Ok(PromptAck {
                log_id: harness.session().id().into(),
                record: harness
                    .session()
                    .log_record(entry_id.clone())
                    .await
                    .map_err(|e| e.to_string())?,
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
            let crate::state::Intent::Run {
                prompt_entry_ids, ..
            } = &current.operation.intent
            else {
                unreachable!()
            };
            let Some(id) = prompt_entry_ids.first() else {
                return Err("accepted prompt has no entries".to_string());
            };
            let id = id.clone();
            let ack = PromptAck {
                log_id: harness.session().id().into(),
                record: harness
                    .session()
                    .log_record(id.clone())
                    .await
                    .map_err(|e| e.to_string())?,
                operation_id: current.operation.operation_id.to_string(),
                entry_id: id.to_string(),
                mode: "prompt",
            };
            let harness = harness.clone();
            drives.spawn(async move {
                let _ = harness.drive(current).await;
            });
            Ok(ack)
        }
        Err(HarnessError::Busy(_)) => match harness.steer_claimed(MAIN_LANE, text).await {
            Ok((entry_id, op)) => Ok(PromptAck {
                log_id: harness.session().id().into(),
                record: harness
                    .session()
                    .log_record(entry_id.clone())
                    .await
                    .map_err(|e| e.to_string())?,
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
    token: String,
    finished: bool,
}

impl Drop for CreateGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.inner.abort_create(&self.id, &self.token);
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
    digest.iter().take(16).fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

pub(crate) fn bind_profile_environment(
    harness: &Harness,
    project: Arc<Project>,
    id: String,
    tool_names: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
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
                active_tool_names: tool_names(),
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
    use super::{CreateSpec, Inner, page_transcript};
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

    fn prepared(id: &str, mime: &str, sha256: &str) -> super::attach::Prepared {
        super::attach::Prepared {
            saved: super::attach::Saved {
                id: id.into(),
                name: "report.md".into(),
                path: format!("/workspace/tmp/{id}/report.md"),
                bytes: 42,
                mime: mime.into(),
            },
            sha256: sha256.into(),
        }
    }

    #[test]
    fn user_notice_persists_attachment_metadata_and_deduplicates_by_content() {
        let (notice, digest) = Inner::user_notice(
            "miku",
            "Done",
            vec![prepared("first", "text/markdown", "abc")],
        )
        .unwrap();
        let (_, repeated_digest) = Inner::user_notice(
            "miku",
            "Done",
            vec![prepared("second", "text/markdown", "abc")],
        )
        .unwrap();
        let (_, changed_digest) =
            Inner::user_notice("miku", "Done", vec![prepared("third", "text/plain", "abc")])
                .unwrap();
        assert_eq!(digest, repeated_digest);
        assert_ne!(digest, changed_digest);
        assert_eq!(notice.custom_type.as_deref(), Some("user_notice"));
        assert_eq!(
            notice.payload.as_ref().unwrap()["attachments"][0]["id"],
            "first"
        );
        assert_eq!(
            notice.payload.as_ref().unwrap()["attachments"][0]["mime"],
            "text/markdown"
        );
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
