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

/// How long a full window with no reset time is believed.
const UNDATED_LIMIT_MS: f64 = 6.0 * 3_600_000.0;

/// A window's usage, 0 once its reset time has passed: the statusline only
/// refreshes a reading while the account is in use, so it can be long over.
fn live_pct(pct: Option<f64>, resets_at: Option<f64>, now_secs: f64) -> f64 {
    if resets_at.is_some_and(|t| t > 0.0 && t <= now_secs) {
        0.0
    } else {
        pct.unwrap_or(0.0)
    }
}

/// When either window is at/near its cap now, the limit period: the window's
/// reset time, or 0 when it has none. 95% instead of 100% because Claude Code
/// stops issuing turns slightly before the reported number reaches an exact
/// 100.
pub fn limit_at(q: &Quota, now_secs: f64) -> Option<u64> {
    let recent = now_secs * 1000.0 - q.updated_ms < UNDATED_LIMIT_MS;
    let full = |pct: Option<f64>, resets_at: Option<f64>| {
        let dated = resets_at.filter(|&t| t > 0.0);
        (pct.is_some_and(|p| p >= 95.0) && dated.map_or(recent, |t| t > now_secs)).then(|| dated.unwrap_or(0.0) as u64)
    };
    // With both full, the one that resets last is what holds the account.
    full(q.five_pct, q.five_resets_at).max(full(q.seven_pct, q.seven_resets_at))
}

/// `limit_at` the present moment.
pub fn limit(q: &Quota) -> Option<u64> {
    limit_at(q, now_secs())
}

pub fn is_exhausted(quota: &Quota) -> bool {
    limit(quota).is_some()
}

/// Highest of the two usage percentages, for ranking candidate profiles.
pub fn usage(quota: &Quota) -> f64 {
    let now = now_secs();
    live_pct(quota.five_pct, quota.five_resets_at, now).max(live_pct(quota.seven_pct, quota.seven_resets_at, now))
}

/// Whether `limit` is news for `account`: each limit period is announced once.
/// `told` remembers what was announced.
pub fn announce(told: &mut std::collections::HashMap<String, u64>, account: &str, limit: Option<u64>) -> bool {
    match limit {
        Some(period) => told.insert(account.to_string(), period) != Some(period),
        None => {
            // A period with no reset time can only be told apart by the gap.
            if told.get(account) == Some(&0) {
                told.remove(account);
            }
            false
        }
    }
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
    if let Some(period) = limit_at(q, now_secs) {
        parts.push(match period {
            0 => "at limit".to_string(),
            t => format!("at limit, resets {}", in_text(t as f64, now_secs)),
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
    fn a_limit_ends_when_its_window_resets() {
        let now = 1_000_000.0;
        let q = |pct: f64, resets: Option<f64>, age_h: f64| Quota { five_pct: Some(pct), five_resets_at: resets, updated_ms: (now - age_h * 3600.0) * 1000.0, ..Default::default() };
        assert_eq!(limit_at(&q(97.0, Some(now + 60.0), 48.0), now), Some(1_000_060));
        assert_eq!(limit_at(&q(97.0, Some(now - 60.0), 1.0), now), None);
        assert_eq!(limit_at(&q(97.0, None, 5.0), now), Some(0));
        assert_eq!(limit_at(&q(97.0, None, 7.0), now), None);
        assert_eq!(limit_at(&q(90.0, Some(now + 60.0), 0.0), now), None);
        let both = Quota { seven_pct: Some(99.0), seven_resets_at: Some(now + 900.0), ..q(97.0, Some(now + 60.0), 0.0) };
        assert_eq!(limit_at(&both, now), Some(1_000_900));
        assert_eq!(live_pct(Some(80.0), Some(now - 1.0), now), 0.0);
    }

    #[test]
    fn each_limit_period_is_announced_once() {
        let mut told = Default::default();
        assert!(announce(&mut told, "a", Some(10)));
        assert!(!announce(&mut told, "a", Some(10)));
        assert!(announce(&mut told, "b", Some(10)));
        assert!(!announce(&mut told, "a", None));
        assert!(!announce(&mut told, "a", Some(10)), "the same period seen again");
        assert!(announce(&mut told, "a", Some(20)));
        assert!(announce(&mut told, "c", Some(0)));
        announce(&mut told, "c", None);
        assert!(announce(&mut told, "c", Some(0)));
    }

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
