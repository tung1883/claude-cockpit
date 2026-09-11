use crate::{paths, ui};
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
pub fn launch(profile: &str, args: &[String]) -> Result<i32> {
    let target = claude_target();
    let mut cmd = if target.shell {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(&target.command);
        c.args(args);
        c
    } else {
        let mut c = Command::new(&target.command);
        c.args(&target.prefix).args(args);
        c
    };
    cmd.env("CLAUDE_CONFIG_DIR", paths::profile_dir(profile));
    match cmd.status() {
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
