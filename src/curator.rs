//! Background skill-library maintenance.
//!
//! Ported from Hermes' curator: usage sidecar, `active → stale → archived`
//! (never delete), pin / adopt, and host-side prune. The optional LLM
//! umbrella pass stays a `/curator` skill, not a forked aux-model.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::skills::{self, Skill};

/// Scaffold skill names. Off-limits unless `curator.prune_builtins` is on.
pub const BUNDLED_SKILLS: &[&str] = &[
    "browser",
    "computer",
    "create-skill",
    "curator",
    "learn",
    "memory",
    "plugins",
    "routines",
    "secrets",
    "vm",
];

const DEFAULT_INTERVAL_HOURS: u32 = 24 * 7;
const DEFAULT_STALE_AFTER_DAYS: u32 = 30;
const DEFAULT_ARCHIVE_AFTER_DAYS: u32 = 90;
const DEFAULT_BACKUP_KEEP: u32 = 5;

const STATE_ACTIVE: &str = "active";
const STATE_STALE: &str = "stale";
const STATE_ARCHIVED: &str = "archived";

#[derive(Debug, Error)]
pub enum CuratorError {
    #[error("{0}")]
    Message(String),
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

impl CuratorError {
    fn msg(text: impl Into<String>) -> Self {
        Self::Message(text.into())
    }
}

pub type Result<T, E = CuratorError> = std::result::Result<T, E>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub enabled: bool,
    pub interval_hours: u32,
    pub stale_after_days: u32,
    pub archive_after_days: u32,
    pub prune_builtins: bool,
    pub backup_enabled: bool,
    pub backup_keep: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_hours: DEFAULT_INTERVAL_HOURS,
            stale_after_days: DEFAULT_STALE_AFTER_DAYS,
            archive_after_days: DEFAULT_ARCHIVE_AFTER_DAYS,
            prune_builtins: false,
            backup_enabled: true,
            backup_keep: DEFAULT_BACKUP_KEEP,
        }
    }
}

impl Config {
    fn load(root: &Path) -> Self {
        let text = std::fs::read_to_string(root.join("config.yml")).ok();
        let Some(text) = text else {
            return Self::default();
        };
        let parsed: FileConfig = serde_yaml::from_str(&text).unwrap_or_default();
        parsed.curator.into_config()
    }
}

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    #[serde(default)]
    curator: FileCurator,
}

#[derive(Debug, Default, Deserialize)]
struct FileCurator {
    enabled: Option<bool>,
    interval_hours: Option<u32>,
    stale_after_days: Option<u32>,
    archive_after_days: Option<u32>,
    prune_builtins: Option<bool>,
    backup: Option<FileBackup>,
}

#[derive(Debug, Default, Deserialize)]
struct FileBackup {
    enabled: Option<bool>,
    keep: Option<u32>,
}

impl FileCurator {
    fn into_config(self) -> Config {
        let backup = self.backup.unwrap_or_default();
        Config {
            enabled: self.enabled.unwrap_or(true),
            interval_hours: nonzero(self.interval_hours, DEFAULT_INTERVAL_HOURS),
            stale_after_days: nonzero(self.stale_after_days, DEFAULT_STALE_AFTER_DAYS),
            archive_after_days: nonzero(self.archive_after_days, DEFAULT_ARCHIVE_AFTER_DAYS),
            prune_builtins: self.prune_builtins.unwrap_or(false),
            backup_enabled: backup.enabled.unwrap_or(true),
            backup_keep: nonzero(backup.keep, DEFAULT_BACKUP_KEEP),
        }
    }
}

