//! Saved pane layouts: what was open, so it can be reopened after cpit quits,
//! crashes or the machine restarts.
//!
//! Each running split owns one `<id>.json` under `workspaces/` and holds a lock
//! on `<id>.lock` beside it. A layout whose lock is free belongs to no running
//! cpit and can be restored; so several windows never overwrite each other.
//!
//! Which session a Claude pane is in changes with `/clear` and `/resume`, so a
//! `SessionStart` hook (`cpit session-track`, see `plugin.rs`) writes it to the
//! pane's track file.

use crate::sidebar::{Mode, WorktreeInfo};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Names the track file of the pane a session runs in.
pub const TRACK_ENV: &str = "CPIT_TRACK_FILE";
/// Restorable layouts kept; older ones are deleted.
const KEEP: usize = 5;
/// Age at which a track file is taken to be left over from a killed cpit. A
/// pane removes its own when it closes.
const TRACK_MAX_AGE: Duration = Duration::from_secs(60 * 24 * 3600);

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SavedPane {
    pub profile: String,
    /// The folder the pane list groups it under: the repo for a worktree pane.
    pub project: PathBuf,
    pub shell: bool,
    pub mode: Mode,
    pub session_id: Option<String>,
    pub worktree: Option<WorktreeInfo>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Workspace {
    /// The folder cockpit ran in (where new panes and shells start).
    pub cwd: PathBuf,
    pub focus: usize,
    pub panes: Vec<SavedPane>,
}

pub fn dir() -> PathBuf {
    crate::paths::root().join("workspaces")
}

// ------------------------------------------------------------------ tracking

fn track_dir() -> PathBuf {
    crate::paths::root().join("track")
}

/// A new, unused path for a pane's track file.
pub fn new_track_path() -> PathBuf {
    track_dir().join(format!("{}.json", uuid::Uuid::new_v4()))
}

/// Deletes track files not written for `max_age`.
fn prune_tracks_in(dir: &Path, max_age: Duration) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let age = entry.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok());
        if age.is_some_and(|a| a >= max_age) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The session id in a `SessionStart` hook's input.
fn hook_session_id(input: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(input).ok()?;
    v.get("session_id").and_then(|s| s.as_str()).filter(|s| !s.is_empty()).map(str::to_string)
}

fn write_track(path: &Path, id: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    crate::sessions::write_atomic(path, &serde_json::json!({ "session_id": id }).to_string());
}

/// The session id last written to a track file.
pub fn read_track(path: &Path) -> Option<String> {
    hook_session_id(&std::fs::read_to_string(path).ok()?)
}

/// `cpit session-track`: the `SessionStart` hook. It must print nothing (its
/// output would be added to the model's context) and never fail.
pub fn track_from_stdin() {
    let Some(path) = std::env::var_os(TRACK_ENV) else { return };
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_ok() {
        if let Some(id) = hook_session_id(&input) {
            write_track(Path::new(&path), &id);
        }
    }
}

// ------------------------------------------------------------------ storage

/// Takes the lock beside a layout; None while another split holds it.
fn lock(layout: &Path) -> Option<File> {
    let file = File::options().create(true).truncate(false).write(true).open(layout.with_extension("lock")).ok()?;
    file.try_lock().ok()?;
    Some(file)
}

fn delete(layout: &Path, lock: File) {
    let _ = std::fs::remove_file(layout);
    drop(lock);
    let _ = std::fs::remove_file(layout.with_extension("lock"));
}

/// The layout of one running split.
pub struct Store {
    path: PathBuf,
    lock: Option<File>,
    written: String,
}

impl Store {
    pub fn create_in(dir: &Path) -> Option<Store> {
        std::fs::create_dir_all(dir).ok()?;
        let path = dir.join(format!("{}.json", uuid::Uuid::new_v4()));
        let lock = Some(lock(&path)?);
        Some(Store { path, lock, written: String::new() })
    }

