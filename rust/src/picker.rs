use crate::input::read_key;
use crate::layout::{self, Stat};
use crate::profiles::Profile;
use crate::ui;
use crossterm::event::KeyCode;

pub enum Action {
    Quit,
    Open(String),
    Detail(String),
    Notes,
    Add,
    Import,
    Sync(String),
    Handoff(String),
    Delete(String),
    Refresh,
    Shell,
    Memory(String),
    Panels(String),
    ResumeSession,
    Settings(String),
}

/// Explainer shown every time someone presses "n" while Notes is off.
fn notes_onboarding(profiles: &[Profile], selected: usize, stats: &[Stat]) -> std::io::Result<bool> {
    let mut header = layout::account_lines(profiles, selected as i64, stats);
    header.push(format!("{} {}", ui::color::orange(ui::glyph::back()), ui::color::bold("Notes")));
    header.push(String::new());
    header.push(ui::color::dim("A TODO.md + PLAN.md per project. Off by default. Turn it on?"));
    header.push(String::new());
    let rows = vec![
        ScrollRow::pick("  Yes, turn on Notes", "yes"),
        ScrollRow::pick("  No, keep it off", "no"),
    ];
    match scroll_screen(&header, &rows, None, None)? {
        ListChoice::Picked(v) => Ok(v == "yes"),
        _ => Ok(false),
    }
}

/// The main account picker. One blocking `read_key()` call per loop
/// iteration — raw mode was already enabled once for the whole process by
/// `ui::Terminal::enter`, so there's no per-screen mode toggling here at all.
pub fn pick_profile(profiles: &[Profile], start: usize, has_session: bool) -> std::io::Result<Action> {
    if profiles.is_empty() {
        return Ok(Action::Add);
    }
    let mut selected = start.min(profiles.len() - 1);
    let stats: Vec<Stat> = profiles.iter().map(|p| layout::account_stat(&p.name)).collect();
    ui::hide_cursor();

    loop {
        let notes_on = crate::settings::notes_enabled();
        let mut lines = layout::account_lines(profiles, selected as i64, &stats);
        if has_session {
            lines.push(String::new());
            lines.push(format!("  {} a detached session is running", ui::color::orange(ui::glyph::spark())));
        }
        let row1: Vec<(&str, &str)> =
            vec![("↑/↓", "move"), ("→", "view"), ("n", "notes"), ("r", "refresh"), ("h", "handoff"), ("m", "memory"), ("q", "quit")];
        let mut row2: Vec<(&str, &str)> =
            vec![("a", "add"), ("i", "import"), ("d", "delete"), ("s", "sync"), ("g", "shell"), ("p", "panels"), (",", "settings")];
        if has_session {
            row2.push(("^B", "resume"));
        }
        lines.extend(layout::hint_grid(&[&row1, &row2], 16));
        ui::repaint(&lines);

        let key = read_key()?;
        let quit = (key.ctrl && key.code == KeyCode::Char('c'))
            || key.code == KeyCode::Char('q')
            || key.code == KeyCode::Esc;
        if quit {
            ui::show_cursor();
            return Ok(Action::Quit);
        }
        if has_session && key.ctrl && key.code == KeyCode::Char('b') {
            ui::show_cursor();
            return Ok(Action::ResumeSession);
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
                selected = (selected + profiles.len() - 1) % profiles.len();
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                selected = (selected + 1) % profiles.len();
            }
            KeyCode::Enter => {
                ui::show_cursor();
                return Ok(Action::Open(profiles[selected].name.clone()));
            }
            KeyCode::Right | KeyCode::Char('l') => {
                ui::show_cursor();
                return Ok(Action::Detail(profiles[selected].name.clone()));
            }
            KeyCode::Char('n') if notes_on => {
                ui::show_cursor();
                return Ok(Action::Notes);
            }
            KeyCode::Char('n') => {
                // Not enabled yet — re-explain and ask every time, rather
                // than remembering a decline, so saying "no" once doesn't
                // hide the feature forever.
                if notes_onboarding(profiles, selected, &stats)? {
                    crate::settings::set_notes_enabled(true);
                    ui::show_cursor();
                    return Ok(Action::Notes);
                }
                ui::hide_cursor();
            }
            KeyCode::Char('a') => {
                ui::show_cursor();
                return Ok(Action::Add);
            }
            KeyCode::Char('i') => {
                ui::show_cursor();
                return Ok(Action::Import);
            }
            KeyCode::Char('s') => {
                ui::show_cursor();
                return Ok(Action::Sync(profiles[selected].name.clone()));
            }
            KeyCode::Char('h') => {
                ui::show_cursor();
                return Ok(Action::Handoff(profiles[selected].name.clone()));
            }
            KeyCode::Char('d') => {
                ui::show_cursor();
                return Ok(Action::Delete(profiles[selected].name.clone()));
            }
            KeyCode::Char('r') => {
                ui::show_cursor();
                return Ok(Action::Refresh);
            }
            KeyCode::Char('g') => {
                ui::show_cursor();
                return Ok(Action::Shell);
            }
            KeyCode::Char('m') => {
                ui::show_cursor();
                return Ok(Action::Memory(profiles[selected].name.clone()));
            }
            KeyCode::Char('p') => {
                ui::show_cursor();
                return Ok(Action::Panels(profiles[selected].name.clone()));
            }
            KeyCode::Char(',') => {
                ui::show_cursor();
                return Ok(Action::Settings(profiles[selected].name.clone()));
            }
            _ => {}
        }
    }
}

