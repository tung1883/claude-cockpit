use std::path::PathBuf;

/// `~/.claude-cockpit`, or the pre-rename `~/.claude-multi-cockpit` while that
/// is the only one on disk (see `scripts/migrate-data-dir.ps1`).
pub fn root() -> PathBuf {
    let home = dirs::home_dir().expect("home directory not found");
    let new = home.join(".claude-cockpit");
    let old = home.join(".claude-multi-cockpit");
    if !new.exists() && old.exists() {
        old
    } else {
        new
    }
}

pub fn profiles_file() -> PathBuf {
    root().join("profiles.json")
}

pub fn profiles_dir() -> PathBuf {
    root().join("profiles")
}

pub fn profile_dir(name: &str) -> PathBuf {
    root().join("profiles").join(name)
}
