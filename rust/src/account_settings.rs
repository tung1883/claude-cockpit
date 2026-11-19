//! Per-account Claude Code settings as a searchable form over
//! `<profile>/settings.json`, instead of hand-editing JSON.
//!
//! The list of settings comes from the official "All settings" reference page
//! (see `settings_docs.rs`): a cached copy refreshed in the background, or the
//! snapshot shipped in `settings_catalog.json`. Only the keys you change are
//! written; every other key — including ones the catalog does not know — is kept
//! byte for byte and in its original order. Dotted keys such as
//! `permissions.defaultMode` live inside their parent object.

use crate::input::{self, read_key};
use crate::profiles::Profile;
use crate::settings_docs::{self, CatEntry};
use crate::{layout, paths, ui};
use crossterm::event::KeyCode;
use serde::Deserialize;
use serde_json::{value::RawValue, Value};
use std::path::PathBuf;
use std::sync::OnceLock;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Bool,
    Enum,
    Int,
    Num,
    Str,
    Json,
}

#[derive(Clone)]
struct Item {
    key: String,
    kind: Kind,
    group: String,
    desc: String,
    opts: Vec<String>,
    default: Option<Value>,
    known: bool,
    /// lower-cased "key group description" for search
    hay: String,
}

const OTHER: &str = "Other (not in catalog)";

/// Display order of the topics; anything else follows alphabetically, then
/// Deprecated, then keys the catalog does not know.
const TOPICS: [&str; 15] = [
    "Model and responses",
    "Memory and context",
    "Permission settings",
    "Git and attribution",
    "Interface and terminal",
    "Remote, desktop, and notifications",
    "Hooks and automation",
    "Plugins and skills",
    "MCP",
    "Agents, sessions, and worktrees",
    "Authentication and providers",
    "Sandbox settings",
    "Privacy and telemetry",
    "Updates and versioning",
    "Tools",
];

fn group_rank(g: &str) -> usize {
    match g {
        OTHER => 99,
        "Deprecated" => 90,
        _ => TOPICS.iter().position(|t| *t == g).unwrap_or(50),
    }
}

fn to_item(d: CatEntry) -> Item {
    let kind = match d.t.as_str() {
        "bool" => Kind::Bool,
        "enum" => Kind::Enum,
        "int" => Kind::Int,
        "num" => Kind::Num,
        "str" => Kind::Str,
        _ => Kind::Json,
    };
    let hay = format!("{} {} {}", d.k, d.g, d.desc).to_lowercase();
    Item { key: d.k, kind, group: d.g, desc: d.desc, opts: d.e, default: d.d, known: true, hay }
}

/// The catalog shipped inside the binary.
fn catalog() -> &'static Vec<Item> {
    static CAT: OnceLock<Vec<Item>> = OnceLock::new();
    CAT.get_or_init(|| serde_json::from_str::<Vec<CatEntry>>(include_str!("settings_catalog.json")).unwrap_or_default().into_iter().map(to_item).collect())
}

/// Where the catalog in use came from.
enum Source {
    Builtin,
    Cache { days: u64 },
}

/// The refreshed cache when there is one, else the built-in snapshot.
fn active_catalog() -> (Vec<Item>, Source) {
    match settings_docs::load_cache() {
        Some((items, days)) => (items.into_iter().map(to_item).collect(), Source::Cache { days }),
        None => (catalog().clone(), Source::Builtin),
    }
}

// ---------- the settings.json file ----------

/// Top-level keys in file order, each value kept as its original text.
struct Top(Vec<(String, Box<RawValue>)>);

impl<'de> Deserialize<'de> for Top {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = Top;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut m: A) -> Result<Top, A::Error> {
                let mut v = Vec::new();
                while let Some(e) = m.next_entry::<String, Box<RawValue>>()? {
                    v.push(e);
                }
                Ok(Top(v))
            }
        }
        d.deserialize_map(V)
    }
}

struct SettingsFile {
    path: PathBuf,
    entries: Vec<(String, Box<RawValue>)>,
    /// The file exists but is not a JSON object — never overwrite it.
    broken: bool,
    backed_up: bool,
}

impl SettingsFile {
    fn load(path: PathBuf) -> Self {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        Self::parse(path, &text)
    }

