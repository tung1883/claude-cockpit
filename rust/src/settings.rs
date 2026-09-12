use crate::paths;
use serde::{Deserialize, Serialize};

/// Cockpit-wide feature toggles, persisted once for the whole tool (not
/// per-profile) — a spot to grow on/off switches into.
#[derive(Serialize, Deserialize)]
struct Settings {
    #[serde(default)]
    notes_enabled: bool,
    /// Launch a profile (Enter) into the wrapped session view — its own PTY,
    /// composited so `/shell`, panels, and Ctrl+B detach work — instead of a
    /// plain native Claude with inherited stdio. Default on.
    #[serde(default = "default_true")]
    session_wrapped: bool,
}

fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Settings { notes_enabled: false, session_wrapped: true }
    }
}

fn path() -> std::path::PathBuf {
    paths::root().join("settings.json")
}

fn load() -> Settings {
    std::fs::read_to_string(path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn save(settings: &Settings) {
    if let Ok(json) = serde_json::to_string_pretty(settings) {
        let _ = std::fs::write(path(), json);
    }
}

pub fn notes_enabled() -> bool {
    load().notes_enabled
}

pub fn set_notes_enabled(enabled: bool) {
    let mut settings = load();
    settings.notes_enabled = enabled;
    save(&settings);
}

pub fn session_wrapped() -> bool {
    load().session_wrapped
}

pub fn set_session_wrapped(enabled: bool) {
    let mut settings = load();
    settings.session_wrapped = enabled;
    save(&settings);
}
