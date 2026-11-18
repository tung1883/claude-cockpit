//! Block-level transcript editor.
//!
//! The raw `.jsonl` of a session is thousands of lines even for a short chat
//! (hook output, attachments, request blobs, one line per content block).
//! This screen shows only what matters — one row per content block (text,
//! thinking, tool call, tool result, subagent, MCP call) — and edits the file
//! through that view: delete (pair-aware), truncate, edit a block's text or
//! JSON, search, filter, undo. Everything happens on an in-memory copy; `w`
//! writes it back (after a `.bak-…` copy of the original).

use crate::input::{self, read_key};
use crate::layout::{self, Stat};
use crate::picker::{self, ListChoice, ListOption, ScrollRow};
use crate::profiles::Profile;
use crate::ui;
use crossterm::event::KeyCode;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Rows of the detail pane under the list (1 title line + body lines).
const DET: usize = 6;
/// Longest payload kept per row for the pane + search (the file keeps all of it).
const ROW_TEXT_CAP: usize = 20_000;
/// Lines the full-text reader will wrap before saying "truncated".
const READ_CAP: usize = 400_000;

// ------------------------------------------------------------------ model

/// One physical line of the .jsonl. `raw` is written back byte-for-byte unless
/// the line was modified (then it is re-serialized from `v`), so untouched
/// lines keep their original key order and spacing.
#[derive(Clone)]
struct L {
    raw: String,
    v: Option<Value>,
    dirty: bool,
}

impl L {
    fn parse(raw: &str) -> L {
        L { raw: raw.to_string(), v: serde_json::from_str(raw).ok(), dirty: false }
    }
    fn out(&self) -> String {
        match (&self.v, self.dirty) {
            (Some(v), true) => v.to_string(),
            _ => self.raw.clone(),
        }
    }
    fn ty(&self) -> &str {
        self.v.as_ref().and_then(|v| v.get("type")).and_then(|t| t.as_str()).unwrap_or("")
    }
    fn uuid(&self) -> Option<&str> {
        self.v.as_ref().and_then(|v| v.get("uuid")).and_then(|t| t.as_str())
    }
    fn parent(&self) -> Option<&str> {
        self.v.as_ref().and_then(|v| v.get("parentUuid")).and_then(|t| t.as_str())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Text,
    Think,
    Tool,
    Mcp,
    Agent,
    Result,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Text => "text",
            Kind::Think => "think",
            Kind::Tool => "tool",
            Kind::Mcp => "mcp",
            Kind::Agent => "agent",
            Kind::Result => "↳ res",
        }
    }
    fn filter_name(self) -> &'static str {
        match self {
            Kind::Text => "text",
            Kind::Think => "thinking",
            Kind::Tool => "tool_use",
            Kind::Mcp => "mcp",
            Kind::Agent => "agent",
            Kind::Result => "tool_result",
        }
    }
}

struct Row {
    line: usize,
    blk: usize,
    turn: usize,
    sub: usize,
    user: bool,
    /// Who the row is from: You, Claude, or Tool (a tool's output — stored as a
    /// user message by the API, but not something the user typed).
    who: &'static str,
    kind: Kind,
    name: String,
    tid: String,
    preview: String,
    /// Capped payload (pane + search); the full one is re-read from the doc.
    text: String,
    chars: usize,
    lower: String,
    big: Option<String>,
    agent_id: Option<String>,
    agent_prompt: String,
}

impl Row {
    fn label(&self) -> String {
        if self.sub == 0 {
            self.turn.to_string()
        } else {
            format!("{}.{}", self.turn, self.sub + 1)
        }
    }
    fn key(&self) -> (usize, usize) {
        (self.line, self.blk)
    }
}

fn cap_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect()
    }
}

fn flat(s: &str, n: usize) -> String {
    let mut out = String::new();
    let mut last_space = true;
    let mut count = 0usize;
    for c in s.chars() {
        if c.is_whitespace() {
            if !last_space {
                out.push(' ');
                count += 1;
                last_space = true;
            }
        } else {
            out.push(c);
            count += 1;
            last_space = false;
        }
        if count >= n {
            break;
        }
    }
    out.trim_end().to_string()
}