/// The settings screen: a short list of cockpit-wide toggles, drawn under
/// the same account list as the profile picker (same selected account
/// highlighted) so backing out returns to exactly where you left off. ↑/↓
/// move; Enter/Space flips a plain on/off row. The Notifications row is
/// 3-state (off/toast/terminal) — ←/→ cycle it either way, Enter is the
/// same as → (so Enter still "does something" there instead of only
/// working on the boolean rows).
pub fn settings_menu(profiles: &[Profile], selected_account: i64) -> std::io::Result<()> {
    use crate::settings::NotifyMode;
    const NOTIFY_ROW: usize = 2;
    let stats: Vec<Stat> = profiles.iter().map(|p| layout::account_stat(&p.name)).collect();
    let mut selected = 0usize;
    const COUNT: usize = 3;
    ui::hide_cursor();
    loop {
        let wrapped = crate::settings::session_wrapped();
        let notes = crate::settings::notes_enabled();
        let mode = crate::settings::notify_mode();
        let mode_text = if mode == NotifyMode::Off { ui::color::dim(mode.label()) } else { ui::color::green(mode.label()).to_string() };
        let rows: [(&str, String); COUNT] = [
            (
                "Launch sessions wrapped",
                if wrapped {
                    format!("{}   (/shell, panels, Ctrl+B detach)", ui::color::green("on"))
                } else {
                    format!("{}  (native Claude — full mouse/paste fidelity)", ui::color::dim("off"))
                },
            ),
            (
                "Notes (TODO.md + PLAN.md per project)",
                if notes { ui::color::green("on").to_string() } else { ui::color::dim("off").to_string() },
            ),
            ("Notifications", format!("{}   (needs input / finished)", mode_text)),
        ];
        let mut lines = layout::account_lines(profiles, selected_account, &stats);
        lines.push(format!("{} {}", ui::color::orange(ui::glyph::back()), ui::color::bold("Settings")));
        lines.push(String::new());
        for (i, (label, value)) in rows.iter().enumerate() {
            let pointer = if i == selected { ui::color::orange(ui::glyph::pointer()) } else { " ".to_string() };
            lines.push(format!(" {pointer} {}  {value}", layout::pad_to(label, 40)));
        }
        lines.push(String::new());
        lines.push(ui::color::dim(&format!("↑/↓ move · ←/→/Enter/Space change · {}/Esc back", ui::glyph::back())));
        ui::repaint(&lines);

        let key = read_key()?;
        // Left cycles every row now instead of doubling as "back" — Esc/h/q
        // are the only way out of this screen.
        let back = matches!(key.code, KeyCode::Esc | KeyCode::Char('h') | KeyCode::Char('q')) || (key.ctrl && key.code == KeyCode::Char('c'));
        if back {
            ui::show_cursor();
            return Ok(());
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => selected = (selected + COUNT - 1) % COUNT,
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => selected = (selected + 1) % COUNT,
            KeyCode::Left if selected == NOTIFY_ROW => crate::settings::set_notify_mode(mode.cycle(false)),
            KeyCode::Right if selected == NOTIFY_ROW => crate::settings::set_notify_mode(mode.cycle(true)),
            KeyCode::Left | KeyCode::Right if selected == 0 => crate::settings::set_session_wrapped(!wrapped),
            KeyCode::Left | KeyCode::Right if selected == 1 => crate::settings::set_notes_enabled(!notes),
            KeyCode::Enter | KeyCode::Char(' ') => match selected {
                0 => crate::settings::set_session_wrapped(!wrapped),
                1 => crate::settings::set_notes_enabled(!notes),
                NOTIFY_ROW => crate::settings::set_notify_mode(mode.cycle(true)),
                _ => {}
            },
            _ => {}
        }
    }
}

