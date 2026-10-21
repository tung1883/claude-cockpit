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
use crate::conpty;
use crate::notify;
use crate::osc7501;
use crate::sidebar::{self, HasMeta, Mode, PaneMeta, Status};
use crate::{input::read_key, launch, layout, profiles::Profile, session, settings, ui};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use portable_pty::{native_pty_system, Child, MasterPty, PtySize};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Pane {
    /// Everything the pane list and the mode rules need (see `sidebar.rs`).
    meta: PaneMeta,
    parser: Arc<Mutex<vt100::Parser>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    dirty: Arc<AtomicBool>,
    /// What the pane's program reported over OSC 7501; written by the reader
    /// thread, read by the UI loop (status badge, notifications).
    reported: Arc<Mutex<osc7501::Reports>>,
    /// Temp file holding this pane's composed system prompt (if any); removed
    /// when the pane is dropped. Killing the child stays explicit elsewhere.
    sysprompt_tmp: Option<std::path::PathBuf>,
    /// Whether this pane runs Claude (vs. a plain shell).
    claude: bool,
    /// When set, a Ctrl+L is due to be sent to this (Claude) pane — see
    /// `RESIZE_NUDGE`.
    nudge_at: Mutex<Option<std::time::Instant>>,
    /// When input was last forwarded to this pane (or when it was created).
    last_input: Mutex<std::time::Instant>,
    /// When this pane last raised a notification (rate limit, see `notify`).
    notified_at: Option<std::time::Instant>,
    /// Same for sub-agent notifications.
    sub_notified_at: Option<std::time::Instant>,
}

/// How long after a Claude pane's last resize it gets a Ctrl+L (screen
/// redraw). After a column resize the inner ConPTY and Claude disagree by one
/// column about where the prompt cursor is, so the first typed character lands
/// one cell left (over the space after `❯`) and later ones leave a gap; the
/// stray character then survives backspacing. Reproduced with a headless
/// probe (examples/probe.rs) at several widths; a Ctrl+L once the resize has
/// settled makes Claude redraw cleanly and the glitch does not occur.
const RESIZE_NUDGE: Duration = Duration::from_millis(800);

/// A Claude pane that has had no input for this long gets a Ctrl+L written in
/// front of the next keystroke. Left idle for several seconds, the inner
/// ConPTY re-syncs on the next input and writes the first typed character one
/// column too far left (`❯ab` instead of `❯ ab`, with the stray character
/// surviving backspace). Reproduced with bare `claude.exe` in a ConPTY and no
/// cpit involved: fine after a 4 s wait, broken after 9 s and 20 s; a Ctrl+L
/// immediately before the first key avoids it every time.
const IDLE_NUDGE: Duration = Duration::from_secs(3);

/// Writes `bytes` to the pane's PTY as one write, preceded by a Ctrl+L when
/// it is a Claude pane that has been idle past `IDLE_NUDGE` (see above).
fn send_to_pane(pane: &Pane, bytes: &[u8]) {
    let now = std::time::Instant::now();
    let idle = match pane.last_input.lock() {
        Ok(mut t) => {
            let idle = now.duration_since(*t) >= IDLE_NUDGE;
            *t = now;
            idle
        }
        Err(_) => false,
    };
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len() + 1);
    if pane.claude && idle && bytes != [0x0c] {
        out.push(0x0c);
    }
    out.extend_from_slice(bytes);
    if let Ok(mut w) = pane.writer.lock() {
        let _ = w.write_all(&out);
        let _ = w.flush();
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        if let Some(path) = &self.sysprompt_tmp {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl HasMeta for Pane {
    fn meta(&self) -> &PaneMeta {
        &self.meta
    }
    fn meta_mut(&mut self) -> &mut PaneMeta {
        &mut self.meta
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
fn process_output(
    carry: &mut Vec<u8>,
    osc: &mut osc7501::Scanner,
    chunk: &[u8],
    parser: &Mutex<vt100::Parser>,
    writer: &Mutex<Box<dyn Write + Send>>,
    reported: &Mutex<osc7501::Reports>,
) {
    let mut scan = std::mem::take(carry);
    let mut fed = scan.len(); // everything before this index was parsed last time
    scan.extend_from_slice(chunk);
    let mut replies: Vec<Vec<u8>> = Vec::new();
    if let Ok(mut p) = parser.lock() {
        let ends = scan.windows(CURSOR_QUERY.len()).enumerate().filter(|(_, w)| *w == CURSOR_QUERY).map(|(i, _)| i + CURSOR_QUERY.len());
        for end in ends {
            p.process(&scan[fed..end]);
            fed = end;
            let (row, col) = p.screen().cursor_position();
            replies.push(format!("\x1b[{};{}R", row + 1, col + 1).into_bytes());
        }
        p.process(&scan[fed..]);
    }
    // Claude Code only reports its status (OSC 7501) once the query has been
    // answered; the answer rides in the same write as any cursor replies.
    for ev in osc.feed(chunk) {
        if ev == osc7501::Event::Query {
            replies.push(osc7501::QUERY_REPLY.to_vec());
        } else if let Ok(mut r) = reported.lock() {
            r.apply(ev);
        }
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
            let _ = w.write_all(reply);
        }
        let _ = w.flush();
    }
}

/// Starts a Claude session for `profile` in `project`. The pane comes back
/// hidden and unnumbered; `register` + `sidebar::set_mode` place it.
fn spawn_pane(profile: &str, project: &std::path::Path, cols: u16, rows: u16) -> Result<Pane> {
    let (cmd, sysprompt_tmp) = launch::claude_pty_command(profile, project);
    spawn_pane_cmd(cmd, sysprompt_tmp, true, profile, project, cols, rows)
}

/// A pane running a plain interactive shell instead of Claude, listed under
/// `profile` (the profile of the pane it was opened from).
fn spawn_shell_pane(profile: &str, cwd: &std::path::Path, cols: u16, rows: u16) -> Result<Pane> {
    spawn_pane_cmd(launch::shell_pty_command(cwd), None, false, profile, cwd, cols, rows)
}

fn spawn_pane_cmd(
    cmd: portable_pty::CommandBuilder,
    sysprompt_tmp: Option<std::path::PathBuf>,
    claude: bool,
    profile: &str,
    project: &std::path::Path,
    cols: u16,
    rows: u16,
) -> Result<Pane> {
    conpty::init();
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
    let reported = Arc::new(Mutex::new(osc7501::Reports::default()));
    let reported_bg = reported.clone();
    let writer_bg = writer.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        let mut carry = Vec::with_capacity(CURSOR_QUERY.len());
        let mut osc = osc7501::Scanner::default();
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    process_output(&mut carry, &mut osc, &buf[..n], &parser_bg, &writer_bg, &reported_bg);
                    dirty_bg.store(true, Ordering::Relaxed);
                }
            }
        }
    });
    let meta = PaneMeta {
        id: 0,
        profile: profile.to_string(),
        project: project.to_path_buf(),
        title: String::new(),
        shell: !claude,
        mode: Mode::Hidden,
        status: Status::Unknown,
        last_focus: 0,
    };
    Ok(Pane { meta, parser, writer, master: pair.master, child, dirty, reported, sysprompt_tmp, claude, nudge_at: Mutex::new(None), last_input: Mutex::new(std::time::Instant::now()), notified_at: None, sub_notified_at: None })
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
/// Chrome lines that aren't pane cells: the header, the rule above the
/// footer, and the key-hint footer.
const CHROME_ROWS: u16 = 3;
/// Below these a pane is too small for Claude's UI to be usable; they bound
/// how many panes fit.
const MIN_PANE_COLS: u16 = 24;
const MIN_PANE_ROWS: u16 = 6;
/// Most panes drawn at once — more is unreadable and a 3x3 grid is the limit
/// of what tiles sensibly.
const MAX_SHOWN: usize = 9;
/// Most panes open at once (shown or in the list); each is a live session.
const MAX_TOTAL: usize = 64;
/// The pane list's width and the narrowest terminal that gets it beside the
/// grid; below that it is an overlay on `F2` instead.
const SIDEBAR_W: u16 = 34;
const SIDEBAR_MIN_COLS: u16 = 100;

