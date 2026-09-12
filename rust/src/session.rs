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

/// Bounded-cost approximation of `session_details`, for a *list preview*
/// row rather than a session you've committed to. A few transcripts in
/// this profile run 30-40MB; `session_details`'s cheap-prefilter still has
/// to `read_to_string` the whole file, and on a file that size the I/O
/// alone is the remaining cost, regardless of how little gets JSON-parsed.
/// This never reads more than a small head slice (session_name) plus a
/// bounded tail slice (recap/last prompt) — those almost always live near
/// the end of the file anyway, and a preview doesn't need perfect accuracy.
pub fn session_preview(path: &std::path::Path) -> (String, String) {
    use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};

    let mut session_name = String::new();
    if let Ok(file) = std::fs::File::open(path) {
        for line in BufReader::new(file).lines().map_while(Result::ok).take(30) {
            if !(line.contains("\"session_name\"") || line.contains("\"sessionName\"")) {
                continue;
            }
            if let Ok(entry) = serde_json::from_str::<serde_json::Value>(&line) {
                if let Some(n) = get_str(&entry, &["session_name", "sessionName"]) {
                    session_name = n.to_string();
                    break;
                }
            }
        }
    }

    const TAIL: u64 = 300_000;
    let mut summary = String::new();
    if let Ok(mut file) = std::fs::File::open(path) {
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        let start = len.saturating_sub(TAIL);
        if start > 0 {
            let _ = file.seek(SeekFrom::Start(start));
        }
        let mut buf = Vec::new();
        if file.read_to_end(&mut buf).is_ok() {
            let text = String::from_utf8_lossy(&buf);
            let mut recap = String::new();
            let mut last_prompt = String::new();
            for (i, line) in text.lines().enumerate() {
                if start > 0 && i == 0 {
                    continue; // likely a partial line left over from the seek
                }
                let relevant = line.contains("\"type\":\"summary\"") || line.contains("\"type\":\"compact_summary\"") || line.contains("\"type\":\"user\"");
                if !relevant {
                    continue;
                }
                let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else { continue };
                let entry_type = entry.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if entry_type == "summary" || entry_type == "compact_summary" {
                    if let Some(s) = entry.get("summary").and_then(|v| v.as_str()).or_else(|| entry.get("content").and_then(|v| v.as_str())) {
                        recap = s.to_string();
                    }
                }
                if entry_type == "user" {
                    if let Some(c) = entry.get("message").and_then(|m| m.get("content")).and_then(|v| v.as_str()) {
                        last_prompt = c.to_string();
                    }
                }
            }
            summary = if !recap.is_empty() { recap } else { last_prompt };
        }
    }
    (session_name, summary)
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
    let files = session_files(profile);

    // `session_cwd` is cheap per call but this is still one file open+read
    // per session — with a couple hundred sessions that serially adds up to
    // real, visible latency. The reads are independent, so spread them
    // across a small worker pool; grouping itself stays single-threaded and
    // order-preserving, using the results once every read is back.
    let mut cwds: Vec<String> = vec![String::new(); files.len()];
    let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 8);
    let chunk_size = files.len().div_ceil(workers).max(1);
    std::thread::scope(|scope| {
        let handles: Vec<_> = files
            .chunks(chunk_size)
            .enumerate()
            .map(|(chunk_idx, chunk)| {
                let base = chunk_idx * chunk_size;
                scope.spawn(move || (base, chunk.iter().map(|f| session_cwd(&f.file)).collect::<Vec<_>>()))
            })
            .collect();
        for h in handles {
            let (base, results) = h.join().unwrap();
            for (i, r) in results.into_iter().enumerate() {
                cwds[base + i] = r;
            }
        }
    });

    let mut order: Vec<String> = Vec::new();
    let mut map: std::collections::HashMap<String, Vec<SessionFile>> = std::collections::HashMap::new();
    for (f, raw) in files.into_iter().zip(cwds) {
        let folder = if raw.is_empty() { "(unknown project)".to_string() } else { raw.replace('\\', "/") };
        map.entry(folder.clone()).or_insert_with(|| { order.push(folder.clone()); Vec::new() }).push(f);
    }
    order.into_iter().map(|folder| { let sessions = map.remove(&folder).unwrap_or_default(); SessionGroup { folder, sessions } }).collect()
}

pub struct Turn {
    pub role: String, // "user" | "assistant"
    pub text: String,
}

/// Plain-text turns from a transcript, for a read-only viewer — tool calls
/// and their results are skipped (this is for reading the conversation,
/// not auditing it), keeping only actual message text.
pub fn read_transcript(file: &std::path::Path) -> Vec<Turn> {
    let mut turns = Vec::new();
    let Ok(content) = std::fs::read_to_string(file) else { return turns };
    for line in content.lines() {
        if line.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        let role = entry.get("type").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if role != "user" && role != "assistant" {
            continue;
        }
        let Some(content_val) = entry.get("message").and_then(|m| m.get("content")) else { continue };
        let text = match content_val {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(blocks) => blocks
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        let text = text.trim().to_string();
        if !text.is_empty() {
            turns.push(Turn { role, text });
        }
    }
    turns
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
///
/// Some transcripts in this profile run 20-40MB+, and the original version
/// of this scan parsed *every* line into a dynamic `Value` to check it —
/// on a file that size, most of that cost is spent fully parsing huge
/// assistant/tool-call payloads just to throw them away. Two changes make
/// this cheap regardless of file size: project_path comes from the already-
/// bounded `session_cwd` (first ~20 lines) instead of scanning to the end,
/// and every other line gets a plain substring check before it's handed to
/// serde_json — skipping JSON parsing entirely for the vast majority of
/// lines, which are assistant/tool entries this scan doesn't care about.
pub fn session_details(session: &SessionFile) -> SessionDetails {
    let mut d = SessionDetails {
        id: session.id.clone(),
        modified: session.modified,
        session_name: String::new(),
        recap: String::new(),
        last_prompt: String::new(),
        project_path: session_cwd(&session.file),
    };
    let Ok(content) = std::fs::read_to_string(&session.file) else { return d };
    for line in content.lines() {
        if line.is_empty() {
            continue;
        }
        let maybe_relevant = line.contains("\"session_name\"")
            || line.contains("\"sessionName\"")
            || line.contains("\"type\":\"summary\"")
            || line.contains("\"type\":\"compact_summary\"")
            || line.contains("\"type\":\"last-prompt\"")
            || line.contains("\"type\":\"user\"");
        if !maybe_relevant {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if let Some(n) = get_str(&entry, &["session_name", "sessionName"]) {
            d.session_name = n.to_string();
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
