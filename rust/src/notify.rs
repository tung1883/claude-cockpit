//! Desktop/terminal notifications raised by cpit itself when a pane's program
//! reports (OSC 7501) that it finished or needs input. Replaces the Claude
//! Code hook scripts for wrapped sessions: cpit knows which pane it was and
//! whether the user is already looking at it.

use crate::settings::NotifyMode;
use crate::sidebar::Status;
use std::io::Write;
use std::time::{Duration, Instant};

/// A pane that notified less than this long ago stays quiet.
const MIN_GAP: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Finished,
    NeedsInput,
    /// A sub-agent of the pane's program ended.
    Subagent,
}

/// What a status change deserves, if anything. `Done` only counts after
/// `Working`, so a pane that starts out reporting `done` (or whose previous
/// status was never seen) stays silent.
pub fn decide(prev: Status, new: Status, watching: bool, last: Option<Instant>, now: Instant) -> Option<Kind> {
    let kind = match (prev, new) {
        (Status::Working, Status::Done) => Kind::Finished,
        (p, Status::Blocked) if p != Status::Blocked => Kind::NeedsInput,
        _ => return None,
    };
    if watching || last.is_some_and(|t| now.duration_since(t) < MIN_GAP) {
        return None;
    }
    Some(kind)
}

/// A sub-agent ending is not a status change of the pane, so only the watch
/// and rate-limit rules apply.
pub fn decide_subagent(watching: bool, last: Option<Instant>, now: Instant) -> Option<Kind> {
    (!watching && !last.is_some_and(|t| now.duration_since(t) < MIN_GAP)).then_some(Kind::Subagent)
}

