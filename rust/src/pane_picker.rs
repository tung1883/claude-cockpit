//! The Account | Folder | Session columns of the "add a pane" picker: where the
//! cursor is, what Enter on it means, and how a column is drawn. The queue,
//! worktrees and spawning stay in `panels.rs`.

use crate::session::SessionGroup;
use crate::{dirpick, layout, sessions, ui};
use std::path::{Path, PathBuf};

/// Narrowest space the three columns are drawn side by side in; below it only
/// the active one shows.
pub const MIN_COLUMNS_W: usize = 90;
const GAP: &str = " │ ";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Column {
    Account,
    Folder,
    Session,
}

/// One row of the folder column.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FolderRow {
    /// A folder to start in. `group` indexes the account's sessions there.
    Folder { path: PathBuf, current: bool, group: Option<usize> },
    ChoosePath,
    NewWorktree,
}

/// The current folder, then the account's other project folders (newest
/// first, as given), then the two ways to name another.
pub fn folder_rows(cwd: &Path, groups: &[SessionGroup]) -> Vec<FolderRow> {
    let here = sessions::norm_path(&cwd.to_string_lossy());
    let is_here = |g: &SessionGroup| sessions::same_path(&sessions::norm_path(&g.folder), &here);
    let mut rows = vec![FolderRow::Folder { path: cwd.to_path_buf(), current: true, group: groups.iter().position(is_here) }];
    for (i, g) in groups.iter().enumerate() {
        // Sessions with no recorded folder are only reachable by search.
        if !is_here(g) && g.folder != "(unknown project)" {
            rows.push(FolderRow::Folder { path: PathBuf::from(g.folder.replace('/', std::path::MAIN_SEPARATOR_STR)), current: false, group: Some(i) });
        }
    }
    rows.extend([FolderRow::ChoosePath, FolderRow::NewWorktree]);
    rows
}

/// How many rows the session column has for a folder row: "+ New session" and
/// the sessions there. None for rows without sessions.
pub fn session_count(row: &FolderRow, groups: &[SessionGroup]) -> usize {
    match row {
        FolderRow::Folder { group, .. } => 1 + group.and_then(|g| groups.get(g)).map_or(0, |g| g.sessions.len()),
        _ => 0,
    }
}

/// What Enter means where the cursor is.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Choice {
    /// A new session in this folder.
    Fresh(PathBuf),
    /// Session `session` of folder group `group`, listed under `folder`.
    Resume { folder: PathBuf, group: usize, session: usize },
    ChoosePath,
    NewWorktree,
}

#[derive(Clone, Copy, Debug)]
pub struct Cursor {
    pub column: Column,
    pub account: usize,
    pub folder: usize,
    pub session: usize,
    /// On the "Start" row above the columns.
    pub on_start: bool,
}

impl Default for Cursor {
    fn default() -> Self {
        Cursor { column: Column::Account, account: 0, folder: 0, session: 0, on_start: false }
    }
}

impl Cursor {
    /// Moves up or down in the active column of `len` rows, wrapping. With a
    /// Start row, it sits above the account column.
    pub fn vertical(&mut self, down: bool, len: usize, has_start: bool) {
        if self.on_start {
            self.on_start = false;
            self.account = if down { 0 } else { len.saturating_sub(1) };
            return;
        }
        let len = len.max(1);
        let at = match self.column {
            Column::Account => self.account,
            Column::Folder => self.folder,
            Column::Session => self.session,
        };
        if has_start && !down && at == 0 && self.column == Column::Account {
            self.on_start = true;
            return;
        }
        let next = if down { (at + 1) % len } else { (at + len - 1) % len };
        self.set(next);
    }

    /// Puts the active column's cursor on `row`. Rows to the right start over.
    pub fn set(&mut self, row: usize) {
        match self.column {
            Column::Account => {
                if row != self.account {
                    (self.folder, self.session) = (0, 0);
                }
                self.account = row;
            }
            Column::Folder => {
                if row != self.folder {
                    self.session = 0;
                }
                self.folder = row;
            }
            Column::Session => self.session = row,
        }
    }

    /// One column right; `sessions` is the session column's row count.
    pub fn right(&mut self, sessions: usize) {
        if self.on_start {
            return;
        }
        match self.column {
            Column::Account => self.column = Column::Folder,
            Column::Folder if sessions > 0 => self.column = Column::Session,
            _ => {}
        }
    }

    /// One column left; false when already on the first one.
    pub fn left(&mut self) -> bool {
        self.column = match self.column {
            Column::Account => return false,
            Column::Folder => Column::Account,
            Column::Session => Column::Folder,
        };
        true
    }

    pub fn choice(&self, rows: &[FolderRow], cwd: &Path) -> Choice {
        if self.column == Column::Account {
            return Choice::Fresh(cwd.to_path_buf());
        }
        match rows.get(self.folder) {
            Some(FolderRow::Folder { path, group, .. }) => match group {
                Some(group) if self.column == Column::Session && self.session > 0 => {
                    Choice::Resume { folder: path.clone(), group: *group, session: self.session - 1 }
                }
                _ => Choice::Fresh(path.clone()),
            },
            Some(FolderRow::ChoosePath) => Choice::ChoosePath,
            Some(FolderRow::NewWorktree) => Choice::NewWorktree,
            None => Choice::Fresh(cwd.to_path_buf()),
        }
    }
}

