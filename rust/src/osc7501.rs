//! Consumer side of OSC 7501, the Program Status Protocol: a program reports
//! `idle`, `working`, `done`, `blocked`, ... to its terminal with
//! `ESC ] 7501 ; state=working : app=claude-code ST`, and asks whether the
//! terminal understands it with `ESC ] 7501 ; ? ST`. Claude Code only starts
//! reporting once the query has been answered.
//!
//! What Claude Code 2.1.305 sends (captured through cpit's own scanner):
//! - its own status as a record with no `id` (`idle`, `working` with a `msg`
//!   such as "Printing current date", `done`);
//! - a question to the user as `blocked:kind=question:msg=<base64 question>`;
//! - each sub-agent as `working:id=<agent>:title=<base64>`, ended by
//!   `clear:id=<agent>` (the sub-agent never reports `done` itself).
//!
//! The `vt100` crate drops unknown OSCs, so the raw PTY stream is scanned
//! here, carrying partial sequences across read boundaries.

use crate::sidebar::Status;
use std::collections::HashMap;

/// Reply to `OSC 7501 ; ?`: the states and blocked kinds this consumer knows.
pub const QUERY_REPLY: &[u8] = b"\x1b]7501;?:states=idle,working,done,blocked,error,clear:kinds=permission,question,auth\x1b\\";

const PREFIX: &[u8] = b"\x1b]7501;";
/// The spec caps a whole sequence at 4096 bytes; anything longer is junk.
const MAX_LEN: usize = 4096;
/// Sub-agent records remembered per pane, so a program that never clears its
/// records cannot grow this without bound.
const MAX_SUBS: usize = 64;

/// A report for the program's own record (no `id`).
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Report {
    pub status: Status,
    /// Why it is blocked: `permission`, `question` or `auth`.
    pub kind: Option<String>,
    /// The program's one-line description of what it is doing or asking.
    pub msg: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    /// `OSC 7501 ; ?`: the program wants to know whether we support this.
    Query,
    Report(Report),
    /// A sub-task record (with an `id`) started or is running.
    SubWorking { id: String, title: Option<String> },
    /// A sub-task record ended (`clear`, `done`, `idle` or `error`).
    SubEnded { id: String },
}

/// What the UI needs from one pane's reports; written by the reader thread
/// through `apply`, read by the UI loop.
#[derive(Default)]
pub struct Reports {
    pub status: Option<Report>,
    subs: HashMap<String, String>,
    /// Titles of sub-tasks that ended since the UI last looked.
    finished: Vec<String>,
}

impl Reports {
    pub fn apply(&mut self, ev: Event) {
        match ev {
            Event::Query => {}
            Event::Report(r) => self.status = Some(r),
            Event::SubWorking { id, title } => {
                if self.subs.len() < MAX_SUBS || self.subs.contains_key(&id) {
                    let known = self.subs.get(&id).cloned();
                    self.subs.insert(id, title.or(known).unwrap_or_default());
                }
            }
            Event::SubEnded { id } => {
                if let Some(title) = self.subs.remove(&id) {
                    self.finished.push(title);
                }
            }
        }
    }

    pub fn status(&self) -> Status {
        self.status.as_ref().map(|r| r.status).unwrap_or(Status::Unknown)
    }

    pub fn take_finished(&mut self) -> Vec<String> {
        std::mem::take(&mut self.finished)
    }
}

#[derive(Default)]
pub struct Scanner {
    /// Bytes of a sequence that started in an earlier chunk and has not ended.
    partial: Vec<u8>,
}

