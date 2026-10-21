// Pane list ("sidebar") and pane-state rules for the panel view.
//
// Every open pane is in one of three modes: `Small` (drawn in the grid),
// `Full` (fills the main area) or `Hidden` (still running, not drawn, shown
// only in the list). At most `cap` panes are drawn at once (`Small` + `Full`);
// showing another evicts the one that needs the user least. Everything here
// is pure — it works on `PaneMeta` through the `HasMeta` trait so it can be
// unit-tested without a PTY — and `panels.rs` wires it to the real panes.
use crate::{layout, ui::color};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Small,
    Full,
    Hidden,
}

/// What a pane is doing. Nothing detects this yet (that is a later roadmap
/// item: OSC 7501 + screen heuristics), so every pane is `Unknown` for now;
/// the variants and their handling exist so the UI and the eviction rule are
/// ready for it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[allow(dead_code)]
pub enum Status {
    Unknown,
    Idle,
    Working,
    Blocked,
    Done,
}

#[derive(Clone, Debug)]
pub struct PaneMeta {
    pub id: u64,
    pub profile: String,
    pub project: PathBuf,
    pub title: String,
    pub shell: bool,
    pub mode: Mode,
    pub status: Status,
    /// Monotonic counter bumped whenever the pane takes focus.
    pub last_focus: u64,
}

pub trait HasMeta {
    fn meta(&self) -> &PaneMeta;
    fn meta_mut(&mut self) -> &mut PaneMeta;
}

impl HasMeta for PaneMeta {
    fn meta(&self) -> &PaneMeta {
        self
    }
    fn meta_mut(&mut self) -> &mut PaneMeta {
        self
    }
}

pub fn project_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| p.display().to_string())
}

pub fn shown_count<T: HasMeta>(items: &[T]) -> usize {
    items.iter().filter(|p| p.meta().mode != Mode::Hidden).count()
}

pub fn full_index<T: HasMeta>(items: &[T]) -> Option<usize> {
    items.iter().position(|p| p.meta().mode == Mode::Full)
}

/// Indices of the panes that are drawn, in list order. A full pane is the
/// only one drawn.
pub fn drawn<T: HasMeta>(items: &[T]) -> Vec<usize> {
    if let Some(f) = full_index(items) {
        return vec![f];
    }
    items.iter().enumerate().filter(|(_, p)| p.meta().mode == Mode::Small).map(|(i, _)| i).collect()
}

fn next_clock<T: HasMeta>(items: &[T]) -> u64 {
    items.iter().map(|p| p.meta().last_focus).max().unwrap_or(0) + 1
}

pub fn next_id<T: HasMeta>(items: &[T]) -> u64 {
    items.iter().map(|p| p.meta().id).max().map_or(1, |m| m + 1)
}

/// Gives focus to `idx` and records it as the most recently focused.
pub fn focus_pane<T: HasMeta>(items: &mut [T], focus: &mut usize, idx: usize) {
    let c = next_clock(items);
    items[idx].meta_mut().last_focus = c;
    *focus = idx;
}

/// Most recently focused drawn pane other than `exclude`.
pub fn mru_shown<T: HasMeta>(items: &[T], exclude: usize) -> Option<usize> {
    items.iter().enumerate().filter(|(i, p)| *i != exclude && p.meta().mode != Mode::Hidden).max_by_key(|(_, p)| p.meta().last_focus).map(|(i, _)| i)
}

fn eviction_rank(s: Status) -> u8 {
    match s {
        Status::Idle | Status::Done => 0,
        Status::Working | Status::Unknown => 1,
        Status::Blocked => 2,
    }
}

/// The drawn pane to send to the list when there is no room: the one that
/// needs the user least (idle/done, then working, blocked last), ties going to
/// the least recently focused. Panes in `protect` are never picked.
pub fn pick_eviction<T: HasMeta>(items: &[T], protect: &[usize]) -> Option<usize> {
    items
        .iter()
        .enumerate()
        .filter(|(i, p)| p.meta().mode != Mode::Hidden && !protect.contains(i))
        .min_by_key(|(i, p)| (eviction_rank(p.meta().status), p.meta().last_focus, *i))
        .map(|(i, _)| i)
}

fn evict_msg(m: &PaneMeta, cap: usize) -> String {
    let why = match m.status {
        Status::Idle | Status::Done => "idle, least recent",
        Status::Working | Status::Unknown => "least recently used",
        Status::Blocked => "all busy, least recent",
    };
    format!("{cap} shown — {} ({}: {why}) moved to the list", m.title, project_name(&m.project))
}

