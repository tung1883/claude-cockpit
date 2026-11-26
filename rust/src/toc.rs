//! Table of contents of a session: one entry per thing the user asked for,
//! plus markers for compactions and harness notices. Pure; the transcript
//! editor feeds it the rows it shows and draws the result.

use serde_json::Value;
use std::collections::HashSet;

/// Characters kept of an entry's one-line label.
const LABEL_CAP: usize = 200;

/// Openings of user-role text that the user did not type.
const WRAPPERS: &[&str] = &[
    "<system-reminder>",
    "<local-command-",
    "<bash-stdout>",
    "<bash-stderr>",
    "<task-notification>",
    "<persisted-output>",
    "<ide_",
    "[Request interrupted",
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EntryKind {
    Prompt,
    /// A slash command.
    Command,
    /// A `!` shell command typed at the prompt.
    Shell,
    Compaction,
    Notice,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Entry {
    /// Index of the entry's first row.
    pub row: usize,
    pub turn: usize,
    pub kind: EntryKind,
    pub label: String,
    /// Tool, MCP and agent calls made before the next entry.
    pub tools: usize,
    /// Distinct files those calls edited.
    pub files: usize,
}

/// One row of the transcript, as far as the outline cares.
pub struct RowRef<'a> {
    pub turn: usize,
    /// Identifies the transcript line the row comes from.
    pub line: usize,
    /// The line's JSON, for message lines.
    pub value: Option<&'a Value>,
    /// Set on a harness notice row: its text.
    pub notice: Option<String>,
    /// A tool, MCP or agent call, with the file it edits if it edits one.
    pub call: Option<Option<String>>,
}

/// The file a tool call writes to.
pub fn edited_file(block: &Value) -> Option<String> {
    let name = block.get("name")?.as_str()?;
    if !matches!(name, "Edit" | "Write" | "MultiEdit" | "NotebookEdit") {
        return None;
    }
    let input = block.get("input")?;
    input.get("file_path").or_else(|| input.get("notebook_path"))?.as_str().map(str::to_string)
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(LABEL_CAP).collect()
}

/// The text between `<name>` and `</name>`.
fn tag<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let start = s.find(&open)? + open.len();
    let end = s[start..].find(&format!("</{name}>"))? + start;
    Some(s[start..end].trim())
}

