// One place that reads a key. Unlike the JS version, raw mode is enabled
// exactly once for the whole session (in ui::Terminal::enter) and every
// screen just calls this blocking read in a loop — there is no per-screen
// enable/disable to get wrong, which is what caused the stdin-wedging bug.
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use std::sync::atomic::{AtomicBool, Ordering};

pub struct Key {
    pub code: KeyCode,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
}

/// j/k/h/l navigation is a Settings toggle. With it off, those letters are
/// turned into a no-op (`Null`) on list screens. `keep` lists letters that are
/// real actions on that screen (hide/handoff/Keep, ...) and must still pass.
/// Only call this from navigation screens, never from text entry.
pub fn gate_vim(mut key: Key, keep: &[char]) -> Key {
    if !key.ctrl && !crate::settings::vim_bindings() {
        if let KeyCode::Char(c @ ('j' | 'k' | 'h' | 'l')) = key.code {
            if !keep.contains(&c) {
                key.code = KeyCode::Null;
            }
        }
    }
    key
}

static LAST_G: AtomicBool = AtomicBool::new(false);

/// Vim-style jumps for a list cursor: `gg` top, `G` bottom, Ctrl+d / Ctrl+u
/// half a page. Only active with the Vim bindings setting on. Returns true when
/// the key was one of these (the caller should just redraw). `allow_gg` is off
/// on screens where a plain `g` already means something else.
pub fn vim_jump(key: &Key, selected: &mut usize, len: usize, half: usize, allow_gg: bool) -> bool {
    let g_prev = LAST_G.swap(false, Ordering::Relaxed);
    if len == 0 || !crate::settings::vim_bindings() {
        return false;
    }
    let last = len - 1;
    match key.code {
        KeyCode::Char('d') if key.ctrl => *selected = (*selected + half).min(last),
        KeyCode::Char('u') if key.ctrl => *selected = selected.saturating_sub(half),
        KeyCode::Char('G') if !key.ctrl => *selected = last,
        KeyCode::Char('g') if !key.ctrl && allow_gg => {
            if g_prev {
                *selected = 0;
            } else {
                LAST_G.store(true, Ordering::Relaxed);
            }
        }
        _ => return false,
    }
    true
}

pub fn read_key() -> std::io::Result<Key> {
    loop {
        match event::read()? {
            Event::Key(k) => {
                // Windows can report both Press and Release under some
                // terminal modes; only act on Press (and held-key Repeat).
                if matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                    return Ok(Key {
                        code: k.code,
                        ctrl: k.modifiers.contains(KeyModifiers::CONTROL),
                        shift: k.modifiers.contains(KeyModifiers::SHIFT),
                        alt: k.modifiers.contains(KeyModifiers::ALT),
                    });
                }
            }
            // Every screen's loop is render-then-read-key, so returning
            // here on a resize (instead of silently looping past it, which
            // left every screen frozen on stale, wrongly-sized content
            // until the next real keypress) makes that same loop redraw
            // immediately at the new size. `Null` isn't matched by any
            // screen's key handling, so it falls through their `_ => {}`
            // catch-all as a harmless no-op — the point is the redraw
            // that already happens right before this call returns.
            Event::Resize(_, _) => {
                return Ok(Key { code: KeyCode::Null, ctrl: false, shift: false, alt: false });
            }
            _ => {}
        }
    }
}