pub struct ListOption {
    pub label: String,
    pub value: String,
    pub note: Option<String>,
}

pub enum ListChoice {
    Back,
    Picked(String),
    External(String),
    Command(char),
    /// A row-scoped key (see `scroll_screen_ext`'s `row_action_key`) pressed
    /// on the currently highlighted row — carries that row's value, same as
    /// `External` does for `external_key`.
    RowAction(char, String),
}

/// Arrow-key list picker: shows the account list for context, then `options`
/// with a ❯ pointer. ↑/↓ move, Enter selects, ←/Esc backs out. If
/// `external_key` is set, pressing it resolves ListChoice::External instead.
pub fn choose_from_list(
    profiles: &[Profile],
    mark: i64,
    heading: &str,
    hint: Option<&str>,
    options: &[ListOption],
    external_key: Option<char>,
) -> std::io::Result<ListChoice> {
    choose_from_list_lazy(profiles, mark, heading, hint, options, None, external_key, None)
}

/// Same as `choose_from_list`, but a row whose own `label`/`note` are cheap
/// placeholders can be filled in on demand instead of eagerly for the whole
/// list: `lazy(option)` is called at most once per row — the first time
/// that row actually scrolls into the viewport — and its `(label, note)`
/// results are cached from then on. Use this when computing a row's real
/// content is expensive per-row (e.g. reading a file) and the list can be
/// long enough that most rows are never actually looked at.
///
/// `start` re-opens on the row whose value matches it (same convention as
/// `scroll_screen`'s `start`) — pass the value you last picked so returning
/// from wherever it led keeps your place instead of resetting to the top.
pub fn choose_from_list_lazy(
    profiles: &[Profile],
    mark: i64,
    heading: &str,
    hint: Option<&str>,
    options: &[ListOption],
    start: Option<&str>,
    external_key: Option<char>,
    lazy: Option<&dyn Fn(&ListOption) -> (Option<String>, Option<String>)>,
) -> std::io::Result<ListChoice> {
    let stats: Vec<Stat> = profiles.iter().map(|p| layout::account_stat(&p.name)).collect();
    let mut selected = start.and_then(|s| options.iter().position(|o| o.value == s)).unwrap_or(0);
    let mut top = 0usize;
    // Memoized per row index — computed at most once, the first time that
    // row is actually rendered (i.e. scrolled near), not for the whole list
    // up front.
    let mut resolved: Vec<Option<(Option<String>, Option<String>)>> = vec![None; options.len()];
    ui::hide_cursor();
    loop {
        let mut lines = layout::account_lines(profiles, mark, &stats);
        lines.push(format!("{} {}", ui::color::orange(ui::glyph::back()), ui::color::bold(heading)));
        if let Some(h) = hint {
            lines.push(ui::color::dim(&format!("   {h}")));
        }
        lines.push(String::new());

        // Fixed chrome above/below the list (account block + heading/hint +
        // blank + rule + hint line + a little slack) so a long list (e.g.
        // hundreds of sessions) scrolls in place instead of overflowing the
        // terminal and letting its own scrollback fake the movement.
        let chrome = lines.len() + 4;
        let viewport = (crossterm::terminal::size().map(|(_, h)| h as usize).unwrap_or(24)).saturating_sub(chrome).max(3);
        if selected < top {
            top = selected;
        }
        if selected >= top + viewport {
            top = selected - viewport + 1;
        }
        top = top.min(options.len().saturating_sub(viewport));
        let end = (top + viewport).min(options.len());

        if top > 0 {
            lines.push(ui::color::dim(&format!("    ↑ {top} more")));
        }
        // Every label padded to the widest one so notes start on a shared
        // column, then the note itself clipped to whatever's left of the
        // terminal width — otherwise a long recap/path runs straight past
        // the divider below instead of stopping at it.
        let label_w = options.iter().map(|o| o.label.chars().count()).max().unwrap_or(0).clamp(8, 24);
        // Match ui::rule's own width — the divider below is exactly the
        // terminal width, so the budget has to use the same figure or text
        // can run past it.
        let note_budget = (ui::term_width() as isize - label_w as isize - 8).max(0) as usize;
        // Resolve only near the *selection*, not the whole visible slice —
        // a tall terminal can fit an entire long list on screen at once, at
        // which point "visible" is no different from "everything" and this
        // would be exactly as eager as computing it all up front. A small
        // radius means moving the cursor pays for a handful of rows, not
        // however many happen to fit on screen.
        if let Some(f) = lazy {
            let radius = 3usize;
            let lo = selected.saturating_sub(radius);
            let hi = (selected + radius).min(options.len().saturating_sub(1));
            for i in lo..=hi {
                if resolved[i].is_none() {
                    resolved[i] = Some(f(&options[i]));
                }
            }
        }
        for (i, opt) in options.iter().enumerate().take(end).skip(top) {
            let (label_src, note_src) = match &resolved[i] {
                Some((lbl, nt)) => (lbl.clone().unwrap_or_else(|| opt.label.clone()), nt.clone().or_else(|| opt.note.clone())),
                None => (opt.label.clone(), opt.note.clone()),
            };
            let active = i == selected;
            let marker = if active { ui::color::orange(ui::glyph::pointer()) } else { " ".to_string() };
            let label_text = layout::pad_to(&label_src, label_w);
            let label = if active { ui::color::orange_bold(&label_text) } else { label_text };
            let note = note_src.as_ref().map(|n| {
                let clipped: String = n.chars().take(note_budget).collect();
                format!("   {}", ui::color::dim(&clipped))
            }).unwrap_or_default();
            lines.push(format!(" {marker} {label}{note}"));
        }
        if end < options.len() {
            lines.push(ui::color::dim(&format!("    ↓ {} more", options.len() - end)));
        }
        lines.push(String::new());
        lines.push(ui::rule(None));
        let ext_hint = external_key.map(|k| format!("    {} external editor", ui::color::dim(&k.to_string()))).unwrap_or_default();
        lines.push(format!(
            " {} move    {} select{ext_hint}    {} back",
            ui::color::dim("↑/↓"),
            ui::color::dim("Enter"),
            ui::color::dim(&format!("{}/Esc", ui::glyph::back())),
        ));
        ui::repaint(&lines);

        let key = read_key()?;
        if key.ctrl && key.code == KeyCode::Char('c') {
            ui::show_cursor();
            std::process::exit(130);
        }
        match key.code {
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') => {
                ui::show_cursor();
                return Ok(ListChoice::Back);
            }
            KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
                selected = (selected + options.len() - 1) % options.len();
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                selected = (selected + 1) % options.len();
            }
            KeyCode::Enter => {
                ui::show_cursor();
                return Ok(ListChoice::Picked(options[selected].value.clone()));
            }
            KeyCode::Char(c) if external_key == Some(c) => {
                ui::show_cursor();
                return Ok(ListChoice::External(options[selected].value.clone()));
            }
            _ => {}
        }
    }
}