    /// Writes the layout if it changed; one with no panes is removed. Returns
    /// whether what is on disk now matches `ws`.
    pub fn save(&mut self, ws: &Workspace) -> bool {
        if ws.panes.is_empty() {
            self.written.clear();
            return std::fs::remove_file(&self.path).is_ok() || !self.path.exists();
        }
        let Ok(json) = serde_json::to_string_pretty(ws) else { return false };
        if json != self.written {
            crate::sessions::write_atomic(&self.path, &json);
            if std::fs::read_to_string(&self.path).ok().as_deref() != Some(json.as_str()) {
                return false;
            }
            self.written = json;
        }
        true
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        // The layout stays (that is the point); a lock with no layout goes.
        drop(self.lock.take());
        if !self.path.exists() {
            let _ = std::fs::remove_file(self.path.with_extension("lock"));
        }
    }
}

/// A layout no running cpit owns.
pub struct Saved {
    pub path: PathBuf,
    pub modified: SystemTime,
    pub workspace: Workspace,
}

/// Restorable layouts, newest first.
pub fn list_in(dir: &Path) -> Vec<Saved> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<Saved> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|path| {
            let _free = lock(&path)?;
            let workspace: Workspace = serde_json::from_str(&std::fs::read_to_string(&path).ok()?).ok()?;
            let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok()?;
            (!workspace.panes.is_empty()).then_some(Saved { path, modified, workspace })
        })
        .collect();
    out.sort_by(|a, b| b.modified.cmp(&a.modified));
    out
}

/// Deletes all but the `keep` newest restorable layouts, and stray locks.
fn prune_in(dir: &Path, keep: usize) {
    for old in list_in(dir).into_iter().skip(keep) {
        if let Some(l) = lock(&old.path) {
            delete(&old.path, l);
        }
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for path in entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "lock")) {
        let layout = path.with_extension("json");
        if !layout.exists() {
            if let Some(l) = lock(&layout) {
                delete(&layout, l);
            }
        }
    }
}

pub fn list() -> Vec<Saved> {
    prune_in(&dir(), KEEP);
    prune_tracks_in(&track_dir(), TRACK_MAX_AGE);
    list_in(&dir())
}

/// A saved layout being restored: locked so no other window takes it, and
/// deleted only once its panes are saved again.
pub struct Claim {
    path: PathBuf,
    lock: File,
}

impl Claim {
    pub fn new(path: &Path) -> Option<Claim> {
        let lock = lock(path)?;
        path.exists().then(|| Claim { path: path.to_path_buf(), lock })
    }

    /// The layout has been taken over: delete it.
    pub fn finish(self) {
        delete(&self.path, self.lock);
    }
}

pub fn discard(path: &Path) {
    if let Some(claim) = Claim::new(path) {
        claim.finish();
    }
}

/// "3 panes · main: cockpit, work: site, shell"
pub fn describe(ws: &Workspace) -> String {
    let names: Vec<String> = ws
        .panes
        .iter()
        .map(|p| {
            let place = p.worktree.as_ref().map(|w| format!("wt:{}", w.name)).unwrap_or_else(|| crate::sidebar::project_name(&p.project));
            let who = if p.shell { "shell" } else { p.profile.as_str() };
            format!("{who}: {place}")
        })
        .collect();
    let n = ws.panes.len();
    format!("{n} pane{} · {}", if n == 1 { "" } else { "s" }, names.join(", "))
}

// ------------------------------------------------------------------ planning