/// Whether the user hid the pane list box (it is then only reachable as a
/// full-width overlay, like on a narrow terminal).
static SIDEBAR_HIDDEN: AtomicBool = AtomicBool::new(false);

fn side_width_for(cols: u16, hidden: bool) -> u16 {
    if hidden || cols < SIDEBAR_MIN_COLS {
        0
    } else {
        SIDEBAR_W
    }
}

fn side_width(cols: u16) -> u16 {
    side_width_for(cols, SIDEBAR_HIDDEN.load(Ordering::Relaxed))
}

/// (x offset, width) of the area the pane grid occupies.
fn grid_area(cols: u16) -> (u16, u16) {
    let s = side_width(cols);
    if s == 0 {
        (0, cols)
    } else {
        (s + 1, cols - s - 1)
    }
}

/// Grid shape (rows, cols) for `n` panes — as square as possible, filling
/// left-to-right, top-to-bottom. n=2 stays a single row of two (side by
/// side); n=3/4 → 2x2; n=5/6 → 2x3; up to 3x3.
fn grid_dims(n: usize) -> (usize, usize) {
    let n = n.max(1);
    let gcols = (n as f64).sqrt().ceil() as usize;
    let grows = n.div_ceil(gcols);
    (grows, gcols)
}

/// Per-pane PTY size (cols, rows) for `n` panes tiled across a grid area of
/// `cols`x`rows` terminal rows. Each grid row also spends one line on the
/// pane labels, and adjacent grid rows are split by a rule.
fn pane_geometry(cols: u16, rows: u16, n: usize) -> (u16, u16) {
    let (grows, gcols) = grid_dims(n);
    let sep_w = (gcols as u16 - 1) * SEP_COLS;
    let pane_cols = cols.saturating_sub(sep_w) / gcols as u16;
    let chrome = CHROME_ROWS + (grows as u16 - 1);
    let cell_h = rows.saturating_sub(chrome) / grows as u16;
    (pane_cols.max(10), cell_h.saturating_sub(1).max(3))
}

/// Whether `n` panes tile `cols`x`rows` while every pane stays usable.
fn panes_fit(cols: u16, rows: u16, n: usize) -> bool {
    let (grows, gcols) = grid_dims(n);
    let pane_cols = cols.saturating_sub((gcols as u16 - 1) * SEP_COLS) / gcols as u16;
    let cell_h = rows.saturating_sub(CHROME_ROWS + (grows as u16 - 1)) / grows as u16;
    pane_cols >= MIN_PANE_COLS && cell_h.saturating_sub(1) >= MIN_PANE_ROWS
}

/// Largest pane count that still tiles a `cols`x`rows` grid area usably,
/// capped at `MAX_SHOWN` (and always at least 1 so a solo session is possible).
fn max_panes_for(cols: u16, rows: u16) -> usize {
    let mut best = 1;
    for n in 2..=MAX_SHOWN {
        if panes_fit(cols, rows, n) {
            best = n;
        }
    }
    best
}

/// How many panes can be drawn at once on a `cols`x`rows` terminal (the grid
/// area excludes the pane list).
fn shown_cap(cols: u16, rows: u16) -> usize {
    let (_, w) = grid_area(cols);
    max_panes_for(w, rows)
}

/// Where the grid's cells sit for `n` drawn panes.
struct Layout {
    main_x: u16,
    grows: usize,
    gcols: usize,
    pane_cols: u16,
    pane_rows: u16,
}

fn make_layout(cols: u16, rows: u16, n: usize) -> Layout {
    let (main_x, gw) = grid_area(cols);
    let (grows, gcols) = grid_dims(n);
    let (pane_cols, pane_rows) = pane_geometry(gw, rows, n);
    Layout { main_x, grows, gcols, pane_cols, pane_rows }
}

/// Queue box on the right of the pane picker: narrowest terminal that gets it,
/// and its width.
const QUEUE_BOX_MIN_COLS: u16 = 100;
const QUEUE_BOX_W: usize = 36;

