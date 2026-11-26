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
use crate::{toc, ui};
use crossterm::event::KeyCode;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Rows of the detail pane under the list (1 title line + body lines).
const DET: usize = 6;
/// Longest payload kept per row for the pane + search (the file keeps all of it).
const ROW_TEXT_CAP: usize = 20_000;
/// Lines the full-text reader will wrap before saying "truncated".
const READ_CAP: usize = 400_000;
/// Rows of key hints under the detail pane.
const HINT_ROWS: usize = 4;

// ------------------------------------------------------------------ model

/// One physical line of the .jsonl. `raw` is written back byte-for-byte unless
/// the line was modified (then it is re-serialized from `v`), so untouched
/// lines keep their original key order and spacing.
///
/// Only message lines (user/assistant) are fully parsed up front — they carry
/// the content the editor shows. Everything else (hooks, attachments, request
/// blobs, snapshots; most of a big file's bytes) is read just far enough to get
/// type/uuid/parentUuid, and parsed in full only if it has to be changed.
/// Lines are shared (`Arc`) so an undo snapshot costs a pointer per line.
#[derive(Clone)]
struct L {
    raw: Raw,
    ty: String,
    uuid: Option<String>,
    parent: Option<String>,
    v: Option<Value>,
    dirty: bool,
}

/// The text of one line. Lines loaded from a file are windows into the single
/// buffer the file was read into (no per-line copy: a 230 MB transcript used to
/// be held twice); lines built any other way own their text.
#[derive(Clone)]
enum Raw {
    Slice(Arc<String>, usize, usize),
    Owned(String),
}

impl Raw {
    fn as_str(&self) -> &str {
        match self {
            Raw::Slice(buf, a, b) => &buf[*a..*b],
            Raw::Owned(s) => s,
        }
    }
}

#[derive(serde::Deserialize)]
struct Head {
    #[serde(rename = "type")]
    ty: Option<String>,
    uuid: Option<String>,
    #[serde(rename = "parentUuid")]
    parent: Option<String>,
}

impl L {
    fn parse(raw: &str) -> L {
        L::from_raw(Raw::Owned(raw.to_string()))
    }
    fn from_raw(raw: Raw) -> L {
        let text = raw.as_str();
        let (ty, uuid, parent) = match serde_json::from_str::<Head>(text) {
            Ok(h) => (h.ty.unwrap_or_default(), h.uuid, h.parent),
            Err(_) => (String::new(), None, None),
        };
        let v = if matches!(ty.as_str(), "user" | "assistant" | "system") { serde_json::from_str(text).ok() } else { None };
        L { raw, ty, uuid, parent, v, dirty: false }
    }
    fn raw(&self) -> &str {
        self.raw.as_str()
    }
    /// Full JSON of the line, parsed on first use for lines not parsed up front.
    fn value_mut(&mut self) -> Option<&mut Value> {
        if self.v.is_none() {
            self.v = serde_json::from_str(self.raw.as_str()).ok();
        }
        self.v.as_mut()
    }
    fn out(&self) -> String {
        match (&self.v, self.dirty) {
            (Some(v), true) => v.to_string(),
            _ => self.raw().to_string(),
        }
    }
    fn ty(&self) -> &str {
        &self.ty
    }
    fn uuid(&self) -> Option<&str> {
        self.uuid.as_deref()
    }
    fn parent(&self) -> Option<&str> {
        self.parent.as_deref()
    }
    fn set_parent(&mut self, p: Option<String>) {
        if let Some(v) = self.value_mut() {
            v["parentUuid"] = p.clone().map(Value::String).unwrap_or(Value::Null);
            self.dirty = true;
        }
        self.parent = p;
    }
}

type Doc = Vec<Arc<L>>;

/// Splits and parses the file's lines across a few threads. A 200 MB
/// transcript is ~20k lines; parsing them on one thread was the main cost of
/// opening the editor. Takes the file's text by value: the lines stay windows
/// into that one buffer.
fn parse_doc(text: String) -> Doc {
    let buf = Arc::new(text);
    let base = buf.as_ptr() as usize;
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for line in buf.split('\n') {
        let start = line.as_ptr() as usize - base;
        let mut end = start + line.len();
        if end > start && buf.as_bytes()[end - 1] == b'\r' {
            end -= 1;
        }
        ranges.push((start, end));
    }
    if ranges.last().map(|&(a, b)| a == b).unwrap_or(false) {
        ranges.pop();
    }
    let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 8);
    let chunk = ranges.len().div_ceil(workers).max(1);
    let mut out: Doc = Vec::with_capacity(ranges.len());
    std::thread::scope(|scope| {
        let handles: Vec<_> = ranges
            .chunks(chunk)
            .map(|c| {
                let buf = buf.clone();
                scope.spawn(move || c.iter().map(|&(a, b)| Arc::new(L::from_raw(Raw::Slice(buf.clone(), a, b)))).collect::<Vec<_>>())
            })
            .collect();
        for h in handles {
            out.extend(h.join().unwrap());
        }
    });
    out
}

/// Writes the document line by line (no giant joined String for big files).
fn write_doc(doc: &[Arc<L>], path: &Path) -> std::io::Result<()> {
    use std::io::Write;
    let mut w = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(path)?);
    for l in doc {
        match (&l.v, l.dirty) {
            (Some(v), true) => serde_json::to_writer(&mut w, v)?,
            _ => w.write_all(l.raw().as_bytes())?,
        }
        w.write_all(b"\n")?;
    }
    w.flush()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Text,
    Think,
    Tool,
    Mcp,
    Agent,
    Result,
    /// A harness notice stored as a `system` line (safeguard stops, fallbacks…).
    Notice,
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
            Kind::Notice => "note",
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
            Kind::Notice => "notice",
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
    agent_id: Option<String>,
    agent_prompt: String,
    /// Text of the row (preview, capped payload, size), built the first time the
    /// row is drawn / edited and not for the thousands of rows nobody looks at.
    info: std::cell::OnceCell<Info>,
    /// Lowercased search text; built on the first search or text filter.
    lower: std::cell::OnceCell<String>,
}