impl Scanner {
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<Event> {
        let mut events = Vec::new();
        let mut buf = std::mem::take(&mut self.partial);
        buf.extend_from_slice(chunk);
        let mut i = 0;
        while let Some(rel) = find(&buf[i..], PREFIX) {
            let body = i + rel + PREFIX.len();
            match end_of(&buf[body..]) {
                Some((len, term)) => {
                    if let Some(ev) = parse(&buf[body..body + len]) {
                        events.push(ev);
                    }
                    i = body + len + term;
                }
                None if buf.len() - (i + rel) > MAX_LEN => i = body,
                None => {
                    // Unterminated so far: keep it for the next chunk.
                    self.partial = buf[i + rel..].to_vec();
                    return events;
                }
            }
        }
        // A chunk may end partway through the prefix itself.
        let tail = (PREFIX.len() - 1).min(buf.len() - i);
        let tail_start = buf.len() - tail;
        for start in tail_start..buf.len() {
            if PREFIX.starts_with(&buf[start..]) {
                self.partial = buf[start..].to_vec();
                break;
            }
        }
        events
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Length of the payload and of the terminator (BEL or `ESC \`), if present.
fn end_of(s: &[u8]) -> Option<(usize, usize)> {
    for (k, &b) in s.iter().enumerate() {
        if b == 0x07 {
            return Some((k, 1));
        }
        if b == 0x1b {
            return match s.get(k + 1) {
                Some(b'\\') => Some((k, 2)),
                // A bare ESC inside the payload ends the sequence abnormally.
                Some(_) => Some((k, 0)),
                None => None,
            };
        }
    }
    None
}

fn parse(body: &[u8]) -> Option<Event> {
    let text = std::str::from_utf8(body).ok()?;
    if text == "?" || text.starts_with("?:") {
        return Some(Event::Query);
    }
    let (mut state, mut id, mut kind, mut title, mut msg) = (None, None, None, None, None);
    for pair in text.split(':') {
        let Some((k, v)) = pair.split_once('=') else { continue };
        match k {
            "state" => state = Some(v),
            "id" => id = Some(v),
            "kind" => kind = Some(v.to_string()),
            "title" => title = decode_text(v),
            "msg" => msg = decode_text(v),
            _ => {}
        }
    }
    let state = state?;
    if let Some(id) = id {
        return match state {
            "working" | "blocked" => Some(Event::SubWorking { id: id.to_string(), title }),
            "clear" | "done" | "idle" | "error" => Some(Event::SubEnded { id: id.to_string() }),
            _ => None,
        };
    }
    let status = match state {
        "idle" => Status::Idle,
        "working" => Status::Working,
        "done" | "error" => Status::Done,
        "blocked" => Status::Blocked,
        "clear" => Status::Unknown,
        _ => return None,
    };
    Some(Event::Report(Report { status, kind: kind.filter(|_| status == Status::Blocked), msg }))
}

/// A base64 `title`/`msg` value as one clean line; None if it is not valid
/// base64 UTF-8 (the spec says such a report is discarded; here only the
/// text is, the state still applies).
fn decode_text(v: &str) -> Option<String> {
    let bytes = base64(v)?;
    let s = String::from_utf8(bytes).ok()?;
    let s: String = s.chars().filter(|c| !c.is_control()).collect();
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

fn base64(v: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(v.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in v.bytes() {
        let n = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => return None,
        };
        acc = (acc << 6) | n as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(chunks: &[&[u8]]) -> Vec<Event> {
        let mut s = Scanner::default();
        chunks.iter().flat_map(|c| s.feed(c)).collect()
    }

    fn rep(status: Status) -> Event {
        Event::Report(Report { status, kind: None, msg: None })
    }

    #[test]
    fn query_and_report() {
        let ev = feed_all(&[b"x\x1b]7501;?\x1b\\y\x1b]7501;state=working:app=claude-code\x07z"]);
        assert_eq!(ev, vec![Event::Query, rep(Status::Working)]);
    }

    #[test]
    fn split_across_chunks_at_every_byte() {
        let data = b"\x1b[0m\x1b]7501;state=blocked:kind=permission\x1b\\\x1b]7501;?\x07";
        let want = vec![Event::Report(Report { status: Status::Blocked, kind: Some("permission".into()), msg: None }), Event::Query];
        for cut in 1..data.len() {
            assert_eq!(feed_all(&[&data[..cut], &data[cut..]]), want, "cut at {cut}");
        }
    }

    #[test]
    fn junk_is_ignored() {
        let ev = feed_all(&[b"\x1b]7501;state=bogus\x07\x1b]7501;nope\x07\x1b]0;title\x07\x1b]7501;state=working:id=a\x07"]);
        assert_eq!(ev, vec![Event::SubWorking { id: "a".into(), title: None }]);
    }

    #[test]
    fn states_map() {
        let ev = feed_all(&[b"\x1b]7501;state=idle\x07\x1b]7501;state=done\x07\x1b]7501;state=error\x07\x1b]7501;state=clear\x07"]);
        assert_eq!(ev, vec![rep(Status::Idle), rep(Status::Done), rep(Status::Done), rep(Status::Unknown)]);
    }

    #[test]
    fn captured_claude_sequences() {
        // Taken from a real Claude Code 2.1.305 session.
        let ev = feed_all(&[
            b"\x1b]7501;state=blocked:app=claude-code:kind=question:msg=YW5zd2VyOiBXaGljaCBjb2xvciB5b3UgcHJlZmVyPyAoUmVkIMK3IEJsdWUp\x1b\\",
            b"\x1b]7501;state=working:app=claude-code:id=a8046911d4a19a00c:title=UmVwbHkgd2l0aCBvaw==\x1b\\",
            b"\x1b]7501;state=clear:id=a8046911d4a19a00c\x1b\\",
        ]);
        assert_eq!(
            ev,
            vec![
                Event::Report(Report { status: Status::Blocked, kind: Some("question".into()), msg: Some("answer: Which color you prefer? (Red · Blue)".into()) }),
                Event::SubWorking { id: "a8046911d4a19a00c".into(), title: Some("Reply with ok".into()) },
                Event::SubEnded { id: "a8046911d4a19a00c".into() },
            ]
        );
    }

    #[test]
    fn subagent_titles_are_reported_once_when_they_end() {
        let mut r = Reports::default();
        r.apply(Event::SubWorking { id: "a".into(), title: Some("Reply with ok".into()) });
        r.apply(Event::SubWorking { id: "a".into(), title: None });
        r.apply(Event::SubEnded { id: "a".into() });
        r.apply(Event::SubEnded { id: "a".into() });
        r.apply(Event::SubEnded { id: "never-started".into() });
        assert_eq!(r.take_finished(), vec!["Reply with ok".to_string()]);
        assert!(r.take_finished().is_empty());
    }

    #[test]
    fn bad_base64_drops_only_the_text() {
        let ev = feed_all(&[b"\x1b]7501;state=blocked:msg=***\x07"]);
        assert_eq!(ev, vec![rep(Status::Blocked)]);
    }

    #[test]
    fn unterminated_flood_does_not_grow_forever() {
        let mut s = Scanner::default();
        s.feed(b"\x1b]7501;state=working");
        for _ in 0..10 {
            s.feed(&[b'a'; 1000]);
        }
        assert!(s.partial.len() <= MAX_LEN + 1000);
    }
}