/// Text of a tool_result's `content` (string, or list of text/image blocks).
fn result_text(c: &Value) -> String {
    match c {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .map(|b| match b.get("type").and_then(|t| t.as_str()) {
                Some("text") => b.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string(),
                Some("image") => "[image]".to_string(),
                _ => b.to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_string(),
    }
}

/// The message blocks of a line as (index, block). A plain-string `content`
/// (a typed user prompt) is presented as one synthetic text block.
fn blocks_of(l: &L) -> Vec<Value> {
    let Some(v) = &l.v else { return Vec::new() };
    match v.get("message").and_then(|m| m.get("content")) {
        Some(Value::String(s)) => vec![serde_json::json!({"type": "text", "text": s})],
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    }
}

/// Full editable body of one block.
fn block_body(b: &Value) -> String {
    match b.get("type").and_then(|t| t.as_str()).unwrap_or("") {
        "text" => b.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string(),
        "thinking" => b.get("thinking").and_then(|t| t.as_str()).unwrap_or("").to_string(),
        "tool_use" => serde_json::to_string_pretty(b.get("input").unwrap_or(&Value::Null)).unwrap_or_default(),
        "tool_result" => result_text(b.get("content").unwrap_or(&Value::Null)),
        _ => serde_json::to_string_pretty(b).unwrap_or_default(),
    }
}

fn is_message_line(l: &L) -> bool {
    let Some(v) = &l.v else { return false };
    matches!(l.ty(), "user" | "assistant")
        && v.get("isMeta").and_then(|x| x.as_bool()) != Some(true)
        && v.get("isSidechain").and_then(|x| x.as_bool()) != Some(true)
}

fn build_rows(doc: &[L], agent_dir: &Path) -> Vec<Row> {
    // tool_use id -> subagent id, taken from the tool_result line's metadata.
    let mut agent_of: HashMap<String, String> = HashMap::new();
    for l in doc {
        if l.ty() != "user" {
            continue;
        }
        let Some(v) = &l.v else { continue };
        let Some(aid) = v.get("toolUseResult").and_then(|t| t.get("agentId")).and_then(|a| a.as_str()) else { continue };
        for b in blocks_of(l) {
            if b.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                if let Some(tid) = b.get("tool_use_id").and_then(|t| t.as_str()) {
                    agent_of.insert(tid.to_string(), aid.to_string());
                }
            }
        }
    }
    let _ = agent_dir;

    let mut rows = Vec::new();
    let mut turn = 0usize;
    let mut sub = 0usize;
    let mut last_user: Option<bool> = None;
    for (li, l) in doc.iter().enumerate() {
        if !is_message_line(l) {
            continue;
        }
        let user = l.ty() == "user";
        let blocks = blocks_of(l);
        let mut first_in_line = true;
        for (bi, b) in blocks.iter().enumerate() {
            let ty = b.get("type").and_then(|t| t.as_str()).unwrap_or("");
            let body = block_body(b);
            let mut name = String::new();
            let mut tid = String::new();
            let mut big = None;
            let mut agent_id = None;
            let mut agent_prompt = String::new();
            let (kind, preview) = match ty {
                "text" => {
                    if body.trim().is_empty() || body.trim_start().starts_with("<system-reminder>") {
                        continue;
                    }
                    (Kind::Text, flat(&body, 300))
                }
                "thinking" => (Kind::Think, if body.is_empty() { "(empty, signature only)".to_string() } else { flat(&body, 300) }),
                "tool_use" => {
                    name = b.get("name").and_then(|n| n.as_str()).unwrap_or("?").to_string();
                    tid = b.get("id").and_then(|n| n.as_str()).unwrap_or("").to_string();
                    let input = b.get("input").map(|i| i.to_string()).unwrap_or_default();
                    if name == "Agent" || name == "Task" {
                        agent_id = agent_of.get(&tid).cloned();
                        agent_prompt = b.get("input").and_then(|i| i.get("prompt")).and_then(|p| p.as_str()).unwrap_or("").to_string();
                        let st = b.get("input").and_then(|i| i.get("subagent_type")).and_then(|p| p.as_str()).unwrap_or("agent");
                        (Kind::Agent, flat(&format!("{st}  {input}"), 300))
                    } else if let Some(rest) = name.strip_prefix("mcp__") {
                        let mut parts = rest.splitn(2, "__");
                        let server = parts.next().unwrap_or("");
                        let tool = parts.next().unwrap_or("");
                        (Kind::Mcp, flat(&format!("{server} · {tool}  {input}"), 300))
                    } else {
                        (Kind::Tool, flat(&format!("{name}  {input}"), 300))
                    }
                }
                "tool_result" => {
                    tid = b.get("tool_use_id").and_then(|n| n.as_str()).unwrap_or("").to_string();
                    let n = body.chars().count();
                    if n > 20_000 {
                        big = Some(format!("{} KB", n / 1000));
                    }
                    (Kind::Result, flat(&body, 300))
                }
                "image" => (Kind::Text, "[image]".to_string()),
                _ => continue,
            };
            // A new turn starts at every user line, or when the speaker changes.
            if first_in_line {
                if user || last_user != Some(false) {
                    turn += 1;
                    sub = 0;
                }
                first_in_line = false;
                last_user = Some(user);
            }
            let chars = body.chars().count();
            let text = cap_chars(&body, ROW_TEXT_CAP);
            let lower = format!("{} {} {}", kind.filter_name(), name, text).to_lowercase();
            rows.push(Row { line: li, blk: bi, turn, sub, user, who: if user { "You" } else { "Claude" }, kind, name, tid, preview, text, chars, lower, big, agent_id, agent_prompt });
            sub += 1;
        }
    }
    // Tool results come back as user messages, but only the answers to
    // AskUserQuestion are really the user's; everything else is tool output.
    let names: HashMap<String, String> = rows.iter().filter(|r| r.kind != Kind::Result && !r.tid.is_empty()).map(|r| (r.tid.clone(), r.name.clone())).collect();
    for r in rows.iter_mut() {
        if r.kind == Kind::Result {
            r.who = if names.get(&r.tid).map(|n| n == "AskUserQuestion").unwrap_or(false) { "You" } else { "Tool" };
        }
    }
    rows
}

/// "42" -> turn 42; "42.3" -> third block of turn 42. Returns the index into
/// `vis` (the rows currently listed) to move the cursor to.
fn goto_target(rows: &[Row], vis: &[usize], input: &str) -> Result<usize, String> {
    let t = input.trim().trim_start_matches('#');
    let (turn_s, sub_s) = match t.split_once('.') {
        Some((a, b)) => (a, Some(b)),
        None => (t, None),
    };
    let turn: usize = turn_s.trim().parse().map_err(|_| "type a turn number like 42, or 42.3".to_string())?;
    let sub: usize = match sub_s {
        Some(b) => b.trim().parse::<usize>().map_err(|_| "type a turn number like 42, or 42.3".to_string())?.saturating_sub(1),
        None => 0,
    };
    if let Some(n) = vis.iter().position(|&ri| rows[ri].turn == turn && rows[ri].sub == sub) {
        return Ok(n);
    }
    if rows.iter().any(|r| r.turn == turn && r.sub == sub) {
        return Err("that row is hidden by the current filter".into());
    }
    let last = rows.iter().map(|r| r.turn).max().unwrap_or(0);
    if turn > last {
        Err(format!("no turn {turn} — the last one is {last}"))
    } else {
        Err(format!("no block {turn}.{} (turn {turn} has fewer blocks)", sub + 1))
    }
}

// ------------------------------------------------------------- operations

/// What a delete would touch, shown to the user before they confirm.
struct DeletePlan {
    set: HashSet<(usize, usize)>,
    selected: usize,
    paired: usize,
    agents: Vec<String>,
}

fn plan_delete(rows: &[Row], picked: &HashSet<(usize, usize)>) -> DeletePlan {
    let mut set: HashSet<(usize, usize)> = picked.clone();
    let tids: HashSet<&str> = rows.iter().filter(|r| picked.contains(&r.key()) && !r.tid.is_empty()).map(|r| r.tid.as_str()).collect();
    let mut paired = 0;
    for r in rows {
        if !r.tid.is_empty() && tids.contains(r.tid.as_str()) && set.insert(r.key()) {
            paired += 1;
        }
    }
    let agents = rows.iter().filter(|r| set.contains(&r.key())).filter_map(|r| r.agent_id.clone()).collect();
    DeletePlan { selected: picked.len(), set, paired, agents }
}

/// Removes the given blocks (dropping lines left empty), relinks the parentUuid
/// chain around removed lines, and scrubs request-blob lines that still quote
/// the removed content. Returns the number of lines removed.
fn delete_blocks(doc: &mut Vec<L>, rows: &[Row], set: &HashSet<(usize, usize)>) -> usize {
    let mut needles: Vec<String> = Vec::new();
    for r in rows.iter().filter(|r| set.contains(&r.key())) {
        if !r.tid.is_empty() {
            needles.push(r.tid.clone());
        } else if r.chars >= 30 {
            needles.push(cap_chars(&r.text, 60));
        }
    }
    let mut by_line: HashMap<usize, Vec<usize>> = HashMap::new();
    for (l, b) in set {
        by_line.entry(*l).or_default().push(*b);
    }
    let mut removed: HashSet<usize> = HashSet::new();
    for (li, mut blks) in by_line {
        blks.sort_unstable_by(|a, b| b.cmp(a));
        let line = &mut doc[li];
        let Some(v) = line.v.as_mut() else { continue };
        let Some(content) = v.get_mut("message").and_then(|m| m.get_mut("content")) else { continue };
        match content {
            Value::Array(a) => {
                for b in blks {
                    if b < a.len() {
                        a.remove(b);
                    }
                }
                if a.is_empty() {
                    removed.insert(li);
                } else {
                    line.dirty = true;
                }
            }
            _ => {
                removed.insert(li);
            }
        }
    }
    let n = removed.len();
    remove_lines(doc, &removed, &needles);
    n
}

/// Drops `removed` lines (plus blob lines quoting `needles`), repointing any
/// surviving line whose parent was removed at the nearest surviving ancestor.
fn remove_lines(doc: &mut Vec<L>, removed: &HashSet<usize>, needles: &[String]) {
    let mut gone_parent: HashMap<String, Option<String>> = HashMap::new();
    for &i in removed {
        if let Some(u) = doc[i].uuid() {
            gone_parent.insert(u.to_string(), doc[i].parent().map(|s| s.to_string()));
        }
    }
    let resolve = |start: Option<String>| -> Option<String> {
        let mut cur = start;
        let mut hops = 0;
        while let Some(u) = cur.clone() {
            match gone_parent.get(&u) {
                Some(p) => cur = p.clone(),
                None => break,
            }
            hops += 1;
            if hops > 100_000 {
                break;
            }
        }
        cur
    };
    let mut keep: Vec<L> = Vec::with_capacity(doc.len());
    for (i, l) in doc.drain(..).enumerate() {
        if removed.contains(&i) {
            continue;
        }
        if l.ty().starts_with("api-request") && !needles.is_empty() {
            let hay = l.out();
            if needles.iter().any(|n| hay.contains(n.as_str())) {
                continue;
            }
        }
        keep.push(l);
    }
    for l in keep.iter_mut() {
        if let Some(p) = l.parent().map(|s| s.to_string()) {
            if gone_parent.contains_key(&p) {
                let np = resolve(Some(p));
                if let Some(v) = l.v.as_mut() {
                    v["parentUuid"] = np.map(Value::String).unwrap_or(Value::Null);
                    l.dirty = true;
                }
            }
        }
        if l.ty() == "last-prompt" {
            if let Some(leaf) = l.v.as_ref().and_then(|v| v.get("leafUuid")).and_then(|x| x.as_str()).map(|s| s.to_string()) {
                if gone_parent.contains_key(&leaf) {
                    let np = resolve(Some(leaf));
                    if let Some(v) = l.v.as_mut() {
                        v["leafUuid"] = np.map(Value::String).unwrap_or(Value::Null);
                        l.dirty = true;
                    }
                }
            }
        }
    }
    *doc = keep;
}

/// Turn to keep when cutting after `turn`: a turn holding a tool call also
/// keeps the turn carrying its result, so no tool_use is left unanswered.
fn snap_turn(rows: &[Row], turn: usize) -> usize {
    let mut keep = turn;
    loop {
        let open: HashSet<&str> = rows.iter().filter(|r| r.turn <= keep && r.kind != Kind::Result && !r.tid.is_empty()).map(|r| r.tid.as_str()).collect();
        let answered: HashSet<&str> = rows.iter().filter(|r| r.turn <= keep && r.kind == Kind::Result).map(|r| r.tid.as_str()).collect();
        let next = rows.iter().filter(|r| r.turn > keep && r.kind == Kind::Result && open.contains(r.tid.as_str()) && !answered.contains(r.tid.as_str())).map(|r| r.turn).min();
        match next {
            Some(t) => keep = t,
            None => return keep,
        }
    }
}

const KEEP_TYPES: [&str; 9] = ["last-prompt", "mode", "permission-mode", "session-owner", "atis-latch", "ai-title", "custom-title", "cost-state", "summary"];

/// Cuts everything after `keep_turn`: later message lines and the hook /
/// attachment / blob lines that follow the last kept message. Session metadata
/// lines stay; `last-prompt` is pointed at the new last message.
fn truncate_doc(doc: &mut Vec<L>, rows: &[Row], keep_turn: usize) -> Vec<String> {
    let cut = rows.iter().filter(|r| r.turn <= keep_turn).map(|r| r.line).max();
    let Some(cut) = cut else { return Vec::new() };
    let agents: Vec<String> = rows.iter().filter(|r| r.turn > keep_turn).filter_map(|r| r.agent_id.clone()).collect();
    let leaf = doc[cut].uuid().map(|s| s.to_string());
    let mut i = 0usize;
    doc.retain(|l| {
        let idx = i;
        i += 1;
        idx <= cut || KEEP_TYPES.contains(&l.ty())
    });
    for l in doc.iter_mut() {
        if l.ty() == "last-prompt" {
            if let (Some(v), Some(leaf)) = (l.v.as_mut(), leaf.as_ref()) {
                v["leafUuid"] = Value::String(leaf.clone());
                l.dirty = true;
            }
        }
    }
    agents
}

/// Returns the transcript with every thinking block removed (lines left empty
/// are dropped and the parentUuid chain relinked), and how many blocks went.
/// A thinking block's signature only validates for the organization that
/// produced it, so a session handed to another account must not carry them.
pub fn strip_thinking(text: &str) -> (String, usize) {
    let mut doc: Vec<L> = text.split('\n').map(|s| L::parse(s.trim_end_matches('\r'))).collect();
    let trailing = doc.last().map(|l| l.raw.is_empty()).unwrap_or(false);
    if trailing {
        doc.pop();
    }
    let mut emptied: HashSet<usize> = HashSet::new();
    let mut total = 0usize;
    for (i, l) in doc.iter_mut().enumerate() {
        if !l.raw.contains("thinking") {
            continue;
        }
        let outcome = {
            let Some(v) = l.v.as_mut() else { continue };
            let Some(Value::Array(a)) = v.get_mut("message").and_then(|m| m.get_mut("content")) else { continue };
            let before = a.len();
            a.retain(|b| !matches!(b.get("type").and_then(|t| t.as_str()), Some("thinking") | Some("redacted_thinking")));
            (before - a.len(), a.is_empty())
        };
        if outcome.0 == 0 {
            continue;
        }
        total += outcome.0;
        if outcome.1 {
            emptied.insert(i);
        } else {
            l.dirty = true;
        }
    }
    if total == 0 {
        return (text.to_string(), 0);
    }
    remove_lines(&mut doc, &emptied, &[]);
    let mut out = doc.iter().map(|l| l.out()).collect::<Vec<_>>().join("\n");
    if trailing {
        out.push('\n');
    }
    (out, total)
}

fn set_block(doc: &mut [L], r: &Row, apply: impl FnOnce(&mut Value)) -> bool {
    let Some(v) = doc[r.line].v.as_mut() else { return false };
    let Some(content) = v.get_mut("message").and_then(|m| m.get_mut("content")) else { return false };
    match content {
        Value::Array(a) => match a.get_mut(r.blk) {
            Some(b) => {
                apply(b);
                doc[r.line].dirty = true;
                true
            }
            None => false,
        },
        Value::String(_) => {
            // typed user prompt: edit through a synthetic block, write the text back
            let mut b = serde_json::json!({"type": "text", "text": content.as_str().unwrap_or("")});
            apply(&mut b);
            if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                *content = Value::String(t.to_string());
                doc[r.line].dirty = true;
                return true;
            }
            false
        }
        _ => false,
    }
}

