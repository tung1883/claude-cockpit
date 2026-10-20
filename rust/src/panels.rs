// Split-screen: N Claude sessions side by side in one cockpit-owned
// terminal, each backed by its own real PTY (portable-pty) whose ANSI
// output is parsed into an offscreen grid (vt100) and composited into its
// own column instead of the child owning the whole console.
//
// Why a PTY host on Windows has to answer "\x1b[6n" (Cursor Position
// Report request), and answer it in one write — see `process_output`:
// portable-pty always creates the pseudoconsole with INHERIT_CURSOR, which
// makes conhost send that query the moment the first child connects and
// then block *the child's console connect* (the child sits in
// KERNELBASE!ConsoleInitialize forever, and dies with STATUS_DLL_INIT_FAILED
// 0xC0000142 if the conhost is later killed) until a well-formed reply
// arrives. See PLAN.md for the full history of chasing this.
use crate::{input::read_key, launch, layout, profiles::Profile, session, ui};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use portable_pty::{native_pty_system, Child, MasterPty, PtySize};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Pane {
    label: String,
    parser: Arc<Mutex<vt100::Parser>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    dirty: Arc<AtomicBool>,
    /// Temp file holding this pane's composed system prompt (if any); removed
    /// when the pane is dropped. Killing the child stays explicit elsewhere.
    sysprompt_tmp: Option<std::path::PathBuf>,
    /// Whether this pane runs Claude (vs. a plain shell).
    claude: bool,
    /// When set, a Ctrl+L is due to be sent to this (Claude) pane — see
    /// `RESIZE_NUDGE`.
    nudge_at: Mutex<Option<std::time::Instant>>,
}

/// How long after a Claude pane's last resize it gets a Ctrl+L (screen
/// redraw). After a column resize the inner ConPTY and Claude disagree by one
/// column about where the prompt cursor is, so the first typed character lands
/// one cell left (over the space after `❯`) and later ones leave a gap; the
/// stray character then survives backspacing. Reproduced with a headless
/// probe (examples/probe.rs) at several widths; a Ctrl+L once the resize has
/// settled makes Claude redraw cleanly and the glitch does not occur.
const RESIZE_NUDGE: Duration = Duration::from_millis(800);

impl Drop for Pane {
    fn drop(&mut self) {
        if let Some(path) = &self.sysprompt_tmp {
            let _ = std::fs::remove_file(path);
        }
    }
}

const CURSOR_QUERY: &[u8] = b"\x1b[6n";

/// Feeds one chunk of PTY output to the pane's screen and answers every
/// Cursor Position Report request ("\x1b[6n") in it with the cursor
/// position *at that point of the stream* — vt100 already tracks the
/// cursor while parsing, so the chunk is parsed up to each query, the
/// reply captured, and parsing resumed. `carry` holds the tail of the
/// previous chunk (already parsed), so a query split across two reads is
/// still seen exactly once.
///
/// The reply MUST go out as a single write. `write!` issues one pipe write
/// per format fragment ("\x1b[", "1", ";", "1", "R"), and conhost's input
/// state machine flushes a chunk that ends mid-sequence as literal keys
/// (bare Esc, '['...), so a fragmented reply is never recognised as the
/// CPR — and conhost keeps waiting, with the child's console connect
/// blocked behind it. That single detail is what made every earlier PTY
/// attempt in this repo look like a deep OS-level spawn failure.
///
/// Shares the one PTY writer with keystroke forwarding (portable_pty's
/// `take_writer` can only be called once per master), hence the mutex.
/// The parser lock is never held while writing to the PTY.
fn process_output(carry: &mut Vec<u8>, chunk: &[u8], parser: &Mutex<vt100::Parser>, writer: &Mutex<Box<dyn Write + Send>>) {
    let mut scan = std::mem::take(carry);
    let mut fed = scan.len(); // everything before this index was parsed last time
    scan.extend_from_slice(chunk);
    let mut replies: Vec<String> = Vec::new();
    if let Ok(mut p) = parser.lock() {
        let ends = scan.windows(CURSOR_QUERY.len()).enumerate().filter(|(_, w)| *w == CURSOR_QUERY).map(|(i, _)| i + CURSOR_QUERY.len());
        for end in ends {
            p.process(&scan[fed..end]);
            fed = end;
            let (row, col) = p.screen().cursor_position();
            replies.push(format!("\x1b[{};{}R", row + 1, col + 1));
        }
        p.process(&scan[fed..]);
    }
    // Keep only a partial tail: never enough bytes to re-match a query
    // this call already answered.
    let keep = scan.len().min(CURSOR_QUERY.len() - 1);
    carry.extend_from_slice(&scan[scan.len() - keep..]);
    if replies.is_empty() {
        return;
    }
    if let Ok(mut w) = writer.lock() {
        for reply in &replies {
            let _ = w.write_all(reply.as_bytes());
        }
        let _ = w.flush();
    }
}

fn spawn_pane(profile: &str, label: String, cwd: &std::path::Path, cols: u16, rows: u16) -> Result<Pane> {
    let (cmd, sysprompt_tmp) = launch::claude_pty_command(profile, cwd);
    spawn_pane_cmd(cmd, sysprompt_tmp, true, label, cols, rows)
}

/// A pane running a plain interactive shell instead of Claude.
fn spawn_shell_pane(cwd: &std::path::Path, cols: u16, rows: u16) -> Result<Pane> {
    spawn_pane_cmd(launch::shell_pty_command(cwd), None, false, "shell".to_string(), cols, rows)
}

fn spawn_pane_cmd(
    cmd: portable_pty::CommandBuilder,
    sysprompt_tmp: Option<std::path::PathBuf>,
    claude: bool,
    label: String,
    cols: u16,
    rows: u16,
) -> Result<Pane> {
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })?;
    let child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader()?;
    let writer: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(pair.master.take_writer()?));
    let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 2000)));
    let dirty = Arc::new(AtomicBool::new(true));
    let parser_bg = parser.clone();
    let dirty_bg = dirty.clone();
    let writer_bg = writer.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        let mut carry = Vec::with_capacity(CURSOR_QUERY.len());
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    process_output(&mut carry, &buf[..n], &parser_bg, &writer_bg);
                    dirty_bg.store(true, Ordering::Relaxed);
                }
            }
        }
    });
    Ok(Pane { label, parser, writer, master: pair.master, child, dirty, sysprompt_tmp, claude, nudge_at: Mutex::new(None) })
}

fn color_code(c: vt100::Color, fg: bool) -> String {
    match c {
        vt100::Color::Default => (if fg { "39" } else { "49" }).to_string(),
        vt100::Color::Idx(n) if n < 8 => (if fg { 30 + n } else { 40 + n }).to_string(),
        vt100::Color::Idx(n) if n < 16 => (if fg { 90 + (n - 8) } else { 100 + (n - 8) }).to_string(),
        vt100::Color::Idx(n) => format!("{};5;{n}", if fg { 38 } else { 48 }),
        vt100::Color::Rgb(r, g, b) => format!("{};2;{r};{g};{b}", if fg { 38 } else { 48 }),
    }
}

