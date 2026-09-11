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
        if let Event::Key(k) = event::read()? {
            // Windows can report both Press and Release under some terminal
            // modes; only act on Press (and held-key Repeat).
            if matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                return Ok(Key {
                    code: k.code,
                    ctrl: k.modifiers.contains(KeyModifiers::CONTROL),
                    shift: k.modifiers.contains(KeyModifiers::SHIFT),
                    alt: k.modifiers.contains(KeyModifiers::ALT),
                });
            }
        }
    }
}
