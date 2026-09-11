use crate::paths;
use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

pub struct SessionFile {
    #[allow(dead_code)]
    pub id: String,
    #[allow(dead_code)]
    pub file: PathBuf,
    pub modified: SystemTime,
}

/// Walks <profile>/projects recursively for .jsonl transcripts, newest first.
pub fn session_files(profile: &str) -> Vec<SessionFile> {
    let root = paths::profile_dir(profile).join("projects");
    let mut result = Vec::new();
    if !root.exists() {
        return result;
    }
    let mut queue = vec![root];
    while let Some(current) = queue.pop() {
        let Ok(entries) = fs::read_dir(&current) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                queue.push(path);
            } else if path.extension().is_some_and(|e| e == "jsonl") {
                let modified = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                let id = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                result.push(SessionFile { id, file: path, modified });
            }
        }
    }
    result.sort_by(|a, b| b.modified.cmp(&a.modified));
    result
}

pub struct SessionGroup {
    pub folder: String,
    pub sessions: Vec<SessionFile>,
}

/// Just the `cwd` (or workspace.current_dir), stopping as soon as it's
/// found — cwd is set on essentially the first line of a transcript, but
/// `session_details` can't stop there since it also needs the *last*
/// recap/prompt in the file, forcing a full read. Grouping only needs the
/// folder, so this is the cheap path: on a profile with hundreds of
/// sessions, that full-file scan per session is what made the sessions
/// view slow to open.
fn session_cwd(path: &std::path::Path) -> String {
    use std::io::{BufRead, BufReader};
    let Ok(file) = std::fs::File::open(path) else { return String::new() };
    for line in BufReader::new(file).lines().map_while(Result::ok).take(20) {
        if line.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
        if let Some(p) = entry.get("cwd").and_then(|v| v.as_str()).or_else(|| entry.get("workspace").and_then(|w| w.get("current_dir")).and_then(|v| v.as_str())) {
            return p.to_string();
        }
    }
    String::new()
}

/// Sessions grouped by project folder, each group's sessions newest-first.
/// Groups themselves come out ordered by their own most recent session:
/// `session_files` already returns newest-first, so a folder's position the
/// first time it's seen while scanning *is* that order — no separate sort
/// needed.
pub fn grouped_sessions(profile: &str) -> Vec<SessionGroup> {
    let mut order: Vec<String> = Vec::new();
    let mut map: std::collections::HashMap<String, Vec<SessionFile>> = std::collections::HashMap::new();
    for f in session_files(profile) {
        let raw = session_cwd(&f.file);
        let folder = if raw.is_empty() { "(unknown project)".to_string() } else { raw.replace('\\', "/") };
        map.entry(folder.clone()).or_insert_with(|| { order.push(folder.clone()); Vec::new() }).push(f);
    }
    order.into_iter().map(|folder| { let sessions = map.remove(&folder).unwrap_or_default(); SessionGroup { folder, sessions } }).collect()
}

pub struct SessionDetails {
    pub id: String,
    pub modified: SystemTime,
    pub session_name: String,
    pub recap: String,
    pub last_prompt: String,
    pub project_path: String,
}

fn get_str<'a>(v: &'a serde_json::Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| v.get(k).and_then(|x| x.as_str()))
}

/// Best-effort scan of a transcript for display metadata. A deleted or
/// partially-written session should still be listed, just with blanks.
pub fn session_details(session: &SessionFile) -> SessionDetails {
    let mut d = SessionDetails {
        id: session.id.clone(),
        modified: session.modified,
        session_name: String::new(),
        recap: String::new(),
        last_prompt: String::new(),
        project_path: String::new(),
    };
    let Ok(content) = std::fs::read_to_string(&session.file) else { return d };
    for line in content.lines() {
        if line.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if let Some(n) = get_str(&entry, &["session_name", "sessionName", "name"]) {
            d.session_name = n.to_string();
        }
        if let Some(p) = entry.get("cwd").and_then(|v| v.as_str()).or_else(|| entry.get("workspace").and_then(|w| w.get("current_dir")).and_then(|v| v.as_str())) {
            d.project_path = p.to_string();
        }
        let entry_type = entry.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if entry_type == "summary" || entry_type == "compact_summary" {
            if let Some(s) = entry.get("summary").and_then(|v| v.as_str()).or_else(|| entry.get("content").and_then(|v| v.as_str())) {
                d.recap = s.to_string();
            }
        }
        if entry_type == "last-prompt" {
            if let Some(p) = entry.get("lastPrompt").and_then(|v| v.as_str()) {
                d.last_prompt = p.to_string();
            }
        }
        if entry_type == "user" {
            if let Some(c) = entry.get("message").and_then(|m| m.get("content")).and_then(|v| v.as_str()) {
                d.last_prompt = c.to_string();
            }
        }
    }
    d
}

pub fn relative_age(date: Option<SystemTime>) -> String {
    let Some(date) = date else { return "never".to_string() };
    let seconds = SystemTime::now()
        .duration_since(date)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
        .max(0.0);
    if seconds < 60.0 {
        "just now".to_string()
    } else if seconds < 3600.0 {
        format!("{}m ago", (seconds / 60.0) as u64)
    } else if seconds < 86400.0 {
        format!("{}h ago", (seconds / 3600.0) as u64)
    } else {
        format!("{}d ago", (seconds / 86400.0) as u64)
    }
}
