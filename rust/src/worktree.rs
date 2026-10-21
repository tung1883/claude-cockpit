//! Git worktrees for panes: a second checkout of the same repo (own folder,
//! own branch, shared history) so panes working on one repo don't share files.
//!
//! Layout and naming follow Claude Code's own `--worktree` defaults, so
//! `claude rm` and friends recognise what cpit creates: the folder is
//! `<repo>/.claude/worktrees/<name>` and the branch `worktree-<name>`.
//! Everything shells out to `git`.

use anyhow::{bail, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const BRANCH_PREFIX: &str = "worktree-";
const EXCLUDE_LINE: &str = ".claude/worktrees/";
const MAX_NAME: usize = 48;

#[derive(Debug)]
pub struct Created {
    pub path: PathBuf,
    pub branch: String,
}

/// What would be lost by removing a worktree.
#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
pub struct Status {
    /// Modified, staged or untracked files.
    pub changed: usize,
    /// Commits on the worktree's branch that its base does not have.
    pub ahead: usize,
}

fn git(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd
}

/// stdout of a successful git command, trimmed.
fn read(mut cmd: Command) -> Option<String> {
    let out = cmd.output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// git prints forward slashes even on Windows; rebuild the path with the
/// platform's separators so it joins and displays consistently.
fn native(p: &str) -> PathBuf {
    Path::new(p).components().collect()
}

/// The main checkout of the repo `dir` belongs to (also when `dir` is inside
/// one of its worktrees), or `None` outside a git repo.
pub fn repo_root(dir: &Path) -> Option<PathBuf> {
    let mut common = git(dir);
    common.args(["rev-parse", "--path-format=absolute", "--git-common-dir"]);
    if let Some(c) = read(common) {
        let c = native(&c);
        if c.file_name().is_some_and(|n| n == ".git") {
            return c.parent().map(Path::to_path_buf);
        }
    }
    let mut top = git(dir);
    top.args(["rev-parse", "--show-toplevel"]);
    read(top).map(|t| native(&t))
}

/// The branch new worktrees start from: the remote's default branch, or the
/// local HEAD when there is no remote (or it has no default set).
pub fn default_base(repo: &Path) -> String {
    let mut sym = git(repo);
    sym.args(["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]);
    if let Some(name) = read(sym) {
        let mut verify = git(repo);
        verify.args(["rev-parse", "--verify", "--quiet", &name]);
        if read(verify).is_some() {
            return name;
        }
    }
    "HEAD".to_string()
}

/// The checked-out branch of `repo`, or `HEAD` when detached.
pub fn current_branch(repo: &Path) -> String {
    let mut cmd = git(repo);
    cmd.args(["rev-parse", "--abbrev-ref", "HEAD"]);
    read(cmd).unwrap_or_else(|| "HEAD".to_string())
}

/// Letters, digits, `.`, `_` and `-`; starts with a letter or digit; at most
/// 48 characters. Keeps the name safe as both a folder and a branch suffix.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && name.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !name.ends_with('.')
        && !name.contains("..")
}

pub fn branch_name(name: &str) -> String {
    format!("{BRANCH_PREFIX}{name}")
}

pub fn worktree_path(repo: &Path, name: &str) -> PathBuf {
    repo.join(".claude").join("worktrees").join(name)
}

/// Names of the worktrees cpit/Claude already made under `<repo>/.claude/worktrees`.
pub fn existing_names(repo: &Path) -> Vec<String> {
    let dir = repo.join(".claude").join("worktrees");
    std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok()).filter(|e| e.path().is_dir()).map(|e| e.file_name().to_string_lossy().to_string()).collect())
        .unwrap_or_default()
}

/// `pane-<n>` for the first `n` (from `start`) not already `taken`.
pub fn default_name(taken: &[String], start: usize) -> String {
    (start..).map(|n| format!("pane-{n}")).find(|c| !taken.contains(c)).unwrap_or_default()
}

/// Why `name` cannot be used for a new worktree of `repo`, if it cannot.
pub fn name_problem(repo: &Path, name: &str) -> Option<String> {
    if !valid_name(name) {
        return Some("use letters, digits, . _ - (start with a letter or digit, up to 48)".to_string());
    }
    if worktree_path(repo, name).exists() {
        return Some("a worktree with that name already exists".to_string());
    }
    let mut cmd = git(repo);
    cmd.args(["rev-parse", "--verify", "--quiet", &format!("refs/heads/{}", branch_name(name))]);
    read(cmd).is_some().then(|| format!("branch {} already exists", branch_name(name)))
}

/// Creates `<repo>/.claude/worktrees/<name>` on a new branch `worktree-<name>`
/// started from `base`.
pub fn create(repo: &Path, name: &str, base: &str) -> Result<Created> {
    if let Some(p) = name_problem(repo, name) {
        bail!("{p}");
    }
    ensure_excluded(repo);
    let path = worktree_path(repo, name);
    let branch = branch_name(name);
    let out = git(repo).args(["worktree", "add", "-b", &branch]).arg(&path).arg(base).output()?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(Created { path, branch })
}

/// Keeps `.claude/worktrees/` out of `git status` through the repo's local
/// exclude file, so no tracked file (like `.gitignore`) is touched. Best effort.
fn ensure_excluded(repo: &Path) {
    let git_dir = repo.join(".git");
    if !git_dir.is_dir() {
        return;
    }
    let file = git_dir.join("info").join("exclude");
    let current = std::fs::read_to_string(&file).unwrap_or_default();
    if current.lines().any(|l| l.trim() == EXCLUDE_LINE) {
        return;
    }
    let _ = std::fs::create_dir_all(git_dir.join("info"));
    let sep = if current.is_empty() || current.ends_with('\n') { "" } else { "\n" };
    let _ = std::fs::write(&file, format!("{current}{sep}{EXCLUDE_LINE}\n"));
}

/// Uncommitted files in the worktree and commits not in `base`.
pub fn status(path: &Path, base: &str) -> Status {
    let mut changes = git(path);
    changes.args(["status", "--porcelain"]);
    let changed = read(changes).map_or(0, |s| s.lines().filter(|l| !l.trim().is_empty()).count());
    let mut count = git(path);
    count.args(["rev-list", "--count", &format!("{base}..HEAD")]);
    let ahead = read(count).and_then(|s| s.parse().ok()).unwrap_or(0);
    Status { changed, ahead }
}

/// Deletes the worktree's folder; the branch and its commits stay. Git itself
/// refuses when the worktree has modified or untracked files.
pub fn remove(repo: &Path, path: &Path) -> Result<()> {
    let out = git(repo).args(["worktree", "remove"]).arg(path).output()?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn norm(p: &Path) -> String {
        p.to_string_lossy().replace('\\', "/").to_lowercase()
    }

    /// A throwaway repo with one commit on `main`, deleted on drop.
    struct Repo(PathBuf);
    impl Repo {
        fn new() -> Repo {
            static N: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!("cpit-wt-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
            std::fs::create_dir_all(&dir).unwrap();
            let r = Repo(dir);
            r.git(&["init", "-q", "-b", "main"]);
            std::fs::write(r.0.join("a.txt"), "a\n").unwrap();
            r.git(&["add", "."]);
            r.git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "first"]);
            r
        }
        fn git(&self, args: &[&str]) {
            let ok = git(&self.0).args(args).output().unwrap().status.success();
            assert!(ok, "git {args:?} failed");
        }
    }
    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn names_are_validated() {
        for ok in ["fix-login", "pane-2", "a", "v1.2_x", "A9"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["", "-x", ".x", "a b", "a/b", "a\\b", "x..y", "end.", "é", &"a".repeat(49)] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn naming_helpers() {
        assert_eq!(branch_name("fix"), "worktree-fix");
        assert_eq!(norm(&worktree_path(Path::new("C:/r"), "fix")), "c:/r/.claude/worktrees/fix");
        assert_eq!(default_name(&[], 2), "pane-2");
        assert_eq!(default_name(&["pane-2".into(), "pane-3".into()], 2), "pane-4");
    }

    #[test]
    fn create_makes_folder_branch_and_local_exclude() {
        let r = Repo::new();
        assert_eq!(default_base(&r.0), "HEAD"); // no remote
        assert_eq!(current_branch(&r.0), "main");
        let made = create(&r.0, "one", "HEAD").unwrap();
        assert!(made.path.join("a.txt").exists());
        assert_eq!(made.branch, "worktree-one");
        assert_eq!(current_branch(&made.path), "worktree-one");
        create(&r.0, "two", "HEAD").unwrap();
        let exclude = std::fs::read_to_string(r.0.join(".git/info/exclude")).unwrap();
        assert_eq!(exclude.lines().filter(|l| l.trim() == EXCLUDE_LINE).count(), 1);
        // The worktrees do not show up as untracked files in the main checkout.
        assert_eq!(status(&r.0, "HEAD").changed, 0);
    }

    #[test]
    fn taken_and_invalid_names_are_rejected() {
        let r = Repo::new();
        create(&r.0, "dup", "HEAD").unwrap();
        assert!(create(&r.0, "dup", "HEAD").unwrap_err().to_string().contains("already exists"));
        r.git(&["branch", "worktree-brn"]);
        assert!(name_problem(&r.0, "brn").unwrap().contains("branch"));
        assert!(name_problem(&r.0, "bad name").is_some());
        assert!(name_problem(&r.0, "fresh").is_none());
    }

    #[test]
    fn status_counts_changes_and_commits() {
        let r = Repo::new();
        let w = create(&r.0, "st", "HEAD").unwrap();
        assert_eq!(status(&w.path, "main"), Status { changed: 0, ahead: 0 });
        std::fs::write(w.path.join("new.txt"), "x").unwrap();
        std::fs::write(w.path.join("a.txt"), "changed\n").unwrap();
        assert_eq!(status(&w.path, "main").changed, 2);
        git(&w.path).args(["add", "."]).output().unwrap();
        git(&w.path).args(["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "w"]).output().unwrap();
        assert_eq!(status(&w.path, "main"), Status { changed: 0, ahead: 1 });
    }

    #[test]
    fn remove_deletes_the_folder_but_keeps_the_branch_and_refuses_dirty_ones() {
        let r = Repo::new();
        let w = create(&r.0, "rm", "HEAD").unwrap();
        std::fs::write(w.path.join("scratch.txt"), "x").unwrap();
        assert!(remove(&r.0, &w.path).is_err());
        assert!(w.path.exists());
        std::fs::remove_file(w.path.join("scratch.txt")).unwrap();
        remove(&r.0, &w.path).unwrap();
        assert!(!w.path.exists());
        assert!(name_problem(&r.0, "rm").unwrap().contains("branch")); // branch kept
    }

    #[test]
    fn repo_root_resolves_from_inside_a_worktree() {
        let r = Repo::new();
        let w = create(&r.0, "rt", "HEAD").unwrap();
        assert_eq!(norm(&repo_root(&r.0).unwrap()), norm(&r.0));
        assert_eq!(norm(&repo_root(&w.path).unwrap()), norm(&r.0));
        assert!(repo_root(&std::env::temp_dir().join("definitely-not-a-repo-xyz")).is_none());
    }
}