#[derive(Default)]
struct Info {
    preview: String,
    /// Capped payload (pane + search); the full one is re-read from the doc.
    text: String,
    chars: usize,
    big: Option<String>,
}

fn compute_info(l: &L, blk: usize, kind: Kind, name: &str) -> Info {
    let mut info = Info::default();
    for_each_block(l, |i, b| {
        if i != blk {
            return;
        }
        let body = block_body(b);
        info.chars = body.chars().count();
        info.text = cap_chars(&body, ROW_TEXT_CAP);
        let input = || b.get("input").map(|i| i.to_string()).unwrap_or_default();
        info.preview = match kind {
            Kind::Text if b.get("type").and_then(|t| t.as_str()) == Some("image") => "[image]".to_string(),
            Kind::Text => flat(&body, 300),
            Kind::Think => {
                if body.is_empty() {
                    "(empty, signature only)".to_string()
                } else {
                    flat(&body, 300)
                }
            }
            Kind::Agent => {
                let st = b.get("input").and_then(|i| i.get("subagent_type")).and_then(|p| p.as_str()).unwrap_or("agent");
                flat(&format!("{st}  {}", input()), 300)
            }
            Kind::Mcp => {
                let rest = name.strip_prefix("mcp__").unwrap_or(name);
                let mut parts = rest.splitn(2, "__");
                let server = parts.next().unwrap_or("");
                let tool = parts.next().unwrap_or("");
                flat(&format!("{server} · {tool}  {}", input()), 300)
            }
            Kind::Tool => flat(&format!("{name}  {}", input()), 300),
            Kind::Notice => flat(&body, 300),
            Kind::Result => {
                if info.chars > 20_000 {
                    info.big = Some(format!("{} KB", info.chars / 1000));
                }
                flat(&body, 300)
            }
        };
    });
    info
}