// ------------------------------------------------------------------ state

enum PromptKind {
    Goto,
    Search,
    Filter,
    Delete(DeletePlan),
    Leave,
}

struct Prompt {
    q: String,
    buf: String,
    kind: PromptKind,
}

struct State {
    file: PathBuf,
    id: String,
    title: String,
    agent_dir: PathBuf,
    doc: Vec<L>,
    rows: Vec<Row>,
    hist: Vec<(Vec<L>, Vec<String>)>,
    picked: HashSet<(usize, usize)>,
    /// subagent ids whose files get set aside when the changes are saved
    drop_agents: Vec<String>,
    dirty: bool,
    sel: usize,
    top: usize,
    query: String,
    filter: String,
    flash: String,
    turns0: usize,
    blocks0: usize,
}

impl State {
    fn load(file: &Path, id: &str, title: &str) -> std::io::Result<State> {
        let text = std::fs::read_to_string(file)?;
        let mut doc: Vec<L> = text.split('\n').map(|s| L::parse(s.trim_end_matches('\r'))).collect();
        if doc.last().map(|l| l.raw.is_empty()).unwrap_or(false) {
            doc.pop();
        }
        let agent_dir = file.with_extension("").join("subagents");
        let rows = build_rows(&doc, &agent_dir);
        let turns0 = rows.iter().map(|r| r.turn).max().unwrap_or(0);
        let blocks0 = rows.len();
        Ok(State {
            file: file.to_path_buf(),
            id: id.to_string(),
            title: title.to_string(),
            agent_dir,
            doc,
            rows,
            hist: Vec::new(),
            picked: HashSet::new(),
            drop_agents: Vec::new(),
            dirty: false,
            sel: 0,
            top: 0,
            query: String::new(),
            filter: String::new(),
            flash: String::new(),
            turns0,
            blocks0,
        })
    }