pub struct ScrollRow {
    pub text: String,
    pub selectable: bool,
    pub value: Option<String>,
}

impl ScrollRow {
    pub fn line(text: impl Into<String>) -> Self {
        ScrollRow { text: text.into(), selectable: false, value: None }
    }
    pub fn pick(text: impl Into<String>, value: impl Into<String>) -> Self {
        ScrollRow { text: text.into(), selectable: true, value: Some(value.into()) }
    }
    /// Navigable (↑/↓ stop on it, it highlights) but nothing to open - Enter
    /// is a no-op on it, same as `pick` rows with no value.
    pub fn info(text: impl Into<String>) -> Self {
        ScrollRow { text: text.into(), selectable: true, value: None }
    }
}

/// Scrollable read-only list. ↑/↓ move the highlight over selectable rows
/// (scrolling the viewport); Enter on a row with a value picks it; ←/Esc/q
/// back out. `start` re-opens on the row whose value matches it, so
/// returning from a sub-screen keeps your place. If `external_key` is set,
/// pressing it on a valued row resolves `ListChoice::External` instead.
pub fn scroll_screen(header: &[String], rows: &[ScrollRow], start: Option<&str>, external_key: Option<char>) -> std::io::Result<ListChoice> {
    scroll_screen_ext(header, rows, start, external_key, None, None)
}

