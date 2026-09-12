use crate::{notes, paths, session, ui};
use anyhow::Result;
use regex::Regex;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

struct Target {
    command: PathBuf,
    prefix: Vec<PathBuf>,
    shell: bool,
}

/// Same resolved Claude binary as `launch()`, but as a `portable_pty`
/// CommandBuilder instead of a `std::process::Command` — for panels.rs,
/// which needs a PTY-attached child rather than one that simply inherits
/// this process's own stdio. Composes the profile/project system prompts
/// the same way `launch()` does (no per-session sidecar, since panes start
/// fresh); returns the temp file's path alongside so the caller can delete
/// it when the pane closes.
pub(crate) fn claude_pty_command(profile: &str, cwd: &Path) -> (portable_pty::CommandBuilder, Option<PathBuf>) {
    let target = claude_target();
    let mut cmd = if target.shell {
        let mut c = portable_pty::CommandBuilder::new("cmd");
        c.arg("/C");
        c.arg(&target.command);
        c
    } else {
        let mut c = portable_pty::CommandBuilder::new(&target.command);
        for p in &target.prefix {
            c.arg(p);
        }
        c
    };
    let sysprompt = write_system_prompt_file(profile, None);
    if let Some(path) = &sysprompt {
        cmd.arg("--append-system-prompt-file");
        cmd.arg(path);
    }
    cmd.cwd(cwd);
    cmd.env("CLAUDE_CONFIG_DIR", paths::profile_dir(profile));
    (cmd, sysprompt)
}

/// The three cockpit-managed system-prompt levels, coarse to fine:
/// profile-wide, then this project, then (only when resuming) this specific
/// session. Composed into one string, in that order.
fn compose_system_prompt(profile: &str, resume_session_id: Option<&str>) -> Vec<String> {
    let mut pieces = Vec::new();
    if let Some(text) = nonempty_content(&paths::profile_dir(profile).join(".cockpit-system-prompt.md")) {
        pieces.push(text);
    }
    if let Ok(cwd) = std::env::current_dir() {
        let project_root = notes::project_root(&cwd);
        if let Some(text) = nonempty_content(&project_root.join(".cockpit-system-prompt.md")) {
            pieces.push(text);
        }
    }
    if let Some(session_id) = resume_session_id {
        if let Some(session_path) = session::session_file_path(profile, session_id) {
            if let Some(text) = nonempty_content(&session::system_prompt_sidecar(&session_path)) {
                pieces.push(text);
            }
        }
    }
    pieces
}

/// Writes the composed system prompt to a unique temp file (or None if there
/// is nothing to append). The caller owns cleanup. Unique per call so
/// several panes launching at once don't clobber each other's file.
pub(crate) fn write_system_prompt_file(profile: &str, resume_session_id: Option<&str>) -> Option<PathBuf> {
    let pieces = compose_system_prompt(profile, resume_session_id);
    if pieces.is_empty() {
        return None;
    }
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = paths::root().join(format!(".launch-system-prompt-{}-{}.md", std::process::id(), seq));
    std::fs::write(&path, pieces.join("\n\n")).ok().map(|_| path)
}

/// Resolve how to start Claude. On Windows we avoid the `claude.cmd` shim so
/// Ctrl+C inside Claude does not drop to cmd.exe's "Terminate batch job
/// (Y/N)?" prompt — we run the real .exe (or `node cli.js`) that the shim
/// points at.
fn claude_target() -> &'static Target {
    static TARGET: OnceLock<Target> = OnceLock::new();
    TARGET.get_or_init(|| {
        if let Ok(bin) = std::env::var("CLAUDE_BIN") {
            let shell = bin.to_lowercase().ends_with(".cmd") || bin.to_lowercase().ends_with(".bat");
            return Target { command: PathBuf::from(bin), prefix: vec![], shell };
        }
        if !cfg!(windows) {
            return Target { command: PathBuf::from("claude"), prefix: vec![], shell: false };
        }

        let marker_re = Regex::new(r#"(?i)%[~a-z0-9]*dp0%[\\/]?([^"'\s]+?\.(?:exe|js))"#).unwrap();
        for shim in which::which_all("claude").into_iter().flatten() {
            let dir = shim.parent().unwrap_or(Path::new(".")).to_path_buf();
            let mut guesses: Vec<PathBuf> = Vec::new();
            let ext = shim.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
            if matches!(ext.as_str(), "cmd" | "bat" | "ps1") {
                if let Ok(text) = std::fs::read_to_string(&shim) {
                    if let Some(caps) = marker_re.captures(&text) {
                        guesses.push(dir.join(&caps[1]));
                    }
                }
            }
            guesses.push(dir.join("node_modules/@anthropic-ai/claude-code/bin/claude.exe"));
            guesses.push(dir.join("node_modules/@anthropic-ai/claude-code/cli.js"));
            if let Some(hit) = guesses.into_iter().find(|p| p.exists()) {
                if hit.extension().and_then(|e| e.to_str()).unwrap_or("").eq_ignore_ascii_case("js") {
                    let node = which::which("node").unwrap_or_else(|_| PathBuf::from("node"));
                    return Target { command: node, prefix: vec![hit], shell: false };
                }
                return Target { command: hit, prefix: vec![], shell: false };
            }
        }
        Target { command: PathBuf::from("claude.cmd"), prefix: vec![], shell: true }
    })
}