/// Drops the `<pasted_content ...>` markers around pasted text, keeping it.
fn without_paste_tags(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(a) = rest.find('<') {
        let (before, from) = rest.split_at(a);
        out.push_str(before);
        let is_tag = from.starts_with("<pasted_content") || from.starts_with("</pasted_content");
        match from.find('>') {
            Some(end) if is_tag => {
                out.push(' ');
                rest = &from[end + 1..];
            }
            _ => {
                out.push('<');
                rest = &from[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// What a message line adds to the outline: None for anything the user did not
/// type, other than a compaction.
pub fn classify(v: &Value) -> Option<(EntryKind, String)> {
    let flag = |k: &str| v.get(k).and_then(Value::as_bool) == Some(true);
    if v.get("type").and_then(Value::as_str) != Some("user") || flag("isSidechain") {
        return None;
    }
    if flag("isCompactSummary") {
        return Some((EntryKind::Compaction, "conversation compacted".to_string()));
    }
    if flag("isMeta") {
        return None;
    }
    // Newer transcripts say who sent a message; older ones are judged by text.
    let origin = v.get("origin").and_then(|o| o.get("kind")).and_then(Value::as_str);
    if origin.is_some_and(|k| k != "human") {
        return None;
    }
    let content = v.get("message")?.get("content")?;
    let mut texts: Vec<&str> = Vec::new();
    let mut image = false;
    match content {
        Value::String(s) => texts.push(s),
        Value::Array(blocks) => {
            for b in blocks {
                match b.get("type").and_then(Value::as_str) {
                    Some("text") => texts.extend(b.get("text").and_then(Value::as_str)),
                    Some("image") => image = true,
                    _ => {}
                }
            }
        }
        _ => return None,
    }
    let mut typed: Vec<&str> = Vec::new();
    for t in texts {
        let t = t.trim();
        if t.starts_with("<command-name>") || t.starts_with("<command-message>") {
            let name = tag(t, "command-name")?;
            let args = tag(t, "command-args").unwrap_or("");
            return Some((EntryKind::Command, one_line(&format!("{name} {args}"))));
        }
        if t.starts_with("<bash-input>") {
            return Some((EntryKind::Shell, one_line(&format!("! {}", tag(t, "bash-input")?))));
        }
        if !t.is_empty() && !WRAPPERS.iter().any(|w| t.starts_with(w)) {
            typed.push(t);
        }
    }
    let label = one_line(&without_paste_tags(&typed.join(" ")));
    if !label.is_empty() {
        Some((EntryKind::Prompt, label))
    } else if image {
        Some((EntryKind::Prompt, "[image]".to_string()))
    } else {
        None
    }
}

/// Builds the outline. Calls are counted towards the latest prompt, command,
/// shell or compaction entry; a notice is only a marker.
pub fn outline<'a>(rows: impl Iterator<Item = RowRef<'a>>) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    let mut owner: Option<usize> = None;
    let mut files: HashSet<String> = HashSet::new();
    let mut last_line = None;
    for (row, r) in rows.enumerate() {
        let entry = |kind, label| Entry { row, turn: r.turn, kind, label, tools: 0, files: 0 };
        if let Some(text) = &r.notice {
            out.push(entry(EntryKind::Notice, one_line(text)));
            continue;
        }
        if last_line != Some(r.line) {
            last_line = Some(r.line);
            if let Some((kind, label)) = r.value.and_then(classify) {
                owner = Some(out.len());
                files.clear();
                out.push(entry(kind, label));
            }
        }
        if let (Some(file), Some(o)) = (&r.call, owner) {
            out[o].tools += 1;
            if let Some(f) = file {
                files.insert(f.clone());
                out[o].files = files.len();
            }
        }
    }
    out
}

/// The entry the row `row` belongs to: the last one starting at or before it.
pub fn entry_at(entries: &[Entry], row: usize) -> Option<usize> {
    entries.iter().rposition(|e| e.row <= row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user(content: Value) -> Value {
        json!({"type": "user", "message": {"role": "user", "content": content}})
    }

    fn prompt(s: &str) -> Option<(EntryKind, String)> {
        Some((EntryKind::Prompt, s.to_string()))
    }

    #[test]
    fn typed_prompts() {
        assert_eq!(classify(&user(json!("fix  the\nbug"))), prompt("fix the bug"));
        let mut v = user(json!([{"type": "text", "text": "<ide_opened_file>a.rs</ide_opened_file>"}, {"type": "text", "text": "explain"}]));
        v["origin"] = json!({"kind": "human"});
        assert_eq!(classify(&v), prompt("explain"));
        assert_eq!(classify(&user(json!([{"type": "image"}]))), prompt("[image]"));
        assert_eq!(classify(&user(json!("<html> is broken"))), prompt("<html> is broken"));
        assert_eq!(classify(&user(json!("<pasted_content id=\"a1\">\nerror: x\n</pasted_content id=\"a1\">"))), prompt("error: x"));
    }

    #[test]
    fn commands_and_shell() {
        let v = user(json!("<command-message>review</command-message>\n<command-name>/review</command-name>\n<command-args>main</command-args>"));
        assert_eq!(classify(&v), Some((EntryKind::Command, "/review main".into())));
        assert_eq!(classify(&user(json!("<bash-input>cargo test</bash-input>"))), Some((EntryKind::Shell, "! cargo test".into())));
    }

    #[test]
    fn what_the_user_did_not_type_is_skipped() {
        for text in WRAPPERS {
            assert_eq!(classify(&user(json!(format!("{text} something")))), None, "{text}");
        }
        assert_eq!(classify(&user(json!([{"type": "tool_result", "content": "ok"}]))), None);
        assert_eq!(classify(&json!({"type": "assistant", "message": {"content": "hi"}})), None);
        for (key, value) in [("isMeta", json!(true)), ("isSidechain", json!(true)), ("origin", json!({"kind": "peer"}))] {
            let mut v = user(json!("hello"));
            v[key] = value;
            assert_eq!(classify(&v), None, "{key}");
        }
    }

    #[test]
    fn compaction_is_a_marker() {
        let mut v = user(json!("This session is being continued..."));
        v["isCompactSummary"] = json!(true);
        assert_eq!(classify(&v), Some((EntryKind::Compaction, "conversation compacted".into())));
    }

    #[test]
    fn edited_files() {
        let edit = json!({"name": "Edit", "input": {"file_path": "a.rs"}});
        assert_eq!(edited_file(&edit), Some("a.rs".into()));
        assert_eq!(edited_file(&json!({"name": "NotebookEdit", "input": {"notebook_path": "n.ipynb"}})), Some("n.ipynb".into()));
        assert_eq!(edited_file(&json!({"name": "Read", "input": {"file_path": "a.rs"}})), None);
    }

    #[test]
    fn counts_stop_at_the_next_entry() {
        let (a, b, tool) = (user(json!("first")), user(json!("second")), json!({"type": "assistant"}));
        let row = |line: usize, v, call: Option<Option<&str>>| RowRef { turn: line, line, value: Some(v), notice: None, call: call.map(|c| c.map(str::to_string)) };
        let rows = vec![
            row(0, &a, None),
            row(1, &tool, Some(Some("x.rs"))),
            row(2, &tool, Some(Some("x.rs"))),
            RowRef { turn: 2, line: 3, value: None, notice: Some("stopped".into()), call: None },
            row(4, &tool, Some(None)),
            row(5, &b, None),
            row(6, &tool, Some(Some("y.rs"))),
        ];
        let got = outline(rows.into_iter());
        let summary: Vec<_> = got.iter().map(|e| (e.row, e.kind, e.tools, e.files)).collect();
        assert_eq!(summary, vec![(0, EntryKind::Prompt, 3, 1), (3, EntryKind::Notice, 0, 0), (5, EntryKind::Prompt, 1, 1)]);
        assert_eq!(entry_at(&got, 4), Some(1));
        assert_eq!(entry_at(&got, 6), Some(2));
        assert_eq!(entry_at(&[], 6), None);
    }
}