/// Like `scroll_screen`, but with a second, selection-independent key (e.g.
/// "c" to close a feature) that returns `ListChoice::Command` regardless of
/// which row is highlighted, and/or a third, row-scoped key (e.g. "h" to
/// hide the highlighted row's project) that returns `ListChoice::RowAction`
/// carrying that row's value — same idea as `external_key`, just under a
/// different key/label so a screen can offer both.
pub fn scroll_screen_ext(
    header: &[String],
    rows: &[ScrollRow],
    start: Option<&str>,
    external_key: Option<char>,
    command_key: Option<(char, &str)>,
    row_action_key: Option<(char, &str)>,
) -> std::io::Result<ListChoice> {
    let selectable_idx: Vec<usize> = rows.iter().enumerate().filter(|(_, r)| r.selectable).map(|(i, _)| i).collect();
    let any_pickable = rows.iter().any(|r| r.value.is_some());
    let mut pos = start
        .and_then(|s| selectable_idx.iter().position(|&i| rows[i].value.as_deref() == Some(s)))
        .unwrap_or(0);
    let mut top = 0usize;
    let viewport = (crossterm::terminal::size().map(|(_, h)| h as usize).unwrap_or(24)).saturating_sub(header.len() + 5).max(5);
    ui::hide_cursor();

    // Which group header (if any) each selectable row belongs to, so
    // scrolling back up to a group's *later* rows (not just its first)
    // still surfaces its header instead of leaving it cut off above the
    // viewport. A header is any non-selectable row with non-empty text; an
    // empty non-selectable row (a blank spacer) ends the group.
    let mut owner: Vec<Option<usize>> = vec![None; rows.len()];
    let mut current_header: Option<usize> = None;
    for (i, row) in rows.iter().enumerate() {
        if row.selectable {
            owner[i] = current_header;
        } else {
            current_header = if row.text.trim().is_empty() { None } else { Some(i) };
        }
    }

    loop {
        let active = selectable_idx.get(pos).copied();
        if let Some(active) = active {
            if active < top {
                top = owner[active].unwrap_or(active);
            }
            if active >= top + viewport {
                top = active - viewport + 1;
            }
        }
        top = top.min(rows.len().saturating_sub(viewport));

        let mut lines = header.to_vec();
        let end = (top + viewport).min(rows.len());
        // Counts of *selectable* rows above/below the viewport — rows.len()
        // includes header/blank spacer lines too, which made "N more" count
        // rows nobody can actually scroll to (e.g. a lone trailing blank
        // line inflating it past the one real row left).
        let above = selectable_idx.iter().filter(|&&i| i < top).count();
        let below = selectable_idx.iter().filter(|&&i| i >= end).count();
        if above > 0 {
            lines.push(ui::color::dim(&format!("    ↑ {above} more")));
        }
        for (i, row) in rows.iter().enumerate().take(end).skip(top) {
            if !row.selectable {
                lines.push(row.text.clone());
                continue;
            }
            let on = Some(i) == active;
            let text = if on { ui::color::orange_bold(&row.text) } else { row.text.clone() };
            let marker = if on { ui::color::orange(ui::glyph::pointer()) } else { " ".to_string() };
            lines.push(format!(" {marker} {text}"));
        }
        if below > 0 {
            lines.push(ui::color::dim(&format!("    ↓ {below} more")));
        }
        lines.push(String::new());
        lines.push(ui::rule(None));
        let open_hint = if any_pickable { format!("    {} open", ui::color::dim("Enter")) } else { String::new() };
        let ext_hint = external_key.map(|k| format!("    {} external editor", ui::color::dim(&k.to_string()))).unwrap_or_default();
        let cmd_hint = command_key.map(|(k, label)| format!("    {} {label}", ui::color::dim(&k.to_string()))).unwrap_or_default();
        let row_hint = row_action_key
            .filter(|_| active.is_some() && rows[active.unwrap()].value.is_some())
            .map(|(k, label)| format!("    {} {label}", ui::color::dim(&k.to_string())))
            .unwrap_or_default();
        lines.push(format!(" {} move{open_hint}{ext_hint}{cmd_hint}{row_hint}    {} back", ui::color::dim("↑/↓"), ui::color::dim(&format!("{}/Esc", ui::glyph::back()))));
        ui::repaint(&lines);

        let key = read_key()?;
        if key.ctrl && key.code == KeyCode::Char('c') {
            ui::show_cursor();
            std::process::exit(130);
        }
        match key.code {
            // Checked ahead of the Esc/Left/h/q back arm so a row_action_key
            // of 'h' (hide) wins over h's usual meaning of "back" while a
            // row with a value is highlighted.
            KeyCode::Char(c) if row_action_key.map(|(k, _)| k) == Some(c) && active.is_some() && rows[active.unwrap()].value.is_some() => {
                ui::show_cursor();
                return Ok(ListChoice::RowAction(c, rows[active.unwrap()].value.clone().unwrap()));
            }
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') | KeyCode::Char('q') => {
                ui::show_cursor();
                return Ok(ListChoice::Back);
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                if let Some(i) = active {
                    if let Some(v) = &rows[i].value {
                        ui::show_cursor();
                        return Ok(ListChoice::Picked(v.clone()));
                    }
                }
                if !any_pickable {
                    ui::show_cursor();
                    return Ok(ListChoice::Back);
                }
            }
            KeyCode::Char(c) if external_key == Some(c) && active.is_some() && rows[active.unwrap()].value.is_some() => {
                ui::show_cursor();
                return Ok(ListChoice::External(rows[active.unwrap()].value.clone().unwrap()));
            }
            KeyCode::Char(c) if command_key.map(|(k, _)| k) == Some(c) => {
                ui::show_cursor();
                return Ok(ListChoice::Command(c));
            }
            KeyCode::Up | KeyCode::Char('k') if !selectable_idx.is_empty() => {
                pos = (pos + selectable_idx.len() - 1) % selectable_idx.len();
            }
            KeyCode::Down | KeyCode::Char('j') if !selectable_idx.is_empty() => {
                pos = (pos + 1) % selectable_idx.len();
            }
            KeyCode::PageUp if !selectable_idx.is_empty() => pos = pos.saturating_sub(5),
            KeyCode::PageDown if !selectable_idx.is_empty() => pos = (pos + 5).min(selectable_idx.len() - 1),
            _ => {}
        }
    }
}

