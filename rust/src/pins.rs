//! Pinned sessions and pinned *parts* of sessions.
//!
//! A pin is a reference (account + session id, plus an optional range of turns)
//! with a label and a saved excerpt of the text. The excerpt is a snapshot, so a
//! pin stays readable even if the session is later edited, truncated or deleted;
//! the turn range is a pointer back into the session (found again by the
//! message's uuid, so it survives edits that renumber turns).

use crate::paths;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Pin {
    pub id: String,
    pub acct: String,
    pub session_id: String,
    /// The session's title when it was pinned.
    pub title: String,
    /// What the user called this pin.
    pub label: String,
    /// `None` = the whole session.
    pub from_turn: Option<usize>,
    pub to_turn: Option<usize>,
    pub from_uuid: Option<String>,
    pub to_uuid: Option<String>,
    /// Saved text of the pinned part (capped).
    pub excerpt: String,
    pub created_ms: u64,
}

impl Pin {
    pub fn is_part(&self) -> bool {
        self.from_turn.is_some()
    }

    /// "turns 12–30", "turn 12", or "whole session".
    pub fn range_label(&self) -> String {
        match (self.from_turn, self.to_turn) {
            (Some(a), Some(b)) if a != b => format!("turns {a}–{b}"),
            (Some(a), _) => format!("turn {a}"),
            _ => "whole session".to_string(),
        }
    }
}

fn path() -> PathBuf {
    paths::root().join("pins.json")
}

pub fn load() -> Vec<Pin> {
    std::fs::read_to_string(path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn save(pins: &[Pin]) {
    if let Ok(json) = serde_json::to_string_pretty(pins) {
        let p = path();
        let tmp = p.with_extension("json.tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Adds a pin (newest first). Pinning the exact same range of a session twice
/// replaces the earlier pin instead of duplicating it.
pub fn add(mut pin: Pin) {
    pin.id = uuid::Uuid::new_v4().to_string();
    pin.created_ms = now_ms();
    let mut pins = load();
    pins.retain(|p| !(p.acct == pin.acct && p.session_id == pin.session_id && p.from_turn == pin.from_turn && p.to_turn == pin.to_turn));
    pins.insert(0, pin);
    save(&pins);
}

pub fn remove(id: &str) {
    let mut pins = load();
    pins.retain(|p| p.id != id);
    save(&pins);
}

pub fn rename(id: &str, label: &str) {
    let mut pins = load();
    if let Some(p) = pins.iter_mut().find(|p| p.id == id) {
        p.label = label.trim().to_string();
    }
    save(&pins);
}

/// Word-wraps `text` to `width` columns, keeping its own line breaks.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(10);
    let mut out = Vec::new();
    for raw in text.split('\n') {
        if raw.trim().is_empty() {
            out.push(String::new());
            continue;
        }
        let mut cur = String::new();
        for word in raw.split(' ') {
            let w = word.chars().count();
            let c = cur.chars().count();
            if c > 0 && c + 1 + w > width {
                out.push(std::mem::take(&mut cur));
            }
            // a single word longer than the line is cut
            let mut word = word.to_string();
            while word.chars().count() > width {
                let head: String = word.chars().take(width).collect();
                word = word.chars().skip(width).collect();
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                out.push(head);
            }
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(&word);
        }
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pin(acct: &str, sid: &str, from: Option<usize>, to: Option<usize>) -> Pin {
        Pin { id: String::new(), acct: acct.into(), session_id: sid.into(), title: "t".into(), label: "l".into(), from_turn: from, to_turn: to, from_uuid: None, to_uuid: None, excerpt: "x".into(), created_ms: 0 }
    }

    #[test]
    fn range_labels() {
        assert_eq!(pin("a", "s", None, None).range_label(), "whole session");
        assert_eq!(pin("a", "s", Some(3), Some(3)).range_label(), "turn 3");
        assert_eq!(pin("a", "s", Some(3), Some(9)).range_label(), "turns 3–9");
        assert!(pin("a", "s", Some(3), Some(9)).is_part() && !pin("a", "s", None, None).is_part());
    }

    #[test]
    fn wrap_keeps_breaks_and_cuts_long_words() {
        assert_eq!(wrap("aaa bbb ccc", 7), vec!["aaa bbb", "ccc"]);
        assert_eq!(wrap("one\n\ntwo", 20), vec!["one", "", "two"]);
        assert_eq!(wrap("abcdefghijklmnopqrstuvwxy", 10), vec!["abcdefghij", "klmnopqrst", "uvwxy"]);
    }
}
