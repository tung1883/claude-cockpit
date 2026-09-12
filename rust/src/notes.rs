// Per-project TODO.md / PLAN.md, auto-created and .gitignore'd, mirrored into
// one global master TODO.md / PLAN.md so every project's notes show up in one
// place. Sync is bidirectional and live while a cockpit-launched Claude
// session is running.
//
// Ported from src/notes.js. One deliberate simplification: the JS version
// used per-file fs.watchFile + a 300ms debounce; here a background thread
// just re-reconciles both files every 500ms while a session is open. Same
// observable behavior (edits propagate live, no feedback loops because
// reconcile is a no-op when nothing actually changed), simpler mechanism.
//
// Conflict rule: last write wins. A per-project content hash
// (notes-state.json) tells us which side actually changed since the last
// sync, so an untouched side never clobbers a genuinely edited one; if both
// changed, whichever file has the newer mtime wins.
use crate::paths;
use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use wait_timeout::ChildExt;

const TODO_NAME: &str = "TODO.md";
const PLAN_NAME: &str = "PLAN.md";
const GITIGNORE_MARKER: &str = "# ccpit";

pub fn enabled() -> bool {
    std::env::var("CLAUDE_COCKPIT_NOTES").as_deref() != Ok("0")
}

pub fn notes_dir() -> PathBuf {
    std::env::var("CLAUDE_COCKPIT_NOTES_DIR").map(PathBuf::from).unwrap_or_else(|_| paths::root())
}

pub fn master_paths() -> (PathBuf, PathBuf) {
    let dir = notes_dir();
    (dir.join(TODO_NAME), dir.join(PLAN_NAME))
}

fn state_path() -> PathBuf {
    paths::root().join("notes-state.json")
}

fn hash(content: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(content.trim().as_bytes());
    format!("{:x}", hasher.finalize())
}

fn read_safe(file: &Path) -> String {
    std::fs::read_to_string(file).unwrap_or_default()
}

fn mtime_of(file: &Path) -> std::time::SystemTime {
    std::fs::metadata(file).and_then(|m| m.modified()).unwrap_or(std::time::SystemTime::UNIX_EPOCH)
}

/// Never fails outright: a concurrently-open file (another agent, an editor,
/// an AV scanner) can make the rename fail transiently. Returns false so the
/// caller can skip updating sync state and just retry next time, rather than
/// crashing or wedging anything.
fn write_atomic(file: &Path, content: &str) -> bool {
    let Some(dir) = file.parent() else { return false };
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let tmp = file.with_extension(format!("tmp-{}", std::process::id()));
    if std::fs::write(&tmp, content).is_err() {
        return false;
    }
    std::fs::rename(&tmp, file).is_ok()
}

type SyncState = HashMap<String, HashMap<String, String>>; // project path -> { "todo"|"plan": hash }