pub struct MultiOption {
    pub label: String,
    pub value: String,
    pub items: Vec<String>, // non-empty = drillable with →
    pub note: String,
    pub checked: bool,
}

#[derive(Clone)]
pub struct MultiState {
    pub checked: Vec<bool>,
    pub sub: Vec<Option<Vec<String>>>, // None = "all", Some(list) = specific subset
    pub selected: usize,
}

pub enum MultiResult {
    Back,
    Go(MultiState),
    Drill(usize, MultiState),
}

/// Arrow-key checklist. ↑/↓ move, Space/Enter toggle a row, → drills into an
/// option's `items` for a per-item subset, the last row is "→ Go". `state`
/// re-enters with prior selections (used when returning from a drill).
pub fn choose_multi(
    profiles: &[Profile],
    mark: i64,
    heading: &str,
    hint: Option<&str>,
    options: &[MultiOption],
    state: Option<MultiState>,
) -> std::io::Result<MultiResult> {
    let mut checked: Vec<bool> = state.as_ref().map(|s| s.checked.clone()).unwrap_or_else(|| options.iter().map(|o| o.checked).collect());
    let mut sub: Vec<Option<Vec<String>>> = state.as_ref().map(|s| s.sub.clone()).unwrap_or_else(|| vec![None; options.len()]);
    let mut selected = state.map(|s| s.selected).unwrap_or(0);
    let rows = options.len() + 1;
    let stats: Vec<Stat> = profiles.iter().map(|p| layout::account_stat(&p.name)).collect();
    let mut top = 0usize;
    let viewport = (crossterm::terminal::size().map(|(_, h)| h as usize).unwrap_or(24))
        .saturating_sub(if profiles.is_empty() { 4 } else { profiles.len() + 6 } + 8)
        .max(4);
    ui::hide_cursor();

    loop {
        if selected < top {
            top = selected;
        }
        if selected >= top + viewport {
            top = selected - viewport + 1;
        }
        top = top.min(options.len().saturating_sub(viewport));

        let mut lines = layout::account_lines(profiles, mark, &stats);
        lines.push(format!("{} {}", ui::color::orange(ui::glyph::back()), ui::color::bold(heading)));
        if let Some(h) = hint {
            lines.push(ui::color::dim(&format!("   {h}")));
        }
        lines.push(String::new());
        let end = (top + viewport).min(options.len());
        if top > 0 {
            lines.push(ui::color::dim(&format!("    ↑ {top} more")));
        }
        for (i, opt) in options.iter().enumerate().take(end).skip(top) {
            let active = i == selected;
            let marker = if active { ui::color::orange(ui::glyph::pointer()) } else { " ".to_string() };
            let boxc = if checked[i] { ui::color::orange("[x]") } else { ui::color::dim("[ ]") };
            let label = if active { ui::color::orange_bold(&opt.label) } else { opt.label.clone() };
            let arrow = if !opt.items.is_empty() { ui::color::dim(" →") } else { String::new() };
            let note = if !opt.items.is_empty() {
                match &sub[i] {
                    Some(s) => format!("{} of {}", s.len(), opt.items.len()),
                    None if checked[i] => format!("all {}", opt.items.len()),
                    None => opt.note.clone(),
                }
            } else {
                opt.note.clone()
            };
            let note_s = if note.is_empty() { String::new() } else { format!("   {}", ui::color::dim(&note)) };
            lines.push(format!(" {marker} {boxc} {label}{arrow}{note_s}"));
        }
        if end < options.len() {
            lines.push(ui::color::dim(&format!("    ↓ {} more", options.len() - end)));
        }
        lines.push(String::new());
        let go_active = selected == options.len();
        let count = checked.iter().filter(|&&c| c).count();
        let go_label = if go_active { ui::color::orange_bold("Go") } else { "Go".to_string() };
        let go_line = if go_active {
            format!("{} {go_label}", ui::color::orange(&format!("{} →", ui::glyph::pointer())))
        } else {
            format!("   {} Go", ui::color::dim("→"))
        };
        lines.push(format!("{go_line}   {}", ui::color::dim(&format!("{count} selected"))));
        lines.push(String::new());
        lines.push(ui::rule(None));
        let drill_hint = if options.iter().any(|o| !o.items.is_empty()) { format!("    {} choose items", ui::color::dim("→")) } else { String::new() };
        lines.push(format!(
            " {} move    {} toggle    {} toggle / go{drill_hint}    {} back",
            ui::color::dim("↑/↓"),
            ui::color::dim("Space"),
            ui::color::dim("Enter"),
            ui::color::dim(&format!("{}/Esc", ui::glyph::back())),
        ));
        ui::repaint(&lines);

        let key = read_key()?;
        if key.ctrl && key.code == KeyCode::Char('c') {
            ui::show_cursor();
            std::process::exit(130);
        }
        let on_go = selected == options.len();
        match key.code {
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') => {
                ui::show_cursor();
                return Ok(MultiResult::Back);
            }
            KeyCode::Up | KeyCode::Char('k') => selected = (selected + rows - 1) % rows,
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => selected = (selected + 1) % rows,
            KeyCode::Right | KeyCode::Char('l') if !on_go && !options[selected].items.is_empty() => {
                ui::show_cursor();
                return Ok(MultiResult::Drill(selected, MultiState { checked, sub, selected }));
            }
            KeyCode::Char(' ') => {
                if !on_go {
                    checked[selected] = !checked[selected];
                }
            }
            KeyCode::Enter => {
                if on_go {
                    ui::show_cursor();
                    return Ok(MultiResult::Go(MultiState { checked, sub, selected }));
                }
                checked[selected] = !checked[selected];
            }
            _ => {}
        }
    }
}

/// A single-line text prompt: type, Backspace, Enter to submit, Esc/← on an
/// empty buffer to cancel (returns None).
pub fn prompt_line(question: &str) -> std::io::Result<Option<String>> {
    use std::io::Write;
    let mut buf = String::new();
    print!("{} {question}", crate::ui::color::orange(crate::ui::glyph::back()));
    std::io::stdout().flush()?;
    loop {
        let key = read_key()?;
        match key.code {
            KeyCode::Esc => return Ok(None),
            KeyCode::Left if buf.is_empty() => return Ok(None),
            KeyCode::Enter => {
                println!();
                return Ok(if buf.trim() == "<-" { None } else { Some(buf) });
            }
            KeyCode::Backspace => {
                if buf.pop().is_some() {
                    print!("\u{8} \u{8}");
                    std::io::stdout().flush()?;
                }
            }
            KeyCode::Char(c) if !key.ctrl => {
                buf.push(c);
                print!("{c}");
                std::io::stdout().flush()?;
            }
            _ => {}
        }
    }
}