impl Row {
    fn info(&self, doc: &[Arc<L>]) -> &Info {
        self.info.get_or_init(|| compute_info(&doc[self.line], self.blk, self.kind, &self.name))
    }
    /// Search text: the whole block, not the capped copy the preview and the
    /// detail pane use.
    fn lower(&self, doc: &[Arc<L>]) -> &str {
        self.lower.get_or_init(|| {
            let body = block_at(&doc[self.line], self.blk).map(|b| block_body(&b)).unwrap_or_default();
            format!("{} {} {body}", self.kind.filter_name(), self.name).to_lowercase()
        })
    }
    /// A row standing in for one block, to address it when editing.
    fn probe(line: usize, blk: usize, kind: Kind) -> Row {
        Row {
            line,
            blk,
            turn: 0,
            sub: 0,
            user: false,
            who: "",
            kind,
            name: String::new(),
            tid: String::new(),
            agent_id: None,
            agent_prompt: String::new(),
            info: Default::default(),
            lower: Default::default(),
        }
    }
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
fn for_each_block(l: &L, mut f: impl FnMut(usize, &Value)) {
    let Some(v) = &l.v else { return };
    if l.ty() == "system" {
        if let Some(t) = notice_text(v) {
            f(0, &serde_json::json!({"type": "text", "text": t}));
        }
        return;
    }
    match v.get("message").and_then(|m| m.get("content")) {
        Some(Value::String(s)) => {
            let b = serde_json::json!({"type": "text", "text": s});
            f(0, &b);
        }
        Some(Value::Array(a)) => {
            for (i, b) in a.iter().enumerate() {
                f(i, b);
            }
        }
        _ => {}
    }
}

/// Text of a `system` line worth showing: the harness's informational notices
/// and model-refusal records. Other system lines (timings, boundaries) are noise.
fn notice_text(v: &Value) -> Option<String> {
    let sub = v.get("subtype").and_then(|s| s.as_str()).unwrap_or("");
    if !(sub == "informational" || sub.starts_with("model_refusal")) || v.get("isMeta").and_then(|x| x.as_bool()) == Some(true) {
        return None;
    }
    let get = |k: &str| v.get(k).and_then(|c| c.as_str()).map(str::trim).filter(|c| !c.is_empty());
    get("content").or_else(|| get("apiRefusalExplanation")).map(str::to_string)
}

/// One block cloned out of its line (to edit or read in full).
fn block_at(l: &L, blk: usize) -> Option<Value> {
    let mut out = None;
    for_each_block(l, |i, b| {
        if i == blk {
            out = Some(b.clone());
        }
    });
    out
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

fn build_rows(doc: &[Arc<L>], agent_dir: &Path) -> Vec<Row> {
    // tool_use id -> subagent id, taken from the tool_result line's metadata.
    let mut agent_of: HashMap<String, String> = HashMap::new();
    for l in doc {
        if l.ty() != "user" {
            continue;
        }
        let Some(v) = &l.v else { continue };
        let Some(aid) = v.get("toolUseResult").and_then(|t| t.get("agentId")).and_then(|a| a.as_str()) else { continue };
        for_each_block(l, |_, b| {
            if b.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                if let Some(tid) = b.get("tool_use_id").and_then(|t| t.as_str()) {
                    agent_of.insert(tid.to_string(), aid.to_string());
                }
            }
        });
    }
    let _ = agent_dir;

    let mut rows = Vec::new();
    let mut turn = 0usize;
    let mut sub = 0usize;
    let mut last_user: Option<bool> = None;
    for (li, l) in doc.iter().enumerate() {
        if l.ty() == "system" {
            // Notices hang off the turn they follow (no new turn of their own);
            // ones before the first message are turn 0.
            if l.v.as_ref().and_then(notice_text).is_some() {
                rows.push(Row {
                    line: li,
                    blk: 0,
                    turn,
                    sub,
                    user: false,
                    who: "System",
                    kind: Kind::Notice,
                    name: String::new(),
                    tid: String::new(),
                    agent_id: None,
                    agent_prompt: String::new(),
                    info: Default::default(),
                    lower: Default::default(),
                });
                sub += 1;
            }
            continue;
        }
        if !is_message_line(l) {
            continue;
        }
        let user = l.ty() == "user";
        let mut first_in_line = true;
        for_each_block(l, |bi, b| {
            let ty = b.get("type").and_then(|t| t.as_str()).unwrap_or("");
            let mut name = String::new();
            let mut tid = String::new();
            let mut agent_id = None;
            let mut agent_prompt = String::new();
            let kind = match ty {
                "text" => {
                    let t = b.get("text").and_then(|t| t.as_str()).unwrap_or("").trim_start();
                    if t.is_empty() || t.starts_with("<system-reminder>") {
                        return;
                    }
                    Kind::Text
                }
                "thinking" => Kind::Think,
                "tool_use" => {
                    name = b.get("name").and_then(|n| n.as_str()).unwrap_or("?").to_string();
                    tid = b.get("id").and_then(|n| n.as_str()).unwrap_or("").to_string();
                    if name == "Agent" || name == "Task" {
                        agent_id = agent_of.get(&tid).cloned();
                        agent_prompt = b.get("input").and_then(|i| i.get("prompt")).and_then(|p| p.as_str()).unwrap_or("").to_string();
                        Kind::Agent
                    } else if name.starts_with("mcp__") {
                        Kind::Mcp
                    } else {
                        Kind::Tool
                    }
                }
                "tool_result" => {
                    tid = b.get("tool_use_id").and_then(|n| n.as_str()).unwrap_or("").to_string();
                    Kind::Result
                }
                "image" => Kind::Text,
                _ => return,
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
            rows.push(Row {
                line: li,
                blk: bi,
                turn,
                sub,
                user,
                who: if user { "You" } else { "Claude" },
                kind,
                name,
                tid,
                agent_id,
                agent_prompt,
                info: Default::default(),
                lower: Default::default(),
            });
            sub += 1;
        });
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
fn delete_blocks(doc: &mut Doc, rows: &[Row], set: &HashSet<(usize, usize)>) -> usize {
    let mut needles: Vec<String> = Vec::new();
    for r in rows.iter().filter(|r| set.contains(&r.key())) {
        if r.kind == Kind::Notice {
            continue;
        } else if !r.tid.is_empty() {
            needles.push(r.tid.clone());
        } else {
            let info = r.info(doc);
            if info.chars >= 30 {
                needles.push(cap_chars(&info.text, 60));
            }
        }
    }
    let mut by_line: HashMap<usize, Vec<usize>> = HashMap::new();
    for (l, b) in set {
        by_line.entry(*l).or_default().push(*b);
    }
    let mut removed: HashSet<usize> = HashSet::new();
    for (li, mut blks) in by_line {
        blks.sort_unstable_by(|a, b| b.cmp(a));
        let line = Arc::make_mut(&mut doc[li]);
        if line.ty() == "system" {
            removed.insert(li);
            continue;
        }
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
fn remove_lines(doc: &mut Doc, removed: &HashSet<usize>, needles: &[String]) {
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
    let mut keep: Doc = Vec::with_capacity(doc.len());
    for (i, l) in doc.drain(..).enumerate() {
        if removed.contains(&i) {
            continue;
        }
        if l.ty().starts_with("api-request") && !needles.is_empty() && needles.iter().any(|n| l.raw().contains(n.as_str())) {
            continue;
        }
        keep.push(l);
    }
    for l in keep.iter_mut() {
        if let Some(p) = l.parent().map(|s| s.to_string()) {
            if gone_parent.contains_key(&p) {
                let np = resolve(Some(p));
                Arc::make_mut(l).set_parent(np);
            }
        }
        if l.ty() == "last-prompt" {
            let m = Arc::make_mut(l);
            let leaf = m.value_mut().and_then(|v| v.get("leafUuid")).and_then(|x| x.as_str()).map(|s| s.to_string());
            if let Some(leaf) = leaf {
                if gone_parent.contains_key(&leaf) {
                    let np = resolve(Some(leaf));
                    if let Some(v) = m.value_mut() {
                        v["leafUuid"] = np.map(Value::String).unwrap_or(Value::Null);
                        m.dirty = true;
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
fn truncate_doc(doc: &mut Doc, rows: &[Row], keep_turn: usize) -> Vec<String> {
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
    if let Some(leaf) = leaf {
        for l in doc.iter_mut() {
            if l.ty() == "last-prompt" {
                let m = Arc::make_mut(l);
                if let Some(v) = m.value_mut() {
                    v["leafUuid"] = Value::String(leaf.clone());
                    m.dirty = true;
                }
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
    if !text.contains("thinking") {
        return (text.to_string(), 0);
    }
    let trailing = text.ends_with('\n');
    let mut doc = parse_doc(text.to_string());
    let mut emptied: HashSet<usize> = HashSet::new();
    let mut total = 0usize;
    for (i, l) in doc.iter_mut().enumerate() {
        if !l.raw().contains("thinking") {
            continue;
        }
        let l = Arc::make_mut(l);
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

fn set_block(doc: &mut [Arc<L>], r: &Row, apply: impl FnOnce(&mut Value)) -> bool {
    let line = Arc::make_mut(&mut doc[r.line]);
    let Some(v) = line.v.as_mut() else { return false };
    let Some(content) = v.get_mut("message").and_then(|m| m.get_mut("content")) else { return false };
    let ok = match content {
        Value::Array(a) => match a.get_mut(r.blk) {
            Some(b) => {
                apply(b);
                true
            }
            None => false,
        },
        Value::String(_) => {
            // typed user prompt: edit through a synthetic block, write the text back
            let mut b = serde_json::json!({"type": "text", "text": content.as_str().unwrap_or("")});
            apply(&mut b);
            match b.get("text").and_then(|t| t.as_str()) {
                Some(t) => {
                    *content = Value::String(t.to_string());
                    true
                }
                None => false,
            }
        }
        _ => false,
    };
    if ok {
        line.dirty = true;
    }
    ok
}

// ------------------------------------------------------------------ state

enum PromptKind {
    Pin(PinDraft),
    Goto,
    Search,
    Filter,
    Delete(DeletePlan),
    Leave,
}

/// What `p` is about to save: the part of the session and its text snapshot.
struct PinDraft {
    from_turn: usize,
    to_turn: usize,
    from_uuid: Option<String>,
    to_uuid: Option<String>,
    excerpt: String,
}

/// Text of the rows of turns `from..=to` (thinking skipped, long bodies cut), for a pin.
fn pin_excerpt(st: &State, from: usize, to: usize) -> String {
    const ROW_CAP: usize = 1500;
    const TOTAL_CAP: usize = 30_000;
    let mut out = String::new();
    for r in st.rows.iter().filter(|r| r.turn >= from && r.turn <= to && r.kind != Kind::Think) {
        let info = r.info(&st.doc);
        let line = match r.kind {
            Kind::Text => format!("{}: {}", r.who, cap_chars(info.text.trim(), ROW_CAP)),
            Kind::Result => format!("[result] {}", cap_chars(&info.preview, 200)),
            _ => format!("[{}] {}", r.kind.label(), cap_chars(&info.preview, 200)),
        };
        if out.chars().count() + line.chars().count() > TOTAL_CAP {
            out.push_str("\n\n… (cut)");
            break;
        }
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&line);
    }
    out
}

/// Turn range, uuids and a default label for the pin of `set` (the rows' turns,
/// from the first to the last).
fn pin_draft(st: &State, set: &HashSet<(usize, usize)>) -> Option<(PinDraft, String)> {
    let picked: Vec<&Row> = st.rows.iter().filter(|r| set.contains(&r.key())).collect();
    let first = *picked.first()?;
    let last = *picked.last()?;
    let (from, to) = (picked.iter().map(|r| r.turn).min()?, picked.iter().map(|r| r.turn).max()?);
    let label = st
        .rows
        .iter()
        .find(|r| r.turn >= from && r.kind == Kind::Text)
        .map(|r| flat(&r.info(&st.doc).text, 50))
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| st.title.clone());
    let draft = PinDraft {
        from_turn: from,
        to_turn: to,
        from_uuid: st.doc[first.line].uuid().map(|u| u.to_string()),
        to_uuid: st.doc[last.line].uuid().map(|u| u.to_string()),
        excerpt: pin_excerpt(st, from, to),
    };
    Some((draft, label))
}

/// Rows of the part a pin points at, found by message uuid first (still right
/// after edits renumbered the turns), else by the saved turn numbers.
fn pin_rows(st: &State, pin: &crate::pins::Pin) -> HashSet<(usize, usize)> {
    let turn_of = |uuid: &Option<String>, fallback: Option<usize>| -> Option<usize> {
        uuid.as_deref().and_then(|u| st.rows.iter().find(|r| st.doc[r.line].uuid() == Some(u)).map(|r| r.turn)).or(fallback)
    };
    let (Some(a), Some(b)) = (turn_of(&pin.from_uuid, pin.from_turn), turn_of(&pin.to_uuid, pin.to_turn)) else { return HashSet::new() };
    let (a, b) = (a.min(b), a.max(b));
    st.rows.iter().filter(|r| r.turn >= a && r.turn <= b).map(|r| r.key()).collect()
}

struct Prompt {
    q: String,
    buf: String,
    kind: PromptKind,
}

struct State {
    acct: String,
    file: PathBuf,
    id: String,
    title: String,
    agent_dir: PathBuf,
    doc: Doc,
    rows: Vec<Row>,
    hist: Vec<(Doc, Vec<String>)>,
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
    fn load(file: &Path, id: &str, title: &str, acct: &str) -> std::io::Result<State> {
        let text = std::fs::read_to_string(file)?;
        let doc = parse_doc(text);
        let agent_dir = file.with_extension("").join("subagents");
        let rows = build_rows(&doc, &agent_dir);
        let turns0 = rows.iter().map(|r| r.turn).max().unwrap_or(0);
        let blocks0 = rows.len();
        Ok(State {
            acct: acct.to_string(),
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
                        r.lower(&self.doc).contains(t.as_str())
                    }
                })
            })
            .collect()
    }

    fn hit(&self, r: &Row) -> bool {
        !self.query.is_empty() && r.lower(&self.doc).contains(&self.query.to_lowercase())
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
        write_doc(&self.doc, &self.file)?;
        layout::invalidate_stats();
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
        write_doc(&doc, &dest)?;
        layout::invalidate_stats();
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

/// First wrapped line showing a case-insensitive match of `q`, including one
/// that the wrapping split over two lines.
fn match_line(lines: &[String], q: &str) -> Option<usize> {
    if q.is_empty() {
        return None;
    }
    let q = q.to_lowercase();
    let mut next = lines.first()?.to_lowercase();
    for i in 0..lines.len() {
        let after = lines.get(i + 1).map(|l| l.to_lowercase()).unwrap_or_default();
        let cur = std::mem::replace(&mut next, after);
        if cur.contains(&q) {
            return Some(i);
        }
        // Split at a space (dropped by the wrapping) or in the middle of a word.
        if !next.contains(&q) && (format!("{cur} {next}").contains(&q) || format!("{cur}{next}").contains(&q)) {
            return Some(i);
        }
    }
    None
}

/// The full reader's wrapped text: at most `limit` lines, or `limit` lines past
/// the first match of `q` when that lies further down. Returns the lines,
/// whether any were left out, and the matching line.
fn reader_lines(body: &str, width: usize, q: &str, limit: usize) -> (Vec<String>, bool, Option<usize>) {
    if q.is_empty() || !body.to_lowercase().contains(&q.to_lowercase()) {
        let (lines, cut) = wrap_text(body, width, limit);
        return (lines, cut, None);
    }
    let (mut lines, _) = wrap_text(body, width, usize::MAX);
    let at = match_line(&lines, q);
    let keep = match at {
        Some(a) if a >= limit => a + limit,
        _ => limit,
    };
    let cut = lines.len() > keep;
    lines.truncate(keep);
    (lines, cut, at)
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

    // footer: detail pane + rule + hint rows + prompt/flash row
    let foot = DET + 1 + HINT_ROWS + 1;
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
        let info = r.info(&st.doc);
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
        } else if let Some(b) = &info.big {
            extra = format!("  {b}");
        }
        let hit = st.hit(r);
        let in_preview = hit && info.preview.to_lowercase().contains(&st.query.to_lowercase());
        let mut tail = clip(&info.preview, preview_w.saturating_sub(extra.chars().count()));
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
                Kind::Result | Kind::Think | Kind::Notice => (ui::color::dim(&kind), if in_preview { tail } else { ui::color::dim(&tail) }),
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
            let info = r.info(&st.doc);
            let (body, more) = wrap_text(&info.text, term_w.saturating_sub(4), DET - 1);
            let total_lines = if more || info.chars > ROW_TEXT_CAP { None } else { Some(body.len()) };
            let head = format!(
                "─ {} {}{} · {} chars{} ",
                r.label(),
                r.kind.filter_name(),
                if r.name.is_empty() { String::new() } else { format!(" · {}", r.name) },
                info.chars,
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
    let cell = (term_w / 6).clamp(14, 22);
    let r1: &[(&str, &str)] = &[("↑/↓", "move"), ("Space", "select"), ("Ctrl+A", "select all"), ("d", "delete"), ("t", "truncate"), ("u", "undo")];
    let r2: &[(&str, &str)] = &[("Enter", "edit"), ("v", "read full"), ("J", "JSON"), ("/", "search n/N"), ("f", "filter"), ("p", "pin part")];
    let r3: &[(&str, &str)] = &[("Ctrl+↑/↓", "top/bottom"), (":", "go to row"), ("w", "save"), ("←/Esc", "back"), ("Shift+↑/↓", "range")];
    let r4: &[(&str, &str)] = &[("o", "outline")];
    let hints: [&[(&str, &str)]; HINT_ROWS] = [r1, r2, r3, r4];
    lines.extend(layout::hint_grid(&hints, cell));

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

/// `start` opens the editor on a pinned part: its rows are selected and the cursor sits on the first one.
pub fn run(profiles: &[Profile], mark: i64, file: &Path, id: &str, title: &str, acct: &str, start: Option<&crate::pins::Pin>) -> std::io::Result<()> {
    let stats: Vec<Stat> = profiles.iter().map(|p| layout::account_stat(&p.name)).collect();
    let header = layout::account_lines(profiles, mark, &stats);
    let short = &id[..id.len().min(8)];
    let mut st = State::load(file, id, &format!("{title} · {short}"), acct)?;
    if let Some(pin) = start.filter(|p| p.is_part()) {
        let set = pin_rows(&st, pin);
        if let Some(i) = st.rows.iter().position(|r| set.contains(&r.key())) {
            st.sel = i;
            st.flash = format!("{} selected", pin.range_label());
        } else {
            st.flash = "that part is no longer in the session".into();
        }
        st.picked = set;
    }
    let mut prompt: Option<Prompt> = None;
    // Shift+arrows: range selection from where it started (`anchor`, an index
    // into the visible rows), on top of whatever was already selected (`base`).
    let mut anchor: Option<usize> = None;
    let mut base: HashSet<(usize, usize)> = HashSet::new();
    ui::hide_cursor();
    loop {
        // One row short of the terminal: a frame that fills every row scrolls the
        // screen by one line on the last write (the header would be pushed off).
        let (tw, th) = crossterm::terminal::size().map(|(w, h)| (w as usize, (h as usize).saturating_sub(1))).unwrap_or((100, 29));
        let vis = st.visible();
        if st.sel >= vis.len() {
            st.sel = vis.len().saturating_sub(1);
        }
        let used = header.len() + 3;
        let vp = th.saturating_sub(used + DET + 1 + HINT_ROWS + 1 + 2).max(3);
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
                        PromptKind::Pin(d) => {
                            let label = if p.buf.trim().is_empty() { st.title.clone() } else { p.buf.trim().to_string() };
                            crate::pins::add(crate::pins::Pin {
                                id: String::new(),
                                acct: st.acct.clone(),
                                session_id: st.id.clone(),
                                title: st.title.split(" · ").next().unwrap_or(&st.title).to_string(),
                                label,
                                from_turn: Some(d.from_turn),
                                to_turn: Some(d.to_turn),
                                from_uuid: d.from_uuid,
                                to_uuid: d.to_uuid,
                                excerpt: d.excerpt,
                                created_ms: 0,
                            });
                            st.flash = format!("pinned turns {}–{}", d.from_turn, d.to_turn);
                            st.picked.clear();
                        }
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
            // select every row of the current list (again: clear)
            KeyCode::Char('a') if key.ctrl => {
                let all: Vec<(usize, usize)> = vis.iter().map(|&ri| st.rows[ri].key()).collect();
                if !all.is_empty() && all.iter().all(|k| st.picked.contains(k)) {
                    for k in &all {
                        st.picked.remove(k);
                    }
                    st.flash = "selection cleared".into();
                } else {
                    st.picked.extend(all.iter().copied());
                    st.flash = format!("{} selected", all.len());
                }
            }
            // pin the selected rows' part of the session (the current row if none are selected)
            KeyCode::Char('p') => {
                let mut set = st.picked.clone();
                if set.is_empty() {
                    if let Some(ri) = cur_row {
                        set.insert(st.rows[ri].key());
                    }
                }
                match pin_draft(&st, &set) {
                    Some((draft, label)) => {
                        let q = format!("Pin turns {}–{} as: ", draft.from_turn, draft.to_turn);
                        prompt = Some(Prompt { q, buf: label, kind: PromptKind::Pin(draft) });
                    }
                    None => st.flash = "nothing to pin".into(),
                }
            }
            KeyCode::Char('o') => {
                open_outline(&mut st, cur_row, &header)?;
                ui::hide_cursor();
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

/// The table of contents of the (in-memory) transcript.
fn outline_of(st: &State) -> Vec<toc::Entry> {
    toc::outline(st.rows.iter().map(|r| {
        let line = &st.doc[r.line];
        let call = matches!(r.kind, Kind::Tool | Kind::Mcp | Kind::Agent).then(|| block_at(line, r.blk).and_then(|b| toc::edited_file(&b)));
        let notice = if r.kind == Kind::Notice { line.v.as_ref().and_then(notice_text) } else { None };
        toc::RowRef { turn: r.turn, line: r.line, value: line.v.as_ref(), notice, call }
    }))
}

/// For each outline entry, the labels of the pinned parts starting inside it.
fn outline_pins(st: &State, entries: &[toc::Entry], pins: &[crate::pins::Pin]) -> Vec<Vec<String>> {
    let mut marks = vec![Vec::new(); entries.len()];
    for pin in pins.iter().filter(|p| p.is_part() && p.acct == st.acct && p.session_id == st.id) {
        let set = pin_rows(st, pin);
        let first = st.rows.iter().position(|r| set.contains(&r.key()));
        if let Some(i) = first.and_then(|row| toc::entry_at(entries, row)) {
            marks[i].push(pin.label.clone());
        }
    }
    marks
}

/// Moves the cursor to row `row`, dropping the filter if it hides that row.
fn goto_row(st: &mut State, row: usize) {
    let mut vis = st.visible();
    if !vis.contains(&row) {
        st.filter.clear();
        st.flash = "filter cleared to show it".into();
        vis = st.visible();
    }
    if let Some(n) = vis.iter().position(|&ri| ri == row) {
        st.sel = n;
        st.top = n.saturating_sub(2);
    }
}

/// The outline screen (`o`): opens on the entry holding the cursor, Enter jumps.
fn open_outline(st: &mut State, cur: Option<usize>, header: &[String]) -> std::io::Result<()> {
    let entries = outline_of(st);
    if entries.is_empty() {
        st.flash = "nothing to outline".into();
        return Ok(());
    }
    let pins = outline_pins(st, &entries, &crate::pins::load());
    let tw = crossterm::terminal::size().map(|(w, _)| w as usize).unwrap_or(100);
    let mut h = header.to_vec();
    h.push(format!("{} {}", ui::color::orange(ui::glyph::back()), ui::color::bold("Outline")));
    h.push(ui::color::dim(&clip(&format!("   {} · {} entries", st.title, entries.len()), tw)));
    h.push(String::new());
    let rows: Vec<ScrollRow> = entries
        .iter()
        .zip(&pins)
        .map(|(e, pins)| {
            let mut counts = Vec::new();
            if e.tools > 0 {
                counts.push(format!("{} tool{}", e.tools, if e.tools == 1 { "" } else { "s" }));
            }
            if e.files > 0 {
                counts.push(format!("{} file{}", e.files, if e.files == 1 { "" } else { "s" }));
            }
            let mut tail = if counts.is_empty() { String::new() } else { format!("  ({})", counts.join(", ")) };
            for p in pins {
                tail.push_str(&format!("  ★ {}", clip(p, 24)));
            }
            let label = clip(&e.label, tw.saturating_sub(12 + tail.chars().count()).max(20));
            let label = match e.kind {
                toc::EntryKind::Prompt => label,
                toc::EntryKind::Command | toc::EntryKind::Shell => ui::color::cyan(&label),
                toc::EntryKind::Compaction => ui::color::yellow(&format!("── {label} ──")),
                toc::EntryKind::Notice => ui::color::dim(&label),
            };
            ScrollRow::pick(format!("{:>5}  {label}{}", e.turn, ui::color::dim(&tail)), e.row.to_string())
        })
        .collect();
    let start = cur.and_then(|row| toc::entry_at(&entries, row)).map(|i| entries[i].row.to_string());
    if let ListChoice::Picked(v) = picker::scroll_screen(&h, &rows, start.as_deref(), None)? {
        if let Ok(row) = v.parse() {
            goto_row(st, row);
        }
    }
    Ok(())
}

/// Opens one block in the in-app editor — plain text for text/result/thinking,
/// pretty JSON for tool input, or the whole block's JSON with `whole`.
fn edit_block(st: &mut State, ri: usize, whole: bool, profiles: &[Profile], mark: i64) -> std::io::Result<()> {
    let r = &st.rows[ri];
    if r.kind == Kind::Notice {
        st.flash = "notices are read-only — v reads, d deletes".into();
        return Ok(());
    }
    let Some(block) = block_at(&st.doc[r.line], r.blk) else { return Ok(()) };
    let block = &block;
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
    let probe = Row::probe(r_line, r_blk, kind);
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
    let Some(block) = block_at(&st.doc[r.line], r.blk) else { return Ok(()) };
    let block = &block;
    let body = if r.kind == Kind::Agent && r.agent_id.is_some() {
        format!("{}\n\n— subagent {} · prompt —\n{}", block_body(block), r.agent_id.as_deref().unwrap_or(""), r.agent_prompt)
    } else {
        block_body(block)
    };
    let (tw, _) = crossterm::terminal::size().map(|(w, h)| (w as usize, h as usize)).unwrap_or((100, 30));
    let (wrapped, cut, at) = reader_lines(&body, tw.saturating_sub(8).min(110), &st.query, READ_CAP / 80);
    let found = at.map(|a| format!(" · \"{}\" at line {}", st.query, a + 1)).unwrap_or_default();
    let mut h = header.to_vec();
    h.push(format!(
        "{} {}",
        ui::color::orange(ui::glyph::back()),
        ui::color::bold(&format!("{} {}{}", r.label(), r.kind.filter_name(), if r.name.is_empty() { String::new() } else { format!(" · {}", r.name) }))
    ));
    h.push(ui::color::dim(&format!("   {} · {} chars · {} lines{}{found}", r.who, r.info(&st.doc).chars, wrapped.len(), if cut { " (shown part)" } else { "" })));
    h.push(String::new());
    let rows: Vec<ScrollRow> = wrapped.into_iter().map(|l| ScrollRow::info(format!("   {}", highlight(&l, &st.query)))).collect();
    match at {
        Some(a) => picker::scroll_screen_at(&h, &rows, a)?,
        None => picker::scroll_screen(&h, &rows, None, None)?,
    };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> Doc {
        let lines = [
            r#"{"type":"last-prompt","leafUuid":"u4"}"#,
            r#"{"type":"user","uuid":"u1","parentUuid":null,"message":{"role":"user","content":"hello there"}}"#,
            r#"{"type":"assistant","uuid":"u2","parentUuid":"u1","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_A","name":"Bash","input":{"command":"ls"}}]}}"#,
            r#"{"type":"user","uuid":"u3","parentUuid":"u2","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_A","content":"a.txt"}]}}"#,
            r#"{"type":"api-request-blob","x":"quotes toolu_A somewhere"}"#,
            r#"{"type":"assistant","uuid":"u4","parentUuid":"u3","message":{"role":"assistant","content":[{"type":"text","text":"done, found a.txt"}]}}"#,
            r#"{"type":"attachment","uuid":"u5","parentUuid":"u4"}"#,
        ];
        lines.iter().map(|s| Arc::new(L::parse(s))).collect()
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
        assert!(lp.out().contains("\"leafUuid\":\"u3\""));
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
        assert_eq!(d[2].out(), d[2].raw()); // untouched line unchanged
    }

    #[test]
    #[ignore]
    fn real_session_smoke() {
        let p = std::env::var("TRANSCRIPT_FILE").expect("TRANSCRIPT_FILE");
        let tr = std::time::Instant::now();
        let text = std::fs::read_to_string(Path::new(&p)).unwrap();
        println!("  read_to_string: {:?}", tr.elapsed());
        let tp = std::time::Instant::now();
        let d = parse_doc(text.clone());
        println!("  parse_doc (8 threads): {:?} ({} lines)", tp.elapsed(), d.len());
        let ts = std::time::Instant::now();
        let mut n = 0usize;
        for line in text.lines() { if serde_json::from_str::<serde_json::Value>(line).is_ok() { n += 1; } }
        println!("  single-thread full Value parse of every line: {:?} ({n} ok)", ts.elapsed());
        drop((text, d));
        let t0 = std::time::Instant::now();
        let st = State::load(Path::new(&p), "x", "t", "a").unwrap();
        println!("State::load: {:?}", t0.elapsed());
        let t1 = std::time::Instant::now();
        let snap = st.doc.clone();
        println!("one undo snapshot (doc.clone): {:?} ({} lines)", t1.elapsed(), snap.len());
        drop(snap);
        let t2 = std::time::Instant::now();
        let _ = build_rows(&st.doc, &st.agent_dir);
        println!("build_rows: {:?}", t2.elapsed());
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
    fn search_reaches_past_the_preview_cap() {
        let long = format!("{}needle_far_down", "x ".repeat(ROW_TEXT_CAP));
        let line = serde_json::json!({"type":"user","uuid":"a","message":{"role":"user","content":long}}).to_string();
        let file = std::env::temp_dir().join(format!("cockpit-search-{}.jsonl", std::process::id()));
        std::fs::write(&file, line).unwrap();
        let mut st = State::load(&file, "id", "t", "acct").unwrap();
        let _ = std::fs::remove_file(&file);
        assert!(!st.rows[0].info(&st.doc).text.contains("needle_far_down"), "preview stays capped");
        st.query = "NEEDLE_far".into();
        assert!(st.hit(&st.rows[0]));
        st.filter = "needle_far_down".into();
        assert_eq!(st.visible(), vec![0]);
    }

    #[test]
    fn reader_finds_the_matching_line() {
        let lines = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(match_line(&lines(&["one", "Two three"]), "two"), Some(1));
        assert_eq!(match_line(&lines(&["alpha beta", "gamma"]), "beta gamma"), Some(0));
        assert_eq!(match_line(&lines(&["abcdef", "ghij"]), "efgh"), Some(0));
        assert_eq!(match_line(&lines(&["one"]), "zzz"), None);
        assert_eq!(match_line(&lines(&["one"]), ""), None);
        assert_eq!(match_line(&[], "a"), None);
    }

    #[test]
    fn reader_keeps_a_match_beyond_the_line_limit() {
        let body = format!("{}needle\n{}", "row\n".repeat(50), "tail\n".repeat(50));
        let (lines, cut, at) = reader_lines(&body, 40, "needle", 10);
        assert_eq!((lines.len(), cut, at), (60, true, Some(50)));
        let (lines, cut, at) = reader_lines(&body, 40, "", 10);
        assert_eq!((lines.len(), cut, at), (10, true, None));
    }

    #[test]
    fn outline_lists_prompts_with_counts_and_pins() {
        let lines = [
            r#"{"type":"user","uuid":"a","message":{"role":"user","content":"add a flag"}}"#,
            r#"{"type":"assistant","uuid":"b","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Edit","input":{"file_path":"a.rs"}}]}}"#,
            r#"{"type":"user","uuid":"c","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#,
            r#"{"type":"user","uuid":"d","message":{"role":"user","content":"now test it"}}"#,
        ];
        let file = std::env::temp_dir().join(format!("cockpit-outline-{}.jsonl", std::process::id()));
        std::fs::write(&file, lines.join("\n")).unwrap();
        let mut st = State::load(&file, "sid", "t", "acct").unwrap();
        let _ = std::fs::remove_file(&file);
        let entries = outline_of(&st);
        let got: Vec<_> = entries.iter().map(|e| (e.row, e.label.as_str(), e.tools, e.files)).collect();
        assert_eq!(got, vec![(0, "add a flag", 1, 1), (3, "now test it", 0, 0)]);
        let pin = crate::pins::Pin { id: String::new(), acct: "acct".into(), session_id: "sid".into(), title: "t".into(), label: "mine".into(), from_turn: Some(2), to_turn: Some(2), from_uuid: Some("b".into()), to_uuid: Some("b".into()), excerpt: String::new(), created_ms: 0 };
        assert_eq!(outline_pins(&st, &entries, &[pin]), vec![vec!["mine".to_string()], vec![]]);
        st.filter = "name:Edit".into();
        goto_row(&mut st, 3);
        assert_eq!((st.filter.as_str(), st.sel), ("", 3));
    }

    #[test]
    fn who_labels_tool_output_vs_user_answers() {
        let d: Doc = [
            r#"{"type":"user","uuid":"a","message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"assistant","uuid":"b","parentUuid":"a","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"AskUserQuestion","input":{}},{"type":"tool_use","id":"t2","name":"Bash","input":{}}]}}"#,
            r#"{"type":"user","uuid":"c","parentUuid":"b","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"User answered: yes"},{"type":"tool_result","tool_use_id":"t2","content":"ok"}]}}"#,
        ]
        .iter()
        .map(|s| Arc::new(L::parse(s)))
        .collect();
        let rows = build_rows(&d, Path::new("."));
        let whos: Vec<&str> = rows.iter().map(|r| r.who).collect();
        assert_eq!(whos, vec!["You", "Claude", "Claude", "You", "Tool"]);
    }

    #[test]
    fn system_notices_show_and_delete() {
        let mut d: Doc = [
            r#"{"type":"user","uuid":"a","message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"system","subtype":"informational","uuid":"n","parentUuid":"a","content":"safeguards stopped the response above"}"#,
            r#"{"type":"system","subtype":"model_refusal_no_fallback","uuid":"m","parentUuid":"n","content":"","apiRefusalExplanation":"cyber"}"#,
            r#"{"type":"system","subtype":"turn_duration","uuid":"t","parentUuid":"m"}"#,
        ]
        .iter()
        .map(|s| Arc::new(L::parse(s)))
        .collect();
        let rows = build_rows(&d, Path::new("."));
        assert_eq!(rows.iter().map(|r| r.kind).collect::<Vec<_>>(), vec![Kind::Text, Kind::Notice, Kind::Notice]);
        let early: Doc = [r#"{"type":"system","subtype":"informational","uuid":"z","content":"early"}"#, r#"{"type":"user","uuid":"a","message":{"role":"user","content":"hi"}}"#]
            .iter()
            .map(|s| Arc::new(L::parse(s)))
            .collect();
        let er = build_rows(&early, Path::new("."));
        assert_eq!((er[0].kind, er[0].turn, er[1].turn), (Kind::Notice, 0, 1));
        assert_eq!(rows[2].info(&d).preview, "cyber");
        let plan = plan_delete(&rows, &[rows[1].key()].into_iter().collect());
        assert_eq!(delete_blocks(&mut d, &rows, &plan.set), 1);
        assert_eq!(build_rows(&d, Path::new(".")).len(), 2);
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

    #[test]
    fn row_text_is_built_on_demand() {
        let d: Doc = [
            r#"{"type":"user","uuid":"a","message":{"role":"user","content":"  hello   world\n\nagain "}}"#,
            r#"{"type":"assistant","uuid":"b","parentUuid":"a","message":{"role":"assistant","content":[{"type":"thinking","thinking":"","signature":"S"},{"type":"tool_use","id":"t1","name":"mcp__chrome__navigate","input":{"url":"x"}},{"type":"tool_use","id":"t2","name":"Agent","input":{"subagent_type":"Explore","prompt":"find"}},{"type":"tool_use","id":"t3","name":"Bash","input":{"command":"ls"}}]}}"#,
            r#"{"type":"user","uuid":"c","parentUuid":"b","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t3","content":"a.txt"},{"type":"image","source":{}}]}}"#,
        ]
        .iter()
        .map(|s| Arc::new(L::parse(s)))
        .collect();
        let rows = build_rows(&d, Path::new("."));
        assert!(rows.iter().all(|r| r.info.get().is_none()), "building rows must not build their text");
        let p: Vec<String> = rows.iter().map(|r| r.info(&d).preview.clone()).collect();
        assert_eq!(p[0], "hello world again");
        assert_eq!(p[1], "(empty, signature only)");
        assert_eq!(p[2], "chrome · navigate {\"url\":\"x\"}");
        assert_eq!(p[3], "Explore {\"prompt\":\"find\",\"subagent_type\":\"Explore\"}");
        assert_eq!(p[4], "Bash {\"command\":\"ls\"}");
        assert_eq!(p[5], "a.txt");
        assert_eq!(p[6], "[image]");
        assert!(rows[0].lower(&d).contains("hello   world"));
        assert!(rows[4].lower(&d).starts_with("tool_use bash "));
    }

    #[test]
    fn pins_are_found_again_by_uuid_after_turns_shift() {
        let d = doc();
        let rows = build_rows(&d, Path::new("."));
        let mut st = State {
            acct: "a".into(),
            file: PathBuf::new(),
            id: "s".into(),
            title: "t".into(),
            agent_dir: PathBuf::new(),
            doc: d,
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
            turns0: 4,
            blocks0: 4,
        };
        let set: HashSet<(usize, usize)> = st.rows.iter().filter(|r| r.turn >= 2 && r.turn <= 3).map(|r| r.key()).collect();
        let (draft, label) = pin_draft(&st, &set).unwrap();
        assert_eq!((draft.from_turn, draft.to_turn), (2, 3));
        assert!(draft.excerpt.contains("[tool] Bash") && draft.excerpt.contains("[result] a.txt"), "{}", draft.excerpt);
        assert!(!label.is_empty());
        let pin = crate::pins::Pin { id: "x".into(), acct: "a".into(), session_id: "s".into(), title: "t".into(), label: "l".into(), from_turn: Some(draft.from_turn), to_turn: Some(draft.to_turn), from_uuid: draft.from_uuid.clone(), to_uuid: draft.to_uuid.clone(), excerpt: draft.excerpt.clone(), created_ms: 0 };
        assert_eq!(pin_rows(&st, &pin), set);
        // drop the first turn: everything renumbers, the pin still finds the same rows by uuid
        let gone: HashSet<(usize, usize)> = st.rows.iter().filter(|r| r.turn == 1).map(|r| r.key()).collect();
        let plan = plan_delete(&st.rows, &gone);
        delete_blocks(&mut st.doc, &st.rows, &plan.set);
        st.rows = build_rows(&st.doc, Path::new("."));
        let now = pin_rows(&st, &pin);
        assert_eq!(now.len(), set.len());
        assert!(st.rows.iter().filter(|r| now.contains(&r.key())).all(|r| r.turn >= 1 && r.turn <= 2), "renumbered");
    }
}