    fn parse(path: PathBuf, text: &str) -> Self {
        if text.trim().is_empty() {
            return SettingsFile { path, entries: Vec::new(), broken: false, backed_up: false };
        }
        match serde_json::from_str::<Top>(text) {
            Ok(t) => SettingsFile { path, entries: t.0, broken: false, backed_up: false },
            Err(_) => SettingsFile { path, entries: Vec::new(), broken: true, backed_up: false },
        }
    }

    /// The value at `k` (dotted for nested keys) as JSON text. A top-level
    /// value keeps its original text; a nested one is re-serialized.
    fn raw(&self, k: &str) -> Option<String> {
        let path: Vec<&str> = k.split('.').collect();
        let top = self.entries.iter().find(|(n, _)| n == path[0])?;
        if path.len() == 1 {
            return Some(top.1.get().to_string());
        }
        self.get(k).map(|v| v.to_string())
    }

    fn get(&self, k: &str) -> Option<Value> {
        let path: Vec<&str> = k.split('.').collect();
        let top = self.entries.iter().find(|(n, _)| n == path[0])?;
        let mut v: Value = serde_json::from_str(top.1.get()).ok()?;
        for seg in &path[1..] {
            v = v.get_mut(*seg)?.take();
        }
        Some(v)
    }

    /// Sets (`Some`) or removes (`None`) the value at the dotted path, creating
    /// parent objects as needed and dropping ones left empty.
    fn edit(&mut self, k: &str, json: Option<String>) -> Result<bool, String> {
        let path: Vec<&str> = k.split('.').collect();
        edit_obj(&mut self.entries, &path, json, 0)
    }

    /// Replaces or appends `k` with `json` (must be valid JSON text).
    fn set_raw(&mut self, k: &str, json: &str) -> Result<(), String> {
        self.edit(k, Some(json.to_string())).map(|_| ())
    }

    fn set(&mut self, k: &str, v: &Value) {
        let pretty = serde_json::to_string_pretty(v).unwrap_or_else(|_| "null".into());
        // continuation lines sit one level deeper than the key
        let pad = "  ".repeat(k.split('.').count());
        let _ = self.set_raw(k, &pretty.replace('\n', &format!("\n{pad}")));
    }

    fn remove(&mut self, k: &str) -> bool {
        self.edit(k, None).unwrap_or(false)
    }

    fn render(&self) -> String {
        format!("{}
", render_obj(&self.entries, 0))
    }

    /// Writes the file (after one backup per screen visit). Atomic via rename.
    fn save(&mut self) -> Result<(), String> {
        if self.broken {
            return Err("settings.json is not valid JSON — press e to fix it first".into());
        }
        if !self.backed_up {
            if let Ok(meta) = std::fs::metadata(&self.path) {
                if meta.len() > 2 {
                    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
                    let name = format!("{}.backup-{ms}", self.path.file_name().and_then(|n| n.to_str()).unwrap_or("settings.json"));
                    let _ = std::fs::copy(&self.path, self.path.with_file_name(name));
                }
            }
            self.backed_up = true;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, self.render()).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &self.path).map_err(|e| e.to_string())
    }
}

type Obj = Vec<(String, Box<RawValue>)>;

fn parse_obj(text: &str) -> Option<Obj> {
    serde_json::from_str::<Top>(text).ok().map(|t| t.0)
}

