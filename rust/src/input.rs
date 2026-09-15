// One place that reads a key. Unlike the JS version, raw mode is enabled
// exactly once for the whole session (in ui::Terminal::enter) and every
// screen just calls this blocking read in a loop — there is no per-screen
// enable/disable to get wrong, which is what caused the stdin-wedging bug.
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

pub struct Key {
    pub code: KeyCode,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
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
