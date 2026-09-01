//! House plugin runtime helpers: slash routing, statusline join, durable keys.
//!
//! Lua declarations live in [`crate::lua`]. This module is the pure shape those
//! callbacks produce and consume.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

/// 18 hours. Plugin timers and loop wakes cannot schedule past this.
pub const MAX_TIMER_MS: u64 = 18 * 60 * 60 * 1000;
pub const MIN_INTERVAL_MS: u64 = 250;
pub const DEFAULT_INTERVAL_MS: u64 = 1000;

/// Snapshot handed to a plugin command/update/tool callback.
#[derive(Debug, Clone)]
pub struct PluginSnapshot {
    pub bot: String,
    pub now: i64,
    pub busy: bool,
    pub lane: String,
    pub state: Map<String, Value>,
}

/// Mutations collected during a plugin callback. Applied only if it returns.
#[derive(Debug, Clone, Default)]
pub struct PluginEffect {
    pub state: Map<String, Value>,
    pub state_dirty: bool,
    pub sends: Vec<(String, String)>,
    pub statusline: BTreeMap<String, String>,
    pub offers: Vec<String>,
    pub retracts: Vec<String>,
    pub timer_ms: Option<u64>,
    pub notice: Option<String>,
}

impl PluginSnapshot {
    pub fn fact_key(plugin: &str) -> String {
        format!("plugin/{plugin}")
    }
}

/// Join per-plugin statusline slots in key order. Empty values are dropped.
pub fn join_statusline(parts: &BTreeMap<String, String>) -> String {
    parts
        .values()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" | ")
}

/// `/loop 5m check` → `("loop", "5m check")`. None if not a plugin slash.
pub fn slash_command(raw: &str) -> Option<(&str, &str)> {
    let trimmed = raw.trim();
    let rest = trimmed.strip_prefix('/')?;
    let (name, args) = match rest.split_once(char::is_whitespace) {
        Some((name, args)) => (name, args.trim()),
        None => (rest, ""),
    };
    if name.is_empty() || !is_plugin_name(name) {
        return None;
    }
    Some((name, args))
}

pub fn is_plugin_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

pub fn clamp_interval_ms(ms: u64) -> u64 {
    ms.clamp(MIN_INTERVAL_MS, MAX_TIMER_MS)
}

pub fn clamp_timer_ms(ms: u64) -> Option<u64> {
    if ms == 0 {
        return None;
    }
    Some(ms.min(MAX_TIMER_MS))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_routes_plugin_commands() {
        assert_eq!(slash_command("/loop 5m check"), Some(("loop", "5m check")));
        assert_eq!(slash_command("  /loop"), Some(("loop", "")));
        assert_eq!(slash_command("/lua-plugins"), Some(("lua-plugins", "")));
        assert_eq!(slash_command("hello"), None);
        assert_eq!(slash_command("/Loop"), None);
        assert_eq!(slash_command("/"), None);
    }

    #[test]
    fn statusline_joins_nonempty_slots_in_key_order() {
        let mut parts = BTreeMap::new();
        parts.insert("loop".into(), "  2 loops  ".into());
        parts.insert("mail".into(), String::new());
        parts.insert("cal".into(), "tue 3pm".into());
        assert_eq!(join_statusline(&parts), "tue 3pm | 2 loops");
    }

    #[test]
    fn timers_clamp_to_eighteen_hours() {
        assert_eq!(clamp_interval_ms(1), MIN_INTERVAL_MS);
        assert_eq!(clamp_timer_ms(0), None);
        assert_eq!(clamp_timer_ms(MAX_TIMER_MS + 1), Some(MAX_TIMER_MS));
    }

    #[test]
    fn fact_key_is_plugin_scoped() {
        assert_eq!(PluginSnapshot::fact_key("loop"), "plugin/loop");
    }
}
