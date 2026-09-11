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
    Delete(String),
    Refresh,
}

/// The main account picker. One blocking `read_key()` call per loop
/// iteration — raw mode was already enabled once for the whole process by
/// `ui::Terminal::enter`, so there's no per-screen mode toggling here at all.
pub fn pick_profile(profiles: &[Profile], start: usize) -> std::io::Result<Action> {
    if profiles.is_empty() {
        return Ok(Action::Add);
    }
    let mut selected = start.min(profiles.len() - 1);
    let stats: Vec<Stat> = profiles.iter().map(|p| layout::account_stat(&p.name)).collect();
    ui::hide_cursor();

    loop {
        let mut lines = layout::account_lines(profiles, selected as i64, &stats);
        lines.extend(layout::hint_grid(
            &[
                &[("↑/↓", "move"), ("→", "view"), ("n", "notes"), ("r", "refresh"), ("q", "quit")],
                &[("a", "add"), ("i", "import"), ("d", "delete"), ("s", "sync")],
            ],
            16,
        ));
        ui::repaint(&lines);

        let key = read_key()?;
        let quit = (key.ctrl && key.code == KeyCode::Char('c'))
            || key.code == KeyCode::Char('q')
            || key.code == KeyCode::Esc;
        if quit {
            ui::show_cursor();
            return Ok(Action::Quit);
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
            KeyCode::Char('n') => {
                ui::show_cursor();
                return Ok(Action::Notes);
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
            KeyCode::Char('d') => {
                ui::show_cursor();
                return Ok(Action::Delete(profiles[selected].name.clone()));
            }
            KeyCode::Char('r') => {
                ui::show_cursor();
                return Ok(Action::Refresh);
            }
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
    let stats: Vec<Stat> = profiles.iter().map(|p| layout::account_stat(&p.name)).collect();
    let mut selected = 0usize;
    ui::hide_cursor();
    loop {
        let mut lines = layout::account_lines(profiles, mark, &stats);
        lines.push(format!("{} {}", ui::color::orange(ui::glyph::back()), ui::color::bold(heading)));
        if let Some(h) = hint {
            lines.push(ui::color::dim(&format!("   {h}")));
        }
        lines.push(String::new());
        for (i, opt) in options.iter().enumerate() {
            let active = i == selected;
            let marker = if active { ui::color::orange(ui::glyph::pointer()) } else { " ".to_string() };
            let label = if active { ui::color::orange_bold(&opt.label) } else { opt.label.clone() };
            let note = opt.note.as_ref().map(|n| format!("   {}", ui::color::dim(n))).unwrap_or_default();
            lines.push(format!(" {marker} {label}{note}"));
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
}

/// Scrollable read-only list. ↑/↓ move the highlight over selectable rows
/// (scrolling the viewport); Enter on a row with a value returns it; ←/Esc/q
/// back out (None). `start` re-opens on the row whose value matches it, so
/// returning from a sub-screen keeps your place.
pub fn scroll_screen(header: &[String], rows: &[ScrollRow], start: Option<&str>) -> std::io::Result<Option<String>> {
    let selectable_idx: Vec<usize> = rows.iter().enumerate().filter(|(_, r)| r.selectable).map(|(i, _)| i).collect();
    let any_pickable = rows.iter().any(|r| r.value.is_some());
    let mut pos = start
        .and_then(|s| selectable_idx.iter().position(|&i| rows[i].value.as_deref() == Some(s)))
        .unwrap_or(0);
    let mut top = 0usize;
    let viewport = (crossterm::terminal::size().map(|(_, h)| h as usize).unwrap_or(24)).saturating_sub(header.len() + 5).max(5);
    ui::hide_cursor();

    loop {
        let active = selectable_idx.get(pos).copied();
        if let Some(active) = active {
            if active < top {
                top = active;
            }
            if active >= top + viewport {
                top = active - viewport + 1;
            }
        }
        top = top.min(rows.len().saturating_sub(viewport));

        let mut lines = header.to_vec();
        let end = (top + viewport).min(rows.len());
        if top > 0 {
            lines.push(ui::color::dim(&format!("    ↑ {top} more")));
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
        if end < rows.len() {
            lines.push(ui::color::dim(&format!("    ↓ {} more", rows.len() - end)));
        }
        lines.push(String::new());
        lines.push(ui::rule(None));
        let open_hint = if any_pickable { format!("    {} open", ui::color::dim("Enter")) } else { String::new() };
        lines.push(format!(" {} move{open_hint}    {} back", ui::color::dim("↑/↓"), ui::color::dim(&format!("{}/Esc", ui::glyph::back()))));
        ui::repaint(&lines);

        let key = read_key()?;
        if key.ctrl && key.code == KeyCode::Char('c') {
            ui::show_cursor();
            std::process::exit(130);
        }
        match key.code {
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') | KeyCode::Char('q') => {
                ui::show_cursor();
                return Ok(None);
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                if let Some(i) = active {
                    if let Some(v) = &rows[i].value {
                        ui::show_cursor();
                        return Ok(Some(v.clone()));
                    }
                }
                if !any_pickable {
                    ui::show_cursor();
                    return Ok(None);
                }
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