/// The right-hand queue box, fully framed: one entry per queued pane
/// (profile + path), the next free slot, and a key hint. The frame turns
/// orange while the box has focus.
fn render_queue_box(queue: &[(String, std::path::PathBuf)], drawn_cap: usize, focused: bool, sel: usize) -> Vec<String> {
    let frame = |t: &str| if focused { ui::color::orange(t) } else { ui::color::dim(t) };
    let inner = QUEUE_BOX_W - 4; // two border columns + a space of padding each side
    let row = |content: &str| format!("{} {} {}", frame("│"), layout::pad_to(content, inner), frame("│"));
    let hint = if focused { "↑/↓ · Enter dir · x remove" } else { "Tab → focus queue" };
    let mut out = vec![
        frame(&format!("┌{}┐", "─".repeat(QUEUE_BOX_W - 2))),
        row(&ui::color::orange_bold(&format!("Queue {}", queue.len()))),
        row(&ui::color::dim(hint)),
        row(&ui::color::dim(&if queue.len() > drawn_cap {
            format!("{drawn_cap} shown, {} in the list", queue.len() - drawn_cap)
        } else {
            format!("up to {drawn_cap} shown at once")
        })),
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
    if queue.len() < MAX_TOTAL {
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
/// stays open until Start or the open-pane limit), and the queue is shown — as a box on
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
        let drawn_cap = shown_cap(term_cols, term_rows);
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
                header.push(format!("{} {}", ui::color::orange(ui::glyph::back()), ui::color::bold(&format!("Panes so far ({}, {drawn_cap} shown at once)", q.len()))));
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
            let box_lines = render_queue_box(queue.as_deref().unwrap(), drawn_cap, focus_q, qsel);
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
                        if q.len() < MAX_TOTAL {
                            q.push(pick);
                        }
                        // At the open-pane limit nothing more can be added,
                        // so launch.
                        if q.len() >= MAX_TOTAL {
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
    let cwd = std::env::current_dir()?;

    let mut chosen: Vec<(String, std::path::PathBuf)> = vec![(first.to_string(), cwd.clone())];
    // The picker keeps adding to `chosen` until "Start" (one pane = a solo
    // session, two or more = a split) or the open-pane limit; Esc cancels the
    // whole thing. Only the first few are drawn (as many as fit, at most 9);
    // the rest start in the pane list.
    if pick_pane_target(profiles, &cwd, "", Some(&mut chosen))?.is_none() {
        return Ok(None);
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
    let cap = shown_cap(cols, rows);
    let (_, gw) = grid_area(cols);
    let (pane_cols, pane_rows) = pane_geometry(gw, rows, targets.len().min(cap));

    // Each spawn opens a ConPTY and starts a process — noticeable serially
    // with several panes, so do them concurrently (order is preserved).
    let spawned: Vec<Result<Pane>> = std::thread::scope(|scope| {
        let handles: Vec<_> = targets
            .iter()
            .map(|(name, project)| scope.spawn(move || spawn_pane(name, project, pane_cols, pane_rows)))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap_or_else(|_| Err(anyhow::anyhow!("pane spawn panicked")))).collect()
    });
    let mut panes: Vec<Pane> = Vec::with_capacity(targets.len());
    for pane in spawned {
        match pane {
            Ok(p) => {
                // The first `cap` are drawn, the rest wait in the list.
                let idx = register(&mut panes, p);
                panes[idx].meta.mode = if idx < cap { Mode::Small } else { Mode::Hidden };
            }
            Err(e) => {
                for p in &mut panes {
                    let _ = p.child.kill();
                }
                return Err(e);
            }
        }
    }
    let mut focus = 0usize;
    sidebar::focus_pane(&mut panes, &mut focus, 0);

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
    settle(&mut session.panes, &mut session.focus, cols, rows);
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

/// Resizes the drawn panes to their cells for the current terminal size. A
/// full pane takes the whole grid area; the panes behind it keep their size
/// (they are not drawn) until it goes back to small. Hidden panes are never
/// resized.
fn relayout(panes: &[Pane], cols: u16, rows: u16) {
    let drawn = sidebar::drawn(panes);
    if drawn.is_empty() {
        return;
    }
    let (_, gw) = grid_area(cols);
    let (pc, pr) = pane_geometry(gw, rows, drawn.len());
    for &i in &drawn {
        resize_pane(&panes[i], pr, pc);
    }
}

fn mark_all(panes: &[Pane]) {
    for p in panes {
        p.dirty.store(true, Ordering::Relaxed);
    }
}

/// Restores the pane-state invariants for the current terminal size (at most
/// `shown_cap` drawn, at least one drawn, focus on a drawn pane), then
/// re-lays out and marks everything for repaint. Call after any change to the
/// pane set, a pane's mode, or the terminal size.
fn settle(panes: &mut [Pane], focus: &mut usize, cols: u16, rows: u16) {
    if panes.is_empty() {
        return;
    }
    sidebar::settle(panes, focus, shown_cap(cols, rows));
    relayout(panes, cols, rows);
    mark_all(panes);
}

/// Adds a spawned pane to the set: unique id and a title numbered within its
/// profile + project + kind. Returns its index.
fn register(panes: &mut Vec<Pane>, mut pane: Pane) -> usize {
    pane.meta.id = sidebar::next_id(panes);
    let kind = if pane.meta.shell { "shell" } else { "claude" };
    let mut n = 1;
    loop {
        let title = format!("{kind} {n}");
        let taken = panes.iter().any(|p| p.meta.profile == pane.meta.profile && p.meta.project == pane.meta.project && p.meta.title == title);
        if !taken {
            pane.meta.title = title;
            break;
        }
        n += 1;
    }
    panes.push(pane);
    panes.len() - 1
}

/// Puts a freshly spawned pane on screen: small (or full when another pane is
/// full), evicting if the grid is at its cap, and focuses it.
fn show_new(panes: &mut Vec<Pane>, focus: &mut usize, pane: Pane, cols: u16, rows: u16) -> Option<String> {
    let idx = register(panes, pane);
    let mode = if sidebar::full_index(panes).is_some() { Mode::Full } else { Mode::Small };
    let toast = sidebar::set_mode(panes, focus, idx, mode, shown_cap(cols, rows));
    if panes[idx].meta.mode != Mode::Hidden {
        sidebar::focus_pane(panes, focus, idx);
    }
    settle(panes, focus, cols, rows);
    toast
}

/// PTY size for a pane about to be added: one more than are drawn now, so it
/// usually starts at its final size.
fn spawn_geometry(panes: &[Pane], cols: u16, rows: u16) -> (u16, u16) {
    let (_, gw) = grid_area(cols);
    let n = (sidebar::shown_count(panes) + 1).min(shown_cap(cols, rows)).max(1);
    pane_geometry(gw, rows, n)
}

/// Opens the profile picker and, if one is chosen, spawns it as a new pane
/// and shows it (see `show_new`). Returns a message worth showing, if any.
fn add_pane(panes: &mut Vec<Pane>, focus: &mut usize, profiles: &[Profile], cwd: &std::path::Path) -> Result<Option<String>> {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    if panes.len() >= MAX_TOTAL {
        return Ok(Some("pane limit reached".to_string()));
    }
    let choice = pick_pane_target(profiles, cwd, "Add a pane — which profile?", None)?;
    ui::hide_cursor(); // the picker shows it again
    let mut toast = None;
    if let Some(PaneAdd::Pick(name, project)) = choice {
        let (pc, pr) = spawn_geometry(panes, cols, rows);
        let pane = spawn_pane(&name, &project, pc, pr)?;
        toast = show_new(panes, focus, pane, cols, rows);
    }
    mark_all(panes);
    Ok(toast)
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

/// Spawns a shell in `cwd` as a new pane, listed under the focused pane's
/// profile, and shows it. Returns a message worth showing, if any.
fn add_shell_pane(panes: &mut Vec<Pane>, focus: &mut usize, cwd: &std::path::Path) -> Result<Option<String>> {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    if panes.len() >= MAX_TOTAL {
        return Ok(Some("pane limit reached".to_string()));
    }
    let profile = panes[*focus].meta.profile.clone();
    let (pc, pr) = spawn_geometry(panes, cols, rows);
    let pane = spawn_shell_pane(&profile, cwd, pc, pr)?;
    let toast = show_new(panes, focus, pane, cols, rows);
    mark_all(panes);
    Ok(toast)
}

/// Kills pane `idx`'s child and drops it from the set, moving focus to the
/// most recently used pane if it was the focused one. Returns true when no
/// panes are left (the caller then exits). Callers `settle` afterwards.
fn remove_pane(panes: &mut Vec<Pane>, focus: &mut usize, idx: usize) -> bool {
    let mut pane = panes.remove(idx);
    let _ = pane.child.kill();
    if panes.is_empty() {
        return true;
    }
    if idx < *focus {
        *focus -= 1;
    } else if idx == *focus {
        *focus = (0..panes.len()).max_by_key(|&i| panes[i].meta.last_focus).unwrap_or(0);
    }
    if *focus >= panes.len() {
        *focus = panes.len() - 1;
    }
    false
}

/// State of the pane list while the run loop is going.
struct SideState {
    focus: bool,
    /// Key of the highlighted row (`sidebar::TreeRow::key`).
    cursor: String,
    collapsed: HashSet<String>,
    scroll: usize,
}

/// Keys while the pane list has focus. Returns (message to show, true when
/// the last pane was closed).
fn side_key(code: KeyCode, panes: &mut Vec<Pane>, focus: &mut usize, side: &mut SideState, cols: u16, rows: u16) -> (Option<String>, bool) {
    let tree = sidebar::build_tree(panes, &side.collapsed);
    if tree.is_empty() {
        side.focus = false;
        return (None, false);
    }
    let ci = tree.iter().position(|r| r.key() == side.cursor).unwrap_or(0);
    let cap = shown_cap(cols, rows);
    let mut toast = None;
    let mut quit = false;
    match code {
        KeyCode::Esc | KeyCode::F(2) => side.focus = false,
        KeyCode::Down => {
            if ci + 1 < tree.len() {
                side.cursor = tree[ci + 1].key().to_string();
            }
        }
        KeyCode::Up => {
            if ci > 0 {
                side.cursor = tree[ci - 1].key().to_string();
            }
        }
        KeyCode::Left => match &tree[ci] {
            sidebar::TreeRow::Pane { .. } => {
                if let Some(parent) = tree[..ci].iter().rev().find(|r| !matches!(r, sidebar::TreeRow::Pane { .. })) {
                    side.cursor = parent.key().to_string();
                }
            }
            row => {
                side.collapsed.insert(row.key().to_string());
            }
        },
        KeyCode::Right => {
            if !matches!(tree[ci], sidebar::TreeRow::Pane { .. }) {
                side.collapsed.remove(tree[ci].key());
            }
        }
        KeyCode::Enter => match &tree[ci] {
            sidebar::TreeRow::Pane { idx, .. } => {
                let idx = *idx;
                // Opening a pane shows it; if another pane is full, this one
                // takes over as the full pane.
                let full = sidebar::full_index(panes);
                if panes[idx].meta.mode == Mode::Hidden || full.is_some_and(|f| f != idx) {
                    let mode = if full.is_some() { Mode::Full } else { Mode::Small };
                    toast = sidebar::set_mode(panes, focus, idx, mode, cap);
                }
                if panes[idx].meta.mode != Mode::Hidden {
                    sidebar::focus_pane(panes, focus, idx);
                    side.focus = false;
                }
            }
            row => {
                let key = row.key().to_string();
                if !side.collapsed.remove(&key) {
                    side.collapsed.insert(key);
                }
            }
        },
        KeyCode::Char('f') => {
            if let sidebar::TreeRow::Pane { idx, .. } = &tree[ci] {
                let idx = *idx;
                toast = sidebar::toggle_full(panes, focus, idx, cap);
                if panes[idx].meta.mode != Mode::Hidden {
                    sidebar::focus_pane(panes, focus, idx);
                }
            }
        }
        KeyCode::Char('h') => {
            if let sidebar::TreeRow::Pane { idx, .. } = &tree[ci] {
                toast = sidebar::set_mode(panes, focus, *idx, Mode::Hidden, cap);
            }
        }
        KeyCode::Char('x') => {
            if let sidebar::TreeRow::Pane { idx, .. } = &tree[ci] {
                quit = remove_pane(panes, focus, *idx);
                if !quit {
                    let after = sidebar::build_tree(panes, &side.collapsed);
                    side.cursor = after[ci.min(after.len() - 1)].key().to_string();
                }
            }
        }
        _ => {}
    }
    if !quit {
        settle(panes, focus, cols, rows);
    }
    (toast, quit)
}

fn pane_label(p: &Pane, width: usize, focused: bool, full: bool) -> String {
    let suffix = if full { "  (full)" } else { "" };
    let badge = sidebar::status_badge(p.meta.status);
    let badge_w = badge.map_or(0, |(t, _)| t.chars().count() + 1);
    let mut text = format!(" {} · {}{suffix}", p.meta.title, sidebar::project_name(&p.meta.project));
    let room = width.saturating_sub(badge_w);
    if text.chars().count() > room {
        text = text.chars().take(room.saturating_sub(1)).collect::<String>() + "…";
    }
    let pad = width.saturating_sub(text.chars().count() + badge_w);
    let styled = if focused { ui::color::orange_bold(&text) } else { ui::color::dim(&text) };
    let tail = badge.map_or(String::new(), |(t, paint)| format!(" {}", paint(t)));
    format!("{styled}{}{tail}", " ".repeat(pad))
}

fn key_hints(pairs: &[(&str, &str)]) -> String {
    pairs.iter().map(|(k, l)| format!("{} {l}", ui::color::dim(k))).collect::<Vec<_>>().join("   ")
}

/// Builds one full frame (every screen line) plus the cursor escape: header,
/// pane list beside the grid (labels + pane rows), rule and key hints.
fn compose_frame(panes: &[Pane], focus: usize, cols: u16, rows: u16, side: &mut SideState, toast: Option<&str>, leader: bool) -> (Vec<String>, String) {
    let drawn = sidebar::drawn(panes);
    let full = sidebar::full_index(panes).is_some();
    let lay = make_layout(cols, rows, drawn.len().max(1));
    let body_h = rows.saturating_sub(CHROME_ROWS) as usize;
    let side_w = side_width(cols) as usize;
    let n = drawn.len();
    let pc = lay.pane_cols as usize;
    let total_w = lay.gcols * pc + (lay.gcols - 1) * SEP_COLS as usize;
    let blank = " ".repeat(pc);

    let mut main: Vec<String> = Vec::with_capacity(body_h);
    {
        let guards: Vec<_> = drawn.iter().map(|&i| panes[i].parser.lock().unwrap()).collect();
        for grow in 0..lay.grows {
            let labels: Vec<String> = (0..lay.gcols)
                .map(|gc| {
                    let pos = grow * lay.gcols + gc;
                    if pos < n { pane_label(&panes[drawn[pos]], pc, drawn[pos] == focus, full) } else { blank.clone() }
                })
                .collect();
            main.push(labels.join(SEP));
            for row in 0..lay.pane_rows {
                let cells: Vec<String> = (0..lay.gcols)
                    .map(|gc| {
                        let pos = grow * lay.gcols + gc;
                        if pos < n { render_row(guards[pos].screen(), row, lay.pane_cols) } else { blank.clone() }
                    })
                    .collect();
                main.push(cells.join(SEP));
            }
            if grow + 1 < lay.grows {
                main.push(ui::color::gray(&"─".repeat(total_w)));
            }
        }
    }
    main.truncate(body_h);
    while main.len() < body_h {
        main.push(String::new());
    }

    let tree = sidebar::build_tree(panes, &side.collapsed);
    let mut body: Vec<String> = Vec::with_capacity(body_h);
    if side_w > 0 {
        let sl = sidebar::render_sidebar(panes, &tree, &side.collapsed, &side.cursor, &mut side.scroll, side.focus, focus, side_w, body_h);
        for (l, m) in sl.into_iter().zip(main) {
            body.push(format!("{l} {m}"));
        }
    } else if side.focus {
        // Narrow terminal: the list takes over the screen until F2/Esc.
        body = sidebar::render_sidebar(panes, &tree, &side.collapsed, &side.cursor, &mut side.scroll, true, focus, cols as usize, body_h);
    } else {
        body = main;
    }

    let blocked = panes.iter().filter(|p| p.meta.status == Status::Blocked).count();
    let mut head = format!(
        " {}   shown {}/{}   total {}",
        ui::color::orange_bold("cpit panels"),
        sidebar::shown_count(panes),
        shown_cap(cols, rows),
        panes.len()
    );
    if blocked > 0 {
        head.push_str(&format!("   {}", ui::color::red(&format!("● {blocked} blocked"))));
    }
    if let Some(t) = toast {
        let room = (cols as usize).saturating_sub(layout::vis_len(&head) + t.chars().count() + 3);
        head.push_str(&" ".repeat(room.max(1)));
        head.push_str(&ui::color::orange(t));
    }

    let hints = if leader {
        format!(
            "{} {}",
            ui::color::orange_bold("Ctrl+\\ ›"),
            key_hints(&[
                ("l", "list"),
                ("h", "hide"),
                ("f", "full/small"),
                ("n", "new"),
                ("s", "shell"),
                ("x", "close"),
                ("w", "waiting"),
                ("b", "sidebar"),
                ("1-9", "jump"),
                ("←→↑↓", "move"),
                ("Esc", "cancel"),
            ])
        )
    } else if side.focus {
        key_hints(&[("↑/↓", "move"), ("←/→", "fold"), ("Enter", "open"), ("f", "full/small"), ("h", "to list"), ("x", "close"), ("F2/Esc", "back")])
    } else {
        key_hints(&[
            ("Ctrl+\\", "menu"),
            ("F2", "list"),
            ("F5", "sidebar"),
            ("F6", "switch"),
            ("F7", "add"),
            ("F9", "shell"),
            ("F10", if full { "unfull" } else { "full" }),
            ("F3", "hide"),
            ("F8", "close"),
            ("Ctrl+B", "back"),
            ("Ctrl+Q", "quit"),
        ])
    };

    let mut lines = Vec::with_capacity(rows as usize);
    lines.push(head);
    lines.extend(body);
    lines.push(ui::color::gray(&"─".repeat(cols as usize)));
    lines.push(format!(" {hints}"));

    let cursor = if side.focus { "\x1b[?25l".to_string() } else { cursor_escape(panes, focus, &lay, &drawn) };
    (lines, cursor)
}

/// Places the real terminal cursor at the focused pane's own cursor (so
/// typing shows a caret where Claude expects one), or hides it if that pane
/// has the cursor hidden. The pane at drawn position `pos` sits in grid cell
/// (pos / gcols, pos % gcols): column `main_x + gc*(pane_cols+SEP_COLS)`, and
/// below the header line, each grid row is a label line, `pane_rows` content
/// lines and a rule (all 1-based for the CUP escape).
/// Builds the cursor-positioning escape sequence without writing it — the
/// caller folds it into the same single write+flush as the frame's content
/// (see the comment on `ui::repaint_body` for why that matters: separate
/// flushed writes for hide/content/reposition is what produced a fast
/// blink instead of one clean cursor move per frame).
fn cursor_escape(panes: &[Pane], focus: usize, lay: &Layout, drawn: &[usize]) -> String {
    let Some(pos) = drawn.iter().position(|&i| i == focus) else { return "\x1b[?25l".to_string() };
    let (gr, gc) = ((pos / lay.gcols) as u16, (pos % lay.gcols) as u16);
    let Ok(p) = panes[focus].parser.lock() else { return String::new() };
    let screen = p.screen();
    if screen.hide_cursor() {
        "\x1b[?25l".to_string()
    } else {
        // Clamped defensively: a resize mid-frame can have this pane's vt100
        // buffer resized to pane_rows/pane_cols while cursor_position() still
        // briefly reports the pre-resize spot (or vice versa, if a caller
        // ever passes stale geometry). An unclamped value here computes a
        // CUP escape that lands outside this pane's cell entirely — into
        // another pane, the chrome lines, or off-screen — which is what
        // "cursor error" was.
        let (cr, cc) = screen.cursor_position();
        let cr = cr.min(lay.pane_rows.saturating_sub(1));
        let cc = cc.min(lay.pane_cols.saturating_sub(1));
        let term_row = gr * (lay.pane_rows + 2) + cr + 3;
        let term_col = lay.main_x + gc * (lay.pane_cols + SEP_COLS) + cc + 1;
        format!("\x1b[{term_row};{term_col}H\x1b[?25h")
    }
}

/// The command that, typed on its own line into a focused pane and followed
/// by Enter, opens a shell pane — the same as pressing F9. cockpit can't see
/// text once it's inside Claude, so it watches the keystrokes it forwards.
const SHELL_CMD: &str = "/shell";

/// How long a message (an eviction, a refused action) stays in the header.
const TOAST_TTL: Duration = Duration::from_millis(2500);

/// Everything a pane-manager command can do, whether it came from an F-key or
/// from the leader key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Act {
    List,
    Hide,
    Full,
    Add,
    Shell,
    Close,
    Waiting,
    Sidebar,
    Next,
    Prev,
    Jump(usize),
    Move(Dir),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dir {
    Left,
    Right,
    Up,
    Down,
}

/// Leader key: `Ctrl+\`. Claude doesn't use it, and it never reaches the pane
/// (press it twice to send a literal one). After it, one more key picks the
/// command (tmux-style), so everything is reachable without F-keys.
fn is_leader(code: KeyCode, ctrl: bool) -> bool {
    ctrl && code == KeyCode::Char('\\')
}

/// The command a key means right after the leader key.
fn leader_action(code: KeyCode) -> Option<Act> {
    match code {
        KeyCode::Char(c) => match c.to_ascii_lowercase() {
            'l' => Some(Act::List),
            'h' => Some(Act::Hide),
            'f' => Some(Act::Full),
            'n' => Some(Act::Add),
            's' => Some(Act::Shell),
            'x' => Some(Act::Close),
            'w' => Some(Act::Waiting),
            'b' => Some(Act::Sidebar),
            d @ '1'..='9' => Some(Act::Jump(d as usize - '0' as usize)),
            _ => None,
        },
        KeyCode::Left => Some(Act::Move(Dir::Left)),
        KeyCode::Right => Some(Act::Move(Dir::Right)),
        KeyCode::Up => Some(Act::Move(Dir::Up)),
        KeyCode::Down => Some(Act::Move(Dir::Down)),
        _ => None,
    }
}

/// The command an F-key means.
fn fkey_action(code: KeyCode, shift: bool) -> Option<Act> {
    match code {
        KeyCode::F(2) => Some(Act::List),
        KeyCode::F(3) => Some(Act::Hide),
        KeyCode::F(4) => Some(Act::Waiting),
        KeyCode::F(5) => Some(Act::Sidebar),
        KeyCode::F(6) => Some(if shift { Act::Prev } else { Act::Next }),
        KeyCode::F(7) => Some(Act::Add),
        KeyCode::F(8) => Some(Act::Close),
        KeyCode::F(9) => Some(Act::Shell),
        KeyCode::F(10) => Some(Act::Full),
        _ => None,
    }
}

/// Mutable state of the run loop that commands touch.
struct Ctl {
    /// What the user has typed into the focused pane since the last line
    /// break, so a lone "/shell"+Enter can be recognised. Reset whenever focus
    /// moves or a non-text key is pressed.
    typed: String,
    side: SideState,
    toast: Option<(String, std::time::Instant)>,
    /// Set when something other than pane output changed what's on screen.
    force: bool,
    /// Last frame's lines, so only changed rows are rewritten. Cleared
    /// whenever something else drew over the screen (picker, prompt) or the
    /// layout changed, which forces one full repaint.
    prev: Vec<String>,
    /// The leader key was pressed; the next key picks a command.
    leader: bool,
    /// Whether the terminal window has focus (focus reports); assumed true
    /// until the terminal says otherwise.
    window_focused: bool,
}

impl Ctl {
    fn say(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), std::time::Instant::now()));
    }
}

/// Runs one command. Returns true when the last pane was closed (the caller
/// then exits the split).
fn act(a: Act, ctl: &mut Ctl, panes: &mut Vec<Pane>, focus: &mut usize, profiles: &[Profile], cwd: &std::path::Path) -> Result<bool> {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let cap = shown_cap(cols, rows);
    ctl.typed.clear();
    ctl.force = true;
    // Moving focus keeps the same cells on screen; everything else can change
    // the layout (or drew a picker over it), so repaint in full.
    if !matches!(a, Act::List | Act::Next | Act::Prev | Act::Jump(_) | Act::Move(_)) {
        ctl.prev.clear();
    }
    match a {
        Act::List => {
            ctl.side.focus = !ctl.side.focus;
            if ctl.side.focus {
                ctl.side.cursor = sidebar::pane_key(panes[*focus].meta.id);
            }
        }
        Act::Sidebar => {
            let hidden = !SIDEBAR_HIDDEN.load(Ordering::Relaxed);
            SIDEBAR_HIDDEN.store(hidden, Ordering::Relaxed);
            settle(panes, focus, cols, rows);
            ctl.say(if hidden { "pane list hidden — open it with F2 or Ctrl+\\ l" } else { "pane list shown" });
        }
        Act::Hide => {
            let f = *focus;
            if let Some(t) = sidebar::set_mode(panes, focus, f, Mode::Hidden, cap) {
                ctl.say(t);
            }
            settle(panes, focus, cols, rows);
        }
        Act::Full => {
            let f = *focus;
            if let Some(t) = sidebar::toggle_full(panes, focus, f, cap) {
                ctl.say(t);
            }
            sidebar::focus_pane(panes, focus, f);
            settle(panes, focus, cols, rows);
        }
        Act::Add => {
            if let Some(t) = add_pane(panes, focus, profiles, cwd)? {
                ctl.say(t);
            }
        }
        Act::Shell => {
            if let Some(t) = add_shell_pane(panes, focus, cwd)? {
                ctl.say(t);
            }
        }
        Act::Close => {
            let f = *focus;
            if remove_pane(panes, focus, f) {
                return Ok(true);
            }
            settle(panes, focus, cols, rows);
        }
        Act::Waiting => {
            // Jumps to the next pane that is blocked or done — nothing reports
            // that yet, so for now it just says so.
            let waiting: Vec<usize> = (0..panes.len()).filter(|&i| i != *focus && matches!(panes[i].meta.status, Status::Blocked | Status::Done)).collect();
            match waiting.iter().copied().min_by_key(|&i| (panes[i].meta.status != Status::Blocked, panes[i].meta.last_focus)) {
                Some(i) => {
                    let mode = if sidebar::full_index(panes).is_some() { Mode::Full } else { Mode::Small };
                    if panes[i].meta.mode == Mode::Hidden || mode == Mode::Full {
                        if let Some(t) = sidebar::set_mode(panes, focus, i, mode, cap) {
                            ctl.say(t);
                        }
                    }
                    sidebar::focus_pane(panes, focus, i);
                    settle(panes, focus, cols, rows);
                }
                None => ctl.say("no pane is waiting"),
            }
        }
        Act::Next | Act::Prev | Act::Jump(_) | Act::Move(_) => {
            if sidebar::full_index(panes).is_some() {
                // Nothing else is drawn; use the list (F2) to bring another up.
                ctl.say("a pane is full — use the list (F2) to switch");
            } else {
                let drawn = sidebar::drawn(panes);
                let n = drawn.len();
                if let Some(pos) = drawn.iter().position(|&i| i == *focus) {
                    let gcols = make_layout(cols, rows, n.max(1)).gcols;
                    let target = match a {
                        Act::Next | Act::Move(Dir::Right) => Some((pos + 1) % n),
                        Act::Prev | Act::Move(Dir::Left) => Some((pos + n - 1) % n),
                        Act::Move(Dir::Down) => (pos + gcols < n).then_some(pos + gcols),
                        Act::Move(Dir::Up) => pos.checked_sub(gcols),
                        Act::Jump(k) => (k >= 1 && k <= n).then_some(k - 1),
                        _ => None,
                    };
                    if let Some(t) = target {
                        sidebar::focus_pane(panes, focus, drawn[t]);
                    }
                }
            }
        }
    }
    mark_all(panes);
    Ok(false)
}

/// Asks the terminal for focus-in/out reports while the split runs, so a
/// pane the user is looking at does not raise a notification only while the
/// window is actually in front.
fn run_loop(panes: &mut Vec<Pane>, focus: &mut usize, profiles: &[Profile], cwd: &std::path::Path) -> Result<LoopExit> {
    focus_reports(true);
    let result = run_loop_inner(panes, focus, profiles, cwd);
    focus_reports(false);
    result
}

fn focus_reports(on: bool) {
    let mut out = std::io::stdout();
    let _ = out.write_all(if on { b"\x1b[?1004h" } else { b"\x1b[?1004l" });
    let _ = out.flush();
}

fn run_loop_inner(panes: &mut Vec<Pane>, focus: &mut usize, profiles: &[Profile], cwd: &std::path::Path) -> Result<LoopExit> {
    let mut ctl = Ctl {
        typed: String::new(),
        side: SideState { focus: false, cursor: String::new(), collapsed: HashSet::new(), scroll: 0 },
        toast: None,
        force: true,
        prev: Vec::new(),
        leader: false,
        window_focused: true,
    };
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
        if ctl.toast.as_ref().is_some_and(|(_, at)| at.elapsed() > TOAST_TTL) {
            ctl.toast = None;
            ctl.force = true;
        }
        for (i, p) in panes.iter_mut().enumerate() {
            let (report, subagents) = match p.reported.lock() {
                Ok(mut r) => (r.status.clone(), r.take_finished()),
                Err(_) => (None, Vec::new()),
            };
            let st = report.as_ref().map(|r| r.status).unwrap_or(Status::Unknown);
            let watching = i == *focus && p.meta.mode != Mode::Hidden && ctl.window_focused;
            let project = sidebar::project_name(&p.meta.project);
            if st != p.meta.status {
                let prev = std::mem::replace(&mut p.meta.status, st);
                ctl.force = true;
                if let Some(kind) = notify::decide(prev, st, watching, p.notified_at, now) {
                    p.notified_at = Some(now);
                    let (block_kind, msg) = report.as_ref().map(|r| (r.kind.as_deref(), r.msg.as_deref())).unwrap_or((None, None));
                    notify::send(settings::notify_mode(), kind, &notify::message(&p.meta.title, &project, kind, block_kind, msg));
                }
            }
            // One notification for a burst of sub-agents, naming the last.
            if let Some(title) = subagents.last() {
                if let Some(kind) = notify::decide_subagent(watching, p.sub_notified_at, now) {
                    p.sub_notified_at = Some(now);
                    notify::send(settings::notify_mode(), kind, &notify::message(&p.meta.title, &project, kind, None, Some(title)));
                }
            }
        }
        // Swap every dirty flag (not short-circuiting) so one repaint clears
        // all of them and picks up every pane that changed this tick.
        let dirty = panes.iter().fold(false, |acc, p| acc | p.dirty.swap(false, Ordering::Relaxed)) | std::mem::take(&mut ctl.force);
        if dirty {
            let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
            // One write+flush for hide+content+reposition+show together —
            // hiding the cursor before the content overwrite (so the
            // redraw can't drag a visible cursor across it) used to be a
            // separate flushed write from the content and from the
            // reposition/show after it; three flushes a frame was enough
            // gap between "cursor off" and "back on" to look like a fast
            // blink instead of one clean frame.
            let toast = ctl.toast.as_ref().map(|(m, _)| m.clone());
            let (lines, cursor) = compose_frame(panes, *focus, cols, rows, &mut ctl.side, toast.as_deref(), ctl.leader);
            let body = ui::repaint_diff(&ctl.prev, &lines);
            ctl.prev = lines;
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
                    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
                    if ctrl && k.code == KeyCode::Char('q') {
                        return Ok(LoopExit::Quit);
                    }
                    // Ctrl+B detaches back to the cockpit with every pane left
                    // running (its reader thread keeps draining output); the
                    // cockpit can resume it, also with Ctrl+B.
                    if ctrl && k.code == KeyCode::Char('b') {
                        return Ok(LoopExit::Detach);
                    }
                    // The key after the leader picks a command; pressing the
                    // leader twice sends a literal one to the pane.
                    if ctl.leader {
                        ctl.leader = false;
                        ctl.force = true;
                        if is_leader(k.code, ctrl) {
                            if !ctl.side.focus {
                                send_to_pane(&panes[*focus], &[0x1c]);
                            }
                        } else if k.code != KeyCode::Esc {
                            match leader_action(k.code) {
                                Some(a) => {
                                    if act(a, &mut ctl, panes, focus, profiles, cwd)? {
                                        return Ok(LoopExit::Quit);
                                    }
                                }
                                None => ctl.say("unknown leader key — Esc cancels"),
                            }
                        }
                        continue;
                    }
                    if is_leader(k.code, ctrl) {
                        ctl.leader = true;
                        ctl.force = true;
                        continue;
                    }
                    // F2 moves keyboard focus between the grid and the pane
                    // list, F5 hides or shows the list box. (Tab/BackTab can't
                    // be used: they belong to the focused pane — Claude uses
                    // Shift+Tab itself.)
                    let fa = fkey_action(k.code, shift);
                    if ctl.side.focus {
                        if matches!(fa, Some(Act::List | Act::Sidebar)) {
                            if act(fa.unwrap(), &mut ctl, panes, focus, profiles, cwd)? {
                                return Ok(LoopExit::Quit);
                            }
                            continue;
                        }
                        let (t, quit) = side_key(k.code, panes, focus, &mut ctl.side, cols, rows);
                        if let Some(t) = t {
                            ctl.say(t);
                        }
                        if quit {
                            return Ok(LoopExit::Quit);
                        }
                        ctl.prev.clear();
                        ctl.force = true;
                        continue;
                    }
                    if let Some(a) = fa {
                        if act(a, &mut ctl, panes, focus, profiles, cwd)? {
                            return Ok(LoopExit::Quit);
                        }
                        continue;
                    }
                    // Typing "/shell" + Enter also opens a shell as a new pane
                    // without leaving the running Claude session(s).
                    if k.code == KeyCode::Enter && !ctrl && !alt && ctl.typed == SHELL_CMD {
                        // The "/shell" chars were already echoed into the
                        // pane as they were typed; erase them and swallow
                        // this Enter so Claude is left with a clean prompt.
                        if let Ok(mut w) = panes[*focus].writer.lock() {
                            let _ = w.write_all(&vec![0x7f; SHELL_CMD.chars().count()]);
                            let _ = w.flush();
                        }
                        if act(Act::Shell, &mut ctl, panes, focus, profiles, cwd)? {
                            return Ok(LoopExit::Quit);
                        }
                        continue;
                    }
                    // Track the current line so the "/shell" command above can
                    // be recognised. Live-forward keystrokes as usual.
                    match k.code {
                        KeyCode::Char(c) if !ctrl && !alt => ctl.typed.push(c),
                        KeyCode::Backspace if !ctrl && !alt => {
                            ctl.typed.pop();
                        }
                        KeyCode::Enter if !ctrl && !alt => ctl.typed.clear(),
                        _ => ctl.typed.clear(),
                    }
                    if let Some(bytes) = key_bytes(k.code, ctrl, alt) {
                        send_to_pane(&panes[*focus], &bytes);
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
                            Event::Key(k) if matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat) && !ctl.side.focus && !ctl.leader => {
                                if let Some(bytes) = key_bytes(k.code, k.modifiers.contains(KeyModifiers::CONTROL), k.modifiers.contains(KeyModifiers::ALT)) {
                                    send_to_pane(&panes[*focus], &bytes);
                                }
                            }
                            _ => {}
                        }
                    }
                    ctl.prev.clear();
                    settle(panes, focus, cols, rows);
                    ctl.force = true;
                }
                Event::FocusGained => ctl.window_focused = true,
                Event::FocusLost => ctl.window_focused = false,
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
                if remove_pane(panes, focus, i) {
                    return Ok(LoopExit::Quit);
                }
                removed = true;
            } else {
                i += 1;
            }
        }
        if removed {
            let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
            settle(panes, focus, cols, rows);
            ctl.prev.clear();
            ctl.force = true;
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
        let mut osc = osc7501::Scanner::default();
        let reported = Mutex::new(osc7501::Reports::default());
        for chunk in chunks {
            process_output(&mut carry, &mut osc, chunk, &parser, &writer, &reported);
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
        // 2 panes = 1x2: width (80-3)/2 = 38; cell height (24-3)/1 = 21,
        // minus the label line = 20 PTY rows.
        assert_eq!(pane_geometry(80, 24, 2), (38, 20));
        // 4 panes = 2x2: width (80-3)/2 = 38; cell height (24 - 3 - 1 rule)/2
        // = 10, minus the label line = 9.
        assert_eq!(pane_geometry(80, 24, 4), (38, 9));
    }

    #[test]
    fn leader_keys_map_to_commands_and_unknown_keys_do_nothing() {
        assert_eq!(leader_action(KeyCode::Char('l')), Some(Act::List));
        assert_eq!(leader_action(KeyCode::Char('L')), Some(Act::List));
        assert_eq!(leader_action(KeyCode::Char('b')), Some(Act::Sidebar));
        assert_eq!(leader_action(KeyCode::Char('1')), Some(Act::Jump(1)));
        assert_eq!(leader_action(KeyCode::Char('9')), Some(Act::Jump(9)));
        assert_eq!(leader_action(KeyCode::Char('0')), None);
        assert_eq!(leader_action(KeyCode::Left), Some(Act::Move(Dir::Left)));
        assert_eq!(leader_action(KeyCode::Char('z')), None);
        assert_eq!(leader_action(KeyCode::Esc), None);
    }

    #[test]
    fn fkeys_and_leader_reach_the_same_commands() {
        for (f, l) in [(2u8, 'l'), (3, 'h'), (4, 'w'), (5, 'b'), (7, 'n'), (8, 'x'), (9, 's'), (10, 'f')] {
            assert_eq!(fkey_action(KeyCode::F(f), false), leader_action(KeyCode::Char(l)), "F{f} vs leader {l}");
        }
        assert_eq!(fkey_action(KeyCode::F(6), false), Some(Act::Next));
        assert_eq!(fkey_action(KeyCode::F(6), true), Some(Act::Prev));
    }

    #[test]
    fn leader_is_ctrl_backslash_only() {
        assert!(is_leader(KeyCode::Char('\\'), true));
        assert!(!is_leader(KeyCode::Char('\\'), false));
        assert!(!is_leader(KeyCode::Char('a'), true));
    }

    #[test]
    fn hiding_the_pane_list_gives_the_grid_the_whole_width() {
        assert_eq!(side_width_for(200, false), SIDEBAR_W);
        assert_eq!(side_width_for(200, true), 0);
        assert_eq!(side_width_for(SIDEBAR_MIN_COLS - 1, false), 0);
    }

    #[test]
    fn max_panes_is_bounded_by_size_and_the_hard_cap() {
        assert_eq!(max_panes_for(40, 24), 1); // one 24-wide column barely fits, no room for two
        assert!(max_panes_for(200, 60) >= 4); // a big terminal fits a grid
        assert!(max_panes_for(10000, 10000) <= MAX_SHOWN); // capped
    }

    #[test]
    fn status_query_is_answered_and_reports_are_recorded() {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let writer: Mutex<Box<dyn Write + Send>> = Mutex::new(Box::new(Recorder(writes.clone())));
        let parser = Mutex::new(vt100::Parser::new(24, 80, 0));
        let (mut carry, mut osc, reported) = (Vec::new(), osc7501::Scanner::default(), Mutex::new(osc7501::Reports::default()));
        process_output(&mut carry, &mut osc, b"hi\x1b]7501;?\x1b\\", &parser, &writer, &reported);
        process_output(&mut carry, &mut osc, b"\x1b]7501;state=wor", &parser, &writer, &reported);
        process_output(&mut carry, &mut osc, b"king:app=claude-code\x07", &parser, &writer, &reported);
        assert_eq!(writes.lock().unwrap().as_slice(), &[osc7501::QUERY_REPLY.to_vec()]);
        assert_eq!(reported.lock().unwrap().status(), Status::Working);
        assert!(parser.lock().unwrap().screen().contents().contains("hi"));
    }
}