fn render_row(screen: &vt100::Screen, row: u16, width: u16) -> String {
    let mut out = String::new();
    // The SGR currently in effect; a new escape is emitted only when a cell's
    // style differs from the previous one, so a mostly-plain row costs a
    // handful of escapes instead of one per cell.
    let mut current = String::new();
    for col in 0..width {
        let Some(cell) = screen.cell(row, col) else {
            out.push(' ');
            continue;
        };
        // The right half of a double-width glyph: the terminal already
        // advances two columns for the glyph itself, so printing a filler
        // here would push the rest of the row (and every pane to its right)
        // one column out of place.
        if cell.is_wide_continuation() {
            continue;
        }
        let mut codes = Vec::new();
        if cell.bold() {
            codes.push("1".to_string());
        }
        if cell.italic() {
            codes.push("3".to_string());
        }
        if cell.underline() {
            codes.push("4".to_string());
        }
        if cell.inverse() {
            codes.push("7".to_string());
        }
        codes.push(color_code(cell.fgcolor(), true));
        codes.push(color_code(cell.bgcolor(), false));
        let sgr = codes.join(";");
        if sgr != current {
            out.push_str(&format!("\x1b[0;{sgr}m"));
            current = sgr;
        }
        let contents = cell.contents();
        if contents.is_empty() {
            out.push(' ');
        } else {
            out.push_str(&contents);
        }
    }
    out.push_str("\x1b[0m");
    out
}

/// Translates a crossterm key press into the raw bytes a real terminal
/// would send a child process — normal typing, arrows, and Ctrl-letter
/// combos; exotic function keys aren't mapped and are silently dropped.
fn key_bytes(code: KeyCode, ctrl: bool, alt: bool) -> Option<Vec<u8>> {
    let base: Vec<u8> = match code {
        KeyCode::Char(c) if ctrl && c.is_ascii_alphabetic() => vec![(c.to_ascii_uppercase() as u8) & 0x1f],
        KeyCode::Char('@') if ctrl => vec![0],
        KeyCode::Char(c) => c.to_string().into_bytes(),
        KeyCode::Enter => vec![b'\r'],
        // Ctrl+Backspace = delete the previous word. crossterm reports it as
        // Backspace with the CONTROL modifier (not a distinct byte), so map it
        // ourselves to Alt+Backspace (ESC DEL), readline's backward-kill-word,
        // which Claude Code honors. Plain Backspace stays a single DEL (0x7f).
        KeyCode::Backspace if ctrl => b"\x1b\x7f".to_vec(),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        _ => return None,
    };
    if alt {
        let mut out = vec![0x1b];
        out.extend(base);
        Some(out)
    } else {
        Some(base)
    }
}

// Panes tile a grid. Within a grid row, panes are separated by SEP (" │ ",
// 3 cells); grid rows are separated by one horizontal rule line. Geometry
// and rendering must agree on both.
const SEP: &str = " │ ";
const SEP_COLS: u16 = 3;
/// Chrome lines that aren't panes: the label header, a blank spacer, and the
/// key-hint footer.
const CHROME_ROWS: u16 = 3;
/// Below these a pane is too small for Claude's UI to be usable; they bound
/// how many panes fit.
const MIN_PANE_COLS: u16 = 24;
const MIN_PANE_ROWS: u16 = 6;
/// Hard ceiling regardless of terminal size — more than this is unreadable
/// and hammers the machine with that many live Claude sessions. 9 = a 3x3
/// grid.
const MAX_PANES: usize = 9;

/// Grid shape (rows, cols) for `n` panes — as square as possible, filling
/// left-to-right, top-to-bottom. n=2 stays a single row of two (side by
/// side); n=3/4 → 2x2; n=5/6 → 2x3; up to 3x3.
fn grid_dims(n: usize) -> (usize, usize) {
    let n = n.max(1);
    let gcols = (n as f64).sqrt().ceil() as usize;
    let grows = n.div_ceil(gcols);
    (grows, gcols)
}

/// Per-pane cell size for `n` panes tiled across one terminal of `cols`x`rows`.
fn pane_geometry(cols: u16, rows: u16, n: usize) -> (u16, u16) {
    let (grows, gcols) = grid_dims(n);
    let sep_w = (gcols as u16 - 1) * SEP_COLS;
    let pane_cols = cols.saturating_sub(sep_w) / gcols as u16;
    // One rule line between adjacent grid rows, on top of the fixed chrome.
    let chrome = CHROME_ROWS + (grows as u16 - 1);
    let pane_rows = rows.saturating_sub(chrome) / grows as u16;
    (pane_cols.max(10), pane_rows.max(3))
}

/// Whether `n` panes tile `cols`x`rows` while every pane stays usable.
fn panes_fit(cols: u16, rows: u16, n: usize) -> bool {
    let (grows, gcols) = grid_dims(n);
    let pane_cols = cols.saturating_sub((gcols as u16 - 1) * SEP_COLS) / gcols as u16;
    let pane_rows = rows.saturating_sub(CHROME_ROWS + (grows as u16 - 1)) / grows as u16;
    pane_cols >= MIN_PANE_COLS && pane_rows >= MIN_PANE_ROWS
}

/// Largest pane count that still tiles `cols`x`rows` usably, capped at
/// `MAX_PANES` (and always at least 1 so a solo session is possible).
fn max_panes_for(cols: u16, rows: u16) -> usize {
    let mut best = 1;
    for n in 2..=MAX_PANES {
        if panes_fit(cols, rows, n) {
            best = n;
        }
    }
    best
}

/// Queue box on the right of the pane picker: narrowest terminal that gets it,
/// and its width.
const QUEUE_BOX_MIN_COLS: u16 = 100;
const QUEUE_BOX_W: usize = 36;

/// The right-hand queue box, fully framed: one entry per queued pane
/// (profile + path), the next free slot, and a key hint. The frame turns
/// orange while the box has focus.
fn render_queue_box(queue: &[(String, std::path::PathBuf)], cap: usize, focused: bool, sel: usize) -> Vec<String> {
    let frame = |t: &str| if focused { ui::color::orange(t) } else { ui::color::dim(t) };
    let inner = QUEUE_BOX_W - 4; // two border columns + a space of padding each side
    let row = |content: &str| format!("{} {} {}", frame("│"), layout::pad_to(content, inner), frame("│"));
    let hint = if focused { "↑/↓ · Enter dir · x remove" } else { "Tab → focus queue" };
    let mut out = vec![
        frame(&format!("┌{}┐", "─".repeat(QUEUE_BOX_W - 2))),
        row(&ui::color::orange_bold(&format!("Queue {}/{cap}", queue.len()))),
        row(&ui::color::dim(hint)),
        row(""),
    ];
    for (i, (name, dir)) in queue.iter().enumerate() {
        let on = focused && i == sel;
        let marker = if on { ui::color::orange(ui::glyph::pointer()) } else { " ".to_string() };
        let first = if i == 0 { ui::color::dim(" (first)") } else { String::new() };
        let title = format!("{} {name}", i + 1);
        out.push(row(&format!("{marker} {}{first}", if on { ui::color::orange_bold(&title) } else { title })));
        let path = dir.display().to_string();
        let max = inner - 2;
        let n = path.chars().count();
        let shown = if n > max { format!("…{}", path.chars().skip(n - (max - 1)).collect::<String>()) } else { path };
        out.push(row(&format!("  {}", ui::color::dim(&shown))));
    }
    if queue.len() < cap {
        out.push(row(&format!("  {}", ui::color::dim(&format!("{} next…", queue.len() + 1)))));
    }
    out.push(frame(&format!("└{}┘", "─".repeat(QUEUE_BOX_W - 2))));
    out
}

