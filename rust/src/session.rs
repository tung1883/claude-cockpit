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

/// Locates one session's transcript by id, for callers that only have the
/// id (e.g. a `--resume <id>` launch arg) rather than an already-loaded
/// `SessionFile`.
pub fn session_file_path(profile: &str, session_id: &str) -> Option<PathBuf> {
    find_in_profile_dir(&paths::profile_dir(profile), session_id)
}

/// `<profile>/projects/<folder>/<id>.jsonl`, found by checking one path per
/// project folder instead of listing every transcript.
pub fn find_in_profile_dir(profile_dir: &std::path::Path, session_id: &str) -> Option<PathBuf> {
    let name = format!("{session_id}.jsonl");
    let root = profile_dir.join("projects");
    let direct = root.join(&name);
    if direct.is_file() {
        return Some(direct);
    }
    for entry in fs::read_dir(&root).ok()?.flatten() {
        let candidate = entry.path().join(&name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// The per-session persisted system-prompt sidecar path for a transcript —
/// same directory, same id, different extension.
pub fn system_prompt_sidecar(session_path: &std::path::Path) -> PathBuf {
    session_path.with_extension("system-prompt.md")
}

/// Deletes a session and all its associated data under the profile directory:
/// the .jsonl transcript, .system-prompt.md sidecar, file-history/<id>/,
/// session-env/<id>/, and tasks/<id>/.
pub fn delete_session(profile_dir: &std::path::Path, session: &SessionFile) -> std::io::Result<()> {
    crate::layout::invalidate_stats();
    crate::sessions::forget(&session.file);
    // `<project>/<id>/` holds this session's subagent transcripts and tool-result
    // dumps; leaving it behind keeps most of a session's disk use.
    let _ = fs::remove_dir_all(session.file.with_extension(""));
    let _ = fs::remove_file(&session.file);
    let _ = fs::remove_file(system_prompt_sidecar(&session.file));
    for subdir in &["file-history", "session-env", "tasks"] {
        let dir = profile_dir.join(subdir).join(&session.id);
        if dir.exists() {
            let _ = fs::remove_dir_all(&dir);
        }
    }
    Ok(())
}

/// Calls `f(path, modified)` for every session transcript of a profile.
///
/// Sessions live at `projects/<folder>/<id>.jsonl`, so only those two levels
/// are read. Below a session id there is a directory of that session's
/// subagent transcripts and tool-result dumps: not sessions (they can't be
/// resumed), and on a busy profile they are more than half of all `.jsonl`
/// files and most of its directories — walking into them made every screen
/// pay for hundreds of directory reads.
fn for_each_session(profile: &str, f: impl FnMut(PathBuf, SystemTime)) {
    for_each_session_in(&paths::profile_dir(profile).join("projects"), f)
}

fn for_each_session_in(root: &std::path::Path, mut f: impl FnMut(PathBuf, SystemTime)) {
    let Ok(top) = fs::read_dir(root) else { return };
    let mut visit = |entry: &fs::DirEntry| {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "jsonl") {
            let modified = entry.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
            f(path, modified);
        }
    };
    for entry in top.flatten() {
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            let Ok(children) = fs::read_dir(entry.path()) else { continue };
            for child in children.flatten() {
                if child.file_type().is_ok_and(|t| t.is_file()) {
                    visit(&child);
                }
            }
        } else if kind.is_file() {
            visit(&entry);
        }
    }
}

/// Number of real session transcripts under a `projects/` directory.
pub fn count_sessions_in(projects: &std::path::Path) -> usize {
    let mut n = 0usize;
    for_each_session_in(projects, |_, _| n += 1);
    n
}

/// Session count and newest modification time, without building (or sorting)
/// the file list — what the account header shows on every screen.
pub fn session_summary(profile: &str) -> (usize, Option<SystemTime>) {
    let mut count = 0usize;
    let mut newest: Option<SystemTime> = None;
    for_each_session(profile, |_, modified| {
        count += 1;
        if newest.map_or(true, |n| modified > n) {
            newest = Some(modified);
        }
    });
    (count, newest)
}

/// Session transcripts of a profile, newest first.
pub fn session_files(profile: &str) -> Vec<SessionFile> {
    let mut result = Vec::new();
    for_each_session(profile, |path, modified| {
        let id = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        result.push(SessionFile { id, file: path, modified });
    });
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
    let sig = std::fs::metadata(path).ok().map(|m| (modified_ms(&m), m.len()));
    let key = path.to_string_lossy().to_string();
    if let Some((ms, len)) = sig {
        if let Some(hit) = previews().lock().ok().and_then(|c| c.map.get(&key).filter(|e| e.mtime_ms == ms && e.len == len).map(|e| (e.name.clone(), e.summary.clone()))) {
            return hit;
        }
    }
    let (name, mut summary) = compute_preview(path);
    // Only the first few dozen characters are ever shown; don't keep (or write
    // to disk) a whole pasted prompt.
    if summary.chars().count() > 300 {
        summary = summary.chars().take(300).collect();
    }
    if let (Some((ms, len)), Ok(mut c)) = (sig, previews().lock()) {
        c.map.insert(key, PreviewEntry { mtime_ms: ms, len, name: name.clone(), summary: summary.clone() });
        c.dirty = true;
    }
    (name, summary)
}

fn modified_ms(m: &std::fs::Metadata) -> u64 {
    m.modified().ok().and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// What a session list row shows for each transcript — title and a one-line
/// recap — remembered on disk, keyed by the file's size and modified time, so a
/// folder with hundreds of sessions does not re-read every transcript's tail on
/// every visit. A session that was resumed (or renamed) changes size and mtime
/// and is simply read again.
#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct PreviewEntry {
    mtime_ms: u64,
    len: u64,
    name: String,
    summary: String,
}

#[derive(Default)]
struct PreviewCache {
    map: std::collections::HashMap<String, PreviewEntry>,
    dirty: bool,
}

fn previews_path() -> PathBuf {
    paths::root().join("preview-cache.json")
}

fn previews() -> &'static std::sync::Mutex<PreviewCache> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<PreviewCache>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| {
        let map = fs::read_to_string(previews_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        std::sync::Mutex::new(PreviewCache { map, dirty: false })
    })
}