/// The notification text. `block_kind` is the `kind` of a `blocked` report
/// (`question`, `permission`, `auth`) and `detail` the program's own line: the
/// question being asked, or the sub-agent's title.
pub fn message(title: &str, project: &str, kind: Kind, block_kind: Option<&str>, detail: Option<&str>) -> String {
    let head = format!("{title} · {project}");
    match kind {
        Kind::Finished => format!("{head} finished"),
        Kind::NeedsInput => {
            let what = match block_kind {
                Some("question") => "asks a question",
                Some("permission") => "needs permission",
                Some("auth") => "needs you to log in",
                _ => "needs input",
            };
            match detail.map(|d| d.strip_prefix("answer: ").unwrap_or(d)).filter(|d| !d.is_empty()) {
                Some(d) => format!("{head} {what}: {}", clip(d, 80)),
                None => format!("{head} {what}"),
            }
        }
        Kind::Subagent => match detail.filter(|d| !d.is_empty()) {
            Some(d) => format!("{head} subagent done: {}", clip(d, 60)),
            None => format!("{head} subagent done"),
        },
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Raises the notification for `mode` without blocking the caller.
pub fn send(mode: NotifyMode, kind: Kind, body: &str) {
    match mode {
        NotifyMode::Off => {}
        NotifyMode::Terminal => {
            terminal(body);
            std::thread::spawn(move || chime(kind));
        }
        NotifyMode::Toast => {
            let body = body.to_string();
            std::thread::spawn(move || {
                if !desktop(&body) {
                    terminal(&body);
                    chime(kind);
                }
            });
        }
    }
}

/// An audible cue for `terminal` mode, which has no notification of its own:
/// the Windows system sounds the old hook scripts used (Asterisk for
/// finished, Exclamation for needs input), the stock macOS sounds on macOS.
/// Nothing on Linux beyond the terminal bell.
fn chime(kind: Kind) {
    #[cfg(windows)]
    {
        let sound = if kind == Kind::NeedsInput { "Exclamation" } else { "Asterisk" };
        let mut cmd = std::process::Command::new("powershell");
        cmd.args(["-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden", "-Command"]).arg(format!("[System.Media.SystemSounds]::{sound}.Play(); Start-Sleep -Milliseconds 1200"));
        run(cmd);
    }
    #[cfg(target_os = "macos")]
    {
        let sound = if kind == Kind::NeedsInput { "Sosumi" } else { "Glass" };
        let mut cmd = std::process::Command::new("afplay");
        cmd.arg(format!("/System/Library/Sounds/{sound}.aiff"));
        run(cmd);
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    let _ = kind;
}

/// BEL (taskbar flash / tab marker in most terminals) plus OSC 9, the
/// desktop-notification sequence iTerm2, Ghostty, kitty and WezTerm act on
/// and everything else ignores.
fn terminal(body: &str) {
    let clean: String = body.chars().filter(|c| !c.is_control()).collect();
    let mut out = std::io::stdout();
    let _ = write!(out, "\x07\x1b]9;{clean}\x07");
    let _ = out.flush();
}

/// Runs a command and waits for it, returning whether it started and succeeded.
fn run(mut cmd: std::process::Command) -> bool {
    use std::process::Stdio;
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd.status().map(|s| s.success()).unwrap_or(false)
}

/// Native notification for this OS. Returns false when it could not be
/// raised, so the caller can fall back to the terminal bell.
fn desktop(body: &str) -> bool {
    // Test hook: run this program with the text as its only argument.
    if let Some(prog) = std::env::var_os("CPIT_NOTIFY_CMD") {
        let mut cmd = std::process::Command::new(prog);
        cmd.arg(body);
        return run(cmd);
    }
    platform(body)
}

#[cfg(windows)]
fn platform(body: &str) -> bool {
    let Some(script) = windows_scripts() else { return false };
    let mut cmd = std::process::Command::new("powershell");
    cmd.args(["-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden", "-File"]).arg(script).args(["-Body", body]);
    run(cmd)
}

/// The toast scripts live in `scripts/notify/` and are embedded, then written
/// out once so PowerShell can run them from disk.
#[cfg(windows)]
fn windows_scripts() -> Option<std::path::PathBuf> {
    const FILES: [(&str, &str); 3] = [
        ("cpit-toast.ps1", include_str!("../../scripts/notify/cpit-toast.ps1")),
        ("claude-toast.ps1", include_str!("../../scripts/notify/claude-toast.ps1")),
        ("claude-shortcut.ps1", include_str!("../../scripts/notify/claude-shortcut.ps1")),
    ];
    let dir = crate::paths::root().join("notify");
    std::fs::create_dir_all(&dir).ok()?;
    for (name, text) in FILES {
        let path = dir.join(name);
        if std::fs::read_to_string(&path).map(|cur| cur != text).unwrap_or(true) {
            std::fs::write(&path, text).ok()?;
        }
    }
    Some(dir.join("cpit-toast.ps1"))
}

#[cfg(target_os = "macos")]
fn platform(body: &str) -> bool {
    let escaped = body.replace('\\', "\\\\").replace('"', "\\\"");
    let mut cmd = std::process::Command::new("osascript");
    cmd.arg("-e").arg(format!("display notification \"{escaped}\" with title \"Claude Cockpit\" sound name \"Glass\""));
    run(cmd)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform(body: &str) -> bool {
    let mut cmd = std::process::Command::new("notify-send");
    cmd.args(["--app-name=Claude Cockpit", "Claude Cockpit", body]);
    run(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finish_and_blocked_notify_when_not_watched() {
        let now = Instant::now();
        assert_eq!(decide(Status::Working, Status::Done, false, None, now), Some(Kind::Finished));
        assert_eq!(decide(Status::Working, Status::Blocked, false, None, now), Some(Kind::NeedsInput));
        assert_eq!(decide(Status::Idle, Status::Blocked, false, None, now), Some(Kind::NeedsInput));
    }

    #[test]
    fn other_changes_stay_silent() {
        let now = Instant::now();
        for (p, n) in [
            (Status::Unknown, Status::Done),
            (Status::Idle, Status::Done),
            (Status::Idle, Status::Working),
            (Status::Done, Status::Idle),
            (Status::Blocked, Status::Working),
            (Status::Working, Status::Idle),
            (Status::Blocked, Status::Blocked),
        ] {
            assert_eq!(decide(p, n, false, None, now), None, "{p:?} -> {n:?}");
        }
    }

    #[test]
    fn a_watched_pane_is_suppressed() {
        assert_eq!(decide(Status::Working, Status::Done, true, None, Instant::now()), None);
    }

    #[test]
    fn rate_limit_per_pane() {
        let t0 = Instant::now();
        assert_eq!(decide(Status::Working, Status::Done, false, Some(t0), t0 + Duration::from_secs(3)), None);
        assert_eq!(decide(Status::Working, Status::Done, false, Some(t0), t0 + Duration::from_secs(11)), Some(Kind::Finished));
    }

    #[test]
    fn message_names_pane_and_project() {
        assert_eq!(message("claude 2", "claude-cockpit", Kind::NeedsInput, None, None), "claude 2 · claude-cockpit needs input");
        assert_eq!(message("shell 1", "app", Kind::Finished, None, None), "shell 1 · app finished");
    }

    #[test]
    fn questions_permissions_and_subagents_say_what_happened() {
        let q = message("claude 1", "app", Kind::NeedsInput, Some("question"), Some("answer: Which color? (Red · Blue)"));
        assert_eq!(q, "claude 1 · app asks a question: Which color? (Red · Blue)");
        assert_eq!(message("claude 1", "app", Kind::NeedsInput, Some("permission"), None), "claude 1 · app needs permission");
        assert_eq!(message("claude 1", "app", Kind::Subagent, None, Some("Reply with ok")), "claude 1 · app subagent done: Reply with ok");
        assert_eq!(message("claude 1", "app", Kind::Subagent, None, None), "claude 1 · app subagent done");
    }

    #[test]
    fn long_details_are_clipped() {
        let long = "x".repeat(200);
        let m = message("c", "p", Kind::NeedsInput, Some("question"), Some(&long));
        assert!(m.ends_with('…') && m.chars().count() < 120);
    }

    #[test]
    fn subagent_notifications_follow_watch_and_rate_limit() {
        let t0 = Instant::now();
        assert_eq!(decide_subagent(false, None, t0), Some(Kind::Subagent));
        assert_eq!(decide_subagent(true, None, t0), None);
        assert_eq!(decide_subagent(false, Some(t0), t0 + Duration::from_secs(2)), None);
    }
}