/// What picking a row in `pick_pane_target` resolves to.
enum PaneAdd {
    Start,
    Pick(String, std::path::PathBuf),
}

/// One flattened, navigable row in `pick_pane_target`'s list.
enum TargetRow {
    Start,
    Profile(usize),
    /// `profile_idx`'s project at `path` — the always-shown "(current)" row
    /// when `is_current`, else one revealed by expanding that profile.
    Project { profile_idx: usize, path: std::path::PathBuf, is_current: bool },
    /// "Choose path" — type any directory for `profile_idx`'s pane.
    ChoosePath(usize),
}

/// Profile picker for adding a pane, with each profile expandable (→) into
/// the projects it has sessions in — collapsed, a profile shows just one
/// row (the cwd cockpit itself was launched from, "(current)"); expanding
/// reveals its other recent projects (from `session::grouped_sessions`,
/// the same source the Notes/Detail screens use) so a pane can be launched
/// into a *different* project than the one cockpit is sitting in.
///
/// `queue` is `run_split`'s list of panes chosen so far: when `Some`, a
/// "Start" row appears first, picks are pushed onto the queue (the picker
/// stays open until Start or the cap), and the queue is shown — as a box on
/// the right when the terminal is wide enough, else as a header list — where
/// Tab moves into it to re-path (Enter) or remove (x) a pane. `None` is the
/// mid-split "add a pane" picker (F7): one pick, returned directly.
fn pick_pane_target(
    profiles: &[Profile],
    cwd: &std::path::Path,
    heading: &str,
    mut queue: Option<&mut Vec<(String, std::path::PathBuf)>>,
) -> Result<Option<PaneAdd>> {
    let cwd_norm = cwd.to_string_lossy().replace('\\', "/");
    // Only one profile expanded at a time — keeps "collapse" unambiguous
    // (see the Left/Esc handling below) instead of needing to track which
    // of several expanded profiles a given key press should affect.
    let mut expanded: Option<usize> = None;
    // Fetched once per profile, only on first expand — grouped_sessions
    // scans every session file's cwd, not worth paying for a profile
    // that's never expanded.
    let mut projects_cache: std::collections::HashMap<usize, Vec<session::SessionGroup>> = std::collections::HashMap::new();
    let mut selected = 0usize;
    let mut top = 0usize;
    // Set on collapse, resolved against the freshly-rebuilt row list at
    // the top of the next iteration — restores the highlight to the
    // profile row itself, instead of leaving `selected` at whatever
    // numeric position it was at among the expanded project rows, which
    // could now land on a completely different (and shorter) profile
    // after the list shrinks back down.
    let mut refocus_profile: Option<usize> = None;
    // Queue-box focus (Tab) and its highlighted pane.
    let mut focus_q = false;
    let mut qsel = 0usize;
    // Computed once, not per frame — this used to call `account_stat`
    // (a full directory walk per profile) on every single keystroke via
    // `quota_note`/`account_lines`, which is what made the screen laggy.
    let stats: Vec<layout::Stat> = profiles.iter().map(|p| layout::account_stat(&p.name)).collect();

    ui::hide_cursor();
    loop {
        // Rebuilt every frame from `expanded` — cheap (a handful of
        // profiles/projects), and simplest way to keep the flattened
        // row list in sync with expand state without a parallel index.
        let mut rows: Vec<TargetRow> = Vec::new();
        if queue.is_some() {
            rows.push(TargetRow::Start);
        }
        for i in 0..profiles.len() {
            rows.push(TargetRow::Profile(i));
            rows.push(TargetRow::Project { profile_idx: i, path: cwd.to_path_buf(), is_current: true });
            if expanded == Some(i) {
                if let Some(groups) = projects_cache.get(&i) {
                    for g in groups {
                        if g.folder.replace('\\', "/") == cwd_norm {
                            continue; // already shown as the "(current)" row
                        }
                        rows.push(TargetRow::Project { profile_idx: i, path: std::path::PathBuf::from(&g.folder), is_current: false });
                    }
                }
                rows.push(TargetRow::ChoosePath(i));
            }
        }
        // The always-shown "(current)" row under each profile is display
        // only, not a distinct pick from the profile row itself (Enter on
        // the profile already means "use current project") — excluded
        // from ↑/↓ so a profile moves straight to the next one instead of
        // stopping on a line that duplicates its own default action.
        let selectable_idx: Vec<usize> =
            rows.iter().enumerate().filter(|(_, r)| !matches!(r, TargetRow::Project { is_current: true, .. })).map(|(i, _)| i).collect();
        if let Some(pi) = refocus_profile.take() {
            if let Some(pos) = selectable_idx.iter().position(|&i| matches!(rows[i], TargetRow::Profile(p) if p == pi)) {
                selected = pos;
            }
        }
        selected = selected.min(selectable_idx.len().saturating_sub(1));
        let active_row = selectable_idx[selected];

        let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let cap = max_panes_for(term_cols, term_rows);
        let show_box = queue.is_some() && term_cols >= QUEUE_BOX_MIN_COLS;
        if let Some(q) = queue.as_deref() {
            qsel = qsel.min(q.len().saturating_sub(1));
        }
        let mut header = layout::account_lines(profiles, -1, &stats);
        match queue.as_deref() {
            Some(q) if show_box => {
                header.push(format!("{} {}", ui::color::orange(ui::glyph::back()), ui::color::bold(&format!("Add pane {}", q.len() + 1))));
            }
            Some(q) => {
                header.push(format!("{} {}", ui::color::orange(ui::glyph::back()), ui::color::bold(&format!("Panes so far ({}/{cap})", q.len()))));
                for (i, (name, dir)) in q.iter().enumerate() {
                    header.push(format!("   {} {name}  {}", ui::color::orange(&(i + 1).to_string()), ui::color::dim(&dir.display().to_string())));
                }
            }
            None => header.push(format!("{} {}", ui::color::orange(ui::glyph::back()), ui::color::bold(heading))),
        }
        header.push(ui::color::dim("   Enter select · → on a profile: recent projects & Choose path"));
        header.push(String::new());
        let body_start = header.len();

        // Same viewport-paging pattern as every other scrollable list here
        // (e.g. `choose_from_list_lazy`) — without it, expanding a profile
        // with several projects can push later rows off the bottom of a
        // short terminal with no way to see them: the selection still
        // moves there on ↓, it's just invisible, which looked like
        // "arrow keys don't work."
        let chrome = header.len() + 4;
        let viewport = (crossterm::terminal::size().map(|(_, h)| h as usize).unwrap_or(24)).saturating_sub(chrome).max(3);
        if active_row < top {
            top = active_row;
        }
        if active_row >= top + viewport {
            top = active_row - viewport + 1;
        }
        top = top.min(rows.len().saturating_sub(viewport));
        let end = (top + viewport).min(rows.len());

        let mut lines = header;
        if top > 0 {
            lines.push(ui::color::dim(&format!("    ↑ {top} more")));
        }
        for (i, row) in rows.iter().enumerate().take(end).skip(top) {
            let on = i == active_row;
            let marker = if on { ui::color::orange(ui::glyph::pointer()) } else { " ".to_string() };
            let text = match row {
                TargetRow::Start => {
                    let n = queue.as_deref().map_or(0, |q| q.len());
                    let label = if n <= 1 { "Start (solo session)".to_string() } else { format!("Start ({n} panes)") };
                    if on { ui::color::orange_bold(&label) } else { label }
                }
                TargetRow::Profile(pi) => {
                    let name = &profiles[*pi].name;
                    if on { ui::color::orange_bold(name) } else { name.clone() }
                }
                TargetRow::ChoosePath(_) => {
                    let text = "    Choose path…";
                    if on { ui::color::orange_bold(text) } else { ui::color::dim(text) }
                }
                TargetRow::Project { path, is_current, .. } => {
                    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    let tag = if *is_current { ui::color::dim("(current)") } else { String::new() };
                    let text = format!("    {name} {tag}");
                    if on { ui::color::orange_bold(&text) } else { ui::color::dim(&text) }
                }
            };
            lines.push(format!(" {marker} {text}"));
        }
        if end < rows.len() {
            lines.push(ui::color::dim(&format!("    ↓ {} more", rows.len() - end)));
        }
        if show_box {
            let box_lines = render_queue_box(queue.as_deref().unwrap(), cap, focus_q, qsel);
            // Keep the box off the terminal's last column — a glyph written there is
            // eaten by the pending-wrap state, which is what hid the right border.
            let left_w = term_cols as usize - QUEUE_BOX_W - 4;
            for (k, bl) in box_lines.into_iter().enumerate() {
                let at = body_start + k;
                if at >= lines.len() {
                    lines.push(String::new());
                }
                lines[at] = format!("{}  {bl}", layout::pad_to(&lines[at], left_w));
            }
        }
        lines.push(String::new());
        lines.push(ui::rule(None));
        lines.push(format!(
            " {} move   {} select   {} expand   {} {}",
            ui::color::dim("↑/↓"),
            ui::color::dim("Enter"),
            ui::color::dim("→"),
            ui::color::dim(&format!("{}/Esc", ui::glyph::back())),
            if expanded.is_some() { "collapse" } else { "back" },
        ) + &if show_box { format!("   {} queue", ui::color::dim("Tab")) } else { String::new() });
        ui::repaint(&lines);

        let key = read_key()?;
        if key.ctrl && key.code == KeyCode::Char('c') {
            ui::show_cursor();
            std::process::exit(130);
        }
        if focus_q {
            if let Some(q) = queue.as_deref_mut() {
                match key.code {
                    KeyCode::Tab | KeyCode::Esc | KeyCode::Left => focus_q = false,
                    KeyCode::Up | KeyCode::Char('k') => qsel = (qsel + q.len() - 1) % q.len(),
                    KeyCode::Down | KeyCode::Char('j') => qsel = (qsel + 1) % q.len(),
                    // Pane 1 is the profile cockpit was launched from; the
                    // start needs at least one pane, so it can't be removed.
                    KeyCode::Char('x') | KeyCode::Delete if qsel > 0 => {
                        q.remove(qsel);
                        qsel = qsel.min(q.len() - 1);
                    }
                    KeyCode::Enter => {
                        if let Some(dir) = prompt_dir(&format!("Pane {} — which directory?", qsel + 1), &q[qsel].1)? {
                            q[qsel].1 = dir;
                        }
                    }
                    _ => {}
                }
            }
            continue;
        }
        match key.code {
            KeyCode::Tab if show_box => focus_q = true,
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') | KeyCode::Char('q') => {
                // First press collapses whichever profile is expanded and
                // stays on this screen; only actually backs out once
                // nothing is expanded — matches every other drill-down
                // list in the cockpit, where Back unwinds one level.
                if let Some(pi) = expanded.take() {
                    refocus_profile = Some(pi);
                } else {
                    ui::show_cursor();
                    return Ok(None);
                }
            }
            KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
                selected = (selected + selectable_idx.len() - 1) % selectable_idx.len();
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                selected = (selected + 1) % selectable_idx.len();
            }
            KeyCode::Right | KeyCode::Char('l') => {
                if let TargetRow::Profile(pi) = &rows[active_row] {
                    let pi = *pi;
                    projects_cache.entry(pi).or_insert_with(|| session::grouped_sessions(&profiles[pi].name));
                    expanded = Some(pi);
                }
            }
            KeyCode::Enter => {
                let pick: (String, std::path::PathBuf) = match &rows[active_row] {
                    TargetRow::Start => {
                        ui::show_cursor();
                        return Ok(Some(PaneAdd::Start));
                    }
                    TargetRow::Profile(pi) => (profiles[*pi].name.clone(), cwd.to_path_buf()),
                    TargetRow::Project { profile_idx, path, .. } => (profiles[*profile_idx].name.clone(), path.clone()),
                    TargetRow::ChoosePath(pi) => {
                        let title = format!("{} — which directory?", profiles[*pi].name);
                        // Esc in the prompt just returns to this list.
                        match prompt_dir(&title, cwd)? {
                            Some(dir) => (profiles[*pi].name.clone(), dir),
                            None => continue,
                        }
                    }
                };
                match queue.as_deref_mut() {
                    Some(q) => {
                        if q.len() < cap {
                            q.push(pick);
                        }
                        // Full: nothing more to add, so launch.
                        if q.len() >= cap {
                            ui::show_cursor();
                            return Ok(Some(PaneAdd::Start));
                        }
                    }
                    None => {
                        ui::show_cursor();
                        return Ok(Some(PaneAdd::Pick(pick.0, pick.1)));
                    }
                }
            }
            _ => {}
        }
    }
}