fn read_state() -> SyncState {
    std::fs::read_to_string(state_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn write_state(state: &SyncState) {
    if let Ok(json) = serde_json::to_string_pretty(state) {
        write_atomic(&state_path(), &json);
    }
}

/// A bounded timeout matters here: this runs synchronously on every notes
/// action, and it's exactly the kind of call that can hang the whole process
/// if another git operation is holding a lock at that moment.
fn git(args: &[&str], cwd: &Path) -> String {
    let Ok(mut child) = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return String::new();
    };
    match child.wait_timeout(Duration::from_secs(3)) {
        Ok(Some(status)) if status.success() => {
            let mut out = String::new();
            if let Some(mut stdout) = child.stdout.take() {
                use std::io::Read;
                let _ = stdout.read_to_string(&mut out);
            }
            out.trim().to_string()
        }
        Ok(Some(_)) => String::new(),
        _ => {
            let _ = child.kill();
            let _ = child.wait();
            String::new()
        }
    }
}

static PROJECT_ROOT_CACHE: OnceLock<Mutex<HashMap<PathBuf, PathBuf>>> = OnceLock::new();

/// The project root is the git top-level if `cwd` is inside a repo, so one
/// TODO/PLAN pair covers the whole project instead of one per subdirectory.
/// Memoized per cwd, same reason as JS: avoid re-shelling to git constantly.
pub fn project_root(cwd: &Path) -> PathBuf {
    let cache = PROJECT_ROOT_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache.lock().unwrap();
    if let Some(root) = cache.get(cwd) {
        return root.clone();
    }
    let out = git(&["rev-parse", "--show-toplevel"], cwd);
    let root = if out.is_empty() { cwd.to_path_buf() } else { PathBuf::from(out) };
    cache.insert(cwd.to_path_buf(), root.clone());
    root
}

fn ensure_file(file: &Path, skeleton: &str) {
    if !file.exists() {
        let _ = std::fs::write(file, skeleton);
    }
}

/// Always adds the .gitignore block, git repo or not — harmless outside one,
/// and it's there ready if the directory is ever git-inited later.
fn ensure_gitignore(root: &Path) {
    let file = root.join(".gitignore");
    let existing = read_safe(&file);
    if existing.contains(GITIGNORE_MARKER) {
        return;
    }
    let block = format!("{GITIGNORE_MARKER}\n{TODO_NAME}\n{PLAN_NAME}\n");
    let sep = if !existing.is_empty() && !existing.ends_with('\n') { "\n" } else { "" };
    let new_content = if existing.is_empty() { block } else { format!("{existing}{sep}\n{block}") };
    let _ = std::fs::write(&file, new_content);
}

static TRACKED_CACHE: OnceLock<Mutex<HashMap<PathBuf, Vec<String>>>> = OnceLock::new();

/// If TODO.md/PLAN.md were already tracked by git before we got here, adding
/// them to .gitignore doesn't untrack them — that needs `git rm --cached`,
/// which rewrites history the user might not want done automatically.
/// Memoized per root, same reason as project_root.
fn already_tracked(root: &Path) -> Vec<String> {
    let cache = TRACKED_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache.lock().unwrap();
    if let Some(v) = cache.get(root) {
        return v.clone();
    }
    let tracked: Vec<String> = [TODO_NAME, PLAN_NAME]
        .into_iter()
        .filter(|name| git(&["ls-files", name], root) == *name)
        .map(String::from)
        .collect();
    cache.insert(root.to_path_buf(), tracked.clone());
    tracked
}

/// Create the project's files/.gitignore if missing. Returns any files that
/// were already git-tracked, so the caller can warn instead of silently
/// doing nothing about it.
pub fn ensure_project_notes(root: &Path) -> Vec<String> {
    if !enabled() {
        return vec![];
    }
    ensure_file(&root.join(TODO_NAME), "# TODO\n\n");
    ensure_file(&root.join(PLAN_NAME), "# PLAN\n\n");
    ensure_gitignore(root);
    already_tracked(root)
}

// --- Master file section format -------------------------------------------
// -------------- <project name> --------------
// <!-- ccpit:path=<absolute project path> -->
// <body>
//
// (sections separated by 2 blank lines)
//
// A heading is only a section boundary when a marker immediately follows it,
// so a project's own "-------------- something --------------" inside its
// body text isn't mistaken for one.

/// Every heading padded out to the same total width so they line up across
/// projects regardless of name length, instead of a fixed dash count per
/// side (which drifted visually for short vs. long names). Never shrinks
/// below a few dashes per side for a name long enough to blow the budget.
const HEADING_WIDTH: usize = 40;

fn section_heading(name: &str) -> String {
    let core = format!(" {name} ");
    let min_dashes = 3;
    let dash_total = HEADING_WIDTH.saturating_sub(core.chars().count()).max(min_dashes * 2);
    let left = dash_total / 2;
    let right = dash_total - left;
    format!("{}{core}{}", "-".repeat(left), "-".repeat(right))
}

struct Section {
    key: String,
    start: usize,
    end: usize,
    body: String,
}

fn find_sections(content: &str) -> Vec<Section> {
    static MARKER_RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = MARKER_RE.get_or_init(|| regex::Regex::new(r"<!-- ccpit:path=(.*?) -->").unwrap());
    let markers: Vec<(usize, usize, String)> =
        re.captures_iter(content).map(|c| { let m = c.get(0).unwrap(); (m.start(), m.end(), c[1].to_string()) }).collect();
    let heading_starts: Vec<usize> = markers
        .iter()
        .map(|(start, _, _)| {
            if *start < 2 {
                0
            } else {
                content[..start - 1].rfind('\n').map(|i| i + 1).unwrap_or(0)
            }
        })
        .collect();
    markers
        .iter()
        .enumerate()
        .map(|(i, (_, end, key))| {
            let section_end = if i + 1 < markers.len() { heading_starts[i + 1] } else { content.len() };
            let body_start = content[*end..].find('\n').map(|p| end + p + 1).unwrap_or(content.len());
            let body = content.get(body_start..section_end).unwrap_or("").trim_end().to_string();
            Section { key: key.clone(), start: heading_starts[i], end: section_end, body }
        })
        .collect()
}

pub fn get_section_body(content: &str, key: &str) -> Option<String> {
    find_sections(content).into_iter().find(|s| s.key == key).map(|s| s.body)
}

/// Every project that has ever synced notes, from the master TODO/PLAN
/// section markers (`<!-- ccpit:path=... -->`) — used to list every other
/// project's notes alongside the current one, not just it.
pub fn known_project_roots() -> Vec<PathBuf> {
    let (master_todo, master_plan) = master_paths();
    let mut set: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
    for path in [master_todo, master_plan] {
        for section in find_sections(&read_safe(&path)) {
            set.insert(PathBuf::from(section.key));
        }
    }
    set.into_iter().collect()
}

pub fn upsert_section(content: &str, key: &str, name: &str, body: &str) -> String {
    let block = format!("{}\n<!-- ccpit:path={key} -->\n{}\n", section_heading(name), body.trim());
    let sections = find_sections(content);
    if let Some(existing) = sections.iter().find(|s| s.key == key) {
        // Force exactly 2 blank lines on both sides of the edited block,
        // rather than trusting whatever spacing (possibly just 1 blank
        // line, from stale content) was already there before it.
        let prefix = content[..existing.start].trim_end_matches('\n');
        let head = if prefix.is_empty() { String::new() } else { format!("{prefix}\n\n\n") };
        let rest = content[existing.end..].trim_start_matches('\n');
        if rest.is_empty() {
            format!("{head}{block}")
        } else {
            format!("{head}{block}\n\n{rest}")
        }
    } else {
        let trimmed = content.trim_end();
        if trimmed.is_empty() { block } else { format!("{trimmed}\n\n\n{block}") }
    }
}

// --- Sync session -----------------------------------------------------

fn reconcile_kind(root: &Path, local_path: &Path, master_path: &Path, key: &str, kind: &str, state: &mut SyncState) {
    let local = read_safe(local_path);
    let master_content = read_safe(master_path);
    let remote = get_section_body(&master_content, key).unwrap_or_default();
    let local_hash = hash(&local);
    let remote_hash = hash(&remote);
    let name = root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let entry = state.entry(key.to_string()).or_default();
    if local_hash == remote_hash {
        entry.insert(kind.to_string(), local_hash);
        write_state(state);
        // Bodies match, but an old master file can still carry a stale
        // heading (e.g. the pre-fix asymmetric dashes) that a body-only
        // hash comparison would never catch — repair it here so it
        // self-heals the next time this project's session syncs, even
        // when nobody actually touched the note text.
        if let Some(section) = find_sections(&master_content).into_iter().find(|s| s.key == key) {
            let expected = section_heading(&name);
            if master_content[section.start..].lines().next() != Some(expected.as_str()) {
                let _ = write_atomic(master_path, &upsert_section(&master_content, key, &name, &local));
            }
        }
        return;
    }
    let last = entry.get(kind).cloned();
    let use_local = if last.as_deref() == Some(local_hash.as_str()) {
        false // local unchanged since last sync, master moved -> master wins
    } else if last.as_deref() == Some(remote_hash.as_str()) {
        true // master unchanged, local moved -> local wins
    } else if local.trim().is_empty() && !remote.trim().is_empty() {
        false
    } else if remote.trim().is_empty() && !local.trim().is_empty() {
        true
    } else {
        mtime_of(local_path) >= mtime_of(master_path)
    };
    if use_local {
        let ok = write_atomic(master_path, &upsert_section(&read_safe(master_path), key, &name, &local));
        if ok {
            state.entry(key.to_string()).or_default().insert(kind.to_string(), local_hash);
            write_state(state);
        }
    } else if std::fs::write(local_path, if remote.ends_with('\n') || remote.is_empty() { remote.clone() } else { format!("{remote}\n") }).is_ok() {
        state.entry(key.to_string()).or_default().insert(kind.to_string(), remote_hash);
        write_state(state);
    }
}

fn reconcile_project(root: &Path) {
    let (master_todo, master_plan) = master_paths();
    let mut state = read_state();
    reconcile_kind(root, &root.join(TODO_NAME), &master_todo, &root.to_string_lossy(), "todo", &mut state);
    reconcile_kind(root, &root.join(PLAN_NAME), &master_plan, &root.to_string_lossy(), "plan", &mut state);
}

/// One instance per cockpit-launched Claude session. Reconciles once
/// immediately (catches edits made while nothing was watching), then a
/// background thread re-reconciles every 500ms until `stop()`/Drop.
pub struct Session {
    pub tracked: Vec<String>,
    root: PathBuf,
    running: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Session {
    pub fn start(root: &Path) -> Session {
        if !enabled() {
            return Session { tracked: vec![], root: root.to_path_buf(), running: Arc::new(AtomicBool::new(false)), handle: None };
        }
        let tracked = ensure_project_notes(root);
        let (master_todo, master_plan) = master_paths();
        ensure_file(&master_todo, "");
        ensure_file(&master_plan, "");
        reconcile_project(root);

        let running = Arc::new(AtomicBool::new(true));
        let thread_running = running.clone();
        let thread_root = root.to_path_buf();
        let handle = std::thread::spawn(move || {
            while thread_running.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(500));
                if thread_running.load(Ordering::Relaxed) {
                    reconcile_project(&thread_root);
                }
            }
        });
        Session { tracked, root: root.to_path_buf(), running, handle: Some(handle) }
    }

    pub fn stop(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        reconcile_project(&self.root); // catch anything that landed just before stop
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.handle.is_some() {
            self.stop_inner();
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering as AtoOrd};

    static COUNTER: AtomicU32 = AtomicU32::new(0);
    fn tmp_dir(name: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, AtoOrd::Relaxed);
        let dir = std::env::temp_dir().join(format!("ccpit-notes-test-{name}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn upsert_inserts_then_replaces_without_disturbing_others() {
        let mut content = String::new();
        content = upsert_section(&content, "/a", "proj-a", "- [ ] first\n## not a boundary\nstill body");
        content = upsert_section(&content, "/b", "proj-b", "- [ ] other");
        assert_eq!(get_section_body(&content, "/a").unwrap(), "- [ ] first\n## not a boundary\nstill body");
        assert_eq!(get_section_body(&content, "/b").unwrap(), "- [ ] other");

        content = upsert_section(&content, "/a", "proj-a", "- [x] first (done)");
        assert_eq!(get_section_body(&content, "/a").unwrap(), "- [x] first (done)");
        assert_eq!(get_section_body(&content, "/b").unwrap(), "- [ ] other", "unrelated section untouched");
        assert!(get_section_body(&content, "/missing").is_none());
    }

    #[test]
    fn ensure_project_notes_creates_files_and_gitignore_idempotently() {
        let root = tmp_dir("project");
        ensure_project_notes(&root);
        assert!(std::fs::read_to_string(root.join("TODO.md")).unwrap().starts_with("# TODO"));
        assert!(std::fs::read_to_string(root.join("PLAN.md")).unwrap().starts_with("# PLAN"));
        let gitignore1 = std::fs::read_to_string(root.join(".gitignore")).unwrap();
        assert!(gitignore1.contains("TODO.md"));
        assert!(gitignore1.contains("PLAN.md"));

        std::fs::write(root.join("TODO.md"), "# TODO\n\n- keep me\n").unwrap();
        ensure_project_notes(&root); // second call must not clobber content or duplicate .gitignore
        assert!(std::fs::read_to_string(root.join("TODO.md")).unwrap().contains("keep me"));
        let gitignore2 = std::fs::read_to_string(root.join(".gitignore")).unwrap();
        assert_eq!(gitignore1, gitignore2, ".gitignore not duplicated on a second run");
    }

    #[test]
    fn session_mirrors_a_fresh_project_file_into_master() {
        std::env::set_var("CLAUDE_COCKPIT_NOTES_DIR", tmp_dir("master1"));
        let root = tmp_dir("project1");
        std::fs::write(root.join("TODO.md"), "# TODO\n\n- [ ] ship it\n").unwrap();

        let session = Session::start(&root);
        let (master_todo, _) = master_paths();
        let content = std::fs::read_to_string(&master_todo).unwrap();
        assert!(content.contains("ship it"), "master content: {content}");
        assert!(content.contains(&format!("ccpit:path={}", root.to_string_lossy())));
        session.stop();
    }

    #[test]
    fn session_pulls_a_master_side_edit_down_on_reconcile() {
        std::env::set_var("CLAUDE_COCKPIT_NOTES_DIR", tmp_dir("master2"));
        let root = tmp_dir("project2");
        std::fs::write(root.join("TODO.md"), "# TODO\n\n- [ ] original\n").unwrap();

        Session::start(&root).stop();

        let (master_todo, _) = master_paths();
        let edited = upsert_section(&read_safe(&master_todo), &root.to_string_lossy(), "project", "- [ ] edited from master");
        std::fs::write(&master_todo, edited).unwrap();

        Session::start(&root).stop(); // reconcile-on-start should pull the master's edit down
        let local = std::fs::read_to_string(root.join("TODO.md")).unwrap();
        assert!(local.contains("edited from master"), "local content: {local}");
        assert!(!local.contains("original"));
    }
}
