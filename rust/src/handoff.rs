use crate::paths;
use anyhow::{bail, Result};
use std::path::PathBuf;

fn find_session(profile_dir: &std::path::Path, session_id: &str) -> Option<PathBuf> {
    let root = profile_dir.join("projects");
    if !root.exists() {
        return None;
    }
    let target_name = format!("{session_id}.jsonl");
    let mut queue = vec![root];
    while let Some(current) = queue.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                queue.push(path);
            } else if entry.file_name().to_string_lossy() == target_name {
                return Some(path);
            }
        }
    }
    None
}

pub fn copy_session(from: &str, to: &str, session_id: &str) -> Result<(PathBuf, PathBuf)> {
    let from_dir = paths::profile_dir(from);
    let source = find_session(&from_dir, session_id).ok_or_else(|| anyhow::anyhow!("Session {session_id} was not found in profile '{from}'."))?;
    let relative = source.strip_prefix(&from_dir)?;
    let target = paths::profile_dir(to).join(relative);
    if target.exists() {
        bail!("Target session already exists: {}", target.display());
    }
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::copy(&source, &target)?;
    Ok((source, target))
}
