use crate::input::read_key;
use crate::layout::{self, Stat};
use crate::profiles::Profile;
use crate::ui;
use crossterm::event::KeyCode;
use std::io::Write;
use std::path::Path;

// `col` is tracked as a character index (not byte index) throughout, so
// typing non-ASCII text (e.g. Vietnamese) doesn't panic on a String method
// expecting a char boundary — these two helpers convert at the edges.
fn char_len(s: &str) -> usize {
    s.chars().count()
}
fn byte_at(s: &str, char_idx: usize) -> usize {
    s.char_indices().nth(char_idx).map(|(b, _)| b).unwrap_or(s.len())
}

/// 3 classes instead of just whitespace/non-whitespace — matches how every
/// real editor draws word boundaries for Ctrl+Left/Right/Backspace, so
/// `foo(bar)` stops at `(` instead of `foo(` being eaten as one "word".
#[derive(PartialEq)]
enum CharClass {
    Space,
    Word,
    Punct,
}
fn class_of(c: char) -> CharClass {
    if c.is_whitespace() {
        CharClass::Space
    } else if c.is_alphanumeric() || c == '_' {
        CharClass::Word
    } else {
        CharClass::Punct
    }
}

/// Where Ctrl+Left / the left side of a Ctrl+Backspace deletion lands:
/// skip trailing whitespace, then skip one run of a single char class.
fn word_left(chars: &[char], col: usize) -> usize {
    let mut c = col;
    while c > 0 && class_of(chars[c - 1]) == CharClass::Space {
        c -= 1;
    }
    if c > 0 {
        let class = class_of(chars[c - 1]);
        while c > 0 && class_of(chars[c - 1]) == class {
            c -= 1;
        }
    }
    c
}

/// Mirror of `word_left` for Ctrl+Right.
fn word_right(chars: &[char], col: usize) -> usize {
    let mut c = col;
    while c < chars.len() && class_of(chars[c]) == CharClass::Space {
        c += 1;
    }
    if c < chars.len() {
        let class = class_of(chars[c]);
        while c < chars.len() && class_of(chars[c]) == class {
            c += 1;
        }
    }
    c
}

/// Runs of space / non-space, alternating, covering the whole line — the
/// unit `wrap_points` packs so a wrap never lands inside a word.
fn tokenize(chars: &[char]) -> Vec<(usize, usize)> {
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let start = i;
        let is_space = chars[i] == ' ';
        while i < chars.len() && (chars[i] == ' ') == is_space {
            i += 1;
        }
        tokens.push((start, i));
    }
    tokens
}

/// Soft-wrap points (start char-index of each visual segment, always
/// starting with 0) for one line, shared by the renderer (building each
/// visual row's char range) and cursor placement (which visual row `col`
/// falls on) so the two can never disagree. Wraps on a space boundary —
/// never splitting a word mid-way — unless a single word alone is longer
/// than `width`, which has no choice but a hard split. This is what every
/// real editor does, instead of horizontally scrolling or letting the
/// terminal hard-wrap at a raw column count (which both desyncs
/// row/cursor math that assumes one buffer line = one screen row, and —
/// worse — chops words apart at whatever column the wrap happens to land
/// on, e.g. "short" becoming "s" / "hort").
fn wrap_points(chars: &[char], width: usize) -> Vec<usize> {
    if chars.is_empty() {
        return vec![0];
    }
    let mut starts = vec![0usize];
    let mut col = 0usize;
    for (t_start, t_end) in tokenize(chars) {
        let mut remaining = t_end - t_start;
        if col > 0 && col + remaining > width {
            starts.push(t_start);
            col = 0;
        }
        if remaining > width {
            let mut pos = t_start;
            while remaining > 0 {
                let take = width.min(remaining);
                pos += take;
                remaining -= take;
                col = take;
                if remaining > 0 {
                    starts.push(pos);
                    col = 0;
                }
            }
        } else {
            col += remaining;
        }
    }
    starts
}

