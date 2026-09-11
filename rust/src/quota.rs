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