/// Changes `idx`'s mode while keeping the invariants: at most one `Full`, at
/// most `cap` drawn panes (evicting via `pick_eviction`), the last drawn pane
/// is never hidden, and focus never rests on a hidden pane. Returns a message
/// worth showing the user, if any.
pub fn set_mode<T: HasMeta>(items: &mut [T], focus: &mut usize, idx: usize, mode: Mode, cap: usize) -> Option<String> {
    let cur = items[idx].meta().mode;
    if cur == mode {
        return None;
    }
    match mode {
        Mode::Hidden => {
            if shown_count(items) <= 1 {
                return Some("can't hide the last shown pane".to_string());
            }
            items[idx].meta_mut().mode = Mode::Hidden;
            if *focus == idx {
                if let Some(next) = mru_shown(items, idx) {
                    *focus = next;
                }
            }
            None
        }
        Mode::Small | Mode::Full => {
            let mut toast = None;
            if cur == Mode::Hidden && shown_count(items) >= cap.max(1) {
                // Prefer to keep the focused pane; if it is the only candidate
                // (cap of 1) it has to go.
                let victim = pick_eviction(items, &[idx, *focus]).or_else(|| pick_eviction(items, &[idx]));
                match victim {
                    Some(v) => {
                        items[v].meta_mut().mode = Mode::Hidden;
                        toast = Some(evict_msg(items[v].meta(), cap));
                        if *focus == v {
                            *focus = idx;
                        }
                    }
                    None => return Some("no room for another pane".to_string()),
                }
            }
            if mode == Mode::Full {
                for (i, it) in items.iter_mut().enumerate() {
                    if i != idx && it.meta().mode == Mode::Full {
                        it.meta_mut().mode = Mode::Small;
                    }
                }
            }
            items[idx].meta_mut().mode = mode;
            toast
        }
    }
}

/// `f` / `F10`: full ↔ small. A hidden pane goes straight to full.
pub fn toggle_full<T: HasMeta>(items: &mut [T], focus: &mut usize, idx: usize, cap: usize) -> Option<String> {
    let to = if items[idx].meta().mode == Mode::Full { Mode::Small } else { Mode::Full };
    set_mode(items, focus, idx, to, cap)
}

/// Brings the drawn count down to `cap` (the terminal shrank) and makes sure
/// at least one pane is drawn.
pub fn settle<T: HasMeta>(items: &mut [T], focus: &mut usize, cap: usize) {
    while shown_count(items) > cap.max(1) {
        match pick_eviction(items, &[*focus]) {
            Some(v) => items[v].meta_mut().mode = Mode::Hidden,
            None => break,
        }
    }
    if !items.is_empty() && shown_count(items) == 0 {
        let i = (0..items.len()).max_by_key(|&i| items[i].meta().last_focus).unwrap_or(0);
        items[i].meta_mut().mode = Mode::Small;
        *focus = i;
    }
    if !items.is_empty() && items[*focus].meta().mode == Mode::Hidden {
        if let Some(i) = mru_shown(items, *focus) {
            *focus = i;
        }
    }
}

// ---- tree -------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum TreeRow {
    Profile { key: String, label: String, count: usize },
    Project { key: String, label: String, count: usize },
    Pane { key: String, idx: usize },
}

impl TreeRow {
    pub fn key(&self) -> &str {
        match self {
            TreeRow::Profile { key, .. } | TreeRow::Project { key, .. } | TreeRow::Pane { key, .. } => key,
        }
    }
}

pub fn pane_key(id: u64) -> String {
    format!("p:{id}")
}

/// Profile → project → pane rows in order of first appearance; folded groups
/// hide their children.
pub fn build_tree<T: HasMeta>(items: &[T], collapsed: &HashSet<String>) -> Vec<TreeRow> {
    let mut rows = Vec::new();
    let mut profiles: Vec<&str> = Vec::new();
    for it in items {
        if !profiles.contains(&it.meta().profile.as_str()) {
            profiles.push(&it.meta().profile);
        }
    }
    for pr in profiles {
        let in_prof: Vec<usize> = (0..items.len()).filter(|&i| items[i].meta().profile == pr).collect();
        let pkey = format!("r:{pr}");
        rows.push(TreeRow::Profile { key: pkey.clone(), label: pr.to_string(), count: in_prof.len() });
        if collapsed.contains(&pkey) {
            continue;
        }
        let mut projects: Vec<&Path> = Vec::new();
        for &i in &in_prof {
            if !projects.contains(&items[i].meta().project.as_path()) {
                projects.push(&items[i].meta().project);
            }
        }
        for pj in projects {
            let in_proj: Vec<usize> = in_prof.iter().copied().filter(|&i| items[i].meta().project == pj).collect();
            let jkey = format!("j:{pr}:{}", pj.display());
            rows.push(TreeRow::Project { key: jkey.clone(), label: project_name(pj), count: in_proj.len() });
            if collapsed.contains(&jkey) {
                continue;
            }
            for i in in_proj {
                rows.push(TreeRow::Pane { key: pane_key(items[i].meta().id), idx: i });
            }
        }
    }
    rows
}