/// Which segment (0-based) of `wrap_points(chars, ..)` a given `col`
/// falls on — the last segment when `col` is at or past the line's end,
/// so the cursor sits right after the last typed character rather than
/// implying a not-yet-existing extra wrapped row.
fn seg_index_for_col(points: &[usize], len: usize, col: usize) -> usize {
    if col >= len {
        return points.len() - 1;
    }
    match points.binary_search(&col) {
        Ok(k) => k,
        Err(k) => k.saturating_sub(1),
    }
}

/// [start, end) char range shown on segment `seg`, given its wrap points.
fn seg_range(points: &[usize], len: usize, seg: usize) -> (usize, usize) {
    let start = points[seg];
    let end = points.get(seg + 1).copied().unwrap_or(len);
    (start, end)
}

fn read_safe_lines(file: &Path) -> Vec<String> {
    let content = std::fs::read_to_string(file).unwrap_or_default();
    let lines: Vec<String> = content.split('\n').map(|s| s.trim_end_matches('\r').to_string()).collect();
    if lines.is_empty() {
        vec![String::new()]
    } else {
        lines
    }
}

/// A small in-terminal text editor — no shelling out to notepad/vim. Shows
/// the real terminal cursor (positioned with a raw ANSI move after each
/// repaint) instead of a drawn `❯` pointer, which is what makes it feel like
/// an editor rather than a menu.
pub fn edit_file(file: &Path, profiles: &[Profile], mark: i64, title: Option<&str>) -> std::io::Result<()> {
    let mut lines = read_safe_lines(file);
    let mut row: usize = 0;
    let mut col: usize = 0;
    // A *visual*-row index now, not a buffer-line index — see the
    // `display_rows`/soft-wrap comment below.
    let mut top: usize = 0;
    let mut dirty = false;
    let mut save_error: Option<String> = None;
    let stats: Vec<Stat> = profiles.iter().map(|p| layout::account_stat(&p.name)).collect();

    let mut save = |lines: &[String], dirty: &mut bool, save_error: &mut Option<String>| -> bool {
        // `join` alone round-trips exactly what `read_safe_lines` split out
        // (its trailing empty element already stands for the file's final
        // newline) — unconditionally appending another "\n" on top double-
        // counted it, so every edit/save cycle grew one more blank line at
        // the end. Only add one when the buffer truly has none.
        let mut content = lines.join("\n");
        if !content.ends_with('\n') {
            content.push('\n');
        }
        match std::fs::write(file, content) {
            Ok(()) => {
                *dirty = false;
                *save_error = None;
                true
            }
            Err(e) => {
                *save_error = Some(e.to_string());
                false
            }
        }
    };

    ui::show_cursor();
    loop {
        row = row.min(lines.len().saturating_sub(1));
        col = col.min(char_len(&lines[row]));

        let mut header = layout::account_lines(profiles, mark, &stats);
        let display_title = title.map(str::to_string).unwrap_or_else(|| {
            file.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
        });
        let dirty_tag = if dirty { ui::color::yellow(" •  unsaved") } else { String::new() };
        header.push(format!("{} {}{}", ui::color::orange(ui::glyph::back()), ui::color::bold(&display_title), dirty_tag));
        header.push(ui::color::dim(&format!("   {}", file.display())));
        header.push(String::new());

        let rows_avail = crossterm::terminal::size().map(|(_, h)| h as usize).unwrap_or(24);
        let viewport = (rows_avail.saturating_sub(header.len() + 5)).max(5);
        let gutter = 4; // "NNN " line-number column
        let cols_avail = crossterm::terminal::size().map(|(w, _)| w as usize).unwrap_or(80).saturating_sub(gutter).max(1);

        // Every buffer line's char length turned into (line_idx, start,
        // end) segments of at most `cols_avail` chars — a long line splits
        // into several consecutive entries instead of one that would
        // either get cut off (`ui::repaint_body`'s own clip) or hard-wrap
        // in a way that desyncs this scroll/cursor math, which treats each
        // entry as exactly one screen row.
        let mut display_rows: Vec<(usize, usize, usize)> = Vec::new();
        let mut cursor_display_row = 0usize;
        let mut cursor_seg_start = 0usize;
        for (i, line) in lines.iter().enumerate() {
            let chars: Vec<char> = line.chars().collect();
            let len = chars.len();
            let points = wrap_points(&chars, cols_avail);
            if i == row {
                let seg = seg_index_for_col(&points, len, col);
                cursor_display_row = display_rows.len() + seg;
                cursor_seg_start = points[seg];
            }
            for seg in 0..points.len() {
                let (s, e) = seg_range(&points, len, seg);
                display_rows.push((i, s, e));
            }
        }

        if cursor_display_row < top {
            top = cursor_display_row;
        }
        if cursor_display_row >= top + viewport {
            top = cursor_display_row - viewport + 1;
        }
        top = top.min(display_rows.len().saturating_sub(viewport));

        let mut out = header.clone();
        let end = (top + viewport).min(display_rows.len());
        if top > 0 {
            out.push(ui::color::dim(&format!("    ↑ {top} more")));
        }
        for &(i, start, seg_end) in display_rows.iter().take(end).skip(top) {
            let seg_text = &lines[i][byte_at(&lines[i], start)..byte_at(&lines[i], seg_end)];
            let num = if start == 0 { format!("{:>3}", i + 1) } else { "   ".to_string() };
            out.push(format!("{} {seg_text}", ui::color::dim(&num)));
        }
        if end < display_rows.len() {
            out.push(ui::color::dim(&format!("    ↓ {} more", display_rows.len() - end)));
        }
        out.push(String::new());
        out.push(ui::rule(None));
        if let Some(err) = &save_error {
            out.push(format!(" {}", ui::color::red(&format!("Save failed: {err}"))));
        }
        out.push(format!(
            " {} save   {} save & exit   {} quit without saving",
            ui::color::dim("Ctrl+S"),
            ui::color::dim("Esc"),
            ui::color::dim("Ctrl+C"),
        ));
        ui::repaint(&out);
        let term_row = header.len() + 1 + (cursor_display_row - top) + usize::from(top > 0);
        let term_col = 1 + gutter + (col - cursor_seg_start);
        print!("\x1b[{term_row};{term_col}H");
        std::io::stdout().flush()?;

        let key = read_key()?;
        if key.ctrl && key.code == KeyCode::Char('c') {
            // Matches the footer's own promise ("Ctrl+C quit without saving")
            // — discard the buffer and return to whatever opened the editor,
            // not exit the whole cockpit.
            ui::show_cursor();
            return Ok(());
        }
        match key.code {
            KeyCode::Char('s') if key.ctrl => {
                save(&lines, &mut dirty, &mut save_error);
            }
            // Esc always saves & exits — matches the footer hint. A failed
            // save (locked file, etc.) keeps you in the editor with the
            // buffer intact instead of losing it; Ctrl+C discards on purpose.
            KeyCode::Esc => {
                if !dirty || save(&lines, &mut dirty, &mut save_error) {
                    ui::show_cursor();
                    return Ok(());
                }
            }
            KeyCode::Up if key.alt => {
                if row > 0 {
                    lines.swap(row, row - 1);
                    row -= 1;
                    dirty = true;
                }
            }
            KeyCode::Down if key.alt => {
                if row + 1 < lines.len() {
                    lines.swap(row, row + 1);
                    row += 1;
                    dirty = true;
                }
            }
            // Paragraph jump: skip any blank lines adjacent in that
            // direction first, then the run of non-blank lines after them —
            // lands on the next blank/non-blank boundary, same convention
            // most editors use for Ctrl+Up/Down.
            KeyCode::Up if key.ctrl => {
                let mut r = row;
                while r > 0 && lines[r - 1].trim().is_empty() {
                    r -= 1;
                }
                while r > 0 && !lines[r - 1].trim().is_empty() {
                    r -= 1;
                }
                row = r;
                col = col.min(char_len(&lines[row]));
            }
            KeyCode::Down if key.ctrl => {
                let mut r = row;
                while r + 1 < lines.len() && lines[r + 1].trim().is_empty() {
                    r += 1;
                }
                while r + 1 < lines.len() && !lines[r + 1].trim().is_empty() {
                    r += 1;
                }
                row = r;
                col = col.min(char_len(&lines[row]));
            }
            KeyCode::Left if key.ctrl => {
                if col == 0 {
                    if row > 0 {
                        row -= 1;
                        col = char_len(&lines[row]);
                    }
                } else {
                    let chars: Vec<char> = lines[row].chars().collect();
                    col = word_left(&chars, col);
                }
            }
            KeyCode::Right if key.ctrl => {
                let chars: Vec<char> = lines[row].chars().collect();
                if col >= chars.len() {
                    if row + 1 < lines.len() {
                        row += 1;
                        col = 0;
                    }
                } else {
                    col = word_right(&chars, col);
                }
            }
            KeyCode::Up => row = row.saturating_sub(1),
            KeyCode::Down => row += 1,
            KeyCode::Left => {
                if col > 0 {
                    col -= 1;
                } else if row > 0 {
                    row -= 1;
                    col = char_len(&lines[row]);
                }
            }
            KeyCode::Right => {
                if col < char_len(&lines[row]) {
                    col += 1;
                } else if row < lines.len() - 1 {
                    row += 1;
                    col = 0;
                }
            }
            KeyCode::Home => col = 0,
            KeyCode::End => col = char_len(&lines[row]),
            KeyCode::PageUp => row = row.saturating_sub(10),
            KeyCode::PageDown => row += 10,
            KeyCode::Enter => {
                let b = byte_at(&lines[row], col);
                let rest = lines[row].split_off(b);
                lines.insert(row + 1, rest);
                row += 1;
                col = 0;
                dirty = true;
            }
            // Deletes the word behind the cursor (trailing whitespace, then
            // the run of non-whitespace before it) instead of one char.
            // Falls back to the plain join-with-previous-line behavior at
            // column 0, same as a regular Backspace there.
            KeyCode::Backspace if key.ctrl => {
                if col == 0 {
                    if row > 0 {
                        let cur = lines.remove(row);
                        col = char_len(&lines[row - 1]);
                        lines[row - 1].push_str(&cur);
                        row -= 1;
                        dirty = true;
                    }
                } else {
                    let chars: Vec<char> = lines[row].chars().collect();
                    let start = word_left(&chars, col);
                    let b_start = byte_at(&lines[row], start);
                    let b_end = byte_at(&lines[row], col);
                    lines[row].replace_range(b_start..b_end, "");
                    col = start;
                    dirty = true;
                }
            }
            KeyCode::Backspace => {
                if col > 0 {
                    let b = byte_at(&lines[row], col - 1);
                    lines[row].remove(b);
                    col -= 1;
                    dirty = true;
                } else if row > 0 {
                    let cur = lines.remove(row);
                    col = char_len(&lines[row - 1]);
                    lines[row - 1].push_str(&cur);
                    row -= 1;
                    dirty = true;
                }
            }
            KeyCode::Delete => {
                if col < char_len(&lines[row]) {
                    let b = byte_at(&lines[row], col);
                    lines[row].remove(b);
                    dirty = true;
                } else if row < lines.len() - 1 {
                    let next = lines.remove(row + 1);
                    lines[row].push_str(&next);
                    dirty = true;
                }
            }
            KeyCode::Tab => {
                let b = byte_at(&lines[row], col);
                lines[row].insert_str(b, "  ");
                col += 2;
                dirty = true;
            }
            KeyCode::Char(c) if !key.ctrl => {
                let b = byte_at(&lines[row], col);
                lines[row].insert(b, c);
                col += 1;
                dirty = true;
            }
            _ => {}
        }
    }
}