/// N Claude sessions side by side, `first` plus profiles chosen here. Panes
/// can be added (F7) and closed (F8) while running, and a pane whose session
/// exits on its own is dropped without disturbing the others; the split ends
/// only when the last pane is gone (or Ctrl+Q). v1 scope: no system-prompt
/// composition for the panes (plain launches only).
pub fn run_split(profiles: &[Profile], first: &str) -> Result<Option<Session>> {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let cap = max_panes_for(cols, rows);
    let cwd = std::env::current_dir()?;

    let mut chosen: Vec<(String, std::path::PathBuf)> = vec![(first.to_string(), cwd.clone())];
    if chosen.len() < cap {
        // The picker keeps adding to `chosen` until "Start" (one pane = a
        // solo session, two or more = a split) or the cap; Esc cancels the
        // whole thing.
        if pick_pane_target(profiles, &cwd, "", Some(&mut chosen))?.is_none() {
            return Ok(None);
        }
    }
    run_split_targets(profiles, &chosen)
}

/// Run a split of exactly the given profiles (one pane each, in order), all
/// in the current directory. `profiles` is the full account list, used only
/// to offer choices when a pane is added mid-split. Split out from the
/// picker so it can be driven directly (tests, a CLI entry).
///
/// Returns `Some(Session)` if the user detached (Ctrl+B) — the panes keep
/// running in the background and the cockpit can `resume` them later — or
/// `None` if they quit (Ctrl+Q) or the last pane exited.
pub fn run_split_profiles(profiles: &[Profile], profile_names: &[String]) -> Result<Option<Session>> {
    let cwd = std::env::current_dir()?;
    let targets: Vec<(String, std::path::PathBuf)> = profile_names.iter().map(|n| (n.clone(), cwd.clone())).collect();
    run_split_targets(profiles, &targets)
}