fn nonzero(value: Option<u32>, default: u32) -> u32 {
    value.filter(|n| *n > 0).unwrap_or(default)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
struct State {
    last_run_at: Option<String>,
    last_run_duration_seconds: Option<u64>,
    last_run_summary: Option<String>,
    last_report_path: Option<String>,
    paused: bool,
    run_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Record {
    #[serde(default)]
    use_count: u64,
    #[serde(default)]
    view_count: u64,
    #[serde(default)]
    patch_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_used_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_viewed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_patched_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_activity_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_by: Option<String>,
    #[serde(default = "default_state")]
    state: String,
    #[serde(default)]
    pinned: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    archived_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<String>,
}

fn default_state() -> String {
    STATE_ACTIVE.to_string()
}

impl Record {
    fn new() -> Self {
        Self {
            use_count: 0,
            view_count: 0,
            patch_count: 0,
            last_used_at: None,
            last_viewed_at: None,
            last_patched_at: None,
            last_activity_at: None,
            created_at: Some(now_iso()),
            created_by: None,
            state: STATE_ACTIVE.to_string(),
            pinned: false,
            archived_at: None,
            source: None,
        }
    }

    fn managed(&self) -> bool {
        self.created_by.as_deref() == Some("agent")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    pub name: String,
    pub dir: PathBuf,
    pub source: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransitionCounts {
    pub checked: u32,
    pub marked_stale: u32,
    pub archived: u32,
    pub reactivated: u32,
    pub seeded: u32,
}

impl TransitionCounts {
    fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.marked_stale > 0 {
            parts.push(format!("{} marked stale", self.marked_stale));
        }
        if self.archived > 0 {
            parts.push(format!("{} archived", self.archived));
        }
        if self.reactivated > 0 {
            parts.push(format!("{} reactivated", self.reactivated));
        }
        if parts.is_empty() {
            "no changes".into()
        } else {
            parts.join(", ")
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunReport {
    pub dry_run: bool,
    pub counts: TransitionCounts,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupInfo {
    pub id: String,
    pub reason: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BackupManifest {
    reason: String,
    created_at: String,
}

pub struct Curator {
    root: PathBuf,
    config: Config,
}

impl Curator {
    pub fn open(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref().to_path_buf();
        let config = Config::load(&root);
        Self { root, config }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    fn curator_dir(&self) -> PathBuf {
        self.root.join(".reve/curator")
    }

    fn state_path(&self) -> PathBuf {
        self.curator_dir().join("state.json")
    }

    fn usage_path(&self) -> PathBuf {
        self.curator_dir().join("usage.json")
    }

    fn backups_dir(&self) -> PathBuf {
        self.curator_dir().join("backups")
    }

    fn load_state(&self) -> State {
        read_json(&self.state_path()).unwrap_or_default()
    }

    fn save_state(&self, state: &State) -> Result<()> {
        write_json(&self.state_path(), state)
    }

    fn load_usage(&self) -> BTreeMap<String, Record> {
        read_json(&self.usage_path()).unwrap_or_default()
    }

    fn save_usage(&self, data: &BTreeMap<String, Record>) -> Result<()> {
        write_json(&self.usage_path(), data)
    }

    pub fn is_paused(&self) -> bool {
        self.load_state().paused
    }

    pub fn set_paused(&self, paused: bool) -> Result<()> {
        let mut state = self.load_state();
        state.paused = paused;
        self.save_state(&state)
    }

    /// Best-effort use bump. Failures are ignored: telemetry is not a gate.
    pub fn record_use(&self, name: &str) {
        if name.is_empty() {
            return;
        }
        let mut data = self.load_usage();
        let rec = data.entry(name.to_string()).or_insert_with(Record::new);
        rec.use_count = rec.use_count.saturating_add(1);
        let ts = now_iso();
        rec.last_used_at = Some(ts.clone());
        rec.last_activity_at = Some(ts);
        if rec.created_at.is_none() {
            rec.created_at = Some(now_iso());
        }
        let _ = self.save_usage(&data);
    }

    /// Mark a skill as curator-managed. Used when `skill_manage` creates one.
    pub fn mark_agent_created(&self, name: &str) {
        if name.is_empty() {
            return;
        }
        let mut data = self.load_usage();
        let rec = data.entry(name.to_string()).or_insert_with(Record::new);
        rec.created_by = Some("agent".into());
        if rec.created_at.is_none() {
            rec.created_at = Some(now_iso());
        }
        let _ = self.save_usage(&data);
    }

    pub fn record_patch(&self, name: &str) {
        if name.is_empty() {
            return;
        }
        let mut data = self.load_usage();
        let rec = data.entry(name.to_string()).or_insert_with(Record::new);
        rec.patch_count = rec.patch_count.saturating_add(1);
        let ts = now_iso();
        rec.last_patched_at = Some(ts.clone());
        rec.last_activity_at = Some(ts);
        let _ = self.save_usage(&data);
    }

    pub fn record_view(&self, name: &str) {
        if name.is_empty() {
            return;
        }
        let mut data = self.load_usage();
        let rec = data.entry(name.to_string()).or_insert_with(Record::new);
        rec.view_count = rec.view_count.saturating_add(1);
        let ts = now_iso();
        rec.last_viewed_at = Some(ts.clone());
        rec.last_activity_at = Some(ts);
        let _ = self.save_usage(&data);
    }

    pub fn should_run_now(&self, now: DateTime<Utc>) -> bool {
        if !self.config.enabled || self.is_paused() {
            return false;
        }
        let mut state = self.load_state();
        let Some(last) = state.last_run_at.as_deref().and_then(parse_ts) else {
            state.last_run_at = Some(rfc3339(now));
            state.last_run_summary = Some(
                "deferred first run — curator seeded, will run after one interval; use `revebot curator run --dry-run` to preview now".into(),
            );
            let _ = self.save_state(&state);
            return false;
        };
        hours_elapsed(last, now) >= u64::from(self.config.interval_hours)
    }

    /// Interval gate used by the house ticker. Explicit `run` bypasses it.
    pub fn maybe_run(&self) -> Result<Option<RunReport>> {
        if !self.should_run_now(Utc::now()) {
            return Ok(None);
        }
        self.run(false).map(Some)
    }

    pub fn run(&self, dry_run: bool) -> Result<RunReport> {
        let start = Utc::now();
        if !dry_run && self.config.backup_enabled {
            let _ = self.backup("pre-curator-run");
        }
        let counts = self.apply_automatic_transitions(start, dry_run)?;
        let auto = counts.summary();
        let prefix = if dry_run { "dry-run auto: " } else { "auto: " };
        let summary = format!("{prefix}{auto}; llm: skipped (consolidation is the /curator skill)");
        let mut state = self.load_state();
        if !dry_run {
            state.last_run_at = Some(rfc3339(start));
            state.run_count = state.run_count.saturating_add(1);
            if let Some(elapsed) = Utc::now()
                .signed_duration_since(start)
                .to_std()
                .ok()
                .map(|d| d.as_secs())
            {
                state.last_run_duration_seconds = Some(elapsed);
            }
        }
        state.last_run_summary = Some(summary.clone());
        self.save_state(&state)?;
        Ok(RunReport {
            dry_run,
            counts,
            summary,
        })
    }

    pub fn apply_automatic_transitions(
        &self,
        now: DateTime<Utc>,
        dry_run: bool,
    ) -> Result<TransitionCounts> {
        let stale_cutoff = days_ago(now, self.config.stale_after_days);
        let archive_cutoff = days_ago(now, self.config.archive_after_days);
        let mut data = self.load_usage();
        let live = self.live_skills();
        let mut counts = TransitionCounts::default();

        let names = self.managed_names(&data, &live);
        for name in names {
            counts.checked = counts.checked.saturating_add(1);
            let persisted = data.contains_key(&name);
            if !persisted {
                if !dry_run {
                    let mut rec = Record::new();
                    rec.created_at = Some(rfc3339(now));
                    if is_bundled(&name) {
                        rec.created_by = Some("bundled".into());
                    } else {
                        rec.created_by = Some("agent".into());
                    }
                    rec.source = live.get(&name).map(|l| l.source.clone());
                    data.insert(name, rec);
                    counts.seeded = counts.seeded.saturating_add(1);
                }
                continue;
            }
            let Some(rec) = data.get(&name).cloned() else {
                continue;
            };
            if rec.pinned {
                continue;
            }
            let anchor = rec
                .last_activity_at
                .as_deref()
                .and_then(parse_ts)
                .or_else(|| rec.created_at.as_deref().and_then(parse_ts))
                .unwrap_or(now);
            let never_used = rec.use_count == 0;
            if never_used && anchor > stale_cutoff {
                if rec.state == STATE_STALE {
                    if !dry_run && let Some(slot) = data.get_mut(&name) {
                        slot.state = STATE_ACTIVE.to_string();
                    }
                    counts.reactivated = counts.reactivated.saturating_add(1);
                }
                continue;
            }
            if anchor <= archive_cutoff && rec.state != STATE_ARCHIVED {
                let archived = dry_run || archive_named(&name, &mut data, &live, now)?;
                if archived {
                    counts.archived = counts.archived.saturating_add(1);
                }
            } else if anchor <= stale_cutoff && rec.state == STATE_ACTIVE {
                if !dry_run && let Some(slot) = data.get_mut(&name) {
                    slot.state = STATE_STALE.to_string();
                }
                counts.marked_stale = counts.marked_stale.saturating_add(1);
            } else if anchor > stale_cutoff && rec.state == STATE_STALE {
                if !dry_run && let Some(slot) = data.get_mut(&name) {
                    slot.state = STATE_ACTIVE.to_string();
                }
                counts.reactivated = counts.reactivated.saturating_add(1);
            }
        }
        if !dry_run {
            self.save_usage(&data)?;
        }
        Ok(counts)
    }

    fn managed_names(
        &self,
        data: &BTreeMap<String, Record>,
        live: &BTreeMap<String, Located>,
    ) -> Vec<String> {
        let mut names = Vec::new();
        for (name, rec) in data {
            if rec.managed() {
                names.push(name.clone());
            }
        }
        if self.config.prune_builtins {
            for name in live.keys() {
                if is_bundled(name) && !names.iter().any(|n| n == name) {
                    names.push(name.clone());
                }
            }
        }
        names.sort();
        names.dedup();
        names
    }

    fn live_skills(&self) -> BTreeMap<String, Located> {
        let mut map = BTreeMap::new();
        for skill in skills::discover_lenient(&self.root.join("workspace")) {
            if let Some(located) = located_from_skill(&skill, "workspace") {
                map.insert(located.name.clone(), located);
            }
        }
        for bot_dir in agent_dirs(&self.root) {
            let Some(id) = file_name(&bot_dir) else {
                continue;
            };
            let source = format!("bot:{id}");
            for skill in skills::discover_lenient(&bot_dir) {
                if let Some(located) = located_from_skill(&skill, &source) {
                    map.insert(located.name.clone(), located);
                }
            }
        }
        map
    }

    pub fn pin(&self, name: &str) -> Result<String> {
        let live = self.live_skills();
        if is_bundled(name) && !self.config.prune_builtins {
            return Err(CuratorError::msg(format!(
                "skill '{name}' is bundled; pin is for curator-managed skills"
            )));
        }
        let mut data = self.load_usage();
        let rec = data.get_mut(name);
        match rec {
            Some(rec) if rec.managed() => {
                rec.pinned = true;
                self.save_usage(&data)?;
                Ok(format!("pinned {name}"))
            }
            _ => {
                if live.contains_key(name) {
                    Err(CuratorError::msg(format!(
                        "skill '{name}' is unmanaged; `revebot curator adopt {name}` first"
                    )))
                } else {
                    Err(CuratorError::msg(format!("skill '{name}' not found")))
                }
            }
        }
    }

    pub fn unpin(&self, name: &str) -> Result<String> {
        let mut data = self.load_usage();
        let Some(rec) = data.get_mut(name) else {
            return Err(CuratorError::msg(format!("skill '{name}' not found")));
        };
        rec.pinned = false;
        self.save_usage(&data)?;
        Ok(format!("unpinned {name}"))
    }

    pub fn adopt(&self, name: &str) -> Result<String> {
        if is_bundled(name) {
            return Err(CuratorError::msg(format!(
                "skill '{name}' is bundled; the curator does not adopt built-ins"
            )));
        }
        let live = self.live_skills();
        let located = live.get(name);
        let mut data = self.load_usage();
        if data.get(name).is_some_and(Record::managed) {
            return Ok(format!("{name} already curator-managed"));
        }
        if located.is_none() && !data.contains_key(name) {
            return Err(CuratorError::msg(format!("skill '{name}' not found")));
        }
        let rec = data.entry(name.to_string()).or_insert_with(Record::new);
        rec.created_by = Some("agent".into());
        if rec.created_at.is_none() {
            rec.created_at = Some(now_iso());
        }
        if let Some(loc) = located {
            rec.source = Some(loc.source.clone());
        }
        self.save_usage(&data)?;
        Ok(format!("adopted {name}"))
    }

    pub fn adopt_all(&self, dry_run: bool) -> Result<Vec<String>> {
        let names: Vec<String> = self.unmanaged().into_iter().map(|l| l.name).collect();
        if dry_run {
            return Ok(names);
        }
        let mut done = Vec::new();
        for name in names {
            self.adopt(&name)?;
            done.push(name);
        }
        Ok(done)
    }

    pub fn unmanaged(&self) -> Vec<Located> {
        let data = self.load_usage();
        let live = self.live_skills();
        let mut out = Vec::new();
        for (name, located) in live {
            if is_bundled(&name) {
                continue;
            }
            if data.get(&name).is_some_and(Record::managed) {
                continue;
            }
            out.push(located);
        }
        out
    }

    pub fn archive(&self, name: &str) -> Result<String> {
        if is_bundled(name) && !self.config.prune_builtins {
            return Err(CuratorError::msg(format!(
                "skill '{name}' is bundled; enable curator.prune_builtins to prune it"
            )));
        }
        let live = self.live_skills();
        let mut data = self.load_usage();
        if !self.config.prune_builtins && !data.get(name).is_some_and(Record::managed) {
            return Err(CuratorError::msg(format!(
                "skill '{name}' is unmanaged; adopt it first"
            )));
        }
        if !archive_named(name, &mut data, &live, Utc::now())? {
            return Err(CuratorError::msg(format!(
                "skill '{name}' not found on disk"
            )));
        }
        self.save_usage(&data)?;
        Ok(format!("archived {name}"))
    }

    pub fn restore(&self, name: &str) -> Result<String> {
        let live = self.live_skills();
        if live.contains_key(name) {
            return Err(CuratorError::msg(format!(
                "skill '{name}' already exists live; restore would collide"
            )));
        }
        if is_bundled(name) && !self.config.prune_builtins {
            return Err(CuratorError::msg(format!(
                "skill '{name}' is bundled; restore would shadow the scaffold"
            )));
        }
        let Some(archived) = self.find_archived(name) else {
            return Err(CuratorError::msg(format!(
                "skill '{name}' is not in any skills/.archive/"
            )));
        };
        let mut data = self.load_usage();
        let dest_parent =
            restore_parent(&self.root, data.get(name).and_then(|r| r.source.as_deref()));
        std::fs::create_dir_all(&dest_parent)?;
        let dest = dest_parent.join(name);
        if dest.exists() {
            return Err(CuratorError::msg(format!(
                "restore destination {} already exists",
                dest.display()
            )));
        }
        move_dir(&archived, &dest)?;
        let rec = data.entry(name.to_string()).or_insert_with(Record::new);
        rec.state = STATE_ACTIVE.to_string();
        rec.archived_at = None;
        self.save_usage(&data)?;
        Ok(format!("restored {name} to {}", dest.display()))
    }

    fn find_archived(&self, name: &str) -> Option<PathBuf> {
        let mut matches = Vec::new();
        collect_archive_dirs(
            &self.root.join("workspace/skills/.archive"),
            name,
            &mut matches,
        );
        for bot in agent_dirs(&self.root) {
            collect_archive_dirs(&bot.join("skills/.archive"), name, &mut matches);
        }
        matches.into_iter().find(|p| file_name(p) == Some(name))
    }

    pub fn list_archived(&self) -> Vec<String> {
        let mut names = Vec::new();
        collect_archive_names(&self.root.join("workspace/skills/.archive"), &mut names);
        for bot in agent_dirs(&self.root) {
            collect_archive_names(&bot.join("skills/.archive"), &mut names);
        }
        names.sort();
        names.dedup();
        names
    }

    pub fn backup(&self, reason: &str) -> Result<BackupInfo> {
        let id = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let dest = self.backups_dir().join(&id);
        std::fs::create_dir_all(&dest)?;
        let ws = self.root.join("workspace/skills");
        if ws.is_dir() {
            copy_tree(&ws, &dest.join("workspace-skills"))?;
        }
        for bot in agent_dirs(&self.root) {
            let skills = bot.join("skills");
            if !skills.is_dir() {
                continue;
            }
            let Some(bot_id) = file_name(&bot) else {
                continue;
            };
            copy_tree(&skills, &dest.join("agents").join(bot_id).join("skills"))?;
        }
        let created_at = now_iso();
        let manifest = BackupManifest {
            reason: reason.to_string(),
            created_at: created_at.clone(),
        };
        write_json(&dest.join("manifest.json"), &manifest)?;
        self.prune_backups();
        Ok(BackupInfo {
            id,
            reason: reason.to_string(),
            created_at,
        })
    }

    fn prune_backups(&self) {
        let keep = usize::try_from(self.config.backup_keep).unwrap_or(5);
        let mut ids = self.backup_ids();
        let extra = ids.len().saturating_sub(keep);
        let stale: Vec<String> = ids.drain(..extra).collect();
        for old in stale {
            let _ = std::fs::remove_dir_all(self.backups_dir().join(old));
        }
    }

    fn backup_ids(&self) -> Vec<String> {
        let Ok(rd) = std::fs::read_dir(self.backups_dir()) else {
            return Vec::new();
        };
        let mut ids: Vec<String> = rd
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        ids.sort();
        ids
    }

    pub fn list_backups(&self) -> Vec<BackupInfo> {
        self.backup_ids()
            .into_iter()
            .filter_map(|id| {
                let path = self.backups_dir().join(&id).join("manifest.json");
                let manifest: BackupManifest = read_json(&path)?;
                Some(BackupInfo {
                    id,
                    reason: manifest.reason,
                    created_at: manifest.created_at,
                })
            })
            .collect()
    }

    pub fn rollback(&self, id: Option<&str>) -> Result<String> {
        let ids = self.backup_ids();
        let target = match id {
            Some(id) => id.to_string(),
            None => ids
                .last()
                .cloned()
                .ok_or_else(|| CuratorError::msg("no curator backups"))?,
        };
        if !self.backups_dir().join(&target).is_dir() {
            return Err(CuratorError::msg(format!("backup '{target}' not found")));
        }
        if self.config.backup_enabled {
            let _ = self.backup(&format!("pre-rollback to {target}"));
        }
        let src = self.backups_dir().join(&target);
        let ws_src = src.join("workspace-skills");
        let ws_dest = self.root.join("workspace/skills");
        if ws_src.is_dir() {
            replace_tree(&ws_src, &ws_dest)?;
        }
        let agents_src = src.join("agents");
        if agents_src.is_dir() {
            let Ok(rd) = std::fs::read_dir(&agents_src) else {
                return Ok(format!("rolled back to {target}"));
            };
            for entry in rd.flatten() {
                let Some(bot_id) = entry.file_name().into_string().ok() else {
                    continue;
                };
                let from = entry.path().join("skills");
                if !from.is_dir() {
                    continue;
                }
                let to = self
                    .root
                    .join("workspace/agents")
                    .join(&bot_id)
                    .join("skills");
                replace_tree(&from, &to)?;
            }
        }
        Ok(format!("rolled back to {target}"))
    }

    pub fn status_text(&self) -> String {
        let state = self.load_state();
        let enabled = self.config.enabled;
        let paused = state.paused;
        let status = if enabled && !paused {
            "ENABLED"
        } else if paused {
            "PAUSED"
        } else {
            "DISABLED"
        };
        let mut lines = vec![
            format!("curator: {status}"),
            format!("  runs:           {}", state.run_count),
            format!(
                "  last run:       {}",
                fmt_ago(state.last_run_at.as_deref())
            ),
        ];
        let summary = state.last_run_summary.as_deref().unwrap_or("(none)");
        if summary.contains('\n') {
            let mut parts = summary.split('\n');
            if let Some(first) = parts.next() {
                lines.push(format!("  last summary:   {first}"));
            }
            for line in parts {
                lines.push(format!("                  {line}"));
            }
        } else {
            lines.push(format!("  last summary:   {summary}"));
        }
        let interval = self.config.interval_hours;
        let interval_label = if interval.is_multiple_of(24) && interval >= 24 {
            format!("{}d", interval / 24)
        } else {
            format!("{interval}h")
        };
        lines.push(format!("  interval:       every {interval_label}"));
        lines.push(format!(
            "  stale after:    {}d unused",
            self.config.stale_after_days
        ));
        lines.push(format!(
            "  archive after:  {}d unused",
            self.config.archive_after_days
        ));
        lines.push("  consolidate:    off (prune-only; LLM merge is the /curator skill)".into());

        let data = self.load_usage();
        let live = self.live_skills();
        let managed: Vec<(&String, &Record)> =
            data.iter().filter(|(_, rec)| rec.managed()).collect();
        if managed.is_empty() {
            lines.push(String::new());
            lines.push("no curator-managed skills".into());
        } else {
            let mut active = 0u32;
            let mut stale = 0u32;
            let mut archived = 0u32;
            let mut pinned = Vec::new();
            for (name, rec) in &managed {
                match rec.state.as_str() {
                    STATE_STALE => stale += 1,
                    STATE_ARCHIVED => archived += 1,
                    _ => active += 1,
                }
                if rec.pinned {
                    pinned.push((*name).clone());
                }
            }
            lines.push(String::new());
            lines.push(format!("curator-managed skills: {} total", managed.len()));
            lines.push(format!("  active     {active}"));
            lines.push(format!("  stale      {stale}"));
            lines.push(format!("  archived   {archived}"));
            if !pinned.is_empty() {
                lines.push(format!(
                    "\npinned ({}): {}",
                    pinned.len(),
                    pinned.join(", ")
                ));
            }
            let mut lru: Vec<(&String, &Record)> = managed
                .iter()
                .copied()
                .filter(|(_, rec)| rec.state != STATE_ARCHIVED)
                .collect();
            lru.sort_by_key(|(_, rec)| rec.last_activity_at.clone().unwrap_or_default());
            if !lru.is_empty() {
                lines.push("\nleast recently used:".into());
                for (name, rec) in lru.into_iter().take(5) {
                    let when = fmt_ago(
                        rec.last_activity_at
                            .as_deref()
                            .or(rec.created_at.as_deref()),
                    );
                    lines.push(format!("  {name:20} {when}"));
                }
            }
        }
        let unmanaged = unmanaged_from(&data, &live);
        if !unmanaged.is_empty() {
            lines.push(format!(
                "\nunmanaged (no provenance marker): {} total",
                unmanaged.len()
            ));
            lines.push(
                "  never auto-staled or archived — `revebot curator adopt <name>` hands one over"
                    .into(),
            );
        }
        lines.join("\n")
    }
}

fn unmanaged_from(
    data: &BTreeMap<String, Record>,
    live: &BTreeMap<String, Located>,
) -> Vec<String> {
    live.keys()
        .filter(|name| !is_bundled(name))
        .filter(|name| !data.get(*name).is_some_and(Record::managed))
        .cloned()
        .collect()
}

/// Best-effort use counter from the house prompt path.
pub fn record_use(root: &Path, name: &str) {
    Curator::open(root).record_use(name);
}

fn located_from_skill(skill: &Skill, source: &str) -> Option<Located> {
    let dir = skill.path.parent()?.to_path_buf();
    Some(Located {
        name: skill.name.clone(),
        dir,
        source: source.to_string(),
    })
}

fn agent_dirs(root: &Path) -> Vec<PathBuf> {
    let agents = root.join("workspace/agents");
    let Ok(rd) = std::fs::read_dir(&agents) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && file_name(p).is_some_and(|n| !n.starts_with('.')))
        .collect();
    out.sort();
    out
}

fn archive_root_for(skill_dir: &Path) -> PathBuf {
    skill_dir
        .parent()
        .map_or_else(|| PathBuf::from(".archive"), |p| p.join(".archive"))
}

fn restore_parent(root: &Path, source: Option<&str>) -> PathBuf {
    match source.and_then(|src| src.strip_prefix("bot:")) {
        Some(id) if !id.is_empty() => root.join("workspace/agents").join(id).join("skills"),
        _ => root.join("workspace/skills"),
    }
}

fn unique_dest(root: &Path, name: &str) -> PathBuf {
    let dest = root.join(name);
    if !dest.exists() {
        return dest;
    }
    let stamp = Utc::now().format("%Y%m%d%H%M%S");
    root.join(format!("{name}-{stamp}"))
}

fn collect_archive_dirs(root: &Path, name: &str, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(root) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if file_name(&path) == Some(name) && path.join("SKILL.md").is_file() {
            out.push(path);
        } else {
            collect_archive_dirs(&path, name, out);
        }
    }
}

fn collect_archive_names(root: &Path, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(root) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path.join("SKILL.md").is_file() {
            if let Some(name) = file_name(&path) {
                out.push(name.to_string());
            }
        } else {
            collect_archive_names(&path, out);
        }
    }
}

fn is_bundled(name: &str) -> bool {
    BUNDLED_SKILLS.contains(&name)
}

fn archive_named(
    name: &str,
    data: &mut BTreeMap<String, Record>,
    live: &BTreeMap<String, Located>,
    now: DateTime<Utc>,
) -> Result<bool> {
    let Some(located) = live.get(name) else {
        if let Some(rec) = data.get_mut(name) {
            rec.state = STATE_ARCHIVED.to_string();
            rec.archived_at = Some(rfc3339(now));
        }
        return Ok(false);
    };
    let dest_root = archive_root_for(&located.dir);
    std::fs::create_dir_all(&dest_root)?;
    let dest = unique_dest(&dest_root, name);
    move_dir(&located.dir, &dest)?;
    let rec = data.entry(name.to_string()).or_insert_with(Record::new);
    rec.state = STATE_ARCHIVED.to_string();
    rec.archived_at = Some(rfc3339(now));
    rec.source = Some(located.source.clone());
    Ok(true)
}

fn move_dir(src: &Path, dest: &Path) -> io::Result<()> {
    if std::fs::rename(src, dest).is_ok() {
        Ok(())
    } else {
        copy_tree(src, dest)?;
        std::fs::remove_dir_all(src)
    }
}

fn replace_tree(src: &Path, dest: &Path) -> io::Result<()> {
    if dest.exists() {
        std::fs::remove_dir_all(dest)?;
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    copy_tree(src, dest)
}

fn copy_tree(src: &Path, dest: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            continue;
        }
        let to = dest.join(entry.file_name());
        if ft.is_dir() {
            copy_tree(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| CuratorError::msg("path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut tmp, value).map_err(|e| CuratorError::msg(e.to_string()))?;
    tmp.write_all(b"\n")?;
    tmp.persist(path).map_err(|e| CuratorError::Io(e.error))?;
    Ok(())
}

fn now_iso() -> String {
    rfc3339(Utc::now())
}

fn rfc3339(now: DateTime<Utc>) -> String {
    now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn parse_ts(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

fn days_ago(now: DateTime<Utc>, days: u32) -> DateTime<Utc> {
    TimeDelta::try_days(i64::from(days))
        .and_then(|d| now.checked_sub_signed(d))
        .unwrap_or(now)
}

fn hours_elapsed(last: DateTime<Utc>, now: DateTime<Utc>) -> u64 {
    now.signed_duration_since(last)
        .to_std()
        .ok()
        .map_or(0, |d| d.as_secs() / 3600)
}

fn fmt_ago(ts: Option<&str>) -> String {
    let Some(ts) = ts else {
        return "never".into();
    };
    let Some(dt) = parse_ts(ts) else {
        return ts.to_string();
    };
    let secs = Utc::now()
        .signed_duration_since(dt)
        .to_std()
        .ok()
        .map_or(0, |d| d.as_secs());
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

fn file_name(path: &Path) -> Option<&str> {
    path.file_name().and_then(|n| n.to_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, rel: &str, name: &str) {
        let path = root.join(rel).join(name).join("SKILL.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            format!("---\nname: {name}\ndescription: x\n---\nbody\n"),
        )
        .unwrap();
    }

    fn backdate(c: &Curator, name: &str, days: i64, use_count: u64) {
        let ts = rfc3339(days_ago(
            Utc::now(),
            u32::try_from(days.max(0)).unwrap_or(0),
        ));
        let mut data = c.load_usage();
        let rec = data.entry(name.to_string()).or_insert_with(Record::new);
        rec.created_by = Some("agent".into());
        rec.created_at = Some(ts.clone());
        rec.use_count = use_count;
        if use_count == 0 {
            rec.last_used_at = None;
            rec.last_activity_at = None;
        } else {
            rec.last_used_at = Some(ts.clone());
            rec.last_activity_at = Some(ts);
        }
        rec.state = STATE_ACTIVE.to_string();
        rec.pinned = false;
        c.save_usage(&data).unwrap();
    }

    #[test]
    fn defaults_match_hermes_prune_window() {
        let cfg = Config::default();
        assert_eq!(cfg.interval_hours, 168);
        assert_eq!(cfg.stale_after_days, 30);
        assert_eq!(cfg.archive_after_days, 90);
        assert!(cfg.enabled);
        assert!(!cfg.prune_builtins);
    }

    #[test]
    fn first_observation_defers_and_seeds() {
        let dir = tempfile::tempdir().unwrap();
        let c = Curator::open(dir.path());
        assert!(!c.should_run_now(Utc::now()));
        let state = c.load_state();
        assert!(state.last_run_at.is_some());
        assert!(!c.should_run_now(Utc::now()));
    }

    #[test]
    fn pause_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let c = Curator::open(dir.path());
        c.set_paused(true).unwrap();
        assert!(c.is_paused());
        c.set_paused(false).unwrap();
        assert!(!c.is_paused());
    }

    #[test]
    fn pinned_skill_is_never_archived() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "workspace/skills", "precious");
        let c = Curator::open(dir.path());
        c.adopt("precious").unwrap();
        backdate(&c, "precious", 400, 1);
        let mut data = c.load_usage();
        data.get_mut("precious").unwrap().pinned = true;
        c.save_usage(&data).unwrap();
        let counts = c.apply_automatic_transitions(Utc::now(), false).unwrap();
        assert_eq!(counts.archived, 0);
        assert_eq!(counts.marked_stale, 0);
        assert_eq!(c.load_usage()["precious"].state, STATE_ACTIVE);
        assert!(
            dir.path()
                .join("workspace/skills/precious/SKILL.md")
                .is_file()
        );
    }

    #[test]
    fn unused_managed_skill_goes_stale_then_archives() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "workspace/skills", "old");
        write_skill(dir.path(), "workspace/skills", "ancient");
        let c = Curator::open(dir.path());
        c.adopt("old").unwrap();
        c.adopt("ancient").unwrap();
        backdate(&c, "old", 40, 1);
        backdate(&c, "ancient", 100, 1);
        let counts = c.apply_automatic_transitions(Utc::now(), false).unwrap();
        assert_eq!(counts.marked_stale, 1);
        assert_eq!(counts.archived, 1);
        assert_eq!(c.load_usage()["old"].state, STATE_STALE);
        assert_eq!(c.load_usage()["ancient"].state, STATE_ARCHIVED);
        assert!(
            !dir.path()
                .join("workspace/skills/ancient/SKILL.md")
                .exists()
        );
        assert!(
            dir.path()
                .join("workspace/skills/.archive/ancient/SKILL.md")
                .is_file()
        );
    }

    #[test]
    fn never_used_young_skill_is_not_archived() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "workspace/skills", "fresh");
        let c = Curator::open(dir.path());
        c.adopt("fresh").unwrap();
        backdate(&c, "fresh", 10, 0);
        let counts = c.apply_automatic_transitions(Utc::now(), false).unwrap();
        assert_eq!(counts.archived, 0);
        assert_eq!(counts.marked_stale, 0);
        assert!(dir.path().join("workspace/skills/fresh/SKILL.md").is_file());
    }

    #[test]
    fn use_reactivates_stale_skill() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "workspace/skills", "revived");
        let c = Curator::open(dir.path());
        c.adopt("revived").unwrap();
        backdate(&c, "revived", 40, 1);
        c.apply_automatic_transitions(Utc::now(), false).unwrap();
        assert_eq!(c.load_usage()["revived"].state, STATE_STALE);
        c.record_use("revived");
        let counts = c.apply_automatic_transitions(Utc::now(), false).unwrap();
        assert_eq!(counts.reactivated, 1);
        assert_eq!(c.load_usage()["revived"].state, STATE_ACTIVE);
    }

    #[test]
    fn bundled_skills_are_not_auto_archived() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "workspace/skills", "create-skill");
        let c = Curator::open(dir.path());
        backdate(&c, "create-skill", 400, 1);
        let mut data = c.load_usage();
        data.get_mut("create-skill").unwrap().created_by = Some("bundled".into());
        c.save_usage(&data).unwrap();
        let counts = c.apply_automatic_transitions(Utc::now(), false).unwrap();
        assert_eq!(counts.archived, 0);
        assert!(
            dir.path()
                .join("workspace/skills/create-skill/SKILL.md")
                .is_file()
        );
    }

    #[test]
    fn unmanaged_skills_are_left_alone_until_adopted() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "workspace/skills", "mine");
        let c = Curator::open(dir.path());
        backdate(&c, "mine", 400, 1);
        // backdate marks created_by agent — simulate unmanaged by clearing it.
        let mut data = c.load_usage();
        data.get_mut("mine").unwrap().created_by = None;
        c.save_usage(&data).unwrap();
        let counts = c.apply_automatic_transitions(Utc::now(), false).unwrap();
        assert_eq!(counts.archived, 0);
        assert!(dir.path().join("workspace/skills/mine/SKILL.md").is_file());
        assert_eq!(c.unmanaged().len(), 1);
        c.adopt("mine").unwrap();
        let counts = c.apply_automatic_transitions(Utc::now(), false).unwrap();
        assert_eq!(counts.archived, 1);
    }

    #[test]
    fn restore_brings_an_archived_skill_back() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "workspace/skills", "gone");
        let c = Curator::open(dir.path());
        c.adopt("gone").unwrap();
        c.archive("gone").unwrap();
        assert!(c.list_archived().contains(&"gone".to_string()));
        c.restore("gone").unwrap();
        assert!(dir.path().join("workspace/skills/gone/SKILL.md").is_file());
        assert_eq!(c.load_usage()["gone"].state, STATE_ACTIVE);
    }

    #[test]
    fn dry_run_does_not_move_skills_or_bump_run_count() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "workspace/skills", "dusty");
        let c = Curator::open(dir.path());
        c.adopt("dusty").unwrap();
        backdate(&c, "dusty", 100, 1);
        let report = c.run(true).unwrap();
        assert!(report.dry_run);
        assert_eq!(report.counts.archived, 1);
        assert!(dir.path().join("workspace/skills/dusty/SKILL.md").is_file());
        assert_eq!(c.load_state().run_count, 0);
    }

    #[test]
    fn backup_and_rollback_restore_the_tree() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "workspace/skills", "keep");
        let c = Curator::open(dir.path());
        c.adopt("keep").unwrap();
        let snap = c.backup("test").unwrap();
        std::fs::remove_dir_all(dir.path().join("workspace/skills/keep")).unwrap();
        c.rollback(Some(&snap.id)).unwrap();
        assert!(dir.path().join("workspace/skills/keep/SKILL.md").is_file());
    }

    #[test]
    fn bot_local_skill_can_be_adopted_and_archived() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "workspace/agents/reve/skills", "local");
        let c = Curator::open(dir.path());
        c.adopt("local").unwrap();
        c.archive("local").unwrap();
        assert!(
            dir.path()
                .join("workspace/agents/reve/skills/.archive/local/SKILL.md")
                .is_file()
        );
        c.restore("local").unwrap();
        assert!(
            dir.path()
                .join("workspace/agents/reve/skills/local/SKILL.md")
                .is_file()
        );
    }

    #[test]
    fn pin_refuses_unmanaged_and_bundled() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "workspace/skills", "create-skill");
        write_skill(dir.path(), "workspace/skills", "hand");
        let c = Curator::open(dir.path());
        assert!(c.pin("create-skill").is_err());
        assert!(c.pin("hand").is_err());
        c.adopt("hand").unwrap();
        assert!(c.pin("hand").unwrap().contains("pinned"));
    }

    #[test]
    fn status_mentions_unmanaged() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "workspace/skills", "loose");
        let c = Curator::open(dir.path());
        let text = c.status_text();
        assert!(text.contains("ENABLED"));
        assert!(text.contains("unmanaged"));
        assert!(text.contains("no curator-managed skills"));
    }

    #[test]
    fn config_yml_overrides_defaults() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.yml"),
            "model: x\ncurator:\n  enabled: false\n  interval_hours: 24\n  prune_builtins: true\n",
        )
        .unwrap();
        let c = Curator::open(dir.path());
        assert!(!c.config().enabled);
        assert_eq!(c.config().interval_hours, 24);
        assert!(c.config().prune_builtins);
    }
}