    fn rebuild(&mut self) {
        self.rows = build_rows(&self.doc, &self.agent_dir);
        self.picked.clear();
    }

    fn snapshot(&mut self) {
        self.hist.push((self.doc.clone(), self.drop_agents.clone()));
        if self.hist.len() > 12 {
            self.hist.remove(0);
        }
    }

    fn undo(&mut self) {
        match self.hist.pop() {
            Some((doc, agents)) => {
                self.doc = doc;
                self.drop_agents = agents;
                self.dirty = true;
                self.rebuild();
                self.flash = "undone".into();
            }
            None => self.flash = "nothing to undo".into(),
        }
    }

    fn visible(&self) -> Vec<usize> {
        let toks: Vec<String> = self.filter.split_whitespace().map(|t| t.to_lowercase()).collect();
        (0..self.rows.len())
            .filter(|&i| {
                let r = &self.rows[i];
                toks.iter().all(|t| {
                    if let Some(k) = t.strip_prefix("kind:") {
                        r.kind.filter_name().contains(k) || r.kind.label().contains(k)
                    } else if let Some(n) = t.strip_prefix("name:") {
                        r.name.to_lowercase().contains(n)
                    } else {
                        r.lower.contains(t.as_str())
                    }
                })
            })
            .collect()
    }

    fn hit(&self, r: &Row) -> bool {
        !self.query.is_empty() && r.lower.contains(&self.query.to_lowercase())
    }

    fn jump(&mut self, vis: &[usize], forward: bool, inclusive: bool) {
        let hits: Vec<usize> = vis.iter().enumerate().filter(|(_, &ri)| self.hit(&self.rows[ri])).map(|(n, _)| n).collect();
        if hits.is_empty() {
            self.flash = "no matches".into();
            return;
        }
        let found = if forward {
            hits.iter().copied().find(|&n| if inclusive { n >= self.sel } else { n > self.sel })
        } else {
            hits.iter().rev().copied().find(|&n| n < self.sel)
        };
        let (n, wrapped) = match found {
            Some(n) => (n, false),
            None => (if forward { hits[0] } else { *hits.last().unwrap() }, true),
        };
        let pos = hits.iter().position(|&h| h == n).unwrap_or(0) + 1;
        self.flash = if wrapped { format!("{pos}/{} wrapped", hits.len()) } else { format!("{pos}/{}", hits.len()) };
        self.sel = n;
    }

    fn save(&mut self) -> std::io::Result<()> {
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let bak = PathBuf::from(format!("{}.bak-{stamp}", self.file.display()));
        std::fs::copy(&self.file, &bak)?;
        let mut out = self.doc.iter().map(|l| l.out()).collect::<Vec<_>>().join("\n");
        out.push('\n');
        std::fs::write(&self.file, out)?;
        // Subagent transcripts of deleted Agent calls: set aside, not erased.
        for aid in self.drop_agents.drain(..) {
            for ext in ["jsonl", "meta.json"] {
                let p = self.agent_dir.join(format!("agent-{aid}.{ext}"));
                if p.exists() {
                    let _ = std::fs::rename(&p, format!("{}.deleted-{stamp}", p.display()));
                }
            }
        }
        self.dirty = false;
        self.flash = format!("saved · backup {}", bak.file_name().and_then(|n| n.to_str()).unwrap_or(""));
        Ok(())
    }

    /// New independent session holding the first `keep_turn` turns of the
    /// current in-memory transcript. The original file is left alone.
    fn fork(&self, keep_turn: usize) -> std::io::Result<String> {
        let mut doc = self.doc.clone();
        truncate_doc(&mut doc, &self.rows, keep_turn);
        let new_id = uuid::Uuid::new_v4().to_string();
        let dest = self.file.with_file_name(format!("{new_id}.jsonl"));
        let mut out = doc.iter().map(|l| l.out()).collect::<Vec<_>>().join("\n");
        out.push('\n');
        std::fs::write(&dest, out)?;
        let kept: Vec<&String> = self.rows.iter().filter(|r| r.turn <= keep_turn).filter_map(|r| r.agent_id.as_ref()).collect();
        if !kept.is_empty() {
            let dir = dest.with_extension("").join("subagents");
            let _ = std::fs::create_dir_all(&dir);
            for aid in kept {
                for ext in ["jsonl", "meta.json"] {
                    let p = self.agent_dir.join(format!("agent-{aid}.{ext}"));
                    if p.exists() {
                        let _ = std::fs::copy(&p, dir.join(format!("agent-{aid}.{ext}")));
                    }
                }
            }
        }
        Ok(new_id)
    }
}

// ------------------------------------------------------------------ views

fn wrap_text(text: &str, width: usize, max_lines: usize) -> (Vec<String>, bool) {
    let width = width.max(10);
    let mut out: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        if raw.trim().is_empty() {
            out.push(String::new());
        } else {
            let chars: Vec<char> = raw.chars().collect();
            let mut start = 0usize;
            while start < chars.len() {
                let mut end = (start + width).min(chars.len());
                if end < chars.len() {
                    if let Some(sp) = chars[start..end].iter().rposition(|c| *c == ' ') {
                        if sp > width / 2 {
                            end = start + sp;
                        }
                    }
                }
                out.push(chars[start..end].iter().collect::<String>());
                start = end;
                while start < chars.len() && chars[start] == ' ' && end < chars.len() {
                    start += 1;
                }
            }
        }
        if out.len() >= max_lines {
            out.truncate(max_lines);
            return (out, true);
        }
    }
    (out, false)
}