/// Same as `run_split_profiles`, but each pane can launch into its own
/// project directory instead of all sharing the caller's cwd — what the
/// interactive picker (`pick_pane_target`, → to pick another project)
/// needs underneath.
fn run_split_targets(profiles: &[Profile], targets: &[(String, std::path::PathBuf)]) -> Result<Option<Session>> {
    let cwd = std::env::current_dir()?;
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let (pane_cols, pane_rows) = pane_geometry(cols, rows, targets.len());

    // Each spawn opens a ConPTY and starts a process — noticeable serially
    // with several panes, so do them concurrently (order is preserved).
    let spawned: Vec<Result<Pane>> = std::thread::scope(|scope| {
        let handles: Vec<_> = targets
            .iter()
            .map(|(name, project)| scope.spawn(move || spawn_pane(name, name.clone(), project, pane_cols, pane_rows)))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap_or_else(|_| Err(anyhow::anyhow!("pane spawn panicked")))).collect()
    });
    let mut panes: Vec<Pane> = Vec::with_capacity(targets.len());
    for pane in spawned {
        match pane {
            Ok(p) => panes.push(p),
            Err(e) => {
                for p in &mut panes {
                    let _ = p.child.kill();
                }
                return Err(e);
            }
        }
    }
    let mut focus = 0usize;

    ui::hide_cursor();
    // The shared `cwd` here (not each pane's own project) is what F9/`
    // shell`'s drop-to-shell and F7's "add a pane" default to — cockpit's
    // own launch directory, not whichever project a given pane happens to
    // be running in.
    let exit = run_loop(&mut panes, &mut focus, profiles, &cwd);
    ui::show_cursor();
    finish(panes, focus, cwd, exit)
}

/// A detached split/session handed back to the cockpit: its panes' PTYs and
/// reader threads keep running so it can be resumed with its screen state
/// intact. Dropping it (cockpit quit, or a quit/errored resume) kills the
/// child processes.
pub struct Session {
    panes: Vec<Pane>,
    focus: usize,
    cwd: std::path::PathBuf,
}

impl Session {
    pub fn pane_count(&self) -> usize {
        self.panes.len()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        for pane in &mut self.panes {
            let _ = pane.child.kill();
        }
    }
}

/// Whether the run loop returned because the user quit/last pane exited, or
/// because they detached back to the cockpit.
enum LoopExit {
    Quit,
    Detach,
}

/// Turns a run-loop outcome into either a live `Session` (detach) or a clean
/// teardown (quit / error), killing children in the latter cases.
fn finish(mut panes: Vec<Pane>, focus: usize, cwd: std::path::PathBuf, exit: Result<LoopExit>) -> Result<Option<Session>> {
    match exit {
        Ok(LoopExit::Detach) => Ok(Some(Session { panes, focus, cwd })),
        Ok(LoopExit::Quit) => {
            for pane in &mut panes {
                let _ = pane.child.kill();
            }
            Ok(None)
        }
        Err(e) => {
            for pane in &mut panes {
                let _ = pane.child.kill();
            }
            Err(e)
        }
    }
}