/// Writes new preview entries to disk (a no-op when nothing changed). Call
/// after a batch of `session_preview` calls.
pub fn flush_preview_cache() {
    let Ok(mut c) = previews().lock() else { return };
    if !c.dirty {
        return;
    }
    // Entries for sessions that no longer exist would pile up forever.
    if c.map.len() > 3000 {
        c.map.retain(|k, _| std::path::Path::new(k).exists());
    }
    if let Ok(json) = serde_json::to_string(&c.map) {
        let path = previews_path();
        let tmp = path.with_extension("json.tmp");
        if fs::write(&tmp, json).is_ok() && fs::rename(&tmp, &path).is_ok() {
            c.dirty = false;
        }
    }
}

fn compute_preview(path: &std::path::Path) -> (String, String) {
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
            let mut custom_title = String::new();
            let mut ai_title = String::new();
            for (i, line) in text.lines().enumerate() {
                if start > 0 && i == 0 {
                    continue; // likely a partial line left over from the seek
                }
                let relevant = line.contains("\"type\":\"summary\"")
                    || line.contains("\"type\":\"compact_summary\"")
                    || line.contains("\"type\":\"user\"")
                    || line.contains("\"type\":\"custom-title\"")
                    || line.contains("\"type\":\"ai-title\"");
                if !relevant {
                    continue;
                }
                let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else { continue };
                let entry_type = entry.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if entry_type == "custom-title" {
                    if let Some(t) = entry.get("customTitle").and_then(|v| v.as_str()) {
                        custom_title = t.to_string();
                    }
                }
                if entry_type == "ai-title" {
                    if let Some(t) = entry.get("aiTitle").and_then(|v| v.as_str()) {
                        ai_title = t.to_string();
                    }
                }
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
            // The terminal title Claude Code shows: a user rename wins over
            // the auto-generated one.
            if !custom_title.is_empty() {
                session_name = custom_title;
            } else if !ai_title.is_empty() {
                session_name = ai_title;
            }
        }
    }
    (session_name, summary)
}

/// Locations (group index, session index) of the `n` most recently modified
/// sessions across all folders, newest first.
pub fn recent_sessions(groups: &[SessionGroup], n: usize) -> Vec<(usize, usize)> {
    let mut all: Vec<(usize, usize, SystemTime)> = groups
        .iter()
        .enumerate()
        .flat_map(|(gi, g)| g.sessions.iter().enumerate().map(move |(si, s)| (gi, si, s.modified)))
        .collect();
    all.sort_by(|a, b| b.2.cmp(&a.2));
    all.into_iter().take(n).map(|(gi, si, _)| (gi, si)).collect()
}