/// Widths of the three columns in `total` terminal columns.
pub fn widths(total: usize, longest_account: usize) -> [usize; 3] {
    let account = (longest_account + 8).clamp(16, 26);
    let rest = total.saturating_sub(account + 2 * GAP.chars().count());
    let folder = (rest * 2 / 5).max(20);
    [account, folder, rest.saturating_sub(folder)]
}

/// `text` cut to `width`, keeping its start.
pub fn clip(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(width.saturating_sub(1)).collect::<String>())
    }
}

/// `text` cut to `width`, keeping its end: for paths.
pub fn clip_left(text: &str, width: usize) -> String {
    let n = text.chars().count();
    if n <= width {
        text.to_string()
    } else {
        format!("…{}", text.chars().skip(n - width.saturating_sub(1)).collect::<String>())
    }
}

/// One column: its title and the rows around `selected` that fit in `height`
/// lines, each exactly `width` wide. `label` is only asked for visible rows; it
/// gets the room it has.
pub fn column(title: &str, count: usize, selected: usize, active: bool, width: usize, height: usize, mut label: impl FnMut(usize, usize) -> String) -> Vec<String> {
    let head = clip(title, width);
    let mut lines = vec![layout::pad_to(&if active { ui::color::orange_bold(&head) } else { ui::color::dim(&head) }, width)];
    let room = height.max(1);
    for i in dirpick::window(count, selected, room) {
        let text = label(i, width.saturating_sub(2));
        let line = if i != selected {
            format!("  {}", if active { text } else { ui::color::dim(&text) })
        } else if active {
            format!("{} {}", ui::color::orange(ui::glyph::pointer()), ui::color::orange_bold(&text))
        } else {
            // Where the other columns are, kept visible.
            format!("{} {text}", ui::color::dim(ui::glyph::pointer()))
        };
        lines.push(layout::pad_to(&line, width));
    }
    lines.resize(room + 1, " ".repeat(width));
    lines
}

/// Columns side by side.
pub fn join(columns: &[Vec<String>]) -> Vec<String> {
    let height = columns.iter().map(Vec::len).max().unwrap_or(0);
    (0..height)
        .map(|row| columns.iter().filter_map(|c| c.get(row).map(String::as_str)).collect::<Vec<_>>().join(GAP))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionFile;

    fn group(folder: &str, sessions: usize) -> SessionGroup {
        let file = |i: usize| SessionFile { id: format!("s{i}"), file: PathBuf::new(), modified: std::time::SystemTime::UNIX_EPOCH };
        SessionGroup { folder: folder.into(), sessions: (0..sessions).map(file).collect() }
    }

    #[test]
    fn the_current_folder_comes_first_and_is_not_repeated() {
        let groups = [group("C:/other", 1), group("c:/Here", 2), group("(unknown project)", 1)];
        let rows = folder_rows(Path::new("C:/here"), &groups);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0], FolderRow::Folder { path: "C:/here".into(), current: true, group: Some(1) });
        assert!(matches!(rows[1], FolderRow::Folder { current: false, group: Some(0), .. }));
        assert_eq!(session_count(&rows[0], &groups), 3);
        assert_eq!(session_count(&rows[2], &groups), 0);
    }

    #[test]
    fn enter_means_what_the_column_shows() {
        let groups = [group("C:/here", 2)];
        let cwd = Path::new("C:/here");
        let rows = folder_rows(cwd, &groups);
        let mut c = Cursor::default();
        assert_eq!(c.choice(&rows, cwd), Choice::Fresh(cwd.into()));
        c.right(3);
        assert_eq!(c.choice(&rows, cwd), Choice::Fresh(cwd.into()));
        c.right(3);
        assert_eq!(c.choice(&rows, cwd), Choice::Fresh(cwd.into()), "+ New session");
        c.vertical(true, 3, false);
        c.vertical(true, 3, false);
        assert_eq!(c.choice(&rows, cwd), Choice::Resume { folder: cwd.into(), group: 0, session: 1 });
        c.left();
        c.vertical(true, rows.len(), false);
        assert_eq!((c.choice(&rows, cwd), c.session), (Choice::ChoosePath, 0));
        c.right(0);
        assert_eq!(c.column, Column::Folder, "no sessions to move into");
        assert!(c.left() && !c.left());
    }

    #[test]
    fn the_start_row_sits_above_the_accounts() {
        let mut c = Cursor { folder: 2, ..Default::default() };
        c.vertical(false, 3, true);
        assert!(c.on_start);
        c.vertical(false, 3, true);
        assert_eq!((c.on_start, c.account, c.folder), (false, 2, 2));
        c.vertical(true, 3, true);
        assert_eq!((c.account, c.folder), (0, 0), "another account starts at its first folder");
        c.vertical(false, 3, false);
        assert_eq!((c.on_start, c.account), (false, 2));
    }

    #[test]
    fn columns_keep_their_width_and_only_ask_for_visible_rows() {
        let mut asked = Vec::new();
        let lines = column("Session", 100, 50, true, 20, 5, |i, room| {
            asked.push(i);
            clip(&format!("a very long session title {i}"), room)
        });
        assert_eq!(asked, vec![48, 49, 50, 51, 52]);
        assert!(lines.iter().all(|l| layout::vis_len(l) == 20), "{lines:?}");
        assert_eq!(clip_left("C:/a/b/rust/src", 8), "…ust/src");
        let [a, f, s] = widths(120, 5);
        assert_eq!(a + f + s + 6, 120);
    }
}