/// Spawn Claude with the profile's isolated config dir, inheriting stdio, and
/// wait for it to exit. Ctrl+C is handled once, globally, at process startup
/// (see main.rs) — nothing special needs to happen here for it.
/// Reads a file if it exists and has actual content, else None.
fn nonempty_content(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    if text.trim().is_empty() { None } else { Some(text) }
}

pub fn launch(profile: &str, args: &[String]) -> Result<i32> {
    let target = claude_target();
    let mut full_args = args.to_vec();
    // Cockpit-managed, persisted system-prompt levels (see the Memory action
    // and the session action menu) — Claude Code itself has no concept of any
    // of this surviving between launches, only the --append-system-prompt*
    // flags for one invocation. Composed into a single file and appended,
    // since the flag only takes one path.
    let resume_id = args.iter().position(|a| a == "--resume").and_then(|i| args.get(i + 1)).map(|s| s.as_str());
    let combined_path = write_system_prompt_file(profile, resume_id);
    if let Some(path) = &combined_path {
        full_args.push("--append-system-prompt-file".to_string());
        full_args.push(path.to_string_lossy().to_string());
    }
    let mut cmd = if target.shell {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(&target.command);
        c.args(&full_args);
        c
    } else {
        let mut c = Command::new(&target.command);
        c.args(&target.prefix).args(&full_args);
        c
    };
    cmd.env("CLAUDE_CONFIG_DIR", paths::profile_dir(profile));
    let result = cmd.status();
    if let Some(path) = &combined_path {
        let _ = std::fs::remove_file(path);
    }
    match result {
        Ok(status) => Ok(status.code().unwrap_or(0)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!(
                "{}",
                ui::color::red(&format!("Claude CLI not found ({}). Install Claude Code or set CLAUDE_BIN to its full path.", target.command.display()))
            );
            Ok(1)
        }
        Err(error) => {
            eprintln!("{}", ui::color::red(&format!("Could not launch Claude CLI: {error}")));
            Ok(1)
        }
    }
}

/// Open a file in the user's editor ($VISUAL / $EDITOR, else notepad/nano),
/// inheriting stdio, and wait for it to close. Handles a multi-word command
/// like "code --wait".
pub fn open_in_editor(file: &Path) -> Result<()> {
    let editor_cmd = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| if cfg!(windows) { "notepad".into() } else { "nano".into() });
    let parts = split_command(&editor_cmd);
    let (cmd, rest) = parts.split_first().map(|(c, r)| (c.clone(), r.to_vec())).unwrap_or((editor_cmd.clone(), vec![]));
    let mut command = Command::new(&cmd);
    command.args(&rest).arg(file);
    match command.status() {
        Ok(_) => Ok(()),
        Err(error) => {
            eprintln!("{}", ui::color::red(&format!("Could not open editor ({cmd}): {error}")));
            eprintln!("{}", ui::color::dim("Set $env:EDITOR to your preferred editor."));
            Ok(())
        }
    }
}

/// Drops into a real interactive shell in `cwd`, inheriting stdio, and
/// waits for it to exit. A side channel for things like git that never
/// touches Claude's process or transcript — the whole point of it existing
/// separately from Claude's own Bash tool.
pub fn open_shell(cwd: &Path) -> Result<()> {
    let mut cmd = if cfg!(windows) {
        // This platform's primary shell is PowerShell — prefer it over
        // dropping to cmd.exe when it's actually on PATH.
        if which::which("pwsh").is_ok() {
            Command::new("pwsh")
        } else if which::which("powershell").is_ok() {
            Command::new("powershell")
        } else {
            Command::new(std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into()))
        }
    } else {
        Command::new(std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()))
    };
    cmd.current_dir(cwd);
    match cmd.status() {
        Ok(_) => Ok(()),
        Err(error) => {
            eprintln!("{}", ui::color::red(&format!("Could not launch a shell: {error}")));
            Ok(())
        }
    }
}

/// Splits "code --wait" into ["code", "--wait"], respecting simple double quotes.
fn split_command(s: &str) -> Vec<String> {
    let re = Regex::new(r#"[^\s"]+|"[^"]*""#).unwrap(); // matches launch.js's split-on-quoted-words
    re.find_iter(s).map(|m| m.as_str().trim_matches('"').to_string()).collect()
}