/// Just the `cwd` (or workspace.current_dir), stopping as soon as it's
/// found — cwd is set on essentially the first line of a transcript, but
/// `session_details` can't stop there since it also needs the *last*
/// recap/prompt in the file, forcing a full read. Grouping only needs the
/// folder, so this is the cheap path: on a profile with hundreds of
/// sessions, that full-file scan per session is what made the sessions
/// view slow to open.
pub fn session_cwd(path: &std::path::Path) -> String {
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
    use std::io::BufRead;
    let mut turns = Vec::new();
    let Ok(f) = std::fs::File::open(file) else { return turns };
    let mut reader = std::io::BufReader::with_capacity(1 << 20, f);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        // Most of a transcript's bytes are hook output, request blobs and tool
        // results. Messages are the only lines worth a JSON parse, and a user
        // line that is a tool result (often megabytes) carries no `text` block.
        if !(line.contains("\"type\":\"user\"") || line.contains("\"type\":\"assistant\"")) {
            continue;
        }
        if line.len() > 50_000 && line.contains("\"tool_result\"") {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(line.trim_end()) else { continue };
        let role = entry.get("type").and_then(|v| v.as_str()).unwrap_or("");
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
            turns.push(Turn { role: role.to_string(), text });
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

#[cfg(test)]
mod bench {
    use super::*;
    use std::time::Instant;

    #[test]
    fn preview_cache_follows_the_file() {
        let path = std::env::temp_dir().join(format!("cockpit-preview-test-{}.jsonl", std::process::id()));
        let line = |t: &str| format!("{{\"type\":\"user\",\"message\":{{\"content\":\"{t}\"}}}}
");
        std::fs::write(&path, line("first prompt")).unwrap();
        assert_eq!(session_preview(&path).1, "first prompt");
        assert_eq!(session_preview(&path).1, "first prompt", "second call is served from the cache");
        // a resumed session changes size/mtime and must be read again
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        std::io::Write::write_all(&mut f, line("second prompt").as_bytes()).unwrap();
        drop(f);
        assert_eq!(session_preview(&path).1, "second prompt");
        // only a short recap is kept
        std::fs::write(&path, line(&"x".repeat(5000))).unwrap();
        assert_eq!(session_preview(&path).1.chars().count(), 300);
        let _ = std::fs::remove_file(&path);
    }

    /// Timing harness for the real profiles on this machine:
    /// `cargo test --release bench_hot -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_hot_paths() {
        let names: Vec<String> = ["main", "main2", "work", "work2"].iter().map(|s| s.to_string()).collect();
        for n in &names {
            let t = Instant::now();
            let files = session_files(n);
            println!("session_files({n}): {} files in {:?}", files.len(), t.elapsed());
        }
        let t = Instant::now();
        for n in &names {
            let _ = crate::layout::account_stat(n);
        }
        println!("account_stat x4 (what every screen pays): {:?}", t.elapsed());
        for n in &names {
            let t = Instant::now();
            let g = grouped_sessions(n);
            println!("grouped_sessions({n}): {} groups in {:?}", g.len(), t.elapsed());
        }
        for n in &names {
            let t = Instant::now();
            let _ = crate::inspect::build_inspection(&paths::profile_dir(n));
            println!("build_inspection({n}) [Enter on account → overview]: {:?}", t.elapsed());
        }
        let g = grouped_sessions("work");
        let biggest = g.iter().max_by_key(|g| g.sessions.len()).unwrap();
        let t = Instant::now();
        for s in &biggest.sessions {
            let _ = session_preview(&s.file);
        }
        println!("session_preview serial x{} in '{}' (cold or warm cache): {:?}", biggest.sessions.len(), biggest.folder, t.elapsed());
        let t = Instant::now();
        for s in &biggest.sessions {
            let _ = compute_preview(&s.file);
        }
        println!("  same, always reading the files (no cache): {:?}", t.elapsed());
        flush_preview_cache();
        let mut big: Vec<&SessionFile> = g.iter().flat_map(|g| g.sessions.iter()).collect();
        big.sort_by_key(|s| std::cmp::Reverse(std::fs::metadata(&s.file).map(|m| m.len()).unwrap_or(0)));
        if let Some(s) = big.first() {
            let len = std::fs::metadata(&s.file).map(|m| m.len()).unwrap_or(0);
            let t = Instant::now();
            let turns = read_transcript(&s.file);
            println!("read_transcript largest ({} MB): {} turns in {:?}", len / 1_000_000, turns.len(), t.elapsed());
            let t = Instant::now();
            let _ = session_preview(&s.file);
            println!("session_preview largest: {:?}", t.elapsed());
        }
    }
}
