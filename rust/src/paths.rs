use std::path::PathBuf;

pub fn root() -> PathBuf {
    dirs::home_dir()
        .expect("home directory not found")
        .join(".claude-multi-cockpit")
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
