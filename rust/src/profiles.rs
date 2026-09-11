use crate::paths;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    #[serde(rename = "createdAt")]
    pub created_at: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    profiles: BTreeMap<String, Profile>,
}

fn ensure_store() -> Result<()> {
    fs::create_dir_all(paths::profiles_dir())?;
    let file = paths::profiles_file();
    if !file.exists() {
        fs::write(&file, serde_json::to_string_pretty(&Store::default())?)?;
    }
    Ok(())
}

fn read_store() -> Result<Store> {
    ensure_store()?;
    let file = paths::profiles_file();
    let raw = fs::read_to_string(&file).with_context(|| format!("Cannot read {}", file.display()))?;
    match serde_json::from_str::<Store>(&raw) {
        Ok(store) => Ok(store),
        Err(_) => Ok(Store::default()),
    }
}

fn write_store(store: &Store) -> Result<()> {
    ensure_store()?;
    let file = paths::profiles_file();
    let tmp = file.with_extension(format!("json.tmp-{}", std::process::id()));
    fs::write(&tmp, serde_json::to_string_pretty(store)?)?;
    fs::rename(&tmp, &file)?;
    Ok(())
}

fn validate_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= 49
        && name.chars().next().unwrap().is_ascii_alphanumeric()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !valid {
        bail!("Profile names must be 1-49 characters: letters, numbers, _ or -");
    }
    Ok(())
}

pub fn add_profile(name: &str) -> Result<Profile> {
    validate_name(name)?;
    let mut store = read_store()?;
    store.profiles.entry(name.to_string()).or_insert_with(|| Profile {
        name: name.to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
    });
    fs::create_dir_all(paths::profile_dir(name))?;
    write_store(&store)?;
    Ok(store.profiles.get(name).unwrap().clone())
}

pub fn list_profiles() -> Result<Vec<Profile>> {
    let store = read_store()?;
    let mut profiles: Vec<Profile> = store.profiles.into_values().collect();
    profiles.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(profiles)
}

pub fn get_profile(name: &str) -> Result<Profile> {
    let store = read_store()?;
    store
        .profiles
        .get(name)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Unknown profile '{name}'. Run: claude-cockpit add {name}"))
}

pub fn remove_profile(name: &str) -> Result<(String, std::path::PathBuf)> {
    let mut store = read_store()?;
    if store.profiles.remove(name).is_none() {
        bail!("Unknown profile '{name}'.");
    }
    write_store(&store)?;
    let dir = paths::profile_dir(name);
    if dir.exists() {
        fs::remove_dir_all(&dir).ok();
    }
    Ok((name.to_string(), dir))
}

/// quiet: skip the printed messages (used when this runs automatically as
/// part of creating/importing a profile). only_if_missing: leave an existing
/// statusLine alone instead of overwriting it (used on import, so we don't
/// clobber a statusline the imported config already had configured).
pub fn install_statusline(name: &str, quiet: bool, only_if_missing: bool) -> Result<bool> {
    let profile = get_profile(name)?;
    let settings_path = paths::profile_dir(&profile.name).join("settings.json");
    let mut settings: serde_json::Value = if settings_path.exists() {
        let raw = fs::read_to_string(&settings_path).with_context(|| format!("Cannot read {}", settings_path.display()))?;
        let parsed: serde_json::Value = serde_json::from_str(&raw).with_context(|| format!("Cannot parse {}", settings_path.display()))?;
        if only_if_missing && parsed.get("statusLine").is_some() {
            return Ok(false);
        }
        let backup = settings_path.with_extension(format!("json.backup-{}", chrono::Utc::now().timestamp_millis()));
        fs::copy(&settings_path, &backup)?;
        if !quiet {
            println!("Backed up settings to {}", backup.display());
        }
        parsed
    } else {
        serde_json::json!({})
    };
    let exe = std::env::current_exe().unwrap_or_else(|_| "cpit".into());
    settings["statusLine"] = serde_json::json!({
        "type": "command",
        "command": format!("\"{}\" statusline", exe.display()),
    });
    fs::create_dir_all(settings_path.parent().unwrap())?;
    fs::write(&settings_path, format!("{}\n", serde_json::to_string_pretty(&settings)?))?;
    if !quiet {
        println!("Installed statusline for '{}'. Restart Claude Code to see it.", profile.name);
    }
    Ok(true)
}

/// Create a profile and wire up the cockpit statusline in one step, so a new
/// account never sits there silently missing stats.
pub fn create_account(name: &str) -> Result<Profile> {
    let profile = add_profile(name)?;
    let _ = install_statusline(&profile.name, true, false);
    Ok(profile)
}

/// Copy an existing Claude Code config (default: ~/.claude + ~/.claude.json)
/// into a new isolated profile, so it keeps that account's login, sessions,
/// plugins, skills, and settings.
pub fn import_profile(name: &str, source: Option<&std::path::Path>) -> Result<Profile> {
    let src = match source {
        Some(s) => s.to_path_buf(),
        None => dirs::home_dir().unwrap().join(".claude"),
    };
    if !src.exists() {
        bail!("Source config dir not found: {}", src.display());
    }
    let profile = add_profile(name)?;
    let dst = paths::profile_dir(&profile.name);
    let skip = ["node_modules", ".git", "cache", "shell-snapshots"];
    copy_tree_skip(&src, &dst, &skip)?;
    let home_json = src.parent().unwrap().join(".claude.json");
    if home_json.exists() {
        fs::copy(&home_json, dst.join(".claude.json"))?;
    }
    // The imported settings.json rarely has our statusline wired up — add
    // it, but leave alone whatever statusline the imported config already had.
    let _ = install_statusline(&profile.name, true, true);
    Ok(profile)
}

fn copy_tree_skip(src: &std::path::Path, dst: &std::path::Path, skip: &[&str]) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if skip.iter().any(|s| name.to_string_lossy() == *s) {
            continue;
        }
        let target = dst.join(&name);
        let ty = entry.file_type()?;
        if ty.is_dir() {
            copy_tree_skip(&entry.path(), &target, skip)?;
        } else if ty.is_symlink() {
            // Windows blocks symlink creation without elevation; follow it
            // and copy the real content instead.
            if let Ok(real) = fs::canonicalize(entry.path()) {
                if real.is_dir() {
                    copy_tree_skip(&real, &target, skip)?;
                } else {
                    fs::copy(&real, &target)?;
                }
            }
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}
