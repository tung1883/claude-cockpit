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
use crate::{launch, picker, profiles::Profile, ui};
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
}

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
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })?;
    let (cmd, sysprompt_tmp) = launch::claude_pty_command(profile, cwd);
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
    Ok(Pane { label, parser, writer, master: pair.master, child, dirty, sysprompt_tmp })
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
    for col in 0..width {
        let Some(cell) = screen.cell(row, col) else {
            out.push(' ');
            continue;
        };
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
        let contents = cell.contents();
        let ch = if contents.is_empty() { " ".to_string() } else { contents };
        out.push_str(&format!("\x1b[0;{}m{ch}", codes.join(";")));
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

/// N Claude sessions side by side, `first` plus profiles chosen here. Panes
/// can be added (F7) and closed (F8) while running, and a pane whose session
/// exits on its own is dropped without disturbing the others; the split ends
/// only when the last pane is gone (or Ctrl+Q). v1 scope: no system-prompt
/// composition for the panes (plain launches only).
pub fn run_split(profiles: &[Profile], first: &str) -> Result<Option<Session>> {
    const START: &str = "\0start"; // sentinel row value; no profile can be named this
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let cap = max_panes_for(cols, rows);

    let mut chosen: Vec<String> = vec![first.to_string()];
    while chosen.len() < cap {
        // A "Start" row launches with whatever's chosen so far (one pane =
        // a solo session, two or more = a split); picking a profile adds
        // another pane; Esc cancels the whole thing.
        let n = chosen.len();
        let start_label = if n == 1 { "Start (solo session)".to_string() } else { format!("Start ({n} panes)") };
        let mut options = vec![picker::ListOption { label: start_label, value: START.to_string(), note: None }];
        options.extend(
            profiles.iter().map(|p| picker::ListOption { label: p.name.clone(), value: p.name.clone(), note: None }),
        );
        let heading = format!("Panes so far: {}", chosen.join(", "));
        match picker::choose_from_list(profiles, -1, &heading, Some("Enter to add a pane · pick Start to launch · Esc to cancel"), &options, None)? {
            picker::ListChoice::Picked(v) if v == START => break,
            picker::ListChoice::Picked(v) => chosen.push(v),
            _ => return Ok(None),
        }
    }
    run_split_profiles(profiles, &chosen)
}

/// Run a split of exactly the given profiles (one pane each, in order).
/// `profiles` is the full account list, used only to offer choices when a
/// pane is added mid-split. Split out from the picker so it can be driven
/// directly (tests, a CLI entry).
///
/// Returns `Some(Session)` if the user detached (Ctrl+B) — the panes keep
/// running in the background and the cockpit can `resume` them later — or
/// `None` if they quit (Ctrl+Q) or the last pane exited.
pub fn run_split_profiles(profiles: &[Profile], profile_names: &[String]) -> Result<Option<Session>> {
    let cwd = std::env::current_dir()?;
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let (pane_cols, pane_rows) = pane_geometry(cols, rows, profile_names.len());

    let mut panes: Vec<Pane> = Vec::with_capacity(profile_names.len());
    for name in profile_names {
        panes.push(spawn_pane(name, name.clone(), &cwd, pane_cols, pane_rows)?);
    }
    let mut focus = 0usize;

    ui::hide_cursor();
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

/// Re-lays every pane for the current pane count and terminal size, and marks
/// them dirty so the next tick repaints. Call after a pane is added, closed,
/// or the terminal is resized.
fn reflow(panes: &[Pane], cols: u16, rows: u16) {
    let (pc, pr) = pane_geometry(cols, rows, panes.len());
    for pane in panes {
        let _ = pane.master.resize(PtySize { rows: pr, cols: pc, pixel_width: 0, pixel_height: 0 });
        if let Ok(mut p) = pane.parser.lock() {
            p.set_size(pr, pc);
        }
        pane.dirty.store(true, Ordering::Relaxed);
    }
}

/// Opens the profile picker and, if one is chosen, spawns it as a new pane
/// appended to the grid and focused. No-op if already at the size-based cap.
fn add_pane(panes: &mut Vec<Pane>, focus: &mut usize, profiles: &[Profile], cwd: &std::path::Path) -> Result<()> {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    if panes.len() >= max_panes_for(cols, rows) {
        return Ok(()); // no room for another usable pane
    }
    let options: Vec<picker::ListOption> =
        profiles.iter().map(|p| picker::ListOption { label: p.name.clone(), value: p.name.clone(), note: None }).collect();
    let choice = picker::choose_from_list(profiles, -1, "Add a pane — which profile?", Some("Enter to add · Esc to cancel"), &options, None)?;
    ui::hide_cursor(); // the picker shows it again
    if let picker::ListChoice::Picked(name) = choice {
        let (pc, pr) = pane_geometry(cols, rows, panes.len() + 1);
        let pane = spawn_pane(&name, name.clone(), cwd, pc, pr)?;
        for p in panes.iter() {
            let _ = p.master.resize(PtySize { rows: pr, cols: pc, pixel_width: 0, pixel_height: 0 });
            if let Ok(mut parser) = p.parser.lock() {
                parser.set_size(pr, pc);
            }
        }
        panes.push(pane);
        *focus = panes.len() - 1;
    }
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
fn draw_cursor(panes: &[Pane], focus: usize, pane_rows: u16, pane_cols: u16) {
    let (_, gcols) = grid_dims(panes.len());
    let (gr, gc) = ((focus / gcols) as u16, (focus % gcols) as u16);
    let mut out = std::io::stdout();
    if let Ok(p) = panes[focus].parser.lock() {
        let screen = p.screen();
        if screen.hide_cursor() {
            let _ = out.write_all(b"\x1b[?25l");
        } else {
            let (cr, cc) = screen.cursor_position();
            let term_row = 1 + gr * (pane_rows + 1) + cr + 1;
            let term_col = gc * (pane_cols + SEP_COLS) + cc + 1;
            let _ = write!(out, "\x1b[{term_row};{term_col}H\x1b[?25h");
        }
    }
    let _ = out.flush();
}

/// Drops into a real interactive shell in `cwd`, then returns. Leaves the
/// compositor's raw mode + alternate screen so the shell gets a normal
/// cooked console on the primary buffer, and restores both on the way back.
/// Callers repaint afterward (the alt buffer's contents are discarded on
/// re-entry).
fn drop_to_shell(cwd: &std::path::Path) -> Result<()> {
    let mut out = std::io::stdout();
    // Back to the primary buffer, cursor shown, cooked input — a plain shell.
    let _ = out.write_all(b"\x1b[?25h\x1b[?1049l");
    let _ = out.flush();
    let _ = crossterm::terminal::disable_raw_mode();

    println!("{} shell — type 'exit' to return to your session", ui::color::dim(ui::glyph::spark()));
    let result = launch::open_shell(cwd);

    let _ = crossterm::terminal::enable_raw_mode();
    let _ = out.write_all(b"\x1b[?1049h\x1b[H\x1b[?25l");
    let _ = out.flush();
    result
}

/// The command that, typed on its own line into a focused pane and followed
/// by Enter, drops to a shell — the same as pressing F9. cockpit can't see
/// text once it's inside Claude, so it watches the keystrokes it forwards.
const SHELL_CMD: &str = "/shell";

fn run_loop(panes: &mut Vec<Pane>, focus: &mut usize, profiles: &[Profile], cwd: &std::path::Path) -> Result<LoopExit> {
    // What the user has typed into the focused pane since the last line break,
    // so a lone "/shell"+Enter can be recognised. Reset whenever focus moves
    // or a non-text key is pressed.
    let mut typed = String::new();
    loop {
        // Swap every dirty flag (not short-circuiting) so one repaint clears
        // all of them and picks up every pane that changed this tick.
        let dirty = panes.iter().fold(false, |acc, p| acc | p.dirty.swap(false, Ordering::Relaxed));
        if dirty {
            let n = panes.len();
            let (grows, gcols) = grid_dims(n);
            let guards: Vec<_> = panes.iter().map(|p| p.parser.lock().unwrap()).collect();
            let (pane_rows, pane_cols) = guards[0].screen().size();
            let labels: Vec<String> = panes
                .iter()
                .enumerate()
                .map(|(i, p)| if i == *focus { ui::color::orange_bold(&p.label) } else { p.label.clone() })
                .collect();
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
            lines.push(format!(
                " {} switch   {} add   {} close   {} shell   {} back   {} quit",
                ui::color::dim("F6"),
                ui::color::dim("F7"),
                ui::color::dim("F8"),
                ui::color::dim("F9"),
                ui::color::dim("Ctrl+B"),
                ui::color::dim("Ctrl+Q"),
            ));
            drop(guards);
            ui::repaint(&lines);
            draw_cursor(panes, *focus, pane_rows, pane_cols);
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
                        drop_to_shell(cwd)?;
                        for pane in panes.iter() {
                            pane.dirty.store(true, Ordering::Relaxed);
                        }
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
                Event::Resize(new_cols, new_rows) => reflow(panes, new_cols, new_rows),
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
            reflow(panes, cols, rows);
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