/// `{ ... }` with keys in order, one level deeper than `depth`; untouched
/// values keep their original text.
fn render_obj(o: &Obj, depth: usize) -> String {
    if o.is_empty() {
        return "{}".to_string();
    }
    let mut s = String::from("{
");
    for (i, (k, v)) in o.iter().enumerate() {
        s.push_str(&"  ".repeat(depth + 1));
        s.push_str(&serde_json::to_string(k).unwrap_or_default());
        s.push_str(": ");
        s.push_str(v.get());
        if i + 1 < o.len() {
            s.push(',');
        }
        s.push('\n');
    }
    s.push_str(&"  ".repeat(depth));
    s.push('}');
    s
}

/// Returns whether anything changed.
fn edit_obj(o: &mut Obj, path: &[&str], new: Option<String>, depth: usize) -> Result<bool, String> {
    let (head, rest) = path.split_first().ok_or("empty key")?;
    let pos = o.iter().position(|(n, _)| n == head);
    if rest.is_empty() {
        return match new {
            Some(t) => {
                let v = RawValue::from_string(t).map_err(|e| format!("not valid JSON: {e}"))?;
                match pos {
                    Some(i) => o[i].1 = v,
                    None => o.push((head.to_string(), v)),
                }
                Ok(true)
            }
            None => Ok(pos.map(|i| o.remove(i)).is_some()),
        };
    }
    let mut child = pos.and_then(|i| parse_obj(o[i].1.get())).unwrap_or_default();
    let changed = edit_obj(&mut child, rest, new.clone(), depth + 1)?;
    if child.is_empty() && new.is_none() {
        if let Some(i) = pos {
            o.remove(i);
        }
        return Ok(changed);
    }
    let v = RawValue::from_string(render_obj(&child, depth + 1)).map_err(|e| e.to_string())?;
    match pos {
        Some(i) => o[i].1 = v,
        None => o.push((head.to_string(), v)),
    }
    Ok(changed)
}

// ---------- items, values ----------

/// Catalog items plus any top-level key in the file the catalog does not know
/// (a key that only parents dotted catalog keys, like `permissions`, is known).
fn build_items(file: &SettingsFile, cat: &[Item]) -> Vec<Item> {
    let mut items: Vec<Item> = cat.to_vec();
    for (k, _) in &file.entries {
        let prefix = format!("{k}.");
        if k == "$schema" || cat.iter().any(|i| &i.key == k || i.key.starts_with(&prefix)) {
            continue;
        }
        let kind = match file.get(k) {
            Some(Value::Bool(_)) => Kind::Bool,
            Some(Value::Number(_)) => Kind::Num,
            Some(Value::String(_)) => Kind::Str,
            _ => Kind::Json,
        };
        let desc = "Not in the settings catalog (a newer setting, a typo, or a custom key). Kept as is.".to_string();
        items.push(Item { hay: format!("{k} other {desc}").to_lowercase(), key: k.clone(), kind, group: OTHER.to_string(), desc, opts: Vec::new(), default: None, known: false });
    }
    items.sort_by(|a, b| group_rank(&a.group).cmp(&group_rank(&b.group)).then_with(|| a.group.cmp(&b.group)).then_with(|| a.key.cmp(&b.key)));
    items
}

/// Items matching every word of `q` (key, group or description).
fn matching(items: &[Item], q: &str) -> Vec<usize> {
    let words: Vec<String> = q.split_whitespace().map(|w| w.to_lowercase()).collect();
    let mut hits: Vec<usize> = items.iter().enumerate().filter(|(_, it)| words.iter().all(|w| it.hay.contains(w.as_str()))).map(|(i, _)| i).collect();
    if !words.is_empty() {
        // a name that starts with / contains the words beats a description match
        let score = |it: &Item| {
            let key = it.key.to_lowercase();
            if words.iter().all(|w| key.starts_with(w.as_str())) {
                0
            } else if words.iter().all(|w| key.contains(w.as_str())) {
                1
            } else {
                2
            }
        };
        hits.sort_by_key(|&i| score(&items[i]));
    }
    hits
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

fn compact(raw: &str) -> String {
    raw.lines().map(str::trim).collect::<Vec<_>>().join("")
}

fn plain(v: &Value) -> String {
    match v {
        Value::Bool(b) => if *b { "on" } else { "off" }.to_string(),
        Value::String(s) => s.clone(),
        Value::Array(a) => format!("{} item{}", a.len(), if a.len() == 1 { "" } else { "s" }),
        Value::Object(o) => format!("{} key{}", o.len(), if o.len() == 1 { "" } else { "s" }),
        Value::Null => "null".into(),
        other => other.to_string(),
    }
}

/// Colored value cell: the set value, or the default in dim when unset.
fn value_cell(it: &Item, file: &SettingsFile, width: usize) -> String {
    let paint = |kind: Kind, text: &str, v: Option<&Value>| match (kind, v) {
        (Kind::Bool, Some(Value::Bool(true))) => ui::color::green(text),
        (Kind::Bool, _) => ui::color::dim(text),
        (Kind::Enum, _) => ui::color::cyan(text),
        _ => ui::color::yellow(text),
    };
    match file.get(&it.key) {
        Some(v) => {
            let shown = clip(&plain(&v), width.saturating_sub(2));
            let tail = if matches!(v, Value::Array(_) | Value::Object(_)) { "  →" } else { "" };
            format!("{} {}{tail}", paint(it.kind, &shown, Some(&v)), ui::color::orange("●"))
        }
        None => match &it.default {
            Some(d) => ui::color::dim(&format!("{} · default", clip(&plain(d), width.saturating_sub(12)))),
            None if it.known => ui::color::dim("not set"),
            None => String::new(),
        },
    }
}

fn type_label(it: &Item) -> String {
    match it.kind {
        Kind::Bool => "on / off".into(),
        Kind::Enum => it.opts.join(" · "),
        Kind::Int => "whole number".into(),
        Kind::Num => "number".into(),
        Kind::Str => "text".into(),
        Kind::Json => "JSON (list or object)".into(),
    }
}

/// Next/previous option for an enum, starting from the current value, else the default.
fn cycle(it: &Item, current: Option<&Value>, dir: i32) -> Option<String> {
    if it.opts.is_empty() {
        return None;
    }
    let cur = current.or(it.default.as_ref()).and_then(|v| v.as_str()).and_then(|s| it.opts.iter().position(|o| o == s));
    let n = it.opts.len() as i32;
    let next = match cur {
        Some(i) => (i as i32 + dir).rem_euclid(n),
        None => if dir >= 0 { 0 } else { n - 1 },
    };
    Some(it.opts[next as usize].clone())
}

/// Turns typed text into a JSON value for `it`'s type (or an error message).
fn parse_input(it: &Item, text: &str) -> Result<String, String> {
    let t = text.trim();
    match it.kind {
        Kind::Str => Ok(serde_json::to_string(t).unwrap_or_default()),
        Kind::Int => t.parse::<i64>().map(|n| n.to_string()).map_err(|_| "needs a whole number".to_string()),
        Kind::Num => t.parse::<f64>().ok().filter(|n| n.is_finite()).map(|n| n.to_string()).ok_or_else(|| "needs a number".to_string()),
        Kind::Json => serde_json::from_str::<Value>(t).map(|_| t.to_string()).map_err(|e| format!("not valid JSON: {e}")),
        Kind::Bool | Kind::Enum => Err("use Space / ←→".to_string()),
    }
}

// ---------- screen ----------

struct Ui {
    sel: usize,
    top: usize,
    q: String,
    searching: bool,
    /// Editing `items[idx]` with this text.
    edit: Option<(usize, String)>,
    flash: String,
    /// Download the settings list on the next turn of the loop.
    refresh: bool,
}

const STRIP: usize = 4;

fn draw(items: &[Item], vis: &[usize], file: &SettingsFile, u: &mut Ui, header: &[String], acct: &str, source: &str, tw: usize, th: usize) -> (Vec<String>, Option<(usize, usize)>) {
    let mut lines: Vec<String> = header.to_vec();
    let mut cursor = None;
    let set_count = items.iter().filter(|i| file.raw(&i.key).is_some()).count();
    let flash = if u.flash.is_empty() { String::new() } else { format!("  {}", ui::color::yellow(&u.flash)) };
    lines.push(format!("{} {}{flash}", ui::color::orange(ui::glyph::back()), ui::color::bold(&format!("{acct} › settings"))));
    if u.searching {
        let head = "   search: ";
        lines.push(format!("{}{}{}", ui::color::dim(head), ui::color::yellow(&u.q), ""));
        cursor = Some((lines.len(), head.chars().count() + u.q.chars().count() + 1));
    } else if !u.q.is_empty() {
        lines.push(ui::color::dim(&format!("   {} of {} match “{}”  ·  / to change, Esc to clear", vis.len(), items.len(), u.q)));
    } else {
        lines.push(ui::color::dim(&clip(&format!("   {} settings · {} set in this account · / to search · list: {source}", items.len(), set_count), tw)));
    }
    lines.push(String::new());

    // body, with a heading before each group
    let mut body: Vec<String> = Vec::new();
    let mut headings: Vec<usize> = Vec::new();
    let mut sel_line = 0usize;
    let mut group = "";
    let key_w = 32usize.min(tw.saturating_sub(24));
    for (n, &i) in vis.iter().enumerate() {
        let it = &items[i];
        if u.q.is_empty() && it.group != group {
            group = &it.group;
            headings.push(body.len());
            body.push(ui::color::dim(&format!(" {group}")));
        }
        let on = n == u.sel;
        if on {
            sel_line = body.len();
        }
        let ptr = if on { ui::color::orange(ui::glyph::pointer()) } else { " ".to_string() };
        let key = layout::pad_to(&clip(&it.key, key_w - 1), key_w);
        let key = if on { ui::color::orange_bold(&key) } else { key };
        let tag = if u.q.is_empty() { String::new() } else { ui::color::dim(&format!("  {}", it.group)) };
        body.push(format!(" {ptr} {key}{}{tag}", value_cell(it, file, tw.saturating_sub(key_w + 6))));
    }
    if vis.is_empty() {
        body.push(ui::color::dim("   (nothing matches — try fewer words)"));
    }

    let foot = 1 + STRIP + 1 + 2; // rule, strip, rule, hints
    let vp = th.saturating_sub(lines.len() + foot).max(3);
    if sel_line < u.top {
        u.top = sel_line;
    }
    if sel_line >= u.top + vp {
        u.top = sel_line + 1 - vp;
    }
    // keep the group heading in view when the selection is its first row
    if u.top > 0 && u.top == sel_line && headings.contains(&(sel_line - 1)) {
        u.top -= 1;
    }
    let end = (u.top + vp).min(body.len());
    let shown = &body[u.top.min(body.len())..end];
    lines.extend(shown.iter().cloned());
    let more = body.len().saturating_sub(end);
    let mut used = shown.len();
    if more > 0 {
        lines.push(ui::color::dim(&format!("    ↓ {more} more rows")));
        used += 1;
    }
    for _ in used..vp {
        lines.push(String::new());
    }

    // detail strip
    lines.push(ui::rule(None));
    match vis.get(u.sel).map(|&i| &items[i]) {
        Some(it) => {
            let default = match &it.default {
                Some(d) => format!("  ·  default {}", plain(d)),
                None => String::new(),
            };
            lines.push(format!(" {}  {}", ui::color::bold(&it.key), ui::color::dim(&format!("{}{default}", type_label(it)))));
            let wrapped = crate::pins::wrap(&it.desc, tw.saturating_sub(4));
            for k in 0..2 {
                lines.push(match wrapped.get(k) {
                    Some(w) if k == 1 && wrapped.len() > 2 => ui::color::dim(&format!("  {}…", clip(w, tw.saturating_sub(5)))),
                    Some(w) => ui::color::dim(&format!("  {w}")),
                    None => String::new(),
                });
            }
            lines.push(ui::color::dim(&if file.raw(&it.key).is_some() { "  set in this account's settings.json  ·  r removes it".to_string() } else { "  not set — Claude Code uses its own default".to_string() }));
        }
        None => {
            for _ in 0..STRIP {
                lines.push(String::new());
            }
        }
    }
    lines.push(ui::rule(None));
    match &u.edit {
        Some((i, text)) => {
            let head = format!(" {} = ", items[*i].key);
            lines.push(format!("{}{}", ui::color::orange(&head), text));
            cursor = Some((lines.len(), head.chars().count() + text.chars().count() + 1));
            lines.push(ui::color::dim(" Enter save  ·  Esc cancel  ·  empty removes the setting"));
        }
        None => {
            let cell = (tw / 5).clamp(14, 22);
            lines.extend(layout::hint_grid(
                &[&[("↑/↓", "move"), ("Space", "toggle"), ("←/→", "cycle"), ("Enter", "edit"), ("Esc", "back")], &[("/", "search"), ("r", "remove"), ("e", "raw JSON"), ("u", "update list")]],
                cell,
            ));
        }
    }
    (lines, cursor)
}

pub fn run(profiles: &[Profile], profile_idx: usize) -> std::io::Result<()> {
    let Some(profile) = profiles.get(profile_idx) else { return Ok(()) };
    let acct = profile.name.clone();
    let path = paths::profile_dir(&acct).join("settings.json");
    let mut file = SettingsFile::load(path.clone());
    let (mut cat, mut source) = active_catalog();
    let mut items = build_items(&file, &cat);
    let header = vec![ui::banner()];
    let mut u = Ui { sel: 0, top: 0, q: String::new(), searching: false, edit: None, flash: String::new(), refresh: false };
    // pick up newer Claude Code settings for next time, without waiting for it
    settings_docs::refresh_in_background_if_stale();
    ui::hide_cursor();
    loop {
        let vis = matching(&items, &u.q);
        if u.sel >= vis.len() {
            u.sel = vis.len().saturating_sub(1);
        }
        let (tw, th) = crossterm::terminal::size().map(|(w, h)| (w as usize, (h as usize).saturating_sub(1))).unwrap_or((100, 29));
        // the rule is as wide as the terminal was when it was built
        let header = [header[0].clone(), ui::rule(Some(tw))];
        let source_note = match &source {
            Source::Builtin => "built-in (u to update)".to_string(),
            Source::Cache { days: 0 } => "updated today".to_string(),
            Source::Cache { days } if *days >= 14 => format!("{days}d old (u to update)"),
            Source::Cache { days } => format!("{days}d old"),
        };
        let (lines, cursor) = draw(&items, &vis, &file, &mut u, &header, &acct, &source_note, tw, th);
        match cursor {
            Some((row, col)) => ui::repaint_at(&lines, row, col),
            None => {
                ui::hide_cursor();
                ui::repaint(&lines);
            }
        }
        if u.refresh {
            u.refresh = false;
            u.flash = match settings_docs::refresh() {
                Ok(n) => {
                    (cat, source) = active_catalog();
                    items = build_items(&file, &cat);
                    format!("list updated · {n} settings")
                }
                Err(e) => e,
            };
            continue;
        }
        let raw = read_key()?;
        if raw.ctrl && raw.code == KeyCode::Char('c') {
            ui::show_cursor();
            return Ok(());
        }
        u.flash.clear();

        // ---- value prompt
        if let Some((i, mut text)) = u.edit.take() {
            match raw.code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    let it = &items[i];
                    if text.trim().is_empty() {
                        if file.remove(&it.key) {
                            u.flash = save_flash(&mut file, "removed");
                        }
                    } else {
                        match parse_input(it, &text) {
                            Ok(json) => match file.set_raw(&it.key, &json) {
                                Ok(()) => u.flash = save_flash(&mut file, "saved"),
                                Err(e) => u.flash = e,
                            },
                            Err(e) => {
                                u.flash = e;
                                u.edit = Some((i, text));
                            }
                        }
                    }
                }
                KeyCode::Backspace => {
                    text.pop();
                    u.edit = Some((i, text));
                }
                KeyCode::Char(c) if !raw.ctrl => {
                    text.push(c);
                    u.edit = Some((i, text));
                }
                _ => u.edit = Some((i, text)),
            }
            continue;
        }

        // ---- search box
        if u.searching {
            match raw.code {
                KeyCode::Esc => {
                    u.searching = false;
                    u.q.clear();
                    u.sel = 0;
                }
                KeyCode::Enter => u.searching = false,
                KeyCode::Backspace => {
                    u.q.pop();
                    u.sel = 0;
                }
                KeyCode::Down => u.sel = (u.sel + 1).min(vis.len().saturating_sub(1)),
                KeyCode::Up => u.sel = u.sel.saturating_sub(1),
                KeyCode::Char(c) if !raw.ctrl => {
                    u.q.push(c);
                    u.sel = 0;
                }
                _ => {}
            }
            continue;
        }

        let key = input::gate_vim(raw, &['r', 'e', 'u']);
        if input::vim_jump(&key, &mut u.sel, vis.len(), 8, true) {
            continue;
        }
        let cur = vis.get(u.sel).copied();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                if !u.q.is_empty() {
                    u.q.clear();
                    u.sel = 0;
                } else {
                    ui::show_cursor();
                    return Ok(());
                }
            }
            KeyCode::Up | KeyCode::Char('k') => u.sel = u.sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => u.sel = (u.sel + 1).min(vis.len().saturating_sub(1)),
            KeyCode::PageUp => u.sel = u.sel.saturating_sub(8),
            KeyCode::PageDown => u.sel = (u.sel + 8).min(vis.len().saturating_sub(1)),
            KeyCode::Home => u.sel = 0,
            KeyCode::End => u.sel = vis.len().saturating_sub(1),
            KeyCode::Char('/') => u.searching = true,
            KeyCode::Char('u') => {
                u.flash = "updating the settings list…".into();
                u.refresh = true;
            }
            KeyCode::Char(' ' | 'l' | 'h') | KeyCode::Right | KeyCode::Left | KeyCode::Enter => {
                let Some(i) = cur else { continue };
                let it = items[i].clone();
                let dir = if matches!(key.code, KeyCode::Left | KeyCode::Char('h')) { -1 } else { 1 };
                match it.kind {
                    Kind::Bool => {
                        let now = file.get(&it.key).and_then(|v| v.as_bool()).or_else(|| it.default.as_ref().and_then(|d| d.as_bool())).unwrap_or(false);
                        file.set(&it.key, &Value::Bool(!now));
                        u.flash = save_flash(&mut file, "saved");
                    }
                    Kind::Enum => {
                        let cur_v = file.get(&it.key);
                        if let Some(next) = cycle(&it, cur_v.as_ref(), dir) {
                            file.set(&it.key, &Value::String(next));
                            u.flash = save_flash(&mut file, "saved");
                        }
                    }
                    _ if key.code == KeyCode::Enter => {
                        let text = match file.raw(&it.key) {
                            Some(r) => match serde_json::from_str::<Value>(&r) {
                                Ok(Value::String(s)) if it.kind == Kind::Str => s,
                                _ => compact(&r),
                            },
                            None => String::new(),
                        };
                        u.edit = Some((i, text));
                    }
                    _ => {}
                }
            }
            KeyCode::Char('r') | KeyCode::Delete => {
                if let Some(i) = cur {
                    if file.remove(&items[i].key) {
                        u.flash = save_flash(&mut file, "removed");
                    }
                }
            }
            KeyCode::Char('e') => {
                let title = format!("{acct} › settings.json");
                let _ = crate::editor::edit_file(&path, profiles, profile_idx as i64, Some(&title));
                ui::hide_cursor();
                let backed_up = file.backed_up;
                file = SettingsFile::load(path.clone());
                file.backed_up = backed_up;
                items = build_items(&file, &cat);
                if file.broken {
                    u.flash = "settings.json is not valid JSON".into();
                }
            }
            _ => {}
        }
        // an unknown key that was removed leaves the list
        items = items.into_iter().filter(|it| it.known || file.raw(&it.key).is_some()).collect();
    }
}