/// Re-enters a previously detached session, reflowing it to the current
/// terminal size first. Same return contract as `run_split_profiles`. On
/// quit or error `session` is dropped, which kills its children.
pub fn resume(mut session: Session, profiles: &[Profile]) -> Result<Option<Session>> {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    reflow(&session.panes, cols, rows);
    ui::hide_cursor();
    let cwd = session.cwd.clone();
    let exit = run_loop(&mut session.panes, &mut session.focus, profiles, &cwd);
    ui::show_cursor();
    match exit {
        Ok(LoopExit::Detach) => Ok(Some(session)),
        Ok(LoopExit::Quit) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Resizes one pane's real PTY and vt100 mirror together — both have to
/// agree, or the child's own auto-wrap point (governed by the real PTY
/// size) and our cursor/render math (governed by the vt100 size) drift
/// apart.
fn resize_pane(pane: &Pane, rows: u16, cols: u16) {
    let changed = pane.parser.lock().map(|p| p.screen().size() != (rows, cols)).unwrap_or(false);
    if changed && pane.claude {
        if let Ok(mut t) = pane.nudge_at.lock() {
            *t = Some(std::time::Instant::now() + RESIZE_NUDGE);
        }
    }
    let _ = pane.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
    if let Ok(mut p) = pane.parser.lock() {
        p.set_size(rows, cols);
    }
    pane.dirty.store(true, Ordering::Relaxed);
}

/// Re-lays every pane for the current pane count and terminal size, and marks
/// them dirty so the next tick repaints. Call after a pane is added, closed,
/// or the terminal is resized.
fn reflow(panes: &[Pane], cols: u16, rows: u16) {
    let (pc, pr) = pane_geometry(cols, rows, panes.len());
    for pane in panes {
        resize_pane(pane, pr, pc);
    }
}

/// Opens the profile picker and, if one is chosen, spawns it as a new pane
/// appended to the grid and focused. No-op if already at the size-based cap.
fn add_pane(panes: &mut Vec<Pane>, focus: &mut usize, profiles: &[Profile], cwd: &std::path::Path) -> Result<()> {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    if panes.len() >= max_panes_for(cols, rows) {
        return Ok(()); // no room for another usable pane
    }
    let choice = pick_pane_target(profiles, cwd, "Add a pane — which profile?", None)?;
    ui::hide_cursor(); // the picker shows it again
    if let Some(PaneAdd::Pick(name, project)) = choice {
        let (pc, pr) = pane_geometry(cols, rows, panes.len() + 1);
        let pane = spawn_pane(&name, name.clone(), &project, pc, pr)?;
        for p in panes.iter() {
            resize_pane(p, pr, pc);
        }
        panes.push(pane);
        *focus = panes.len() - 1;
    }
    for p in panes.iter() {
        p.dirty.store(true, Ordering::Relaxed);
    }
    Ok(())
}

/// Asks which directory a new shell pane should start in. Empty input means
/// `cwd`; a relative path is taken against `cwd`. Esc cancels (None); a path
/// that isn't a directory keeps the prompt open with an error.
fn prompt_dir(title: &str, cwd: &std::path::Path) -> Result<Option<std::path::PathBuf>> {
    let mut buf = String::new();
    let mut error = String::new();
    ui::show_cursor();
    let result = loop {
        let lines = vec![
            format!("{} {}", ui::color::orange(ui::glyph::back()), ui::color::bold(title)),
            ui::color::dim(&format!("   Enter = {}   Esc cancel", cwd.display())),
            String::new(),
            format!("   > {buf}"),
            if error.is_empty() { String::new() } else { ui::color::red(&format!("   {error}")) },
        ];
        ui::repaint(&lines);
        let key = read_key()?;
        match key.code {
            KeyCode::Esc => break None,
            KeyCode::Enter => {
                let input = buf.trim().trim_matches('"');
                if input.is_empty() {
                    break Some(cwd.to_path_buf());
                }
                let home = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")).unwrap_or_default();
                let expanded = match input.strip_prefix('~') {
                    Some(rest) if !home.is_empty() && (rest.is_empty() || rest.starts_with(['/', '\\'])) => format!("{home}{rest}"),
                    _ => input.to_string(),
                };
                let path = cwd.join(expanded); // absolute input replaces cwd
                if path.is_dir() {
                    break Some(path);
                }
                error = format!("not a directory: {}", path.display());
            }
            KeyCode::Backspace => {
                buf.pop();
                error.clear();
            }
            KeyCode::Char(c) if !key.ctrl && !key.alt => {
                buf.push(c);
                error.clear();
            }
            _ => {}
        }
    };
    ui::hide_cursor();
    Ok(result)
}

/// Spawns a shell in `cwd` as a new pane appended to the grid and focused.
/// No-op if already at the size-based cap.
fn add_shell_pane(panes: &mut Vec<Pane>, focus: &mut usize, cwd: &std::path::Path) -> Result<()> {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    if panes.len() >= max_panes_for(cols, rows) {
        return Ok(());
    }
    let (pc, pr) = pane_geometry(cols, rows, panes.len() + 1);
    let pane = spawn_shell_pane(cwd, pc, pr)?;
    for p in panes.iter() {
        resize_pane(p, pr, pc);
    }
    panes.push(pane);
    *focus = panes.len() - 1;
    for p in panes.iter() {
        p.dirty.store(true, Ordering::Relaxed);
    }
    Ok(())
}

/// Kills the focused pane's child and drops it from the split, reflowing the
/// rest. The other panes' sessions keep running untouched. Returns true when
/// no panes are left (the caller then exits the split).
fn close_focused(panes: &mut Vec<Pane>, focus: &mut usize, cols: u16, rows: u16) -> bool {
    let mut pane = panes.remove(*focus);
    let _ = pane.child.kill();
    if panes.is_empty() {
        return true;
    }
    if *focus >= panes.len() {
        *focus = panes.len() - 1;
    }
    reflow(panes, cols, rows);
    false
}

/// Places the real terminal cursor at the focused pane's own cursor (so
/// typing shows a caret where Claude expects one), or hides it if that pane
/// has the cursor hidden. Mirrors the grid layout: line 1 is the label
/// header, then each grid row is `pane_rows` content lines plus one rule
/// line; a pane at grid cell (gr, gc) starts at column
/// `gc*(pane_cols+SEP_COLS)` and line `1 + gr*(pane_rows+1)` (all 1-based
/// for the CUP escape).
/// Builds the cursor-positioning escape sequence without writing it — the
/// caller folds it into the same single write+flush as the frame's content
/// (see the comment on `ui::repaint_body` for why that matters: separate
/// flushed writes for hide/content/reposition is what produced a fast
/// blink instead of one clean cursor move per frame).
fn cursor_escape(panes: &[Pane], focus: usize, pane_rows: u16, pane_cols: u16, zoomed: bool) -> String {
    // Zoomed, the focused pane is drawn alone at grid cell (0, 0) — its
    // real index among `panes` doesn't matter for layout math anymore.
    let (gr, gc) = if zoomed {
        (0u16, 0u16)
    } else {
        let (_, gcols) = grid_dims(panes.len());
        ((focus / gcols) as u16, (focus % gcols) as u16)
    };
    let Ok(p) = panes[focus].parser.lock() else { return String::new() };
    let screen = p.screen();
    if screen.hide_cursor() {
        "\x1b[?25l".to_string()
    } else {
        // Clamped defensively: a resize (zoom) mid-frame can have this
        // pane's vt100 buffer resized to pane_rows/pane_cols while
        // cursor_position() still briefly reports the pre-resize spot (or
        // vice versa, if a caller ever passes stale geometry). An
        // unclamped value here computes a CUP escape that lands outside
        // this pane's cell entirely — into another pane, the chrome
        // lines, or off-screen — which is what "cursor error" was.
        let (cr, cc) = screen.cursor_position();
        let cr = cr.min(pane_rows.saturating_sub(1));
        let cc = cc.min(pane_cols.saturating_sub(1));
        let term_row = 1 + gr * (pane_rows + 1) + cr + 1;
        let term_col = gc * (pane_cols + SEP_COLS) + cc + 1;
        format!("\x1b[{term_row};{term_col}H\x1b[?25h")
    }
}

/// The command that, typed on its own line into a focused pane and followed
/// by Enter, opens a shell pane — the same as pressing F9. cockpit can't see
/// text once it's inside Claude, so it watches the keystrokes it forwards.
const SHELL_CMD: &str = "/shell";

fn run_loop(panes: &mut Vec<Pane>, focus: &mut usize, profiles: &[Profile], cwd: &std::path::Path) -> Result<LoopExit> {
    // What the user has typed into the focused pane since the last line break,
    // so a lone "/shell"+Enter can be recognised. Reset whenever focus moves
    // or a non-text key is pressed.
    let mut typed = String::new();
    // Pane zoom: the focused pane alone, resized to fill the terminal; the
    // rest keep running (their reader threads still drain output, their
    // dirty flags still get set) but aren't drawn or resized until unzoomed
    // — same idea as tmux's zoom, not a separate mode with its own state
    // machine, just a different `n` for layout math.
    let mut zoomed = false;
    // Last frame's lines, so only changed rows are rewritten. Cleared
    // whenever something else drew over the screen (picker, prompt) or the
    // layout changed, which forces one full repaint.
    let mut prev: Vec<String> = Vec::new();
    loop {
        // Send the post-resize Ctrl+L to any Claude pane whose resize has
        // settled (see `RESIZE_NUDGE`).
        let now = std::time::Instant::now();
        for pane in panes.iter() {
            let due = pane.nudge_at.lock().map(|mut t| if t.is_some_and(|at| now >= at) { t.take().is_some() } else { false }).unwrap_or(false);
            if due {
                if let Ok(mut w) = pane.writer.lock() {
                    let _ = w.write_all(&[0x0c]);
                    let _ = w.flush();
                }
            }
        }
        // Swap every dirty flag (not short-circuiting) so one repaint clears
        // all of them and picks up every pane that changed this tick.
        let dirty = panes.iter().fold(false, |acc, p| acc | p.dirty.swap(false, Ordering::Relaxed));
        if dirty {
            let n = if zoomed { 1 } else { panes.len() };
            let (grows, gcols) = grid_dims(n);
            let guards: Vec<_> =
                if zoomed { vec![panes[*focus].parser.lock().unwrap()] } else { panes.iter().map(|p| p.parser.lock().unwrap()).collect() };
            let (pane_rows, pane_cols) = guards[0].screen().size();
            let labels: Vec<String> = if zoomed {
                vec![format!("{} {}", ui::color::orange_bold(&panes[*focus].label), ui::color::dim("(zoomed)"))]
            } else {
                panes.iter().enumerate().map(|(i, p)| if i == *focus { ui::color::orange_bold(&p.label) } else { p.label.clone() }).collect()
            };
            let total_w = gcols * pane_cols as usize + (gcols - 1) * SEP_COLS as usize;
            let blank_cell = " ".repeat(pane_cols as usize);
            let mut lines = Vec::with_capacity(grows * (pane_rows as usize + 1) + 3);
            lines.push(format!(" {}", labels.join(&format!(" {SEP}"))));
            for grow in 0..grows {
                for row in 0..pane_rows {
                    let cells: Vec<String> = (0..gcols)
                        .map(|gcol| {
                            let idx = grow * gcols + gcol;
                            if idx < n {
                                render_row(guards[idx].screen(), row, pane_cols)
                            } else {
                                blank_cell.clone()
                            }
                        })
                        .collect();
                    lines.push(cells.join(SEP));
                }
                if grow + 1 < grows {
                    lines.push(ui::color::gray(&"─".repeat(total_w)));
                }
            }
            lines.push(String::new());
            lines.push(if zoomed {
                format!(" {} unzoom   {} shell   {} back   {} quit", ui::color::dim("F10"), ui::color::dim("F9"), ui::color::dim("Ctrl+B"), ui::color::dim("Ctrl+Q"))
            } else {
                format!(
                    " {} switch   {} add   {} close   {} zoom   {} shell   {} back   {} quit",
                    ui::color::dim("F6"),
                    ui::color::dim("F7"),
                    ui::color::dim("F8"),
                    ui::color::dim("F10"),
                    ui::color::dim("F9"),
                    ui::color::dim("Ctrl+B"),
                    ui::color::dim("Ctrl+Q"),
                )
            });
            // One write+flush for hide+content+reposition+show together —
            // hiding the cursor before the content overwrite (so the
            // redraw can't drag a visible cursor across it) used to be a
            // separate flushed write from the content and from the
            // reposition/show after it; three flushes a frame was enough
            // gap between "cursor off" and "back on" to look like a fast
            // blink instead of one clean frame.
            drop(guards); // cursor_escape below re-locks the focused pane's parser
            let cursor = cursor_escape(panes, *focus, pane_rows, pane_cols, zoomed);
            let body = ui::repaint_diff(&prev, &lines);
            prev = lines;
            let frame = format!("\x1b[?25l{body}{cursor}");
            let mut out = std::io::stdout();
            let _ = out.write_all(frame.as_bytes());
            let _ = out.flush();
        }

        if event::poll(Duration::from_millis(30))? {
            match event::read()? {
                Event::Key(k) if matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                    let alt = k.modifiers.contains(KeyModifiers::ALT);
                    let shift = k.modifiers.contains(KeyModifiers::SHIFT);
                    if ctrl && k.code == KeyCode::Char('q') {
                        return Ok(LoopExit::Quit);
                    }
                    // Ctrl+B detaches back to the cockpit with every pane left
                    // running (its reader thread keeps draining output); the
                    // cockpit can resume it, also with Ctrl+B.
                    if ctrl && k.code == KeyCode::Char('b') {
                        return Ok(LoopExit::Detach);
                    }
                    // F10 toggles zoom: the focused pane alone, resized to
                    // fill the terminal; the others keep running untouched
                    // (still draining output, still marked dirty) but stay
                    // undrawn and unresized until unzoomed.
                    if k.code == KeyCode::F(10) {
                        typed.clear();
                        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
                        zoomed = !zoomed;
                        prev.clear();
                        if zoomed {
                            let (pc, pr) = pane_geometry(cols, rows, 1);
                            resize_pane(&panes[*focus], pr, pc);
                        } else {
                            // Back to the shared grid size for every pane —
                            // the others were never resized while zoomed, so
                            // this is what brings them back in sync with the
                            // one that was.
                            reflow(panes, cols, rows);
                        }
                        for pane in panes.iter() {
                            pane.dirty.store(true, Ordering::Relaxed);
                        }
                        continue;
                    }
                    // Switch/add/close would need to reconcile with a
                    // single resized-to-fill pane mid-zoom — simpler to
                    // just require F10 first, same as tmux.
                    if zoomed && matches!(k.code, KeyCode::F(6) | KeyCode::F(7) | KeyCode::F(8)) {
                        continue;
                    }
                    // F6 cycles focus forward, Shift+F6 backward. (Tab/BackTab
                    // are deliberately left for the focused pane — Claude uses
                    // Shift+Tab itself.)
                    if k.code == KeyCode::F(6) {
                        let n = panes.len();
                        *focus = if shift { (*focus + n - 1) % n } else { (*focus + 1) % n };
                        typed.clear();
                        for pane in panes.iter() {
                            pane.dirty.store(true, Ordering::Relaxed);
                        }
                        continue;
                    }
                    if k.code == KeyCode::F(7) {
                        typed.clear();
                        add_pane(panes, focus, profiles, cwd)?;
                        prev.clear();
                        continue;
                    }
                    if k.code == KeyCode::F(8) {
                        typed.clear();
                        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
                        if close_focused(panes, focus, cols, rows) {
                            return Ok(LoopExit::Quit);
                        }
                        continue;
                    }
                    // F9, or typing "/shell" + Enter, drops into a real
                    // interactive shell without leaving the running Claude
                    // session(s). The panes stay alive in their PTYs (their
                    // reader threads keep draining output) while the shell owns
                    // the terminal; on exit we repaint them from vt100 state.
                    let shell_via_command = k.code == KeyCode::Enter && !ctrl && !alt && typed == SHELL_CMD;
                    if k.code == KeyCode::F(9) || shell_via_command {
                        if shell_via_command {
                            // The "/shell" chars were already echoed into the
                            // pane as they were typed; erase them and swallow
                            // this Enter so Claude is left with a clean prompt.
                            if let Ok(mut w) = panes[*focus].writer.lock() {
                                let _ = w.write_all(&vec![0x7f; SHELL_CMD.chars().count()]);
                                let _ = w.flush();
                            }
                        }
                        typed.clear();
                        add_shell_pane(panes, focus, cwd)?;
                        prev.clear();
                        continue;
                    }
                    // Track the current line so the "/shell" command above can
                    // be recognised. Live-forward keystrokes as usual.
                    match k.code {
                        KeyCode::Char(c) if !ctrl && !alt => typed.push(c),
                        KeyCode::Backspace if !ctrl && !alt => {
                            typed.pop();
                        }
                        KeyCode::Enter if !ctrl && !alt => typed.clear(),
                        _ => typed.clear(),
                    }
                    if let Some(bytes) = key_bytes(k.code, ctrl, alt) {
                        if let Ok(mut w) = panes[*focus].writer.lock() {
                            let _ = w.write_all(&bytes);
                            let _ = w.flush();
                        }
                    }
                }
                Event::Resize(mut cols, mut rows) => {
                    // A mouse-wheel/Ctrl+scroll *font* zoom (unrelated to
                    // pane zoom above — same word, different feature) fires
                    // a burst of Resize events in quick succession, one per
                    // intermediate size — reflow-ing on every single one
                    // means every pane's real PTY and vt100 buffer get
                    // resized many times a second, and Claude's own TUI
                    // (inside that inner PTY) racing to repaint for each
                    // intermediate size is what made it feel broken. Drain
                    // whatever's already queued and reflow once, at the
                    // final size only.
                    while event::poll(Duration::ZERO)? {
                        match event::read()? {
                            Event::Resize(c, r) => {
                                cols = c;
                                rows = r;
                            }
                            // crossterm has no "peek" — a keystroke that
                            // lands mid-burst is already consumed by the
                            // read() above, so forward it here instead of
                            // dropping it (unlikely to happen — scrolling
                            // to zoom and typing at the same time — but
                            // free to handle).
                            Event::Key(k) if matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                                if let Some(bytes) = key_bytes(k.code, k.modifiers.contains(KeyModifiers::CONTROL), k.modifiers.contains(KeyModifiers::ALT)) {
                                    if let Ok(mut w) = panes[*focus].writer.lock() {
                                        let _ = w.write_all(&bytes);
                                        let _ = w.flush();
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    prev.clear();
                    if zoomed {
                        let (pc, pr) = pane_geometry(cols, rows, 1);
                        resize_pane(&panes[*focus], pr, pc);
                    } else {
                        reflow(panes, cols, rows);
                    }
                }
                _ => {}
            }
        }

        // A pane whose Claude exited on its own (/exit, crash) is dropped from
        // the split — the other sessions keep running. Only when the last one
        // is gone does the whole split end.
        let mut i = 0;
        let mut removed = false;
        while i < panes.len() {
            if matches!(panes[i].child.try_wait(), Ok(Some(_))) {
                panes.remove(i);
                removed = true;
            } else {
                i += 1;
            }
        }
        if panes.is_empty() {
            return Ok(LoopExit::Quit);
        }
        if removed {
            if *focus >= panes.len() {
                *focus = panes.len() - 1;
            }
            let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
            if zoomed {
                // The exited pane may have been the zoomed one — resize
                // whichever pane is focused now to fill the screen rather
                // than the shared grid size.
                let (pc, pr) = pane_geometry(cols, rows, 1);
                resize_pane(&panes[*focus], pr, pc);
            } else {
                reflow(panes, cols, rows);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records every individual `write` call, since the whole point of the
    /// DSR reply is that it lands in exactly one.
    struct Recorder(Arc<Mutex<Vec<Vec<u8>>>>);
    impl Write for Recorder {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().push(buf.to_vec());
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn feed(chunks: &[&[u8]]) -> Vec<Vec<u8>> {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let writer: Mutex<Box<dyn Write + Send>> = Mutex::new(Box::new(Recorder(writes.clone())));
        let parser = Mutex::new(vt100::Parser::new(24, 80, 0));
        let mut carry = Vec::new();
        for chunk in chunks {
            process_output(&mut carry, chunk, &parser, &writer);
        }
        let out = writes.lock().unwrap().clone();
        out
    }

    #[test]
    fn cursor_query_is_answered_with_a_single_write() {
        assert_eq!(feed(&[b"\x1b[6n"]), vec![b"\x1b[1;1R".to_vec()]);
    }

    #[test]
    fn reply_reports_the_tracked_cursor_position() {
        // Two lines of output first: cursor now sits on row 3, column 4.
        assert_eq!(feed(&[b"ab\r\ncd\r\nxyz\x1b[6n"]), vec![b"\x1b[3;4R".to_vec()]);
    }

    #[test]
    fn query_split_across_reads_is_answered_exactly_once() {
        assert_eq!(feed(&[b"\x1b", b"[", b"6n"]), vec![b"\x1b[1;1R".to_vec()]);
        // "text" moved the cursor to column 5 before the query; " more"
        // after it must not be counted.
        assert_eq!(feed(&[b"text\x1b[6", b"n more"]), vec![b"\x1b[1;5R".to_vec()]);
    }

    #[test]
    fn every_query_in_a_chunk_gets_its_own_reply_at_its_own_position() {
        assert_eq!(feed(&[b"\x1b[6nab\x1b[6n"]), vec![b"\x1b[1;1R".to_vec(), b"\x1b[1;3R".to_vec()]);
    }

    #[test]
    fn ordinary_output_is_never_answered() {
        assert!(feed(&[b"hello\x1b[31mred\x1b[0m\r\n", b"\x1b[6", b"m"]).is_empty());
    }

    #[test]
    fn plain_backspace_is_one_delete_ctrl_backspace_kills_a_word() {
        assert_eq!(key_bytes(KeyCode::Backspace, false, false), Some(vec![0x7f]));
        assert_eq!(key_bytes(KeyCode::Backspace, true, false), Some(b"\x1b\x7f".to_vec()));
    }

    #[test]
    fn typing_and_ctrl_letters_still_map() {
        assert_eq!(key_bytes(KeyCode::Char('a'), false, false), Some(vec![b'a']));
        assert_eq!(key_bytes(KeyCode::Char('c'), true, false), Some(vec![0x03]));
        assert_eq!(key_bytes(KeyCode::Enter, false, false), Some(vec![b'\r']));
    }

    #[test]
    fn grid_dims_are_as_square_as_possible() {
        assert_eq!(grid_dims(1), (1, 1));
        assert_eq!(grid_dims(2), (1, 2)); // two side by side, no new row
        assert_eq!(grid_dims(3), (2, 2)); // 2 on top, 1 below
        assert_eq!(grid_dims(4), (2, 2));
        assert_eq!(grid_dims(5), (2, 3));
        assert_eq!(grid_dims(6), (2, 3));
        assert_eq!(grid_dims(9), (3, 3));
    }

    #[test]
    fn geometry_tiles_the_grid_minus_separators_and_chrome() {
        // 2 panes = 1x2: width (80-3)/2 = 38; height (24-3)/1 = 21.
        assert_eq!(pane_geometry(80, 24, 2), (38, 21));
        // 4 panes = 2x2: width (80-3)/2 = 38; height (24 - 3 - 1 rule)/2 = 10.
        assert_eq!(pane_geometry(80, 24, 4), (38, 10));
    }

    #[test]
    fn max_panes_is_bounded_by_size_and_the_hard_cap() {
        assert_eq!(max_panes_for(40, 24), 1); // one 24-wide column barely fits, no room for two
        assert!(max_panes_for(200, 60) >= 4); // a big terminal fits a grid
        assert!(max_panes_for(10000, 10000) <= MAX_PANES); // capped
    }
}