/// How a restored pane starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Start {
    Shell,
    /// A new Claude session with this id.
    Fresh(String),
    /// Continue this session.
    Resume(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Planned {
    pub saved: SavedPane,
    /// The folder to start in.
    pub dir: PathBuf,
    pub start: Start,
}

/// What is on disk for a saved session.
pub enum Transcript {
    Missing,
    /// A file with no conversation in it: Claude cannot resume it.
    Empty,
    /// A conversation, started in this folder if that is known.
    Found(Option<PathBuf>),
}

/// What planning a restore needs to know about this machine.
pub trait Facts {
    fn profile_exists(&self, name: &str) -> bool;
    fn dir_exists(&self, path: &Path) -> bool;
    fn transcript(&self, profile: &str, id: &str) -> Transcript;
}

pub struct Disk;

impl Facts for Disk {
    fn profile_exists(&self, name: &str) -> bool {
        crate::profiles::get_profile(name).is_ok()
    }
    fn dir_exists(&self, path: &Path) -> bool {
        path.is_dir()
    }
    fn transcript(&self, profile: &str, id: &str) -> Transcript {
        use std::io::{BufRead, BufReader};
        let Some(file) = crate::session::session_file_path(profile, id) else { return Transcript::Missing };
        let said = File::open(&file).is_ok_and(|f| {
            BufReader::new(f).lines().map_while(Result::ok).any(|l| l.contains("\"type\":\"assistant\""))
        });
        if !said {
            return Transcript::Empty;
        }
        // Native separators: the folder becomes the process's working directory.
        Transcript::Found(crate::sessions::started_folder(&file).map(|f| PathBuf::from(f.replace('/', std::path::MAIN_SEPARATOR_STR))))
    }
}

fn plan_pane(p: &SavedPane, facts: &impl Facts, new_id: &mut impl FnMut() -> String) -> Result<(PathBuf, Start), String> {
    if !facts.profile_exists(&p.profile) {
        return Err(format!("account '{}' no longer exists", p.profile));
    }
    let base = p.worktree.as_ref().map(|w| &w.path).unwrap_or(&p.project);
    let present = |dir: &Path| {
        if facts.dir_exists(dir) {
            Ok(dir.to_path_buf())
        } else {
            Err(format!("folder {} is gone", dir.display()))
        }
    };
    if p.worktree.is_some() {
        present(base)?;
    }
    if p.shell {
        return Ok((present(base)?, Start::Shell));
    }
    let Some(id) = &p.session_id else { return Ok((present(base)?, Start::Fresh(new_id()))) };
    Ok(match facts.transcript(&p.profile, id) {
        Transcript::Missing => (present(base)?, Start::Fresh(id.clone())),
        Transcript::Empty => (present(base)?, Start::Fresh(new_id())),
        // Claude finds a session by the folder it started in.
        Transcript::Found(started) => (present(started.as_deref().unwrap_or(base))?, Start::Resume(id.clone())),
    })
}

/// The panes that can be reopened, the index among them to focus, and why each
/// of the others is skipped.
pub fn plan(ws: &Workspace, facts: &impl Facts, mut new_id: impl FnMut() -> String) -> (Vec<Planned>, usize, Vec<String>) {
    let (mut panes, mut skipped, mut focus) = (Vec::new(), Vec::new(), 0);
    for (i, p) in ws.panes.iter().enumerate() {
        match plan_pane(p, facts, &mut new_id) {
            Ok((dir, start)) => {
                if i == ws.focus {
                    focus = panes.len();
                }
                panes.push(Planned { saved: p.clone(), dir, start });
            }
            Err(why) => skipped.push(format!("{}: {why}", describe(&Workspace { cwd: PathBuf::new(), focus: 0, panes: vec![p.clone()] }).trim_start_matches("1 pane · "))),
        }
    }
    (panes, focus, skipped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cockpit-ws-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn pane(profile: &str, project: &str, id: Option<&str>) -> SavedPane {
        SavedPane { profile: profile.into(), project: project.into(), shell: false, mode: Mode::Small, session_id: id.map(str::to_string), worktree: None }
    }

    fn ws(panes: Vec<SavedPane>) -> Workspace {
        Workspace { cwd: "/c".into(), focus: 0, panes }
    }

    #[test]
    fn a_layout_is_restorable_only_once_its_split_is_gone() {
        let d = temp("lock");
        let mut store = Store::create_in(&d).unwrap();
        let layout = ws(vec![pane("main", "/p", Some("id"))]);
        assert!(store.save(&layout));
        assert!(list_in(&d).is_empty(), "a running split's layout is not offered");
        assert!(Claim::new(&store.path).is_none());
        drop(store);
        let saved = list_in(&d);
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].workspace, layout);

        let claim = Claim::new(&saved[0].path).unwrap();
        assert!(list_in(&d).is_empty(), "a claimed layout is not offered twice");
        drop(claim);
        assert_eq!(list_in(&d).len(), 1, "an unfinished restore keeps the layout");
        discard(&saved[0].path);
        assert!(list_in(&d).is_empty());
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_layout_follows_the_panes_and_goes_with_the_last_one() {
        let d = temp("follow");
        let mut store = Store::create_in(&d).unwrap();
        let two = ws(vec![pane("main", "/a", None), pane("main", "/b", None)]);
        assert!(store.save(&two));
        let one = ws(vec![pane("main", "/b", None)]);
        assert!(store.save(&one));
        assert!(std::fs::read_to_string(&store.path).unwrap().contains("/b"));
        assert!(!std::fs::read_to_string(&store.path).unwrap().contains("/a"));
        assert!(store.save(&ws(Vec::new())));
        assert!(!store.path.exists());
        drop(store);
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn old_layouts_and_stray_locks_are_pruned() {
        let d = temp("prune");
        for _ in 0..4 {
            let mut s = Store::create_in(&d).unwrap();
            s.save(&ws(vec![pane("main", "/p", None)]));
        }
        std::fs::write(d.join("stray.lock"), "").unwrap();
        prune_in(&d, 2);
        assert_eq!(list_in(&d).len(), 2);
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 4, "two layouts and their locks");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn track_file_holds_the_latest_session() {
        let d = temp("track");
        let file = d.join("t.json");
        assert_eq!(read_track(&file), None);
        write_track(&file, "one");
        write_track(&file, "two");
        assert_eq!(read_track(&file).as_deref(), Some("two"));
        prune_tracks_in(&d, TRACK_MAX_AGE);
        assert!(file.exists(), "a recent track file stays");
        prune_tracks_in(&d, Duration::ZERO);
        assert!(!file.exists());
        write_track(&file, "two");
        assert_eq!(hook_session_id(r#"{"session_id":"abc","source":"clear"}"#).as_deref(), Some("abc"));
        assert_eq!(hook_session_id("not json"), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    struct Fake;

    impl Facts for Fake {
        fn profile_exists(&self, name: &str) -> bool {
            name != "deleted"
        }
        fn dir_exists(&self, path: &Path) -> bool {
            !path.ends_with("gone")
        }
        fn transcript(&self, _: &str, id: &str) -> Transcript {
            match id {
                "new" => Transcript::Missing,
                "blank" => Transcript::Empty,
                "moved" => Transcript::Found(Some("/gone".into())),
                "elsewhere" => Transcript::Found(Some("/start".into())),
                _ => Transcript::Found(None),
            }
        }
    }

    #[test]
    fn planning_skips_what_cannot_come_back() {
        let tree = WorktreeInfo { name: "wt".into(), path: "/gone".into(), repo: "/r".into(), branch: "b".into(), base: "main".into() };
        let mut shell = pane("main", "/sh", None);
        shell.shell = true;
        let layout = Workspace {
            cwd: "/c".into(),
            focus: 7,
            panes: vec![
                pane("deleted", "/p", None),
                pane("main", "/p", Some("new")),
                pane("main", "/p", Some("blank")),
                pane("main", "/p", Some("moved")),
                pane("main", "/gone", Some("elsewhere")),
                SavedPane { worktree: Some(tree), ..pane("main", "/r", Some("x")) },
                pane("main", "/gone", None),
                shell,
                pane("main", "/p", Some("x")),
            ],
        };
        let (panes, focus, skipped) = plan(&layout, &Fake, || "fresh".to_string());
        let got: Vec<_> = panes.iter().map(|p| (p.dir.to_str().unwrap(), p.start.clone())).collect();
        assert_eq!(
            got,
            vec![
                ("/p", Start::Fresh("new".into())),
                ("/p", Start::Fresh("fresh".into())),
                ("/start", Start::Resume("elsewhere".into())),
                ("/sh", Start::Shell),
                ("/p", Start::Resume("x".into())),
            ]
        );
        assert_eq!(focus, 3, "the saved focus (the shell) keeps it");
        assert_eq!(skipped.len(), 4);
        assert!(skipped[0].contains("account 'deleted'"), "{}", skipped[0]);
        assert!(skipped[2].contains("wt:wt"), "{}", skipped[2]);
    }

    #[test]
    fn description_names_each_pane() {
        let mut shell = pane("main", "/x/site", None);
        shell.shell = true;
        assert_eq!(describe(&ws(vec![pane("work", "/x/app", None), shell])), "2 panes · work: app, shell: site");
    }
}