/// Splits `s` around case-insensitive matches of `q` and paints the matches
/// yellow. ASCII-only so the byte offsets of the lowercased copy line up.
fn highlight(s: &str, q: &str) -> String {
    if q.is_empty() || !s.is_ascii() || !q.is_ascii() {
        return s.to_string();
    }
    let ls = s.to_ascii_lowercase();
    let lq = q.to_ascii_lowercase();
    let mut out = String::new();
    let mut at = 0usize;
    while let Some(p) = ls[at..].find(&lq) {
        let a = at + p;
        out.push_str(&s[at..a]);
        out.push_str(&ui::color::yellow(&s[a..a + lq.len()]));
        at = a + lq.len();
    }
    out.push_str(&s[at..]);
    out
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

fn pad(s: &str, n: usize) -> String {
    let c = s.chars().count();
    if c >= n {
        s.chars().take(n).collect()
    } else {
        format!("{s}{}", " ".repeat(n - c))
    }
}

fn draw(st: &State, header: &[String], prompt: Option<&Prompt>, vis: &[usize], term_w: usize, term_h: usize) -> (Vec<String>, Option<(usize, usize)>) {
    let mut lines: Vec<String> = header.to_vec();
    let dirty_tag = if st.dirty { ui::color::yellow(" •  unsaved") } else { String::new() };
    let flash = if st.flash.is_empty() { String::new() } else { format!("  {}", ui::color::yellow(&st.flash)) };
    lines.push(format!("{} {}{dirty_tag}{flash}", ui::color::orange(ui::glyph::back()), ui::color::bold("Edit transcript")));
    let turns = st.rows.iter().map(|r| r.turn).max().unwrap_or(0);
    let mut meta = format!("   {} · {}/{} turns · {}/{} blocks", st.title, turns, st.turns0, st.rows.len(), st.blocks0);
    if !st.filter.is_empty() {
        meta.push_str(&format!(" · filter: {}", st.filter));
    }
    if !st.query.is_empty() {
        let n = vis.iter().filter(|&&ri| st.hit(&st.rows[ri])).count();
        meta.push_str(&format!(" · search \"{}\": {} hit{}", st.query, n, if n == 1 { "" } else { "s" }));
    }
    lines.push(ui::color::dim(&clip(&meta, term_w)));
    lines.push(String::new());

    // footer: detail pane + rule + 3 hint rows + prompt/flash row
    let foot = DET + 1 + 3 + 1;
    let used = lines.len();
    let vp = term_h.saturating_sub(used + foot + 2).max(3);

    // list window (top is kept in sync by the caller)
    let top = st.top.min(vis.len().saturating_sub(1));
    let end = (top + vp).min(vis.len());
    if top > 0 {
        lines.push(ui::color::dim(&format!("    ↑ {top} more")));
    }
    let preview_w = term_w.saturating_sub(3 + 1 + 1 + 7 + 7 + 6 + 2);
    for n in top..end {
        let r = &st.rows[vis[n]];
        let on = n == st.sel;
        let ptr = if on { ui::color::orange(ui::glyph::pointer()) } else { " ".to_string() };
        let mark = if st.picked.contains(&r.key()) { ui::color::orange("✓") } else { " ".to_string() };
        let who_t = r.who;
        let mut extra = String::new();
        if r.kind == Kind::Agent {
            let msgs = r.agent_id.as_ref().map(|a| count_lines(&st.agent_dir.join(format!("agent-{a}.jsonl")))).unwrap_or(0);
            if msgs > 0 {
                extra = format!("  ▸ {msgs} msgs");
            }
        } else if let Some(b) = &r.big {
            extra = format!("  {b}");
        }
        let hit = st.hit(r);
        let in_preview = hit && r.preview.to_lowercase().contains(&st.query.to_lowercase());
        let mut tail = clip(&r.preview, preview_w.saturating_sub(extra.chars().count()));
        let label = format!(" {}", pad(&r.label(), 7));
        let line = if on {
            let body = format!("{label}{}{}{}{extra}", pad(who_t, 7), pad(r.kind.label(), 6), tail);
            format!(" {ptr} {mark}{}", ui::color::orange_bold(&body))
        } else {
            if in_preview {
                tail = highlight(&tail, &st.query);
            }
            let who = match r.who {
                "You" => ui::color::orange_bold(&pad(who_t, 7)),
                "Claude" => ui::color::green(&pad(who_t, 7)),
                _ => ui::color::dim(&pad(who_t, 7)),
            };
            let kind = pad(r.kind.label(), 6);
            let (kind, tail) = match r.kind {
                Kind::Tool | Kind::Mcp | Kind::Agent => (ui::color::cyan(&kind), if in_preview { tail } else { ui::color::cyan(&tail) }),
                Kind::Result | Kind::Think => (ui::color::dim(&kind), if in_preview { tail } else { ui::color::dim(&tail) }),
                Kind::Text => (kind, tail),
            };
            let extra = if extra.is_empty() { extra } else { ui::color::yellow(&extra) };
            format!(" {ptr} {mark}{}{who}{kind}{tail}{extra}", ui::color::dim(&label))
        };
        let hit_tag = if hit && !in_preview { ui::color::yellow("  ● match in payload") } else { String::new() };
        lines.push(format!("{line}{hit_tag}"));
    }
    if vis.is_empty() {
        lines.push(ui::color::dim("   (no blocks match the filter)"));
    }
    let below = vis.len().saturating_sub(end);
    if below > 0 {
        lines.push(ui::color::dim(&format!("    ↓ {below} more")));
    }
    while lines.len() < term_h.saturating_sub(foot) {
        lines.push(String::new());
    }

    // detail pane for the highlighted block
    match vis.get(st.sel).map(|&ri| &st.rows[ri]) {
        Some(r) => {
            let (body, more) = wrap_text(&r.text, term_w.saturating_sub(4), DET - 1);
            let total_lines = if more || r.chars > ROW_TEXT_CAP { None } else { Some(body.len()) };
            let head = format!(
                "─ {} {}{} · {} chars{} ",
                r.label(),
                r.kind.filter_name(),
                if r.name.is_empty() { String::new() } else { format!(" · {}", r.name) },
                r.chars,
                total_lines.map(|n| format!(" · {n} lines")).unwrap_or_default()
            );
            let fill = "─".repeat(term_w.saturating_sub(head.chars().count()));
            lines.push(ui::color::gray(&format!("{head}{fill}")));
            let room = DET - 1;
            let shown = if more { room - 1 } else { body.len().min(room) };
            for l in body.iter().take(shown) {
                lines.push(format!("  {}", highlight(l, &st.query)));
            }
            if more {
                lines.push(ui::color::yellow("  … more · v reads it all, Enter edits"));
            }
            for _ in (1 + shown + usize::from(more))..DET {
                lines.push(String::new());
            }
        }
        None => {
            for _ in 0..DET {
                lines.push(String::new());
            }
        }
    }
    lines.push(ui::rule(None));
    let cell = (term_w / 5).clamp(16, 22);
    let r1: &[(&str, &str)] = &[("↑/↓", "move"), ("Space", "select"), ("d", "delete"), ("t", "truncate"), ("u", "undo")];
    let r2: &[(&str, &str)] = &[("Enter", "edit"), ("v", "read full"), ("J", "JSON"), ("/", "search n/N"), ("f", "filter")];
    let r3: &[(&str, &str)] = &[("Ctrl+↑/↓", "top/bottom"), (":", "go to row"), ("w", "save"), ("←/Esc", "back"), ("Shift+↑/↓", "range")];
    lines.extend(layout::hint_grid(&[r1, r2, r3], cell));

    let mut cursor: Option<(usize, usize)> = None;
    match prompt {
        Some(p) => {
            lines.push(format!("{} {}{}", ui::color::orange(ui::glyph::back()), p.q, p.buf));
            let col = 2 + p.q.chars().count() + p.buf.chars().count() + 1;
            cursor = Some((lines.len(), col));
        }
        None => lines.push(String::new()),
    }
    (lines, cursor)
}

fn count_lines(p: &Path) -> usize {
    std::fs::read(p).map(|b| b.iter().filter(|&&c| c == b'\n').count()).unwrap_or(0)
}

// -------------------------------------------------------------- main loop

pub fn run(profiles: &[Profile], mark: i64, file: &Path, id: &str, title: &str) -> std::io::Result<()> {
    let stats: Vec<Stat> = profiles.iter().map(|p| layout::account_stat(&p.name)).collect();
    let header = layout::account_lines(profiles, mark, &stats);
    let short = &id[..id.len().min(8)];
    let mut st = State::load(file, id, &format!("{title} · {short}"))?;
    let mut prompt: Option<Prompt> = None;
    // Shift+arrows: range selection from where it started (`anchor`, an index
    // into the visible rows), on top of whatever was already selected (`base`).
    let mut anchor: Option<usize> = None;
    let mut base: HashSet<(usize, usize)> = HashSet::new();
    ui::hide_cursor();
    loop {
        let (tw, th) = crossterm::terminal::size().map(|(w, h)| (w as usize, h as usize)).unwrap_or((100, 30));
        let vis = st.visible();
        if st.sel >= vis.len() {
            st.sel = vis.len().saturating_sub(1);
        }
        let used = header.len() + 3;
        let vp = th.saturating_sub(used + DET + 1 + 3 + 1 + 2).max(3);
        if st.sel < st.top {
            st.top = st.sel;
        }
        if st.sel >= st.top + vp {
            st.top = st.sel + 1 - vp;
        }
        let (lines, cursor) = draw(&st, &header, prompt.as_ref(), &vis, tw, th);
        match cursor {
            Some((row, col)) => ui::repaint_at(&lines, row, col),
            None => {
                ui::hide_cursor();
                ui::repaint(&lines);
            }
        }

        let raw = read_key()?;
        if raw.ctrl && raw.code == KeyCode::Char('c') {
            ui::show_cursor();
            std::process::exit(130);
        }

        // ---- delete confirmation: one key, only y confirms, anything else backs out
        if matches!(prompt.as_ref().map(|p| &p.kind), Some(PromptKind::Delete(_))) {
            let p = prompt.take().unwrap();
            if let PromptKind::Delete(plan) = p.kind {
                if !raw.ctrl && matches!(raw.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    st.snapshot();
                    let n = delete_blocks(&mut st.doc, &st.rows, &plan.set);
                    st.drop_agents.extend(plan.agents.iter().cloned());
                    st.dirty = true;
                    st.rebuild();
                    st.flash = format!("deleted {} block(s), {n} line(s) · chain relinked", plan.set.len());
                } else {
                    st.flash = "cancelled".into();
                }
            }
            continue;
        }

        // ---- leaving with unsaved changes: one key — y discards, w saves, anything else stays
        if matches!(prompt.as_ref().map(|p| &p.kind), Some(PromptKind::Leave)) {
            prompt = None;
            match raw.code {
                KeyCode::Char('y') | KeyCode::Char('Y') if !raw.ctrl => {
                    ui::show_cursor();
                    return Ok(());
                }
                KeyCode::Char('w') | KeyCode::Char('W') if !raw.ctrl => match st.save() {
                    Ok(()) => {
                        ui::show_cursor();
                        return Ok(());
                    }
                    Err(e) => st.flash = format!("save failed: {e}"),
                },
                _ => st.flash = "staying".into(),
            }
            continue;
        }

        // ---- prompt line mode
        if let Some(p) = prompt.as_mut() {
            match raw.code {
                KeyCode::Esc => prompt = None,
                KeyCode::Backspace => {
                    p.buf.pop();
                }
                KeyCode::Char(c) if !raw.ctrl => p.buf.push(c),
                KeyCode::Enter => {
                    let p = prompt.take().unwrap();
                    st.flash.clear();
                    match p.kind {
                        PromptKind::Goto => {
                            if !p.buf.trim().is_empty() {
                                match goto_target(&st.rows, &vis, &p.buf) {
                                    Ok(n) => st.sel = n,
                                    Err(e) => st.flash = e,
                                }
                            }
                        }
                        PromptKind::Search => {
                            st.query = p.buf.trim().to_string();
                            if !st.query.is_empty() {
                                st.jump(&vis, true, true);
                            }
                        }
                        PromptKind::Filter => {
                            st.filter = p.buf.trim().to_string();
                            st.sel = 0;
                            st.top = 0;
                        }
                        PromptKind::Delete(_) => {}
                        PromptKind::Leave => {}
                    }
                }
                _ => {}
            }
            continue;
        }

        let key = input::gate_vim(raw, &[]);
        st.flash.clear();
        let n = vis.len();
        if input::vim_jump(&key, &mut st.sel, n, (vp / 2).max(1), true) {
            continue;
        }
        let cur_row = vis.get(st.sel).copied();
        let ranged = key.shift && !key.ctrl && matches!(key.code, KeyCode::Up | KeyCode::Down | KeyCode::Home | KeyCode::End | KeyCode::PageUp | KeyCode::PageDown);
        if !ranged {
            anchor = None;
        }
        match key.code {
            _ if ranged => {
                let a = *anchor.get_or_insert_with(|| {
                    base = st.picked.clone();
                    st.sel
                });
                st.sel = match key.code {
                    KeyCode::Up => st.sel.saturating_sub(1),
                    KeyCode::Down => (st.sel + 1).min(n.saturating_sub(1)),
                    KeyCode::Home => 0,
                    KeyCode::End => n.saturating_sub(1),
                    KeyCode::PageUp => st.sel.saturating_sub(5),
                    _ => (st.sel + 5).min(n.saturating_sub(1)),
                };
                let (lo, hi) = (a.min(st.sel), a.max(st.sel));
                st.picked = base.clone();
                for &ri in vis.iter().skip(lo).take(hi + 1 - lo) {
                    st.picked.insert(st.rows[ri].key());
                }
            }
            KeyCode::Up if key.ctrl => st.sel = 0,
            KeyCode::Down if key.ctrl => st.sel = n.saturating_sub(1),
            KeyCode::Home => st.sel = 0,
            KeyCode::End => st.sel = n.saturating_sub(1),
            KeyCode::Up | KeyCode::Char('k') => st.sel = st.sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => st.sel = (st.sel + 1).min(n.saturating_sub(1)),
            KeyCode::PageUp => st.sel = st.sel.saturating_sub(5),
            KeyCode::PageDown => st.sel = (st.sel + 5).min(n.saturating_sub(1)),
            KeyCode::Char(' ') => {
                if let Some(ri) = cur_row {
                    let k = st.rows[ri].key();
                    if !st.picked.remove(&k) {
                        st.picked.insert(k);
                    }
                }
            }
            KeyCode::Char('d') => {
                let mut picked = st.picked.clone();
                if picked.is_empty() {
                    if let Some(ri) = cur_row {
                        picked.insert(st.rows[ri].key());
                    }
                }
                if !picked.is_empty() {
                    let plan = plan_delete(&st.rows, &picked);
                    let mut q = format!("Delete {} block{}", plan.selected, if plan.selected == 1 { "" } else { "s" });
                    if plan.paired > 0 {
                        q.push_str(&format!(" + {} paired", plan.paired));
                    }
                    if !plan.agents.is_empty() {
                        q.push_str(&format!(" + {} subagent file{}", plan.agents.len(), if plan.agents.len() == 1 { "" } else { "s" }));
                    }
                    q.push_str("? (y/n) ");
                    prompt = Some(Prompt { q, buf: String::new(), kind: PromptKind::Delete(plan) });
                }
            }
            KeyCode::Char('t') => {
                if let Some(ri) = cur_row {
                    let keep = snap_turn(&st.rows, st.rows[ri].turn);
                    let dropped_turns = st.rows.iter().filter(|r| r.turn > keep).map(|r| r.turn).collect::<HashSet<_>>().len();
                    if dropped_turns == 0 {
                        st.flash = "nothing after this turn".into();
                    } else {
                        let calls = st.rows.iter().filter(|r| r.turn > keep && matches!(r.kind, Kind::Tool | Kind::Mcp | Kind::Agent)).count();
                        let heading = format!("Truncate after turn {keep}");
                        let hint = format!("keeps turns 1–{keep}, drops {dropped_turns} turn(s) and {calls} tool call(s) — never leaves a tool call unanswered");
                        let options = [
                            ListOption { label: "In place".into(), value: "in".into(), note: Some("this session, unsaved until you press w".into()) },
                            ListOption { label: "Fork".into(), value: "fork".into(), note: Some("new session, original untouched".into()) },
                            ListOption { label: "Cancel".into(), value: "cancel".into(), note: None },
                        ];
                        let choice = picker::choose_from_list(profiles, mark, &heading, Some(&hint), &options, None)?;
                        ui::hide_cursor();
                        if let ListChoice::Picked(v) = choice {
                            match v.as_str() {
                                "in" => {
                                    st.snapshot();
                                    let agents = truncate_doc(&mut st.doc, &st.rows, keep);
                                    st.drop_agents.extend(agents);
                                    st.dirty = true;
                                    st.rebuild();
                                    st.flash = format!("truncated after turn {keep}");
                                }
                                "fork" => match st.fork(keep) {
                                    Ok(new_id) => st.flash = format!("forked → {} (turns 1–{keep})", &new_id[..8]),
                                    Err(e) => st.flash = format!("fork failed: {e}"),
                                },
                                _ => {}
                            }
                        }
                    }
                }
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                if let Some(ri) = cur_row {
                    edit_block(&mut st, ri, false, profiles, mark)?;
                    ui::hide_cursor();
                }
            }
            KeyCode::Char('J') => {
                if let Some(ri) = cur_row {
                    edit_block(&mut st, ri, true, profiles, mark)?;
                    ui::hide_cursor();
                }
            }
            KeyCode::Char('v') => {
                if let Some(ri) = cur_row {
                    read_full(&st, ri, &header)?;
                    ui::hide_cursor();
                }
            }
            KeyCode::Char(':') => prompt = Some(Prompt { q: "Go to turn (42 or 42.3): ".into(), buf: String::new(), kind: PromptKind::Goto }),
            KeyCode::Char('/') => prompt = Some(Prompt { q: "Search (full text incl. tool payloads): ".into(), buf: st.query.clone(), kind: PromptKind::Search }),
            KeyCode::Char('f') => prompt = Some(Prompt { q: "Filter (kind:tool_use  kind:mcp  name:Bash  text — empty clears): ".into(), buf: st.filter.clone(), kind: PromptKind::Filter }),
            KeyCode::Char('n') => st.jump(&vis, true, false),
            KeyCode::Char('N') => st.jump(&vis, false, false),
            KeyCode::Char('u') => st.undo(),
            KeyCode::Char('w') => {
                if let Err(e) = st.save() {
                    st.flash = format!("save failed: {e}");
                }
            }
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('q') | KeyCode::Char('h') => {
                if !st.query.is_empty() {
                    st.query.clear();
                    st.flash = "search cleared".into();
                } else if !st.filter.is_empty() {
                    st.filter.clear();
                    st.flash = "filter cleared".into();
                } else if st.dirty {
                    prompt = Some(Prompt { q: "Unsaved changes — y discard, w save and leave, any other key stays: ".into(), buf: String::new(), kind: PromptKind::Leave });
                } else {
                    ui::show_cursor();
                    return Ok(());
                }
            }
            _ => {}
        }
    }
}

/// Opens one block in the in-app editor — plain text for text/result/thinking,
/// pretty JSON for tool input, or the whole block's JSON with `whole`.
fn edit_block(st: &mut State, ri: usize, whole: bool, profiles: &[Profile], mark: i64) -> std::io::Result<()> {
    let r = &st.rows[ri];
    let blocks = blocks_of(&st.doc[r.line]);
    let Some(block) = blocks.get(r.blk) else { return Ok(()) };
    let json_mode = whole || r.kind == Kind::Tool || r.kind == Kind::Mcp || r.kind == Kind::Agent;
    let original = if whole { serde_json::to_string_pretty(block).unwrap_or_default() } else { block_body(block) };
    let ext = if json_mode { "json" } else { "txt" };
    let tmp = std::env::temp_dir().join(format!("cockpit-transcript-{}.{ext}", std::process::id()));
    std::fs::write(&tmp, &original)?;
    let etitle = format!("{} {}{} — {}", r.label(), r.kind.filter_name(), if whole { " (JSON)" } else { "" }, &st.id[..st.id.len().min(8)]);
    crate::editor::edit_file(&tmp, profiles, mark, Some(&etitle))?;
    let edited = std::fs::read_to_string(&tmp).unwrap_or_default();
    let _ = std::fs::remove_file(&tmp);
    let edited_cmp = edited.trim_end_matches(['\n', '\r']);
    if edited_cmp == original.trim_end_matches(['\n', '\r']) {
        return Ok(());
    }
    let kind = r.kind;
    let r_line = r.line;
    let r_blk = r.blk;
    let probe = Row {
        line: r_line,
        blk: r_blk,
        turn: 0,
        sub: 0,
        user: false,
        who: "",
        kind,
        name: String::new(),
        tid: String::new(),
        preview: String::new(),
        text: String::new(),
        chars: 0,
        lower: String::new(),
        big: None,
        agent_id: None,
        agent_prompt: String::new(),
    };
    let text = edited_cmp.to_string();
    let applied = if whole {
        match serde_json::from_str::<Value>(&text) {
            Ok(nb) => {
                let same = |v: &Value, k: &str| v.get(k).cloned();
                let id_ok = same(&nb, "id") == same(block, "id") && same(&nb, "tool_use_id") == same(block, "tool_use_id") && same(&nb, "type") == same(block, "type");
                if !id_ok {
                    st.flash = "type/id are locked — edit discarded".into();
                    return Ok(());
                }
                st.snapshot();
                set_block(&mut st.doc, &probe, |b| *b = nb)
            }
            Err(e) => {
                st.flash = format!("invalid JSON ({e}) — edit discarded");
                return Ok(());
            }
        }
    } else if json_mode {
        match serde_json::from_str::<Value>(&text) {
            Ok(input) => {
                st.snapshot();
                set_block(&mut st.doc, &probe, |b| b["input"] = input)
            }
            Err(e) => {
                st.flash = format!("invalid JSON ({e}) — edit discarded");
                return Ok(());
            }
        }
    } else {
        st.snapshot();
        match kind {
            Kind::Text => set_block(&mut st.doc, &probe, |b| b["text"] = Value::String(text)),
            Kind::Think => set_block(&mut st.doc, &probe, |b| {
                b["thinking"] = Value::String(text);
                // an edited thought no longer matches its signature
                if let Some(o) = b.as_object_mut() {
                    o.remove("signature");
                }
            }),
            Kind::Result => set_block(&mut st.doc, &probe, |b| b["content"] = Value::String(text)),
            _ => false,
        }
    };
    if applied {
        st.dirty = true;
        st.rebuild();
        st.flash = "edited · press w to save".into();
    } else {
        st.hist.pop();
        st.flash = "could not apply the edit".into();
    }
    Ok(())
}

/// Full-screen, scrollable read-only view of one block's whole payload.
fn read_full(st: &State, ri: usize, header: &[String]) -> std::io::Result<()> {
    let r = &st.rows[ri];
    let blocks = blocks_of(&st.doc[r.line]);
    let Some(block) = blocks.get(r.blk) else { return Ok(()) };
    let body = if r.kind == Kind::Agent && r.agent_id.is_some() {
        format!("{}\n\n— subagent {} · prompt —\n{}", block_body(block), r.agent_id.as_deref().unwrap_or(""), r.agent_prompt)
    } else {
        block_body(block)
    };
    let (tw, _) = crossterm::terminal::size().map(|(w, h)| (w as usize, h as usize)).unwrap_or((100, 30));
    let (wrapped, cut) = wrap_text(&body, tw.saturating_sub(8).min(110), READ_CAP / 80);
    let mut h = header.to_vec();
    h.push(format!(
        "{} {}",
        ui::color::orange(ui::glyph::back()),
        ui::color::bold(&format!("{} {}{}", r.label(), r.kind.filter_name(), if r.name.is_empty() { String::new() } else { format!(" · {}", r.name) }))
    ));
    h.push(ui::color::dim(&format!("   {} · {} chars · {} lines{}", r.who, r.chars, wrapped.len(), if cut { " (shown part)" } else { "" })));
    h.push(String::new());
    let rows: Vec<ScrollRow> = wrapped.into_iter().map(|l| ScrollRow::info(format!("   {l}"))).collect();
    picker::scroll_screen(&h, &rows, None, None)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> Vec<L> {
        let lines = [
            r#"{"type":"last-prompt","leafUuid":"u4"}"#,
            r#"{"type":"user","uuid":"u1","parentUuid":null,"message":{"role":"user","content":"hello there"}}"#,
            r#"{"type":"assistant","uuid":"u2","parentUuid":"u1","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_A","name":"Bash","input":{"command":"ls"}}]}}"#,
            r#"{"type":"user","uuid":"u3","parentUuid":"u2","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_A","content":"a.txt"}]}}"#,
            r#"{"type":"api-request-blob","x":"quotes toolu_A somewhere"}"#,
            r#"{"type":"assistant","uuid":"u4","parentUuid":"u3","message":{"role":"assistant","content":[{"type":"text","text":"done, found a.txt"}]}}"#,
            r#"{"type":"attachment","uuid":"u5","parentUuid":"u4"}"#,
        ];
        lines.iter().map(|s| L::parse(s)).collect()
    }

    #[test]
    fn rows_and_turns() {
        let d = doc();
        let rows = build_rows(&d, Path::new("."));
        assert_eq!(rows.len(), 4);
        assert_eq!(rows.iter().map(|r| r.turn).collect::<Vec<_>>(), vec![1, 2, 3, 4]);
        assert_eq!(rows[1].kind, Kind::Tool);
        assert_eq!(rows[2].kind, Kind::Result);
    }

    #[test]
    fn delete_removes_pair_and_relinks() {
        let mut d = doc();
        let rows = build_rows(&d, Path::new("."));
        let picked: HashSet<(usize, usize)> = [rows[1].key()].into_iter().collect();
        let plan = plan_delete(&rows, &picked);
        assert_eq!(plan.paired, 1);
        let n = delete_blocks(&mut d, &rows, &plan.set);
        assert_eq!(n, 2);
        // blob quoting the tool id is scrubbed too
        assert!(d.iter().all(|l| l.ty() != "api-request-blob"));
        // u4 now hangs off u1, attachment still off u4
        let u4 = d.iter().find(|l| l.uuid() == Some("u4")).unwrap();
        assert_eq!(u4.parent(), Some("u1"));
        let nrows = build_rows(&d, Path::new("."));
        assert_eq!(nrows.len(), 2);
    }

    #[test]
    fn truncate_keeps_pair_and_metadata() {
        let mut d = doc();
        let rows = build_rows(&d, Path::new("."));
        // cutting after the tool call turn must keep its result turn
        assert_eq!(snap_turn(&rows, 2), 3);
        truncate_doc(&mut d, &rows, 3);
        assert!(d.iter().any(|l| l.ty() == "last-prompt"));
        assert!(d.iter().all(|l| l.uuid() != Some("u4") && l.uuid() != Some("u5")));
        let lp = d.iter().find(|l| l.ty() == "last-prompt").unwrap();
        assert_eq!(lp.v.as_ref().unwrap()["leafUuid"], "u3");
    }

    #[test]
    fn strip_thinking_drops_blocks_and_relinks() {
        let text = [
            r#"{"type":"user","uuid":"a","parentUuid":null,"message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"assistant","uuid":"b","parentUuid":"a","message":{"role":"assistant","content":[{"type":"thinking","thinking":"","signature":"XYZ"}]}}"#,
            r#"{"type":"assistant","uuid":"c","parentUuid":"b","message":{"role":"assistant","content":[{"type":"thinking","thinking":"","signature":"Q"},{"type":"text","text":"hello"}]}}"#,
            "",
        ]
        .join("\n");
        let (out, n) = strip_thinking(&text);
        assert_eq!(n, 2);
        assert!(!out.contains("thinking"));
        assert!(out.ends_with('\n'));
        let c: Value = serde_json::from_str(out.lines().nth(1).unwrap()).unwrap();
        assert_eq!(c["uuid"], "c");
        assert_eq!(c["parentUuid"], "a"); // b was dropped, c now hangs off a
        // nothing to strip: returned byte-for-byte
        let plain = "{\"type\":\"user\"}\n";
        assert_eq!(strip_thinking(plain), (plain.to_string(), 0));
    }

    #[test]
    fn edit_string_content_and_untouched_lines_stay_raw() {
        let mut d = doc();
        let rows = build_rows(&d, Path::new("."));
        assert!(set_block(&mut d, &rows[0], |b| b["text"] = Value::String("changed".into())));
        assert!(d[1].out().contains("changed"));
        assert_eq!(d[2].out(), d[2].raw); // untouched line unchanged
    }

    #[test]
    #[ignore]
    fn real_session_smoke() {
        let p = std::env::var("TRANSCRIPT_FILE").expect("TRANSCRIPT_FILE");
        let st = State::load(Path::new(&p), "x", "t").unwrap();
        let agents = st.rows.iter().filter(|r| r.kind == Kind::Agent).count();
        let with_id = st.rows.iter().filter(|r| r.agent_id.is_some()).count();
        println!("lines {} rows {} turns {} agents {} (with subagent id {})", st.doc.len(), st.rows.len(), st.turns0, agents, with_id);
        // delete one tool call + truncate must keep a well-formed chain
        let mut doc = st.doc.clone();
        let r = st.rows.iter().find(|r| r.kind == Kind::Tool).unwrap();
        let plan = plan_delete(&st.rows, &[r.key()].into_iter().collect());
        let n = delete_blocks(&mut doc, &st.rows, &plan.set);
        println!("deleted pair: {} lines removed", n);
        let rows2 = build_rows(&doc, &st.agent_dir);
        assert_eq!(rows2.len(), st.rows.len() - plan.set.len());
        let mut d2 = st.doc.clone();
        truncate_doc(&mut d2, &st.rows, snap_turn(&st.rows, 30));
        println!("truncate@30: {} -> {} lines", st.doc.len(), d2.len());
    }

    #[test]
    fn who_labels_tool_output_vs_user_answers() {
        let d: Vec<L> = [
            r#"{"type":"user","uuid":"a","message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"assistant","uuid":"b","parentUuid":"a","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"AskUserQuestion","input":{}},{"type":"tool_use","id":"t2","name":"Bash","input":{}}]}}"#,
            r#"{"type":"user","uuid":"c","parentUuid":"b","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"User answered: yes"},{"type":"tool_result","tool_use_id":"t2","content":"ok"}]}}"#,
        ]
        .iter()
        .map(|s| L::parse(s))
        .collect();
        let rows = build_rows(&d, Path::new("."));
        let whos: Vec<&str> = rows.iter().map(|r| r.who).collect();
        assert_eq!(whos, vec!["You", "Claude", "Claude", "You", "Tool"]);
    }

    #[test]
    fn goto_parses_turn_and_block() {
        let d = doc();
        let rows = build_rows(&d, Path::new("."));
        let vis: Vec<usize> = (0..rows.len()).collect();
        assert_eq!(goto_target(&rows, &vis, "3"), Ok(2));
        assert_eq!(goto_target(&rows, &vis, "#4"), Ok(3));
        assert!(goto_target(&rows, &vis, "99").unwrap_err().contains("last one is 4"));
        assert!(goto_target(&rows, &vis, "2.5").unwrap_err().contains("no block"));
        assert!(goto_target(&rows, &vis, "abc").is_err());
        // hidden by a filter
        assert!(goto_target(&rows, &[0, 3], "3").unwrap_err().contains("filter"));
    }
}
