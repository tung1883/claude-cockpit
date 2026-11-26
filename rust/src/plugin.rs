//! The Claude Code plugin cpit loads into the sessions it starts, embedded so
//! `cargo install` still gives one exe. Its source is in `rust/plugin/`
//! (`/toc`); the `SessionStart` hook that reports a pane's session id (see
//! `workspace.rs`) is added here, since it names this executable.

use std::path::{Path, PathBuf};

const FILES: &[(&str, &str)] = &[
    (".claude-plugin/plugin.json", include_str!("../plugin/.claude-plugin/plugin.json")),
    ("hooks/hooks.json", include_str!("../plugin/hooks/hooks.json")),
    ("hooks/register.tsx", include_str!("../plugin/hooks/register.tsx")),
];

const HOOKS: &str = "hooks/hooks.json";

/// `hooks.json` with the session-tracking hook running `exe`.
fn hooks_json(source: &str, exe: &Path) -> String {
    // Hooks run in a shell: forward slashes survive every one.
    let command = format!("\"{}\" session-track", exe.display().to_string().replace('\\', "/"));
    let mut json: serde_json::Value = serde_json::from_str(source).expect("embedded hooks.json is valid");
    json["hooks"] = serde_json::json!({ "SessionStart": [ { "hooks": [ { "type": "command", "command": command } ] } ] });
    format!("{json:#}\n")
}

/// Writes the files that differ under `dir`, for the executable `exe`.
fn write_to(dir: &Path, exe: &Path) -> std::io::Result<()> {
    for (name, source) in FILES {
        let text = if *name == HOOKS { hooks_json(source, exe) } else { source.to_string() };
        let path = dir.join(name);
        if std::fs::read_to_string(&path).is_ok_and(|old| old == text) {
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, text)?;
    }
    Ok(())
}

/// The folder to pass to `--plugin-dir`, up to date. None when turned off with
/// `CPIT_NO_PLUGIN` or when it cannot be written (Claude then starts without it).
pub fn dir() -> Option<PathBuf> {
    if std::env::var_os("CPIT_NO_PLUGIN").is_some() {
        return None;
    }
    // One folder per executable name, so cpit and cpit-dev don't overwrite
    // each other's version.
    let exe = std::env::current_exe().ok()?;
    let dir = crate::paths::root().join("plugin").join(exe.file_stem()?);
    write_to(&dir, &exe).ok()?;
    Some(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_and_repairs_the_plugin() {
        let dir = std::env::temp_dir().join(format!("cockpit-plugin-{}", std::process::id()));
        let exe = Path::new("C:\\tools\\cpit.exe");
        write_to(&dir, exe).unwrap();
        for (name, text) in FILES.iter().filter(|(name, _)| *name != HOOKS) {
            assert_eq!(std::fs::read_to_string(dir.join(name)).unwrap(), *text, "{name}");
        }
        let hooks: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join(HOOKS)).unwrap()).unwrap();
        assert_eq!(hooks["modules"][0], "./register.tsx");
        assert_eq!(hooks["hooks"]["SessionStart"][0]["hooks"][0]["command"], "\"C:/tools/cpit.exe\" session-track");
        std::fs::write(dir.join("hooks/register.tsx"), "stale").unwrap();
        write_to(&dir, exe).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("hooks/register.tsx")).unwrap(), FILES[2].1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
