// Minimal ANSI helpers, ported 1:1 from src/ui.js so the rendered output is
// byte-for-byte the same. We use crossterm only for raw-mode/alt-screen
// primitives and reading key events (that's the part Node's bare `readline`
// got wrong on Windows); the actual escape sequences are hand-written, same
// as the JS version, for exact visual parity.
use crossterm::terminal;
use std::io::{IsTerminal, Write};
use std::sync::OnceLock;

fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::io::stdout().is_terminal() && std::env::var("NO_COLOR").is_err())
}

fn truecolor() -> bool {
    static TRUECOLOR: OnceLock<bool> = OnceLock::new();
    *TRUECOLOR.get_or_init(|| {
        matches!(
            std::env::var("COLORTERM").unwrap_or_default().to_lowercase().as_str(),
            "truecolor" | "24bit"
        )
    })
}

fn orange_open() -> &'static str {
    if truecolor() {
        "38;2;217;119;87"
    } else {
        "38;5;173"
    }
}

fn wrap(text: &str, open: &str, close: &str) -> String {
    if enabled() {
        format!("\x1b[{open}m{text}\x1b[{close}m")
    } else {
        text.to_string()
    }
}

pub mod color {
    use super::*;

    pub fn reset() -> &'static str {
        if enabled() { "\x1b[0m" } else { "" }
    }
    pub fn bold(t: &str) -> String { wrap(t, "1", "22") }
    pub fn dim(t: &str) -> String { wrap(t, "2", "22") }
    pub fn orange(t: &str) -> String { wrap(t, orange_open(), "39") }
    pub fn orange_bold(t: &str) -> String {
        if enabled() {
            format!("\x1b[1;{}m{t}\x1b[0m", orange_open())
        } else {
            t.to_string()
        }
    }
    pub fn red(t: &str) -> String { wrap(t, "31", "39") }
    pub fn green(t: &str) -> String { wrap(t, "32", "39") }
    pub fn yellow(t: &str) -> String { wrap(t, "33", "39") }
    pub fn cyan(t: &str) -> String { wrap(t, "36", "39") }
    pub fn gray(t: &str) -> String { wrap(t, "90", "39") }
}

pub mod glyph {
    use super::enabled;

    pub fn pointer() -> &'static str { if enabled() { "❯" } else { ">" } }
    pub fn spark() -> &'static str { if enabled() { "✻" } else { "*" } }
    pub fn dot() -> &'static str { if enabled() { "·" } else { "-" } }
    pub fn back() -> &'static str { if enabled() { "←" } else { "<-" } }
}

pub fn banner() -> String {
    format!("{} {}", color::orange_bold(glyph::spark()), color::bold("Claude Cockpit"))
}

pub fn rule(width: Option<usize>) -> String {
    let cols = width.unwrap_or_else(|| {
        terminal::size().map(|(w, _)| w as usize).unwrap_or(80).min(100)
    });
    color::gray(&"─".repeat(cols.max(8)))
}

fn write_raw(s: &str) {
    if enabled() {
        let mut out = std::io::stdout();
        let _ = out.write_all(s.as_bytes());
        let _ = out.flush();
    }
}

/// Hard wipe — use only for a real reset (startup, returning from Claude).
pub fn clear_screen() {
    write_raw("\x1b[2J\x1b[3J\x1b[H");
}
/// Soft reset for switching between cockpit screens: home + erase below, no flash.
pub fn home_clear() {
    write_raw("\x1b[H\x1b[0J");
}
pub fn hide_cursor() {
    write_raw("\x1b[?25l");
}
pub fn show_cursor() {
    write_raw("\x1b[?25h");
}

/// Repaint a block of lines anchored to the top of the screen: home the
/// cursor, overwrite each line, then erase anything below. No full-screen
/// wipe, so there's no flash — not between keystrokes and not between
/// screens. Every cockpit screen paints from row 1, so switching screens just
/// overwrites in place.
pub fn repaint(lines: &[String]) {
    if !enabled() {
        println!("{}", lines.join("\n"));
        return;
    }
    // Home, clear+write each line, join with CR+LF, then erase below. No
    // trailing newline — a newline on the last row scrolls the page and the
    // next paint lands one row higher: that one-row jitter is the "flicker".
    let body = lines
        .iter()
        .map(|l| format!("\x1b[2K{l}"))
        .collect::<Vec<_>>()
        .join("\r\n");
    write_raw(&format!("\x1b[H{body}\x1b[0J"));
}

/// RAII guard: enables raw mode + the alternate screen buffer on creation,
/// and — critically — restores both on Drop, which fires on every exit path
/// including a panic unwind. This is the thing src/ui.js had to fake with
/// `process.on('exit', ...)` (which doesn't cover every termination path);
/// here the compiler enforces it.
pub struct Terminal {
    in_alt: bool,
}

impl Terminal {
    pub fn enter() -> std::io::Result<Self> {
        if enabled() {
            terminal::enable_raw_mode()?;
            write_raw("\x1b[?1049h\x1b[H");
        }
        Ok(Terminal { in_alt: enabled() })
    }

    /// Temporarily give the real terminal back to a child process (Claude,
    /// an external editor) — leaves alt screen + raw mode, restores both
    /// when the returned guard is dropped.
    pub fn suspend(&mut self) -> SuspendGuard {
        if self.in_alt {
            write_raw("\x1b[?25h\x1b[?1049l");
            let _ = terminal::disable_raw_mode();
        }
        write_raw("\x1b[0m\x1b[?25h");
        SuspendGuard { was_in_alt: self.in_alt }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if self.in_alt {
            write_raw("\x1b[?25h\x1b[?1049l");
            let _ = terminal::disable_raw_mode();
        }
    }
}

pub struct SuspendGuard {
    was_in_alt: bool,
}

impl Drop for SuspendGuard {
    fn drop(&mut self) {
        if self.was_in_alt {
            let _ = terminal::enable_raw_mode();
            write_raw("\x1b[?1049h\x1b[H");
        }
    }
}