fn save_flash(file: &mut SettingsFile, ok: &str) -> String {
    match file.save() {
        Ok(()) => ok.to_string(),
        Err(e) => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(text: &str) -> SettingsFile {
        SettingsFile::parse(PathBuf::from("settings.json"), text)
    }

    #[test]
    fn catalog_loads_and_has_known_keys() {
        let c = catalog();
        assert!(c.len() > 80);
        assert!(c.iter().any(|i| i.key == "autoCompactEnabled" && i.kind == Kind::Bool));
        assert!(c.iter().any(|i| i.key == "effortLevel" && i.kind == Kind::Enum && i.opts.contains(&"high".to_string())));
        assert!(!c.iter().any(|i| i.key == "$schema"));
        assert!(c.iter().any(|i| i.key == "idleCompaction" && i.kind == Kind::Bool && i.default == Some(Value::Bool(true))));
    }

    #[test]
    fn editing_keeps_order_and_untouched_text() {
        let mut f = file("{\n  \"zeta\": {\"b\": 1, \"a\": 2},\n  \"alpha\": true\n}\n");
        f.set("alpha", &Value::Bool(false));
        f.set("model", &Value::String("opus".into()));
        let out = f.render();
        assert!(out.starts_with("{\n  \"zeta\": {\"b\": 1, \"a\": 2},\n  \"alpha\": false,\n  \"model\": \"opus\"\n}"));
        assert!(serde_json::from_str::<Value>(&out).is_ok());
    }

    #[test]
    fn remove_and_empty_file() {
        let mut f = file("{\"a\":1,\"b\":2}");
        assert!(f.remove("a") && !f.remove("a"));
        assert_eq!(f.render(), "{\n  \"b\": 2\n}\n");
        f.remove("b");
        assert_eq!(f.render(), "{}\n");
        assert!(!file("").broken && !file("  ").broken);
    }

    #[test]
    fn nested_keys_live_inside_their_parent_in_order() {
        let mut f = file("{
  \"permissions\": {
    \"deny\": [\"x\"],
    \"allow\": []
  },
  \"model\": \"opus\"
}
");
        f.set("permissions.defaultMode", &Value::String("plan".into()));
        assert_eq!(f.get("permissions.defaultMode"), Some(Value::String("plan".into())));
        assert_eq!(f.get("permissions.deny"), Some(serde_json::json!(["x"])));
        let out = f.render();
        assert!(serde_json::from_str::<Value>(&out).is_ok());
        // untouched siblings keep their order; the new key is appended
        assert!(out.find("\"deny\"").unwrap() < out.find("\"allow\"").unwrap());
        assert!(out.find("\"allow\"").unwrap() < out.find("\"defaultMode\"").unwrap());
        assert!(out.find("\"permissions\"").unwrap() < out.find("\"model\"").unwrap());
    }

    #[test]
    fn nested_set_creates_parents_and_remove_prunes_empty_ones() {
        let mut f = file("{}");
        f.set("sandbox.network.allowLocalBinding", &Value::Bool(true));
        assert_eq!(f.get("sandbox.network.allowLocalBinding"), Some(Value::Bool(true)));
        assert!(serde_json::from_str::<Value>(&f.render()).is_ok());
        assert!(f.remove("sandbox.network.allowLocalBinding"));
        assert_eq!(f.render(), "{}
");
        assert!(!f.remove("sandbox.network.allowLocalBinding"));
    }

    #[test]
    fn a_parent_of_dotted_catalog_keys_is_not_an_unknown_key() {
        let f = file("{\"permissions\": {\"allow\": []}, \"mystery\": 1}");
        let items = build_items(&f, catalog());
        assert!(!items.iter().any(|i| i.key == "permissions" && !i.known));
        assert!(items.iter().any(|i| i.key == "mystery" && !i.known));
    }

    #[test]
    fn broken_file_is_never_overwritten() {
        let mut f = file("{ not json");
        assert!(f.broken);
        assert!(f.save().is_err());
    }

    #[test]
    fn nested_values_are_indented_under_their_key() {
        let mut f = file("{}");
        f.set("permissions", &serde_json::json!({"allow": ["Bash(ls)"]}));
        let out = f.render();
        assert!(serde_json::from_str::<Value>(&out).is_ok());
        assert!(out.contains("\n    \"allow\""));
    }

    #[test]
    fn search_matches_every_word_in_key_group_or_description() {
        let f = file("{\"mystery\": 1}");
        let items = build_items(&f, catalog());
        let hit = matching(&items, "compact auto");
        assert!(hit.iter().any(|&i| items[i].key == "autoCompactEnabled"));
        assert!(matching(&items, "zzzz-nothing").is_empty());
        assert!(items.iter().any(|i| i.key == "mystery" && !i.known && i.kind == Kind::Num));
    }

    #[test]
    fn enum_cycles_from_value_then_default_and_wraps() {
        let it = catalog().iter().find(|i| i.key == "viewMode").unwrap().clone();
        assert_eq!(cycle(&it, Some(&Value::String("focus".into())), 1).as_deref(), Some("default"));
        assert_eq!(cycle(&it, Some(&Value::String("default".into())), -1).as_deref(), Some("focus"));
        assert_eq!(cycle(&it, None, 1).as_deref(), Some("default"));
    }

    #[test]
    fn typed_input_is_validated_per_type() {
        let get = |k: &str| catalog().iter().find(|i| i.key == k).unwrap().clone();
        assert_eq!(parse_input(&get("cleanupPeriodDays"), " 14 ").unwrap(), "14");
        assert!(parse_input(&get("cleanupPeriodDays"), "x").is_err());
        assert_eq!(parse_input(&get("language"), "french").unwrap(), "\"french\"");
        assert!(parse_input(&get("permissions"), "{\"allow\": [").is_err());
        assert!(parse_input(&get("permissions"), "{\"allow\":[\"Bash(ls)\"]}").is_ok());
    }
}
