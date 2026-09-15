use crate::paths;
use serde::{Deserialize, Serialize};

/// Which channel `notify*.ps1` uses for a hook event — a single tri-state
/// instead of two independent on/off toggles, since "both at once" wasn't
/// a real use case and the pair read as two unrelated rows in Settings.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NotifyMode {
    Off,
    Toast,
    Terminal,
}

impl NotifyMode {
    const ORDER: [NotifyMode; 3] = [NotifyMode::Off, NotifyMode::Toast, NotifyMode::Terminal];

    pub fn label(self) -> &'static str {
        match self {
            NotifyMode::Off => "off",
            NotifyMode::Toast => "toast",
            NotifyMode::Terminal => "terminal",
        }
    }

    /// Wraps both directions — Left/Enter and Right cycle through the same
    /// 3 states, just in opposite order.
    pub fn cycle(self, forward: bool) -> NotifyMode {
        let i = Self::ORDER.iter().position(|m| *m == self).unwrap_or(0);
        let n = Self::ORDER.len();
        let next = if forward { (i + 1) % n } else { (i + n - 1) % n };
        Self::ORDER[next]
    }
}

impl Default for NotifyMode {
    fn default() -> Self {
        NotifyMode::Toast
    }
}

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
    /// Read by the notify*.ps1 hook scripts (not just the Rust side) — they
    /// load the same settings.json to decide whether/how to raise a
    /// Windows toast or terminal bell for Notification/Stop/SubagentStop.
    #[serde(default)]
    notify_mode: NotifyMode,
    /// Projects hidden from the Notes screen (their TODO.md/PLAN.md and
    /// master-file section are untouched — this only filters the listing).
    /// `notes::Session::start` clears a root from this set automatically,
    /// so hiding one is undone the next time a session actually opens
    /// there rather than needing an explicit unhide action.
    #[serde(default)]
    hidden_notes_projects: Vec<String>,
}

fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Settings { notes_enabled: false, session_wrapped: true, notify_mode: NotifyMode::default(), hidden_notes_projects: Vec::new() }
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

pub fn notify_mode() -> NotifyMode {
    load().notify_mode
}

pub fn set_notify_mode(mode: NotifyMode) {
    let mut settings = load();
    settings.notify_mode = mode;
    save(&settings);
}

pub fn hidden_notes_projects() -> Vec<String> {
    load().hidden_notes_projects
}

pub fn hide_notes_project(path: &str) {
    let mut settings = load();
    if !settings.hidden_notes_projects.iter().any(|p| p == path) {
        settings.hidden_notes_projects.push(path.to_string());
        save(&settings);
    }
}

/// No-op (skips the write) when `path` wasn't hidden — called on every
/// `notes::Session::start`, so it needs to be cheap in the common case.
pub fn unhide_notes_project(path: &str) {
    let mut settings = load();
    let before = settings.hidden_notes_projects.len();
    settings.hidden_notes_projects.retain(|p| p != path);
    if settings.hidden_notes_projects.len() != before {
        save(&settings);
    }
}