// ---- rendering --------------------------------------------------------

fn fit(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n <= w {
        s.to_string()
    } else if w == 0 {
        String::new()
    } else {
        let mut t: String = s.chars().take(w - 1).collect();
        t.push('…');
        t
    }
}

pub fn status_badge(s: Status) -> Option<(&'static str, fn(&str) -> String)> {
    match s {
        Status::Unknown => None,
        Status::Idle => Some(("idle", color::dim)),
        Status::Working => Some(("working", color::cyan)),
        Status::Blocked => Some(("blocked", color::red)),
        Status::Done => Some(("done", color::green)),
    }
}

/// One neutral bullet per row: filled = drawn (orange when full), hollow =
/// in the list.
pub fn mode_glyph(m: Mode) -> String {
    match m {
        Mode::Small => color::dim("●"),
        Mode::Full => color::orange("●"),
        Mode::Hidden => color::dim("○"),
    }
}

/// The list box: `height` lines, each exactly `width` columns. `scroll` is
/// adjusted so the cursor row stays visible.
#[allow(clippy::too_many_arguments)]
pub fn render_sidebar<T: HasMeta>(
    items: &[T],
    rows: &[TreeRow],
    collapsed: &HashSet<String>,
    cursor: &str,
    scroll: &mut usize,
    focused: bool,
    focus_idx: usize,
    width: usize,
    height: usize,
) -> Vec<String> {
    let frame = |t: &str| if focused { color::orange(t) } else { color::gray(t) };
    let inner = width.saturating_sub(4);
    let title = " Panes ".to_string();
    let tl = title.chars().count().min(width.saturating_sub(4));
    let top = format!("┌─{}{}┐", fit(&title, tl), "─".repeat(width.saturating_sub(3 + tl)));
    let mut out = vec![frame(&top)];

    let view = height.saturating_sub(3).max(1);
    let ci = rows.iter().position(|r| r.key() == cursor).unwrap_or(0);
    if ci < *scroll {
        *scroll = ci;
    }
    if ci >= *scroll + view {
        *scroll = ci + 1 - view;
    }
    *scroll = (*scroll).min(rows.len().saturating_sub(view));

    for i in 0..view {
        let content = match rows.get(*scroll + i) {
            None => String::new(),
            Some(r) => {
                let on = focused && r.key() == cursor;
                let marker = if on { color::orange("❯") } else { " ".to_string() };
                let body = match r {
                    TreeRow::Profile { key, label, count } => {
                        let fold = if collapsed.contains(key) { "▸" } else { "▾" };
                        let left = fit(&format!("{fold} {label}"), inner.saturating_sub(7));
                        let right = format!("({count})");
                        let gap = inner.saturating_sub(1 + left.chars().count() + right.chars().count());
                        format!("{}{}{}", color::orange_bold(&left), " ".repeat(gap), color::dim(&right))
                    }
                    TreeRow::Project { key, label, count } => {
                        let fold = if collapsed.contains(key) { "▸" } else { "▾" };
                        let left = fit(&format!("  {fold} {label}"), inner.saturating_sub(7));
                        let right = format!("({count})");
                        let gap = inner.saturating_sub(1 + left.chars().count() + right.chars().count());
                        format!("{}{}{}", color::bold(&left), " ".repeat(gap), color::dim(&right))
                    }
                    TreeRow::Pane { idx, .. } => {
                        let m = items[*idx].meta();
                        let badge = status_badge(m.status);
                        let right_w = badge.map_or(0, |(t, _)| t.chars().count() + 1);
                        let title_w = inner.saturating_sub(1 + 4 + 2 + right_w);
                        let title = fit(&m.title, title_w);
                        let title_styled = if *idx == focus_idx {
                            color::orange_bold(&title)
                        } else if m.mode == Mode::Hidden {
                            color::dim(&title)
                        } else {
                            title.clone()
                        };
                        let used = 4 + 1 + 1 + title.chars().count();
                        let mut s = format!("    {} {}", mode_glyph(m.mode), title_styled);
                        if let Some((t, paint)) = badge {
                            let gap = inner.saturating_sub(1 + used + t.chars().count());
                            s.push_str(&" ".repeat(gap));
                            s.push_str(&paint(t));
                        }
                        s
                    }
                };
                format!("{marker}{body}")
            }
        };
        out.push(format!("{} {} {}", frame("│"), layout::pad_to(&content, inner), frame("│")));
    }

    let hint = if focused { "↑↓ move  ←→ fold  ⏎ open  f h x" } else { "F2: focus list" };
    out.push(format!("{} {} {}", frame("│"), layout::pad_to(&color::dim(&fit(hint, inner)), inner), frame("│")));
    out.push(frame(&format!("└{}┘", "─".repeat(width.saturating_sub(2)))));
    out.truncate(height.max(1));
    while out.len() < height {
        out.push(" ".repeat(width));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(id: u64, profile: &str, project: &str, mode: Mode, status: Status, last: u64) -> PaneMeta {
        PaneMeta { id, profile: profile.into(), project: PathBuf::from(project), title: format!("claude {id}"), shell: false, mode, status, last_focus: last }
    }

    fn modes(v: &[PaneMeta]) -> Vec<Mode> {
        v.iter().map(|p| p.mode).collect()
    }

    #[test]
    fn eviction_prefers_idle_or_done_then_working_then_blocked() {
        let v = vec![
            pane(1, "a", "p", Mode::Small, Status::Blocked, 1),
            pane(2, "a", "p", Mode::Small, Status::Working, 2),
            pane(3, "a", "p", Mode::Small, Status::Done, 9),
            pane(4, "a", "p", Mode::Small, Status::Idle, 8),
        ];
        // idle (last 8) beats done (last 9) on recency; both beat working/blocked.
        assert_eq!(pick_eviction(&v, &[]), Some(3));
        assert_eq!(pick_eviction(&v, &[3]), Some(2));
        assert_eq!(pick_eviction(&v, &[2, 3]), Some(1));
        assert_eq!(pick_eviction(&v, &[0, 1, 2, 3]), None);
    }

    #[test]
    fn eviction_ties_go_to_the_least_recently_focused_and_skip_hidden() {
        let v = vec![
            pane(1, "a", "p", Mode::Small, Status::Unknown, 5),
            pane(2, "a", "p", Mode::Hidden, Status::Idle, 0),
            pane(3, "a", "p", Mode::Small, Status::Unknown, 3),
        ];
        assert_eq!(pick_eviction(&v, &[]), Some(2));
    }

    #[test]
    fn showing_a_hidden_pane_at_the_cap_evicts_but_never_the_focused_one() {
        let mut v: Vec<PaneMeta> = (1..=3).map(|i| pane(i, "a", "p", Mode::Small, Status::Unknown, i)).collect();
        v.push(pane(4, "a", "p", Mode::Hidden, Status::Unknown, 0));
        let mut focus = 0usize; // the oldest, but focused
        let toast = set_mode(&mut v, &mut focus, 3, Mode::Small, 3);
        assert!(toast.unwrap().contains("moved to the list"));
        assert_eq!(modes(&v), vec![Mode::Small, Mode::Hidden, Mode::Small, Mode::Small]);
        assert_eq!(focus, 0);
        assert_eq!(shown_count(&v), 3);
    }

    #[test]
    fn at_a_cap_of_one_the_focused_pane_gives_way() {
        let mut v = vec![pane(1, "a", "p", Mode::Small, Status::Unknown, 1), pane(2, "a", "p", Mode::Hidden, Status::Unknown, 0)];
        let mut focus = 0usize;
        set_mode(&mut v, &mut focus, 1, Mode::Small, 1);
        assert_eq!(modes(&v), vec![Mode::Hidden, Mode::Small]);
        assert_eq!(focus, 1);
    }

    #[test]
    fn only_one_pane_is_full_and_toggling_returns_to_small() {
        let mut v: Vec<PaneMeta> = (1..=3).map(|i| pane(i, "a", "p", Mode::Small, Status::Unknown, i)).collect();
        let mut focus = 0usize;
        toggle_full(&mut v, &mut focus, 0, 9);
        toggle_full(&mut v, &mut focus, 2, 9);
        assert_eq!(modes(&v), vec![Mode::Small, Mode::Small, Mode::Full]);
        assert_eq!(drawn(&v), vec![2]);
        toggle_full(&mut v, &mut focus, 2, 9);
        assert_eq!(modes(&v), vec![Mode::Small, Mode::Small, Mode::Small]);
    }

    #[test]
    fn a_hidden_pane_goes_straight_to_full_and_counts_toward_the_cap() {
        let mut v = vec![
            pane(1, "a", "p", Mode::Small, Status::Unknown, 1),
            pane(2, "a", "p", Mode::Small, Status::Unknown, 2),
            pane(3, "a", "p", Mode::Hidden, Status::Unknown, 0),
        ];
        let mut focus = 1usize;
        toggle_full(&mut v, &mut focus, 2, 2);
        assert_eq!(modes(&v), vec![Mode::Hidden, Mode::Small, Mode::Full]);
        assert_eq!(shown_count(&v), 2);
    }

    #[test]
    fn the_last_drawn_pane_cannot_be_hidden_and_focus_moves_off_a_hidden_one() {
        let mut v = vec![pane(1, "a", "p", Mode::Small, Status::Unknown, 1), pane(2, "a", "p", Mode::Small, Status::Unknown, 2)];
        let mut focus = 1usize;
        assert!(set_mode(&mut v, &mut focus, 1, Mode::Hidden, 9).is_none());
        assert_eq!(focus, 0);
        assert_eq!(modes(&v), vec![Mode::Small, Mode::Hidden]);
        let msg = set_mode(&mut v, &mut focus, 0, Mode::Hidden, 9);
        assert!(msg.unwrap().contains("last shown"));
        assert_eq!(modes(&v), vec![Mode::Small, Mode::Hidden]);
    }

    #[test]
    fn settle_trims_to_the_cap_and_guarantees_a_drawn_pane() {
        let mut v: Vec<PaneMeta> = (1..=4).map(|i| pane(i, "a", "p", Mode::Small, Status::Unknown, i)).collect();
        let mut focus = 3usize;
        settle(&mut v, &mut focus, 2);
        assert_eq!(shown_count(&v), 2);
        assert_eq!(v[3].mode, Mode::Small); // focused one stays

        let mut w: Vec<PaneMeta> = (1..=2).map(|i| pane(i, "a", "p", Mode::Hidden, Status::Unknown, i)).collect();
        let mut f = 0usize;
        settle(&mut w, &mut f, 9);
        assert_eq!(shown_count(&w), 1);
        assert_eq!(f, 1); // the most recently focused came back
    }

    #[test]
    fn tree_groups_by_profile_then_project_in_first_seen_order_and_folds() {
        let v = vec![
            pane(1, "main", "C:/w/cockpit", Mode::Small, Status::Unknown, 1),
            pane(2, "work", "C:/w/api", Mode::Hidden, Status::Unknown, 1),
            pane(3, "main", "C:/w/site", Mode::Small, Status::Unknown, 1),
            pane(4, "main", "C:/w/cockpit", Mode::Hidden, Status::Unknown, 1),
        ];
        let kinds: Vec<String> = build_tree(&v, &HashSet::new())
            .iter()
            .map(|r| match r {
                TreeRow::Profile { label, count, .. } => format!("R {label} {count}"),
                TreeRow::Project { label, count, .. } => format!("J {label} {count}"),
                TreeRow::Pane { idx, .. } => format!("P {idx}"),
            })
            .collect();
        assert_eq!(kinds, vec!["R main 3", "J cockpit 2", "P 0", "P 3", "J site 1", "P 2", "R work 1", "J api 1", "P 1"]);

        let mut folded = HashSet::new();
        folded.insert("r:main".to_string());
        folded.insert("j:work:C:/w/api".to_string());
        let rows = build_tree(&v, &folded);
        assert_eq!(rows.len(), 3); // main (folded), work, api (folded)
        assert_eq!(rows[0].key(), "r:main");
    }

    #[test]
    fn rendered_sidebar_has_exact_size_and_scrolls_to_the_cursor() {
        let v: Vec<PaneMeta> = (1..=12).map(|i| pane(i, "main", "C:/w/cockpit", Mode::Small, Status::Unknown, i)).collect();
        let rows = build_tree(&v, &HashSet::new());
        let mut scroll = 0usize;
        let last = rows.last().unwrap().key().to_string();
        let out = render_sidebar(&v, &rows, &HashSet::new(), &last, &mut scroll, true, 0, 30, 10);
        assert_eq!(out.len(), 10);
        assert!(out.iter().all(|l| layout::vis_len(l) == 30), "{:?}", out.iter().map(|l| layout::vis_len(l)).collect::<Vec<_>>());
        assert!(scroll > 0, "cursor on the last row must scroll the view");
    }
}
