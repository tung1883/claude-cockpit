//! Consumer side of OSC 7501, the Program Status Protocol: a program reports
//! `idle`, `working`, `done`, `blocked`, ... to its terminal with
//! `ESC ] 7501 ; state=working : app=claude-code ST`, and asks whether the
//! terminal understands it with `ESC ] 7501 ; ? ST`. Claude Code only starts
//! reporting once the query has been answered.
//!
//! The `vt100` crate drops unknown OSCs, so the raw PTY stream is scanned
//! here, carrying partial sequences across read boundaries.

use crate::sidebar::Status;

/// Reply to `OSC 7501 ; ?`: the states and blocked kinds this consumer knows.
pub const QUERY_REPLY: &[u8] = b"\x1b]7501;?:states=idle,working,done,blocked,error,clear:kinds=permission,question,auth\x1b\\";

const PREFIX: &[u8] = b"\x1b]7501;";
/// The spec caps a whole sequence at 4096 bytes; anything longer is junk.
const MAX_LEN: usize = 4096;

#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    /// `OSC 7501 ; ?`: the program wants to know whether we support this.
    Query,
    /// A report for the root record (no `id`): the program's own status.
    Report(Status),
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
    let mut state: Option<&str> = None;
    for pair in text.split(':') {
        let Some((k, v)) = pair.split_once('=') else { continue };
        match k {
            // Records with an id belong to sub-tasks, not the program itself.
            "id" => return None,
            "state" => state = Some(v),
            _ => {}
        }
    }
    Some(Event::Report(match state? {
        "idle" => Status::Idle,
        "working" => Status::Working,
        "done" | "error" => Status::Done,
        "blocked" => Status::Blocked,
        "clear" => Status::Unknown,
        _ => return None,
    }))
}

/// `Status` as a byte, for the atomic shared between the reader thread and the UI.
pub fn to_u8(s: Status) -> u8 {
    match s {
        Status::Unknown => 0,
        Status::Idle => 1,
        Status::Working => 2,
        Status::Blocked => 3,
        Status::Done => 4,
    }
}

pub fn from_u8(v: u8) -> Status {
    match v {
        1 => Status::Idle,
        2 => Status::Working,
        3 => Status::Blocked,
        4 => Status::Done,
        _ => Status::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(chunks: &[&[u8]]) -> Vec<Event> {
        let mut s = Scanner::default();
        chunks.iter().flat_map(|c| s.feed(c)).collect()
    }

    #[test]
    fn query_and_report() {
        let ev = feed_all(&[b"x\x1b]7501;?\x1b\\y\x1b]7501;state=working:app=claude-code\x07z"]);
        assert_eq!(ev, vec![Event::Query, Event::Report(Status::Working)]);
    }

    #[test]
    fn split_across_chunks_at_every_byte() {
        let data = b"\x1b[0m\x1b]7501;state=blocked:kind=permission\x1b\\\x1b]7501;?\x07";
        for cut in 1..data.len() {
            let ev = feed_all(&[&data[..cut], &data[cut..]]);
            assert_eq!(ev, vec![Event::Report(Status::Blocked), Event::Query], "cut at {cut}");
        }
    }

    #[test]
    fn id_records_and_junk_are_ignored() {
        let ev = feed_all(&[b"\x1b]7501;state=working:id=a/b\x07\x1b]7501;state=bogus\x07\x1b]7501;nope\x07\x1b]0;title\x07"]);
        assert!(ev.is_empty());
    }

    #[test]
    fn states_map() {
        let ev = feed_all(&[b"\x1b]7501;state=idle\x07\x1b]7501;state=done\x07\x1b]7501;state=error\x07\x1b]7501;state=clear\x07"]);
        assert_eq!(ev, vec![Event::Report(Status::Idle), Event::Report(Status::Done), Event::Report(Status::Done), Event::Report(Status::Unknown)]);
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

    #[test]
    fn status_byte_round_trip() {
        for s in [Status::Unknown, Status::Idle, Status::Working, Status::Blocked, Status::Done] {
            assert_eq!(from_u8(to_u8(s)), s);
        }
    }
}
