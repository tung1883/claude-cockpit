// Cache of the last quota reading seen by the statusline hook, so the
// cockpit can make an auto-handoff decision after Claude exits without
// having watched its live output (stdio is inherited straight through to
// the terminal while Claude runs — see launch::launch).
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Quota {
    pub five_pct: Option<f64>,
    pub seven_pct: Option<f64>,
    pub five_resets_at: Option<f64>,
    pub seven_resets_at: Option<f64>,
    pub updated_ms: f64,
}

fn cache_file(config_dir: &Path) -> std::path::PathBuf {
    config_dir.join(".cockpit-quota.json")
}

/// Best-effort write, called on every statusline render. Never fails the
/// statusline over a caching problem.
pub fn save(config_dir: &Path, quota: &Quota) {
    if let Ok(text) = serde_json::to_string(quota) {
        let _ = std::fs::write(cache_file(config_dir), text);
    }
}

pub fn load(profile: &str) -> Option<Quota> {
    let text = std::fs::read_to_string(cache_file(&crate::paths::profile_dir(profile))).ok()?;
    serde_json::from_str(&text).ok()
}

/// Whether either window is at/near its cap. 95% instead of 100% because
/// Claude Code stops issuing turns slightly before the reported number
/// reaches an exact 100.
pub fn is_exhausted(quota: &Quota) -> bool {
    quota.five_pct.is_some_and(|p| p >= 95.0) || quota.seven_pct.is_some_and(|p| p >= 95.0)
}

/// Highest of the two usage percentages, for ranking candidate profiles.
pub fn usage(quota: &Quota) -> f64 {
    quota.five_pct.unwrap_or(0.0).max(quota.seven_pct.unwrap_or(0.0))
}

fn now_secs() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// "in 25m", "in 3h", "in 2d" for a reset time given as epoch seconds.
fn in_text(epoch_secs: f64, now: f64) -> String {
    let s = epoch_secs - now;
    if s <= 0.0 {
        "now".to_string()
    } else if s < 3600.0 {
        format!("in {}m", (s / 60.0).round())
    } else if s < 86400.0 {
        format!("in {}h", (s / 3600.0).round())
    } else {
        format!("in {}d", (s / 86400.0).round())
    }
}

/// One line for a hand-off target: both windows, when the full one resets, and
/// a warning when the reading is old (the statusline only refreshes it while
/// that account is being used).
pub fn describe_at(q: &Quota, now_secs: f64) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(p) = q.five_pct {
        parts.push(format!("5h {p:.0}%"));
    }
    if let Some(p) = q.seven_pct {
        parts.push(format!("7d {p:.0}%"));
    }
    if parts.is_empty() {
        return "usage unknown".to_string();
    }
    if is_exhausted(q) {
        let at = if q.five_pct.is_some_and(|p| p >= 95.0) { q.five_resets_at } else { q.seven_resets_at };
        parts.push(match at.filter(|&v| v > 0.0) {
            Some(t) => format!("at limit, resets {}", in_text(t, now_secs)),
            None => "at limit".to_string(),
        });
    }
    let age_h = (now_secs * 1000.0 - q.updated_ms) / 3_600_000.0;
    if age_h > 6.0 {
        parts.push(if age_h >= 48.0 { format!("reading {}d old", (age_h / 24.0).floor()) } else { format!("reading {}h old", age_h.floor()) });
    }
    parts.join(" · ")
}

pub fn describe(q: &Quota) -> String {
    describe_at(q, now_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_windows_limit_and_stale_readings() {
        let now = 1_000_000.0;
        let fresh = Quota { five_pct: Some(34.4), seven_pct: Some(12.0), five_resets_at: None, seven_resets_at: None, updated_ms: now * 1000.0 - 60_000.0 };
        assert_eq!(describe_at(&fresh, now), "5h 34% · 7d 12%");
        let full = Quota { five_pct: Some(97.0), seven_pct: Some(40.0), five_resets_at: Some(now + 7200.0), seven_resets_at: None, updated_ms: now * 1000.0 };
        assert_eq!(describe_at(&full, now), "5h 97% · 7d 40% · at limit, resets in 2h");
        let old = Quota { five_pct: Some(10.0), seven_pct: None, five_resets_at: None, seven_resets_at: None, updated_ms: (now - 3.0 * 86400.0) * 1000.0 };
        assert_eq!(describe_at(&old, now), "5h 10% · reading 3d old");
        assert_eq!(describe_at(&Quota::default(), now), "usage unknown");
    }
}
