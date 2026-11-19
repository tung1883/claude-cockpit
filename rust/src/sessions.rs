//! Session browser.
//!
//! A session is filed by Claude Code under the folder it *started* in, but its
//! lines carry the folder of every moment (`cwd`), so a session you started in
//! `work` and developed in `work/stegosaur` belongs to both. This module keeps
//! an index of what each session touched (plus the text of its prompts and
//! replies, for search) and offers one screen over it:
//!
//!   home    This folder · Recent elsewhere · Folders
//!   folder  one folder's sessions, `/` to filter
//!   search  Ctrl+F: every account, prompts + replies (not tool output)
//!
//! Every list shows a detail strip for the highlighted row. Enter on a session
//! opens the usual session action menu.

use crate::input::{self, read_key};
use crate::layout::{self, Stat};
use crate::profiles::Profile;
use crate::session::{self, SessionFile, SessionGroup};
use crate::ui;
use crossterm::event::KeyCode;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Detail strip: title line + five facts.
const DET: usize = 6;
/// A folder counts as "touched" when at least this share of a session's
/// cwd-tagged lines were there (a stray `cd` for one command is noise).
const MIN_SHARE: f32 = 0.10;
/// Text kept per session for search.
const TEXT_CAP: usize = 300_000;

// ------------------------------------------------------------------ paths

/// One spelling per folder: forward slashes, no trailing slash, `C:/…` (not
/// git-bash's `/c/…`), upper-case drive letter.
pub fn norm_path(p: &str) -> String {
    let mut s = p.replace('\\', "/");
    while s.len() > 1 && s.ends_with('/') {
        s.pop();
    }
    let b = s.as_bytes();
    if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b'/' {
        s = format!("{}:/{}", (b[1] as char).to_ascii_uppercase(), &s[3..]);
    } else if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_lowercase() {
        s = format!("{}{}", (b[0] as char).to_ascii_uppercase(), &s[1..]);
    }
    s
}

fn same_path(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// `p` is `folder` or lies somewhere below it.
fn under(p: &str, folder: &str) -> bool {
    let (p, f) = (p.as_bytes(), folder.as_bytes());
    p.len() >= f.len() && p[..f.len()].eq_ignore_ascii_case(f) && (p.len() == f.len() || p[f.len()] == b'/' || f.last() == Some(&b'/'))
}

/// `…/work/x` for paths under the usual work folder, `~/…` under home.
fn short_path(p: &str) -> String {
    if let Some(home) = dirs::home_dir().map(|h| norm_path(&h.to_string_lossy())) {
        let work = format!("{home}/Documents/work");
        if p.len() >= work.len() && p[..work.len()].eq_ignore_ascii_case(&work) {
            return format!("…/work{}", &p[work.len()..]);
        }
        if p.len() >= home.len() && p[..home.len()].eq_ignore_ascii_case(&home) {
            return format!("~{}", &p[home.len()..]);
        }
    }
    p.to_string()
}

// ------------------------------------------------------------------ index

#[derive(Serialize, Deserialize, Clone)]
struct Meta {
    mtime_ms: u64,
    len: u64,
    /// First folder the session worked in.
    started: String,
    /// Folders with a meaningful share of its activity, in order of first use.
    folders: Vec<(String, f32)>,
    title: String,
    first: String,
    last: String,
    msgs: u32,
    // ---- state to continue a scan instead of redoing it (transcripts only grow)
    /// Bytes of complete lines already read.
    #[serde(default)]
    scanned_to: u64,
    /// Fingerprint of the bytes just before `scanned_to`; if the file was rewritten
    /// (edited, truncated) it no longer matches and the file is read from the start.
    #[serde(default)]
    tail_hash: u64,
    /// Every folder seen with its line count (normalized, first-use order).
    #[serde(default)]
    cwds: Vec<(String, u32)>,
    #[serde(default)]
    ai_title: String,
    #[serde(default)]
    custom_title: String,
}

struct Scanned {
    meta: Meta,
    text: String,
}

fn ms_of(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn flat(s: &str, n: usize) -> String {
    let mut out = String::new();
    let mut sp = true;
    for c in s.chars() {
        if c.is_whitespace() {
            if !sp {
                out.push(' ');
                sp = true;
            }
        } else {
            out.push(c);
            sp = false;
        }
        if out.chars().count() >= n {
            break;
        }
    }
    out.trim_end().to_string()
}

/// The raw (still JSON-escaped) string value after `"key":"` on a line.
fn raw_str_after<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let at = line.find(key)? + key.len();
    let rest = &line[at..];
    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Some(&rest[..i]),
            _ => i += 1,
        }
    }
    None
}

fn unescape(raw: &str) -> String {
    if !raw.contains('\\') {
        return raw.to_string();
    }
    serde_json::from_str::<String>(&format!("\"{raw}\"")).unwrap_or_else(|_| raw.to_string())
}

fn has(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// FNV-1a of the (up to) 4 KB of the file that ends at `end`.
fn tail_fingerprint(f: &mut std::fs::File, end: u64) -> u64 {
    use std::io::{Read, Seek, SeekFrom};
    let start = end.saturating_sub(4096);
    let mut buf = vec![0u8; (end - start) as usize];
    if f.seek(SeekFrom::Start(start)).is_err() || f.read_exact(&mut buf).is_err() {
        return 0;
    }
    buf.iter().fold(0xcbf29ce484222325u64, |h, b| (h ^ *b as u64).wrapping_mul(0x100000001b3))
}

/// Lines longer than this are almost always tool results or attachments; they
/// are looked at only by their first few hundred bytes.
const BIG_LINE: usize = 50_000;

/// One pass over a transcript: which folders it worked in, its titles, first and
/// last prompt, and the text of every prompt and reply (no tool input/output,
/// no thinking).
///
/// With `prev` (the stored meta and text of an earlier pass) it continues from
/// where that pass stopped — only the lines appended since are read. A live
/// session grows by a few KB at a time, so this turns "re-read 230 MB" into a few
/// milliseconds. If the file no longer starts with what was read before (it was
/// edited or truncated) it is read from the beginning.
fn scan_file(path: &Path, prev: Option<(&Meta, &str)>) -> Option<Scanned> {
    use std::io::{BufRead, Seek, SeekFrom};
    let md = std::fs::metadata(path).ok()?;
    let mut f = std::fs::File::open(path).ok()?;
    let len = md.len();
    let resume = prev.filter(|(m, _)| m.scanned_to > 0 && m.scanned_to <= len && tail_fingerprint(&mut f, m.scanned_to) == m.tail_hash);

    let mut cwd_count: HashMap<String, u32> = HashMap::new();
    let mut cwd_order: Vec<String> = Vec::new();
    let (mut ai, mut custom) = (String::new(), String::new());
    let (mut first, mut last) = (String::new(), String::new());
    let mut text = String::new();
    let mut msgs = 0u32;
    let mut offset = 0u64;
    if let Some((m, t)) = resume {
        for (p, n) in &m.cwds {
            cwd_count.insert(p.clone(), *n);
            cwd_order.push(p.clone());
        }
        ai = m.ai_title.clone();
        custom = m.custom_title.clone();
        first = m.first.clone();
        last = m.last.clone();
        text = t.to_string();
        msgs = m.msgs;
        offset = m.scanned_to;
        f.seek(SeekFrom::Start(offset)).ok()?;
    } else {
        f.seek(SeekFrom::Start(0)).ok()?;
    }
    let mut reader = std::io::BufReader::with_capacity(1 << 20, f);
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if buf.last() != Some(&b'\n') {
            break; // a line still being written; the next pass picks it up
        }
        offset += buf.len() as u64;
        if buf.len() > BIG_LINE {
            let head = &buf[..buf.len().min(1500)];
            let is_msg = has(head, b"\"type\":\"user\"") || has(head, b"\"type\":\"assistant\"");
            if !is_msg || has(head, b"\"tool_result\"") {
                continue;
            }
        }
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim_end();
        if buf.len() <= BIG_LINE {
            if let Some(raw) = raw_str_after(line, "\"cwd\":\"") {
                let c = cwd_count.entry(norm_path(&unescape(raw))).or_insert(0);
                if *c == 0 {
                    cwd_order.push(norm_path(&unescape(raw)));
                }
                *c += 1;
            }
            if line.contains("\"aiTitle\"") {
                if let Some(r) = raw_str_after(line, "\"aiTitle\":\"") {
                    ai = unescape(r);
                }
            }
            if line.contains("\"customTitle\"") {
                if let Some(r) = raw_str_after(line, "\"customTitle\":\"") {
                    custom = unescape(r);
                }
            }
        }
        if !(line.contains("\"type\":\"user\"") || line.contains("\"type\":\"assistant\"")) {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if v.get("isSidechain").and_then(|x| x.as_bool()) == Some(true) || v.get("isMeta").and_then(|x| x.as_bool()) == Some(true) {
            continue;
        }
        let Some(content) = v.get("message").and_then(|m| m.get("content")) else { continue };
        let parts: Vec<&str> = match content {
            serde_json::Value::String(s) => vec![s.as_str()],
            serde_json::Value::Array(a) => a.iter().filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text")).filter_map(|b| b.get("text").and_then(|t| t.as_str())).collect(),
            _ => Vec::new(),
        };
        let is_user = v.get("type").and_then(|t| t.as_str()) == Some("user");
        for p in parts {
            if p.trim().is_empty() || p.trim_start().starts_with("<system-reminder>") {
                continue;
            }
            msgs += 1;
            if is_user {
                if first.is_empty() {
                    first = flat(p, 160);
                }
                last = flat(p, 160);
            }
            if text.len() < TEXT_CAP {
                text.push_str(p);
                text.push('\n');
            }
        }
    }
    let mut f = reader.into_inner();
    let tail_hash = tail_fingerprint(&mut f, offset);

    // Same folder spelled two ways is one folder (already merged by `norm_path`
    // + case); drop the stray ones.
    let cwds: Vec<(String, u32)> = {
        let mut merged: Vec<(String, u32)> = Vec::new();
        for p in &cwd_order {
            let n = cwd_count[p];
            match merged.iter_mut().find(|(q, _)| same_path(q, p)) {
                Some(e) => e.1 += n,
                None => merged.push((p.clone(), n)),
            }
        }
        merged
    };
    let total: u32 = cwds.iter().map(|(_, n)| *n).sum::<u32>().max(1);
    let mut folders: Vec<(String, f32)> = cwds.iter().map(|(p, n)| (p.clone(), *n as f32 / total as f32)).filter(|(_, s)| *s >= MIN_SHARE).collect();
    if folders.is_empty() {
        if let Some((p, _)) = cwds.first() {
            folders.push((p.clone(), 1.0));
        }
    }
    let started = cwds.first().map(|(p, _)| p.clone()).unwrap_or_default();
    let id8: String = path.file_stem().map(|s| s.to_string_lossy().chars().take(8).collect()).unwrap_or_default();
    let title = if !custom.is_empty() {
        custom.clone()
    } else if !ai.is_empty() {
        ai.clone()
    } else if !first.is_empty() {
        flat(&first, 60)
    } else {
        id8
    };
    Some(Scanned {
        meta: Meta {
            mtime_ms: ms_of(md.modified().unwrap_or(UNIX_EPOCH)),
            len,
            started,
            folders,
            title,
            first,
            last,
            msgs,
            scanned_to: offset,
            tail_hash,
            cwds,
            ai_title: ai,
            custom_title: custom,
        },
        text,
    })
}

#[derive(Default)]
struct Idx {
    meta: HashMap<String, Meta>,
    /// path -> (mtime_ms, len, text)
    text: HashMap<String, (u64, u64, String)>,
    /// lowercase copy of `text`, for case-insensitive search
    lower: HashMap<String, String>,
    text_ready: bool,
    done: usize,
    total: usize,
    running: bool,
}

fn idx() -> &'static Mutex<Idx> {
    static I: OnceLock<Mutex<Idx>> = OnceLock::new();
    I.get_or_init(|| {
        let meta = std::fs::read_to_string(index_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        Mutex::new(Idx { meta, ..Default::default() })
    })
}

fn index_path() -> PathBuf {
    crate::paths::root().join("session-index.json")
}

fn text_path() -> PathBuf {
    crate::paths::root().join("session-text.json")
}

fn write_atomic(path: &Path, json: &str) {
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// (indexed so far, total to index, still running) — for the "indexing…" note.
pub fn indexing() -> Option<(usize, usize)> {
    let g = idx().lock().ok()?;
    g.running.then_some((g.done, g.total))
}

/// Brings the index up to date in the background: reads only the sessions that
/// are new or changed since last time (by size and modified time), a few at a
/// time, and saves the result. The first run reads every transcript once.
pub fn start_background_index(profile_names: Vec<String>) {
    {
        let Ok(mut g) = idx().lock() else { return };
        if g.running {
            return;
        }
        g.running = true;
        g.done = 0;
        g.total = 0;
    }
    std::thread::spawn(move || index_worker(profile_names));
}

/// The body of the background build; also run directly by the benchmarks.
fn index_worker(profile_names: Vec<String>) {
    let t_all = std::time::Instant::now();
    {
        // search text first, so a search opened early can use what is there
        if let Ok(map) = std::fs::read_to_string(text_path()).map(|s| serde_json::from_str::<HashMap<String, (u64, u64, String)>>(&s).unwrap_or_default()) {
            let entries: Vec<(&String, &(u64, u64, String))> = map.iter().collect();
            let chunk = entries.len().div_ceil(8).max(1);
            let lowered: Vec<(String, String)> = std::thread::scope(|scope| {
                let hs: Vec<_> = entries.chunks(chunk).map(|c| scope.spawn(move || c.iter().map(|(k, v)| ((*k).clone(), v.2.to_lowercase())).collect::<Vec<_>>())).collect();
                hs.into_iter().flat_map(|h| h.join().unwrap()).collect()
            });
            if let Ok(mut g) = idx().lock() {
                g.lower.extend(lowered);
                g.text = map;
            }
        }
        if let Ok(mut g) = idx().lock() {
            g.text_ready = true;
        }
        if cfg!(test) { eprintln!("[phase] load+lowercase text store: {:?}", t_all.elapsed()); }
        let t_list = std::time::Instant::now();
        let files: Vec<SessionFile> = profile_names.iter().flat_map(|n| session::session_files(n)).collect();
        let live: HashSet<String> = files.iter().map(|f| f.file.to_string_lossy().to_string()).collect();
        let stale: Vec<&SessionFile> = {
            let Ok(g) = idx().lock() else { return };
            // Same source as `scan_file` uses. The directory listing's modified time can lag
            // behind the file's own for a file that is open for writing (NTFS), which made
            // a live session look changed on every pass — so each file is stat'ed again, on
            // several threads (it is latency, not CPU).
            let chunk = files.len().div_ceil(8).max(1);
            let g = &*g;
            let flags: Vec<bool> = std::thread::scope(|scope| {
                let hs: Vec<_> = files
                    .chunks(chunk)
                    .map(|c| {
                        scope.spawn(move || {
                            c.iter()
                                .map(|f| {
                                    let key = f.file.to_string_lossy().to_string();
                                    let md = std::fs::metadata(&f.file).ok();
                                    let (ms, len) = (md.as_ref().and_then(|m| m.modified().ok()).map(ms_of).unwrap_or(0), md.map(|m| m.len()).unwrap_or(0));
                                    !matches!(g.meta.get(&key), Some(m) if m.mtime_ms == ms && m.len == len) || !g.text.contains_key(&key)
                                })
                                .collect::<Vec<_>>()
                        })
                    })
                    .collect();
                hs.into_iter().flat_map(|h| h.join().unwrap()).collect()
            });
            files.iter().zip(flags).filter(|(_, stale)| *stale).map(|(f, _)| f).collect()
        };
        if cfg!(test) { eprintln!("[phase] list + stale check ({} files): {:?}", files.len(), t_list.elapsed()); }
        let t_scan = std::time::Instant::now();
        if let Ok(mut g) = idx().lock() {
            g.total = stale.len();
        }
        if cfg!(test) {
            eprintln!("[index] stale sessions: {} ({} MB)", stale.len(), stale.iter().map(|f| std::fs::metadata(&f.file).map(|m| m.len()).unwrap_or(0)).sum::<u64>() / 1_000_000);
            if let Ok(g) = idx().lock() {
                for f in &stale {
                    let key = f.file.to_string_lossy().to_string();
                    let md = std::fs::metadata(&f.file).ok();
                    eprintln!("[index]   {} meta={:?} text={} fs=(mtime {:?}, len {:?})", f.id, g.meta.get(&key).map(|m| (m.mtime_ms, m.len)), g.text.contains_key(&key), md.as_ref().and_then(|m| m.modified().ok()).map(ms_of), md.map(|m| m.len()));
                }
            }
        }
        let mut changed = !stale.is_empty();
        let mut order: Vec<&SessionFile> = stale.clone();
        order.sort_by_key(|f| std::cmp::Reverse(std::fs::metadata(&f.file).map(|m| m.len()).unwrap_or(0)));
        let next = std::sync::atomic::AtomicUsize::new(0);
        let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 4);
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| loop {
                    // one shared queue, biggest sessions first: no worker is left holding the
                    // 200 MB file while the others sit idle
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(f) = order.get(i) else { break };
                    let key = f.file.to_string_lossy().to_string();
                    let prev = idx().lock().ok().and_then(|g| g.meta.get(&key).cloned().zip(g.text.get(&key).map(|t| t.2.clone())));
                    if let Some(s) = scan_file(&f.file, prev.as_ref().map(|(m, t)| (m, t.as_str()))) {
                        if let Ok(mut g) = idx().lock() {
                            g.lower.insert(key.clone(), s.text.to_lowercase());
                            g.text.insert(key.clone(), (s.meta.mtime_ms, s.meta.len, s.text));
                            g.meta.insert(key, s.meta);
                        }
                    }
                    if let Ok(mut g) = idx().lock() {
                        g.done += 1;
                    }
                });
            }
        });
        if cfg!(test) { eprintln!("[phase] scan {} stale: {:?}", stale.len(), t_scan.elapsed()); }
        let t_save = std::time::Instant::now();
        // forget sessions that no longer exist
        if let Ok(mut g) = idx().lock() {
            let before = g.meta.len();
            g.meta.retain(|k, _| live.contains(k));
            g.text.retain(|k, _| live.contains(k));
            g.lower.retain(|k, _| live.contains(k));
            changed |= g.meta.len() != before;
            // A live session grows every few seconds; rewriting the whole store each time
            // is wasted I/O, and losing a few seconds of progress on exit only means the
            // next start resumes a little earlier in that file. Save when a session was
            // indexed for the first time or went away, or when the last save is old.
            static LAST_SAVE_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let now = ms_of(SystemTime::now());
            let structural = g.meta.len() != before || stale.iter().any(|f| !g.text.contains_key(&f.file.to_string_lossy().to_string())) || LAST_SAVE_MS.load(std::sync::atomic::Ordering::Relaxed) == 0;
            let due = now.saturating_sub(LAST_SAVE_MS.load(std::sync::atomic::Ordering::Relaxed)) > 20_000;
            if changed && (structural || due) {
                LAST_SAVE_MS.store(now, std::sync::atomic::Ordering::Relaxed);
                if let Ok(j) = serde_json::to_string(&g.meta) {
                    write_atomic(&index_path(), &j);
                }
                if let Ok(j) = serde_json::to_string(&g.text) {
                    write_atomic(&text_path(), &j);
                }
            }
            g.running = false;
            if cfg!(test) { eprintln!("[phase] retain+save: {:?} (changed={changed})", t_save.elapsed()); }
        }
    }
}

/// Folder the session was started in, if known (index first, else a quick look
/// at the head of the transcript).
pub fn started_folder(file: &Path) -> Option<String> {
    let key = file.to_string_lossy().to_string();
    if let Some(m) = idx().lock().ok().and_then(|g| g.meta.get(&key).cloned()) {
        if !m.started.is_empty() {
            return Some(m.started);
        }
    }
    let c = session::session_cwd(file);
    (!c.is_empty()).then(|| norm_path(&c))
}

/// How many folders the session touched.
pub fn folder_count(file: &Path) -> usize {
    let key = file.to_string_lossy().to_string();
    idx().lock().ok().and_then(|g| g.meta.get(&key).map(|m| m.folders.len())).unwrap_or(1)
}

/// Drops a deleted session from the index.
pub fn forget(file: &Path) {
    let key = file.to_string_lossy().to_string();
    if let Ok(mut g) = idx().lock() {
        g.meta.remove(&key);
        g.text.remove(&key);
        g.lower.remove(&key);
    }
}

// ------------------------------------------------------------------ model

#[derive(Clone)]
struct Sess {
    acct: String,
    id: String,
    file: PathBuf,
    modified: SystemTime,
    started: String,
    folders: Vec<(String, f32)>,
    title: String,
    first: String,
    last: String,
    msgs: u32,
    indexed: bool,
    /// lowercase title + folders, and that plus prompts + account; built once per load
    head_lc: String,
    meta_lc: String,
}

impl Sess {
    fn key(&self) -> String {
        self.file.to_string_lossy().to_string()
    }
    fn touches(&self, folder: &str) -> bool {
        self.folders.iter().any(|(p, _)| under(p, folder))
    }
    fn moved_into(&self, folder: &str) -> bool {
        self.touches(folder) && !under(&self.started, folder)
    }
}

struct Folder {
    path: String,
    /// Sessions that touched this folder or any folder below it, newest first.
    sess: Vec<usize>,
    last: SystemTime,
    /// How many folders in the list contain this one (0 = top level).
    depth: usize,
}

struct Model {
    sessions: Vec<Sess>,
    folders: Vec<Folder>,
    cwd: String,
    scope_label: String,
    pins: Vec<crate::pins::Pin>,
}

#[derive(Clone)]
pub enum Scope {
    All,
    Account(String),
}

/// Rolls subfolders up into the folders above them and orders the list as a tree:
/// top-level folders newest first, each followed by its subfolders (newest first,
/// indented by `depth`). A folder's sessions include everything that touched a folder
/// below it, so `claude-cockpit` counts its `rust/src` sessions too; a parent is
/// never counted in its children (a session that touched `C:/` stays under `C:/`).
fn nest(flat: Vec<Folder>) -> Vec<Folder> {
    let n = flat.len();
    // parent = the longest other folder that contains this one
    let parent: Vec<Option<usize>> = (0..n)
        .map(|i| (0..n).filter(|&j| j != i && !same_path(&flat[j].path, &flat[i].path) && under(&flat[i].path, &flat[j].path)).max_by_key(|&j| flat[j].path.len()))
        .collect();
    let mut sess: Vec<Vec<usize>> = flat.iter().map(|f| f.sess.clone()).collect();
    let mut last: Vec<SystemTime> = flat.iter().map(|f| f.last).collect();
    for i in 0..n {
        let mut a = parent[i];
        while let Some(j) = a {
            let add = flat[i].sess.clone();
            sess[j].extend(add);
            last[j] = last[j].max(flat[i].last);
            a = parent[j];
        }
    }
    for v in sess.iter_mut() {
        v.sort_unstable();
        v.dedup(); // session indexes ascend with age, so this is newest first
    }
    let depth = |mut i: usize| {
        let mut d = 0;
        while let Some(j) = parent[i] {
            d += 1;
            i = j;
        }
        d
    };
    let mut order: Vec<usize> = Vec::with_capacity(n);
    fn walk(i: usize, parent: &[Option<usize>], last: &[SystemTime], order: &mut Vec<usize>) {
        order.push(i);
        let mut kids: Vec<usize> = (0..parent.len()).filter(|&k| parent[k] == Some(i)).collect();
        kids.sort_by(|&a, &b| last[b].cmp(&last[a]));
        for k in kids {
            walk(k, parent, last, order);
        }
    }
    let mut roots: Vec<usize> = (0..n).filter(|&i| parent[i].is_none()).collect();
    roots.sort_by(|&a, &b| last[b].cmp(&last[a]));
    for r in roots {
        walk(r, &parent, &last, &mut order);
    }
    let mut slots: Vec<Option<Folder>> = flat.into_iter().map(Some).collect();
    order
        .into_iter()
        .map(|i| {
            let mut f = slots[i].take().unwrap();
            f.sess = std::mem::take(&mut sess[i]);
            f.last = last[i];
            f.depth = depth(i);
            f
        })
        .collect()
}

impl Model {
    fn load(profiles: &[Profile], scope: &Scope) -> Model {
        let mut sessions = Vec::new();
        for p in profiles {
            if let Scope::Account(a) = scope {
                if &p.name != a {
                    continue;
                }
            }
            for f in session::session_files(&p.name) {
                let key = f.file.to_string_lossy().to_string();
                let meta = idx().lock().ok().and_then(|g| g.meta.get(&key).cloned());
                let s = match meta {
                    Some(m) => Sess { acct: p.name.clone(), id: f.id.clone(), file: f.file.clone(), modified: f.modified, started: m.started, folders: m.folders, title: m.title, first: m.first, last: m.last, msgs: m.msgs, indexed: true, head_lc: String::new(), meta_lc: String::new() },
                    None => {
                        // not indexed yet: what the list already knew how to show
                        let started = norm_path(&session::session_cwd(&f.file));
                        let (name, summary) = session::session_preview(&f.file);
                        let title = if !name.is_empty() { name } else if !summary.is_empty() { flat(&summary, 60) } else { f.id.chars().take(8).collect() };
                        Sess { acct: p.name.clone(), id: f.id.clone(), file: f.file.clone(), modified: f.modified, folders: vec![(started.clone(), 1.0)], started, title, first: flat(&summary, 160), last: flat(&summary, 160), msgs: 0, indexed: false, head_lc: String::new(), meta_lc: String::new() }
                    }
                };
                sessions.push(s);
            }
        }
        for s in sessions.iter_mut() {
            let folders = s.folders.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>().join(" ");
            s.head_lc = format!("{} {}", s.title, folders).to_lowercase();
            s.meta_lc = format!("{} {} {} {}", s.head_lc, s.first, s.last, s.acct).to_lowercase();
        }
        sessions.sort_by(|a, b| b.modified.cmp(&a.modified));
        let mut folders: Vec<Folder> = Vec::new();
        for (i, s) in sessions.iter().enumerate() {
            for (p, _) in &s.folders {
                match folders.iter_mut().find(|f| same_path(&f.path, p)) {
                    Some(f) => f.sess.push(i),
                    None => folders.push(Folder { path: p.clone(), sess: vec![i], last: s.modified, depth: 0 }),
                }
            }
        }
        let folders = nest(folders);
        let cwd = std::env::current_dir().map(|p| norm_path(&p.to_string_lossy())).unwrap_or_default();
        let scope_label = match scope {
            Scope::All => format!("all accounts · {} sessions · {} folders", sessions.len(), folders.len()),
            Scope::Account(a) => format!("account {a} · {} sessions · {} folders", sessions.len(), folders.len()),
        };
        let pins = crate::pins::load().into_iter().filter(|p| match scope {
            Scope::All => true,
            Scope::Account(a) => &p.acct == a,
        });
        Model { sessions, folders, cwd, scope_label, pins: pins.collect() }
    }
}

// ----------------------------------------------------------------- search

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Why {
    Title,
    Prompt,
    Content,
}

struct Hit {
    s: usize,
    why: Why,
    snip: String,
}

fn snippet(text: &str, lower: &str, tok: &str) -> String {
    let Some(i) = lower.find(tok) else { return String::new() };
    let src = if text.len() == lower.len() { text } else { lower };
    let mut a = i.saturating_sub(34);
    while a > 0 && !src.is_char_boundary(a) {
        a -= 1;
    }
    let mut b = (i + tok.len() + 46).min(src.len());
    while b < src.len() && !src.is_char_boundary(b) {
        b += 1;
    }
    let s = flat(&src[a..b], 200);
    if a > 0 {
        format!("…{s}")
    } else {
        s
    }
}

/// One term of a search query.
///   word        substring (case-insensitive) in title, prompts, folders, account or text
///   /regex/     regular expression (falls back to a literal if it does not parse)
///   ~word       fuzzy: the letters of `word` appear in order in the title or a folder's name
///   in:folder   a folder the session touched contains `folder`
///   acct:name   the account name contains `name`
enum Term {
    Lit(String),
    Re(regex::Regex),
    Fuzzy(String),
    In(String),
    Acct(String),
}

fn subsequence(hay: &str, needle: &str) -> bool {
    let mut it = hay.chars();
    needle.chars().all(|c| it.any(|h| h == c))
}

/// Splits on whitespace, but keeps `/a b/` (a regex containing spaces) together.
fn query_terms(q: &str) -> Vec<Term> {
    let mut raw: Vec<String> = Vec::new();
    let mut words = q.split_whitespace();
    while let Some(w) = words.next() {
        let mut w = w.to_string();
        if w.starts_with('/') && !(w.len() > 1 && w.ends_with('/')) {
            let rest: Vec<&str> = words.clone().collect();
            if let Some(end) = rest.iter().position(|r| r.ends_with('/')) {
                for _ in 0..=end {
                    w.push(' ');
                    w.push_str(words.next().unwrap_or(""));
                }
            }
        }
        raw.push(w);
    }
    raw.iter()
        .filter_map(|w| {
            let lw = w.to_lowercase();
            if w.len() > 2 && w.starts_with('/') && w.ends_with('/') {
                let pat = &w[1..w.len() - 1];
                return Some(match regex::RegexBuilder::new(pat).case_insensitive(true).build() {
                    Ok(re) => Term::Re(re),
                    Err(_) => Term::Lit(lw),
                });
            }
            if let Some(x) = lw.strip_prefix("in:") {
                return (!x.is_empty()).then(|| Term::In(norm_path(x).to_lowercase()));
            }
            if let Some(x) = lw.strip_prefix("acct:") {
                return (!x.is_empty()).then(|| Term::Acct(x.to_string()));
            }
            if let Some(x) = lw.strip_prefix('~') {
                return (!x.is_empty()).then(|| Term::Fuzzy(x.to_string()));
            }
            Some(Term::Lit(lw))
        })
        .collect()
}

/// Sessions of `pool` matching every term of `q` (see `Term`). Title/folder
/// matches rank first, then prompts, then conversation text.
fn find(m: &Model, pool: &[usize], q: &str) -> Vec<Hit> {
    let q = q.trim();
    let terms = if q.is_empty() { Vec::new() } else { query_terms(q) };
    if terms.is_empty() {
        return pool.iter().map(|&s| Hit { s, why: Why::Title, snip: String::new() }).collect();
    }
    let g = idx().lock().ok();
    let mut out = Vec::new();
    'sess: for &si in pool {
        let s = &m.sessions[si];
        let key = s.key();
        let lower = g.as_ref().and_then(|g| g.lower.get(&key)).map(|l| l.as_str());
        let (mut all_head, mut all_meta) = (true, true);
        let mut snip_from: Option<String> = None;
        for t in &terms {
            let (in_head, in_meta, in_text) = match t {
                Term::Lit(w) => (s.head_lc.contains(w.as_str()), s.meta_lc.contains(w.as_str()), lower.is_some_and(|l| l.contains(w.as_str()))),
                Term::Re(re) => (re.is_match(&s.head_lc), re.is_match(&s.meta_lc), lower.is_some_and(|l| re.is_match(l))),
                Term::Fuzzy(w) => {
                    // title and folder paths only: letters in order across long prompts match nearly anything
                    // ... and only the last part of each folder path (the shared `C:/Users/…` prefix would match anything)
                    let hay = format!("{} {}", s.title, s.folders.iter().map(|(p, _)| p.rsplit('/').next().unwrap_or(p)).collect::<Vec<_>>().join(" ")).to_lowercase();
                    let ok = subsequence(&hay, w);
                    (ok, ok, false)
                }
                Term::In(f) => {
                    let ok = s.folders.iter().any(|(p, _)| p.to_lowercase().contains(f.as_str()));
                    (ok, ok, false)
                }
                Term::Acct(a) => {
                    let ok = s.acct.to_lowercase().contains(a.as_str());
                    (ok, ok, false)
                }
            };
            if !(in_meta || in_text) {
                continue 'sess;
            }
            all_head &= in_head;
            all_meta &= in_meta;
            if !in_meta && in_text && snip_from.is_none() {
                snip_from = Some(match t {
                    Term::Re(re) => lower.and_then(|l| re.find(l)).map(|f| f.as_str().to_string()).unwrap_or_default(),
                    Term::Lit(w) => w.clone(),
                    _ => String::new(),
                });
            }
        }
        let why = if all_head { Why::Title } else if all_meta { Why::Prompt } else { Why::Content };
        let snip = match (&snip_from, g.as_ref().and_then(|g| g.text.get(&key)), lower) {
            (Some(tok), Some(t), Some(l)) if !tok.is_empty() => snippet(&t.2, l, tok),
            _ => String::new(),
        };
        out.push(Hit { s: si, why, snip });
    }
    out.sort_by(|a, b| a.why.cmp(&b.why).then(m.sessions[b.s].modified.cmp(&m.sessions[a.s].modified)));
    out
}

// ---------------------------------------------------------------- drawing

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

fn highlight(s: &str, q: &str) -> String {
    let q = q.trim();
    if q.is_empty() || !s.is_ascii() || !q.is_ascii() {
        return s.to_string();
    }
    let ls = s.to_ascii_lowercase();
    let lq = q.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
    if lq.is_empty() {
        return s.to_string();
    }
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

fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

fn age(t: SystemTime) -> String {
    session::relative_age(Some(t))
}

/// Cuts `s` to `n` visible columns, passing colour escapes through.
fn clip_visible_to(s: &str, n: usize) -> String {
    let mut out = String::new();
    let mut vis = 0usize;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            out.push(c);
            for c2 in chars.by_ref() {
                out.push(c2);
                if c2 == 'm' {
                    break;
                }
            }
        } else if vis < n {
            out.push(c);
            vis += 1;
        }
    }
    out
}

fn badge(s: &Sess, folder: Option<&str>) -> &'static str {
    match folder {
        Some(f) if s.moved_into(f) => "↦ moved here",
        _ => "",
    }
}

fn acct_color(a: &str, t: &str) -> String {
    if a.starts_with("work") {
        ui::color::yellow(t)
    } else {
        ui::color::cyan(t)
    }
}

fn session_row(s: &Sess, on: bool, marked: bool, folder: Option<&str>, q: &str, width: usize) -> String {
    let bd = badge(s, folder);
    let title_w = width.saturating_sub(3 + 2 + 9 + 7 + if bd.is_empty() { 0 } else { bd.chars().count() + 2 } + 1).max(12);
    let title = clip(&s.title, title_w);
    let ptr = if on { ui::color::orange(ui::glyph::pointer()) } else { " ".to_string() };
    let tag = if bd.is_empty() { String::new() } else { format!("  {}", ui::color::yellow(bd)) };
    // fixed 2-column mark so selecting a row never shifts its text
    let mk = if marked { format!("{} ", ui::color::orange("✓")) } else { "  ".to_string() };
    if on {
        format!(" {ptr} {mk}{}{tag}", ui::color::orange_bold(&format!("{}{}{}", pad(&age(s.modified), 9), pad(&s.acct, 7), title)))
    } else {
        format!(" {ptr} {mk}{}{}{}{tag}", ui::color::dim(&pad(&age(s.modified), 9)), acct_color(&s.acct, &pad(&s.acct, 7)), highlight(&title, q))
    }
}

enum Item {
    Pin(usize),
    Head(String, usize),
    Sess(usize, Option<String>),
    More(bool, usize),
    Folder(usize),
}

fn strip(m: &Model, item: Option<&Item>, q: &str, width: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let row = |k: &str, v: String| format!("  {}{v}", ui::color::dim(&pad(k, 10)));
    let rule = |label: &str| {
        let head = format!("─ {label} ");
        ui::color::gray(&format!("{head}{}", "─".repeat(width.saturating_sub(head.chars().count()))))
    };
    match item {
        Some(Item::Sess(si, _)) => {
            let s = &m.sessions[*si];
            out.push(rule(&clip(&s.title, 64)));
            out.push(row("started", format!("{}   {}", ui::color::orange(&short_path(&s.started)), ui::color::dim(&format!("{} messages · {}{}", s.msgs, s.acct, if s.indexed { "" } else { " · not indexed yet" })))));
            let touched: Vec<String> = s.folders.iter().map(|(p, sh)| format!("{} {}", short_path(p), ui::color::orange(&"▇".repeat(((sh * 10.0).round() as usize).max(1))))).collect();
            out.push(row("touched", touched.join("   ")));
            out.push(row("last said", highlight(&clip(&s.last, width.saturating_sub(14)), q)));
            let sn = if q.trim().is_empty() {
                String::new()
            } else {
                let t = q.trim().split_whitespace().find(|w| !w.starts_with('/') && !w.starts_with('~') && !w.starts_with("in:") && !w.starts_with("acct:")).unwrap_or("").to_lowercase();
                let g = idx().lock().ok();
                match (g.as_ref().and_then(|g| g.text.get(&s.key())), g.as_ref().and_then(|g| g.lower.get(&s.key()))) {
                    (Some(tx), Some(l)) => snippet(&tx.2, l, &t),
                    _ => String::new(),
                }
            };
            out.push(row("match", if sn.is_empty() { ui::color::dim("—") } else { ui::color::yellow(&highlight(&clip(&sn, width.saturating_sub(14)), q)) }));
            out.push(row("resume", format!("here → {}{}", short_path(&m.cwd), ui::color::dim(&format!("    or in started folder → {}", short_path(&s.started))))));
        }
        Some(Item::Pin(pi)) => {
            let pin = &m.pins[*pi];
            let live = m.sessions.iter().find(|s| s.acct == pin.acct && s.id == pin.session_id);
            out.push(rule(&clip(&pin.label, 64)));
            out.push(row("session", format!("{}   {}", clip(&pin.title, 40), ui::color::dim(&format!("{} · {}", pin.acct, if live.is_some() { "still there" } else { "deleted — the saved text below is all that is left" })))));
            out.push(row("part", format!("{}{}", pin.range_label(), if pin.is_part() { ui::color::dim("   (Enter → open it selected in the transcript editor)") } else { String::new() })));
            let lines = crate::pins::wrap(&pin.excerpt, width.saturating_sub(14));
            for (i, l) in lines.iter().take(3).enumerate() {
                out.push(row(if i == 0 { "saved" } else { "" }, clip(l, width.saturating_sub(14))));
            }
        }
        Some(Item::Folder(fi)) => {
            let f = &m.folders[*fi];
            let moved = f.sess.iter().filter(|&&i| m.sessions[i].moved_into(&f.path)).count();
            let mut acc: HashMap<&str, usize> = HashMap::new();
            for &i in &f.sess {
                *acc.entry(m.sessions[i].acct.as_str()).or_insert(0) += 1;
            }
            let mut accs: Vec<String> = acc.iter().map(|(k, v)| format!("{k} {v}")).collect();
            accs.sort();
            out.push(rule(&short_path(&f.path)));
            out.push(row("sessions", format!("{} total · {} moved here from other folders", f.sess.len(), moved)));
            out.push(row("accounts", accs.join("  ·  ")));
            out.push(row("newest", f.sess.first().map(|&i| clip(&m.sessions[i].title, width.saturating_sub(14))).unwrap_or_default()));
        }
        _ => {}
    }
    while out.len() < DET {
        out.push(String::new());
    }
    out
}

// ------------------------------------------------------------------- run

#[derive(PartialEq, Clone, Copy)]
enum Screen {
    Home,
    Folder,
    Search,
}

struct Ui {
    screen: Screen,
    /// None = "all folders" (opened with `/` from the home screen)
    folder: Option<usize>,
    sel: usize,
    top: usize,
    filter: String,
    filtering: bool,
    q: String,
    ssel: usize,
    stop: usize,
    scope: usize,
    open_here: usize,
    open_else: usize,
    flash: String,
    /// Folders (lowercased path) whose children are shown; everything else is folded.
    expanded: HashSet<String>,
    /// Cursor and scroll of the home list, restored when you come back from a folder.
    home_pos: (usize, usize),
    /// Sessions marked with Space (by transcript path), for deleting several at once.
    picked: HashSet<String>,
    /// A question waiting for an answer at the bottom of the screen, and the text typed so far.
    ask: Option<Ask>,
    ask_buf: String,
}

/// What the bottom prompt is asking.
enum Ask {
    /// Delete these sessions (indexes into `Model::sessions`)? y/n.
    Delete(Vec<usize>),
    /// Pin this whole session: type a label.
    PinSession(usize),
    /// Rename this pin: type the new label.
    RenamePin(String),
}

/// Pre-order folder indexes that are visible given which folders are expanded.
fn visible_folders(folders: &[Folder], expanded: &HashSet<String>) -> Vec<usize> {
    let mut out = Vec::new();
    let mut hidden_below: Option<usize> = None;
    for (i, f) in folders.iter().enumerate() {
        if let Some(d) = hidden_below {
            if f.depth > d {
                continue;
            }
            hidden_below = None;
        }
        out.push(i);
        if has_children(folders, i) && !expanded.contains(&f.path.to_lowercase()) {
            hidden_below = Some(f.depth);
        }
    }
    out
}

fn has_children(folders: &[Folder], i: usize) -> bool {
    folders.get(i + 1).is_some_and(|n| n.depth > folders[i].depth)
}

fn home_items(m: &Model, u: &Ui) -> Vec<Item> {
    let here: Vec<usize> = (0..m.sessions.len()).filter(|&i| m.sessions[i].touches(&m.cwd)).collect();
    let shown: HashSet<usize> = here.iter().take(u.open_here).copied().collect();
    let elsewhere: Vec<usize> = (0..m.sessions.len()).filter(|i| !here.contains(i) && !shown.contains(i)).collect();
    let mut items: Vec<Item> = Vec::new();
    if !m.pins.is_empty() {
        items.push(Item::Head("Pinned".to_string(), m.pins.len()));
        items.extend((0..m.pins.len()).map(Item::Pin));
    }
    items.push(Item::Head(format!("This folder · {}", short_path(&m.cwd)), here.len()));
    for &i in here.iter().take(u.open_here) {
        items.push(Item::Sess(i, Some(m.cwd.clone())));
    }
    if here.len() > u.open_here {
        items.push(Item::More(true, here.len() - u.open_here));
    }
    items.push(Item::Head("Recent elsewhere".to_string(), elsewhere.len()));
    for &i in elsewhere.iter().take(u.open_else) {
        items.push(Item::Sess(i, None));
    }
    if elsewhere.len() > u.open_else {
        items.push(Item::More(false, elsewhere.len() - u.open_else));
    }
    items.push(Item::Head("Folders".to_string(), m.folders.len()));
    for i in visible_folders(&m.folders, &u.expanded) {
        items.push(Item::Folder(i));
    }
    items
}

fn folder_items(m: &Model, u: &Ui) -> (Vec<Item>, usize, String) {
    let (pool, ctx): (Vec<usize>, Option<String>) = match u.folder {
        Some(fi) => (m.folders[fi].sess.clone(), Some(m.folders[fi].path.clone())),
        None => ((0..m.sessions.len()).collect(), None),
    };
    let total = pool.len();
    let hits = find(m, &pool, &u.filter);
    let n = hits.len();
    let _ = n;
    let items = hits.into_iter().map(|h| Item::Sess(h.s, ctx.clone())).collect();
    (items, total, ctx.unwrap_or_default())
}

fn search_pool(m: &Model, u: &Ui) -> Vec<usize> {
    match u.scope {
        0 => (0..m.sessions.len()).collect(),
        1 => (0..m.sessions.len()).filter(|&i| m.sessions[i].acct.starts_with("work")).collect(),
        _ => (0..m.sessions.len()).filter(|&i| m.sessions[i].touches(&m.cwd)).collect(),
    }
}

fn search_hits(m: &Model, u: &Ui) -> Vec<Hit> {
    let pool = search_pool(m, u);
    let mut hits = find(m, &pool, &u.q);
    if u.q.trim().is_empty() {
        hits.sort_by(|a, b| m.sessions[b.s].touches(&m.cwd).cmp(&m.sessions[a.s].touches(&m.cwd)).then(m.sessions[b.s].modified.cmp(&m.sessions[a.s].modified)));
    }
    hits
}

const SCOPES: [&str; 3] = ["all accounts", "account: work", "this folder"];

fn selectable(items: &[Item]) -> Vec<usize> {
    items.iter().enumerate().filter(|(_, it)| !matches!(it, Item::Head(..))).map(|(i, _)| i).collect()
}

fn note_indexing() -> String {
    match indexing() {
        Some((d, t)) if t > 0 => format!(" · indexing {d}/{t}…"),
        _ => String::new(),
    }
}

fn draw(m: &Model, u: &mut Ui, header: &[String], tw: usize, th: usize) -> (Vec<String>, Option<(usize, usize)>) {
    let mut lines: Vec<String> = header.to_vec();
    let flash = if u.flash.is_empty() { String::new() } else { format!("  {}", ui::color::yellow(&u.flash)) };
    let mut cursor = None;
    let foot = DET + 1 + 1 + 1;

    if u.screen == Screen::Search {
        let hits = search_hits(m, u);
        if u.ssel >= hits.len() {
            u.ssel = hits.len().saturating_sub(1);
        }
        let mut tabs = String::new();
        for (i, n) in SCOPES.iter().enumerate() {
            tabs.push_str(&if i == u.scope { ui::color::orange_bold(&format!("[{n}]")) } else { ui::color::dim(&format!(" {n} ")) });
            tabs.push(' ');
        }
        lines.push(format!("{} {}   {tabs}{flash}", ui::color::orange(ui::glyph::back()), ui::color::bold("Search")));
        let hint = if u.q.is_empty() { ui::color::dim("  words · /regex/ · ~fuzzy · in:folder · acct:name") } else { String::new() };
        lines.push(format!("   {} {}{hint}", ui::color::dim("Search"), u.q));
        let pool = search_pool(m, u).len();
        lines.push(ui::color::dim(&format!("   {}{}", if u.q.trim().is_empty() { format!("{pool} sessions · this folder first, then recent") } else { format!("{} of {pool} sessions", hits.len()) }, note_indexing())));
        cursor = Some((lines.len() - 1, 3 + 7 + u.q.chars().count() + 1));
        let vp = (th.saturating_sub(lines.len() + foot - 1 + 2) / 2).max(2);
        if u.ssel < u.stop {
            u.stop = u.ssel;
        }
        if u.ssel >= u.stop + vp {
            u.stop = u.ssel + 1 - vp;
        }
        if u.stop > 0 {
            lines.push(ui::color::dim(&format!("    ↑ {} more", u.stop)));
        }
        for (k, h) in hits.iter().skip(u.stop).take(vp).enumerate() {
            let on = u.stop + k == u.ssel;
            let s = &m.sessions[h.s];
            lines.push(session_row(s, on, false, Some(&m.cwd), &u.q, tw));
            let fl = s.folders.iter().map(|(p, _)| short_path(p)).collect::<Vec<_>>().join("  +  ");
            let dot = if s.touches(&m.cwd) { ui::color::orange("● ") } else { "  ".to_string() };
            let sn = if h.snip.is_empty() { String::new() } else { format!("  ·  {}", ui::color::yellow(&highlight(&clip(&h.snip, 40), &u.q))) };
            lines.push(format!("       {dot}{}{sn}", ui::color::dim(&clip(&fl, 56))));
        }
        if hits.is_empty() {
            lines.push(ui::color::dim("   (no sessions match — try fewer words or another scope)"));
        }
        if hits.len() > u.stop + vp {
            lines.push(ui::color::dim(&format!("    ↓ {} more", hits.len() - u.stop - vp)));
        }
        while lines.len() < th.saturating_sub(foot - 1) {
            lines.push(String::new());
        }
        let it = hits.get(u.ssel).map(|h| Item::Sess(h.s, None));
        lines.extend(strip(m, it.as_ref(), &u.q, tw));
        lines.push(ui::rule(None));
        lines.extend(layout::hint_grid(&[&[("↑/↓", "move"), ("Enter", "open"), ("Tab", "scope"), ("type", "to search"), ("Esc", "back")]], (tw / 5).clamp(16, 22)));
        // lines.len() already fills the screen; cursor sits on the Search row
        return (lines, cursor);
    }

    let (items, title, sub) = match u.screen {
        Screen::Folder => {
            let (items, total, _) = folder_items(m, u);
            let name = u.folder.map(|f| short_path(&m.folders[f].path)).unwrap_or_else(|| "all folders".to_string());
            let sub = format!("   {} of {}{}{}", items.len(), total, if u.filter.is_empty() { String::new() } else { format!(" match “{}”", u.filter) }, note_indexing());
            (items, format!("Sessions in {name}"), sub)
        }
        _ => (home_items(m, u), "Sessions".to_string(), format!("   {}{}", m.scope_label, note_indexing())),
    };
    lines.push(format!("{} {}{flash}", ui::color::orange(ui::glyph::back()), ui::color::bold(&title)));
    lines.push(ui::color::dim(&clip(&sub, tw)));
    lines.push(String::new());
    let sel = selectable(&items);
    if u.sel >= sel.len() {
        u.sel = sel.len().saturating_sub(1);
    }
    let cur = sel.get(u.sel).copied();
    let vp = th.saturating_sub(lines.len() + foot + 2).max(3);
    if let Some(c) = cur {
        if c < u.top {
            u.top = c;
        }
        if c >= u.top + vp {
            u.top = c + 1 - vp;
        }
    }
    // a heading just above the window stays visible
    if u.top > 0 && matches!(items.get(u.top - 1), Some(Item::Head(..))) {
        u.top -= 1;
    }
    if u.top > 0 {
        lines.push(ui::color::dim(&format!("    ↑ {} more", u.top)));
    }
    let end = (u.top + vp).min(items.len());
    for (i, it) in items.iter().enumerate().take(end).skip(u.top) {
        let on = Some(i) == cur;
        match it {
            Item::Head(t, n) => lines.push(format!(" {} {}", ui::color::orange_bold(t), ui::color::dim(&format!("({n})")))),
            Item::More(_, n) => {
                let ptr = if on { ui::color::orange(ui::glyph::pointer()) } else { " ".to_string() };
                let txt = format!("   ↓ {n} more");
                lines.push(format!(" {ptr} {}", if on { ui::color::orange_bold(&txt) } else { ui::color::dim(&txt) }));
            }
            Item::Folder(fi) => {
                let f = &m.folders[*fi];
                let ptr = if on { ui::color::orange(ui::glyph::pointer()) } else { " ".to_string() };
                let rel = if f.depth == 0 {
                    short_path(&f.path)
                } else {
                    // below its parent: just the part after the parent's path
                    (0..*fi).rev().find(|&k| m.folders[k].depth + 1 == f.depth && under(&f.path, &m.folders[k].path)).map(|k| f.path[m.folders[k].path.len()..].trim_start_matches('/').to_string()).unwrap_or_else(|| short_path(&f.path))
                };
                let mark = if !has_children(&m.folders, *fi) {
                    " "
                } else if u.expanded.contains(&f.path.to_lowercase()) {
                    "▾"
                } else {
                    "▸"
                };
                let shown = format!("{}{mark} {rel}", "  ".repeat(f.depth));
                let name = pad(&clip(&shown, 44), 44);
                let rest = format!("{}{}", pad(&plural(f.sess.len(), "session"), 14), age(f.last));
                lines.push(format!(" {ptr} {}{}", if on { ui::color::orange_bold(&name) } else { name }, ui::color::dim(&rest)));
            }
            Item::Sess(si, ctx) => lines.push(session_row(&m.sessions[*si], on, u.picked.contains(&m.sessions[*si].key()), ctx.as_deref(), &u.filter, tw)),
            Item::Pin(pi) => {
                let pin = &m.pins[*pi];
                let ptr = if on { ui::color::orange(ui::glyph::pointer()) } else { " ".to_string() };
                let gone = !m.sessions.iter().any(|s| s.acct == pin.acct && s.id == pin.session_id);
                let tail = format!("{} · {} · {}{}", pin.range_label(), clip(&pin.title, 28), pin.acct, if gone { " · session deleted" } else { "" });
                let label = clip(&pin.label, 30);
                let line = if on {
                    ui::color::orange_bold(&format!("{}{}", pad(&label, 32), tail))
                } else {
                    format!("{}{}", pad(&label, 32), ui::color::dim(&tail))
                };
                lines.push(format!(" {ptr} {}{}", ui::color::orange("★ "), clip_visible_to(&line, tw.saturating_sub(7))));
            }
        }
    }
    if items.len() > end {
        lines.push(ui::color::dim(&format!("    ↓ {} more", items.len() - end)));
    }
    while lines.len() < th.saturating_sub(foot) {
        lines.push(String::new());
    }
    lines.extend(strip(m, cur.and_then(|c| items.get(c)), &u.filter, tw));
    lines.push(ui::rule(None));
    let back = if u.screen == Screen::Home { "Esc" } else { "←/Esc" };
    let on_folder = matches!(cur.and_then(|c| items.get(c)), Some(Item::Folder(_)));
    let on_sess = matches!(cur.and_then(|c| items.get(c)), Some(Item::Sess(..)));
    let on_pin = matches!(cur.and_then(|c| items.get(c)), Some(Item::Pin(_)));
    let mut hs: Vec<(&str, &str)> = vec![("↑/↓", "move"), ("Enter", "open"), ("/", "filter"), ("Ctrl+F", "search all")];
    if on_folder {
        hs.push(("←/→", "fold"));
    }
    if on_sess {
        hs.extend([("Space", "select"), ("d", "delete"), ("p", "pin")]);
    }
    if on_pin {
        hs.push(("d", "unpin"));
    }
    hs.push((back, "back"));
    lines.extend(layout::hint_grid(&[&hs], (tw / hs.len()).clamp(14, 22)));
    if let Some(ask) = &u.ask {
        let q = match ask {
            Ask::Delete(v) => {
                let multi = v.iter().filter(|&&i| m.sessions[i].folders.len() > 1).count();
                format!("Delete {} session{}{}? (y/n) ", v.len(), if v.len() == 1 { "" } else { "s" }, if multi > 0 { format!(" ({multi} touched several folders; removed from all)") } else { String::new() })
            }
            Ask::PinSession(_) => "Pin session as: ".to_string(),
            Ask::RenamePin(_) => "Rename pin: ".to_string(),
        };
        if matches!(ask, Ask::Delete(_)) {
            lines.push(format!("{} {}", ui::color::orange(ui::glyph::back()), ui::color::yellow(&q)));
        } else {
            lines.push(format!("{} {q}{}", ui::color::orange(ui::glyph::back()), u.ask_buf));
            cursor = Some((lines.len(), 2 + q.chars().count() + u.ask_buf.chars().count() + 1));
        }
    } else if u.filtering {
        lines.push(format!("{} Filter: {}", ui::color::orange(ui::glyph::back()), u.filter));
        cursor = Some((lines.len(), 2 + 8 + u.filter.chars().count() + 1));
    } else {
        lines.push(String::new());
    }
    (lines, cursor)
}

/// One session's action menu — resume (here / in the folder it started in),
/// hand off, view, rename, clone, edit, delete — is the existing menu, opened
/// on a one-session group so it works the same from every list.
fn open_menu(term: &mut ui::Terminal, profiles: &[Profile], mark: i64, s: &Sess) -> anyhow::Result<()> {
    let group = SessionGroup { folder: short_path(&s.started), sessions: vec![SessionFile { id: s.id.clone(), file: s.file.clone(), modified: s.modified }] };
    crate::open_folder_sessions(term, profiles, mark, &s.acct, &group, Some(&s.id))
}

/// What the pin menu wants the browser to do next.
enum PinOutcome {
    Nothing,
    Rename,
    Changed,
}

/// The menu for one pin: read the saved text, open its part in the transcript
/// editor (selected), open the session's own menu, rename or unpin.
fn pin_menu(term: &mut ui::Terminal, profiles: &[Profile], mark: i64, header: &[String], m: &Model, pin: &crate::pins::Pin) -> anyhow::Result<PinOutcome> {
    use crate::picker::{self, ListChoice, ListOption};
    let live = m.sessions.iter().find(|s| s.acct == pin.acct && s.id == pin.session_id).cloned();
    let mut options = vec![ListOption { label: "View saved text".into(), value: "view".into(), note: Some("read the part as it was pinned".into()) }];
    if live.is_some() {
        if pin.is_part() {
            options.push(ListOption { label: "Open part in editor".into(), value: "edit".into(), note: Some(format!("transcript editor with {} selected", pin.range_label())) });
        }
        options.push(ListOption { label: "Open session".into(), value: "open".into(), note: Some("resume, hand off, view, …".into()) });
    }
    options.push(ListOption { label: "Rename pin".into(), value: "rename".into(), note: None });
    options.push(ListOption { label: "Unpin".into(), value: "unpin".into(), note: Some("forget this pin (the session stays)".into()) });
    let heading = format!("{}  ·  {}", pin.label, pin.range_label());
    let ListChoice::Picked(act) = picker::choose_from_list(profiles, mark, &heading, None, &options, None)? else { return Ok(PinOutcome::Nothing) };
    match act.as_str() {
        "view" => {
            let mut h = header.to_vec();
            h.push(format!("{} {}", ui::color::orange(ui::glyph::back()), ui::color::bold(&pin.label)));
            h.push(ui::color::dim(&format!("   {} · {} · {}", pin.title, pin.acct, pin.range_label())));
            h.push(String::new());
            let width = crossterm::terminal::size().map(|(w, _)| w as usize).unwrap_or(100).saturating_sub(8).min(110);
            let rows: Vec<picker::ScrollRow> = crate::pins::wrap(&pin.excerpt, width).into_iter().map(|l| picker::ScrollRow::info(format!("   {l}"))).collect();
            picker::scroll_screen(&h, &rows, None, None)?;
            Ok(PinOutcome::Nothing)
        }
        "edit" => {
            if let Some(s) = &live {
                crate::transcript::run(profiles, mark, &s.file, &s.id, &s.title, &s.acct, Some(pin))?;
            }
            Ok(PinOutcome::Changed)
        }
        "open" => {
            if let Some(s) = &live {
                open_menu(term, profiles, mark, s)?;
            }
            Ok(PinOutcome::Changed)
        }
        "rename" => Ok(PinOutcome::Rename),
        _ => {
            crate::pins::remove(&pin.id);
            Ok(PinOutcome::Changed)
        }
    }
}

pub fn run(term: &mut ui::Terminal, profiles: &[Profile], mark: i64, scope: Scope) -> anyhow::Result<()> {
    let stats: Vec<Stat> = profiles.iter().map(|p| layout::account_stat(&p.name)).collect();
    let mut header = layout::account_lines(profiles, mark, &stats);
    let mut header_w = ui::term_width();
    let mut m = Model::load(profiles, &scope);
    let mut u = Ui { screen: Screen::Home, folder: None, sel: 0, top: 0, filter: String::new(), filtering: false, q: String::new(), ssel: 0, stop: 0, scope: 0, open_here: 5, open_else: 5, flash: String::new(), expanded: HashSet::new(), home_pos: (0, 0), picked: HashSet::new(), ask: None, ask_buf: String::new() };
    let mut was_indexing = indexing().is_some();
    ui::hide_cursor();
    loop {
        // One row short of the terminal: a frame that fills every row scrolls the
        // screen by one line on the last write (the header would be pushed off).
        let (tw, th) = crossterm::terminal::size().map(|(w, h)| (w as usize, (h as usize).saturating_sub(1))).unwrap_or((100, 29));
        // the header's rule is as wide as the terminal was when it was built
        if tw != header_w {
            header = layout::account_lines(profiles, mark, &stats);
            header_w = tw;
        }
        let (lines, cursor) = draw(&m, &mut u, &header, tw, th);
        match cursor {
            Some((row, col)) => ui::repaint_at(&lines, row, col),
            None => {
                ui::hide_cursor();
                ui::repaint(&lines);
            }
        }
        // while the background index is still filling in, refresh a few times a second
        if indexing().is_some() {
            was_indexing = true;
            if !crossterm::event::poll(Duration::from_millis(400))? {
                continue;
            }
        } else if was_indexing {
            was_indexing = false;
            m = Model::load(profiles, &scope);
            continue;
        }
        let raw = read_key()?;
        if raw.ctrl && raw.code == KeyCode::Char('c') {
            ui::show_cursor();
            std::process::exit(130);
        }
        u.flash.clear();

        // ---- bottom prompt (delete confirmation / label)
        if let Some(ask) = u.ask.take() {
            match ask {
                Ask::Delete(v) => {
                    if !raw.ctrl && matches!(raw.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                        let n = v.len();
                        for &i in &v {
                            let s = &m.sessions[i];
                            let sf = SessionFile { id: s.id.clone(), file: s.file.clone(), modified: s.modified };
                            let _ = session::delete_session(&crate::paths::profile_dir(&s.acct), &sf);
                        }
                        u.picked.clear();
                        m = Model::load(profiles, &scope);
                        u.flash = format!("deleted {n} session{}", if n == 1 { "" } else { "s" });
                    } else {
                        u.flash = "cancelled".into();
                    }
                }
                other => match raw.code {
                    KeyCode::Esc => u.ask_buf.clear(),
                    KeyCode::Enter => {
                        let label = u.ask_buf.trim().to_string();
                        match other {
                            Ask::PinSession(si) => {
                                let s = &m.sessions[si];
                                crate::pins::add(crate::pins::Pin {
                                    id: String::new(),
                                    acct: s.acct.clone(),
                                    session_id: s.id.clone(),
                                    title: s.title.clone(),
                                    label: if label.is_empty() { s.title.clone() } else { label },
                                    from_turn: None,
                                    to_turn: None,
                                    from_uuid: None,
                                    to_uuid: None,
                                    excerpt: format!("{}\n\n… …\n\n{}", s.first, s.last),
                                    created_ms: 0,
                                });
                                u.flash = "pinned".into();
                            }
                            Ask::RenamePin(id) => {
                                if !label.is_empty() {
                                    crate::pins::rename(&id, &label);
                                }
                            }
                            Ask::Delete(_) => {}
                        }
                        u.ask_buf.clear();
                        m = Model::load(profiles, &scope);
                    }
                    KeyCode::Backspace => {
                        u.ask_buf.pop();
                        u.ask = Some(other);
                    }
                    KeyCode::Char(c) if !raw.ctrl => {
                        u.ask_buf.push(c);
                        u.ask = Some(other);
                    }
                    _ => u.ask = Some(other),
                },
            }
            continue;
        }

        // ---- search screen: letters are text
        if u.screen == Screen::Search {
            let hits = search_hits(&m, &u);
            match raw.code {
                KeyCode::Esc => {
                    if !u.q.is_empty() {
                        u.q.clear();
                        u.ssel = 0;
                        u.stop = 0;
                    } else {
                        u.screen = if u.folder.is_some() { Screen::Folder } else { Screen::Home };
                    }
                }
                KeyCode::Down => u.ssel = (u.ssel + 1).min(hits.len().saturating_sub(1)),
                KeyCode::Up => u.ssel = u.ssel.saturating_sub(1),
                KeyCode::PageDown => u.ssel = (u.ssel + 4).min(hits.len().saturating_sub(1)),
                KeyCode::PageUp => u.ssel = u.ssel.saturating_sub(4),
                KeyCode::Tab => {
                    u.scope = (u.scope + 1) % 3;
                    u.ssel = 0;
                    u.stop = 0;
                }
                KeyCode::Backspace => {
                    u.q.pop();
                    u.ssel = 0;
                    u.stop = 0;
                }
                KeyCode::Enter => {
                    if let Some(h) = hits.get(u.ssel) {
                        let s = m.sessions[h.s].clone();
                        open_menu(term, profiles, mark, &s)?;
                        m = Model::load(profiles, &scope);
                        ui::hide_cursor();
                    }
                }
                KeyCode::Char(c) if !raw.ctrl => {
                    u.q.push(c);
                    u.ssel = 0;
                    u.stop = 0;
                }
                _ => {}
            }
            continue;
        }

        // ---- filter prompt
        if u.filtering {
            match raw.code {
                KeyCode::Enter => u.filtering = false,
                KeyCode::Esc => {
                    u.filtering = false;
                    u.filter.clear();
                    u.sel = 0;
                }
                KeyCode::Backspace => {
                    u.filter.pop();
                    u.sel = 0;
                }
                KeyCode::Char(c) if !raw.ctrl => {
                    u.filter.push(c);
                    u.sel = 0;
                }
                _ => {}
            }
            continue;
        }

        let key = input::gate_vim(raw, &[]);
        let items = if u.screen == Screen::Home { home_items(&m, &u) } else { folder_items(&m, &u).0 };
        let sel = selectable(&items);
        let n = sel.len();
        if input::vim_jump(&key, &mut u.sel, n, 8, true) {
            continue;
        }
        match key.code {
            KeyCode::Char('f') if key.ctrl => {
                u.screen = Screen::Search;
                u.q.clear();
                u.ssel = 0;
                u.stop = 0;
                u.scope = 0;
            }
            KeyCode::Up if key.ctrl => u.sel = 0,
            KeyCode::Down if key.ctrl => u.sel = n.saturating_sub(1),
            KeyCode::Up | KeyCode::Char('k') => u.sel = u.sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => u.sel = (u.sel + 1).min(n.saturating_sub(1)),
            KeyCode::PageUp => u.sel = u.sel.saturating_sub(5),
            KeyCode::PageDown => u.sel = (u.sel + 5).min(n.saturating_sub(1)),
            KeyCode::Home => u.sel = 0,
            KeyCode::End => u.sel = n.saturating_sub(1),
            KeyCode::Char(' ') if matches!(sel.get(u.sel).and_then(|&i| items.get(i)), Some(Item::Sess(..))) => {
                if let Some(Item::Sess(si, _)) = sel.get(u.sel).and_then(|&i| items.get(i)) {
                    let k = m.sessions[*si].key();
                    if !u.picked.remove(&k) {
                        u.picked.insert(k);
                    }
                }
            }
            KeyCode::Char('d') if matches!(sel.get(u.sel).and_then(|&i| items.get(i)), Some(Item::Sess(..)) | Some(Item::Pin(_))) => {
                match sel.get(u.sel).and_then(|&i| items.get(i)) {
                    Some(Item::Pin(pi)) => {
                        crate::pins::remove(&m.pins[*pi].id);
                        m = Model::load(profiles, &scope);
                        u.flash = "unpinned".into();
                    }
                    Some(Item::Sess(si, _)) => {
                        // the marked sessions, or just this one when none are marked
                        let mut v: Vec<usize> = (0..m.sessions.len()).filter(|&i| u.picked.contains(&m.sessions[i].key())).collect();
                        if v.is_empty() {
                            v.push(*si);
                        }
                        u.ask = Some(Ask::Delete(v));
                    }
                    _ => {}
                }
            }
            KeyCode::Char('p') if matches!(sel.get(u.sel).and_then(|&i| items.get(i)), Some(Item::Sess(..))) => {
                if let Some(Item::Sess(si, _)) = sel.get(u.sel).and_then(|&i| items.get(i)) {
                    u.ask_buf = m.sessions[*si].title.clone();
                    u.ask = Some(Ask::PinSession(*si));
                }
            }
            KeyCode::Char('/') => {
                if u.screen == Screen::Home {
                    u.home_pos = (u.sel, u.top);
                    u.screen = Screen::Folder;
                    u.folder = None;
                    u.sel = 0;
                    u.top = 0;
                    u.filter.clear();
                }
                u.filtering = true;
            }
            // Fold / unfold folders: right opens the children (and steps into them once
            // open), left closes them (or steps up to the parent). Enter opens a folder.
            KeyCode::Right | KeyCode::Left | KeyCode::Char('l') | KeyCode::Char('h')
                if u.screen == Screen::Home && matches!(sel.get(u.sel).and_then(|&i| items.get(i)), Some(Item::Folder(_))) =>
            {
                let Some(Item::Folder(fi)) = sel.get(u.sel).and_then(|&i| items.get(i)) else { continue };
                let fi = *fi;
                let path = m.folders[fi].path.to_lowercase();
                let right = matches!(key.code, KeyCode::Right | KeyCode::Char('l'));
                if right {
                    if has_children(&m.folders, fi) && !u.expanded.insert(path) && u.sel + 1 < n {
                        u.sel += 1; // already open: step to the first child
                    }
                } else if u.expanded.remove(&path) && has_children(&m.folders, fi) {
                    // closed its children
                } else if m.folders[fi].depth > 0 {
                    // already closed: go up to the parent row
                    if let Some(pk) = (0..fi).rev().find(|&k| m.folders[k].depth + 1 == m.folders[fi].depth) {
                        if let Some(pos) = sel.iter().position(|&ix| matches!(items.get(ix), Some(Item::Folder(k)) if *k == pk)) {
                            u.sel = pos;
                        }
                    }
                }
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                let Some(it) = sel.get(u.sel).and_then(|&i| items.get(i)) else { continue };
                match it {
                    Item::Sess(si, _) => {
                        let s = m.sessions[*si].clone();
                        open_menu(term, profiles, mark, &s)?;
                        m = Model::load(profiles, &scope);
                        ui::hide_cursor();
                    }
                    Item::Folder(fi) => {
                        u.home_pos = (u.sel, u.top);
                        u.screen = Screen::Folder;
                        u.folder = Some(*fi);
                        u.filter.clear();
                        u.sel = 0;
                        u.top = 0;
                    }
                    Item::More(here, _) => {
                        if *here {
                            u.open_here = 20;
                        } else {
                            u.open_else = 20;
                        }
                    }
                    Item::Pin(pi) => {
                        let pin = m.pins[*pi].clone();
                        match pin_menu(term, profiles, mark, &header, &m, &pin)? {
                            PinOutcome::Rename => {
                                u.ask_buf = pin.label.clone();
                                u.ask = Some(Ask::RenamePin(pin.id));
                            }
                            PinOutcome::Changed => m = Model::load(profiles, &scope),
                            PinOutcome::Nothing => {}
                        }
                        ui::hide_cursor();
                    }
                    Item::Head(..) => {}
                }
            }
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('q') | KeyCode::Char('h') => {
                if u.screen == Screen::Folder {
                    if !u.filter.is_empty() {
                        u.filter.clear();
                        u.sel = 0;
                    } else {
                        // back to the home list exactly where you left it
                        u.screen = Screen::Home;
                        (u.sel, u.top) = u.home_pos;
                    }
                } else {
                    ui::show_cursor();
                    return Ok(());
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_have_one_spelling() {
        assert_eq!(norm_path("/c/Users/Tung/work"), "C:/Users/Tung/work");
        assert_eq!(norm_path("c:\\Users\\Tung\\work\\"), "C:/Users/Tung/work");
        assert_eq!(norm_path("C:/Users/Tung/work"), "C:/Users/Tung/work");
        assert_eq!(norm_path("/home/tung"), "/home/tung");
        assert!(same_path("C:/Users/X", "c:/users/x"));
    }

    fn write_session(dir: &Path, name: &str, lines: &[&str]) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, lines.join("\n") + "\n").unwrap();
        p
    }

    #[test]
    fn scan_extracts_folders_titles_and_text() {
        let dir = std::env::temp_dir().join(format!("cockpit-sessions-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut lines: Vec<String> = Vec::new();
        lines.push(r#"{"type":"ai-title","aiTitle":"Fix the build"}"#.to_string());
        lines.push(r#"{"type":"user","cwd":"C:\\proj","message":{"role":"user","content":"please fix the build"}}"#.to_string());
        for i in 0..8 {
            lines.push(format!(r#"{{"type":"assistant","cwd":"/c/proj/sub","message":{{"role":"assistant","content":[{{"type":"text","text":"working on step {i}"}},{{"type":"tool_use","name":"Bash","input":{{"command":"secret-tool-input"}}}}]}}}}"#));
        }
        lines.push(r#"{"type":"user","cwd":"C:\\proj\\tiny","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"secret-tool-output"}]}}"#.to_string());
        lines.push(r#"{"type":"user","cwd":"C:\\proj","message":{"role":"user","content":"thanks, ship it"}}"#.to_string());
        let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        let p = write_session(&dir, "s1.jsonl", &refs);
        let s = scan_file(&p, None).unwrap();
        assert_eq!(s.meta.title, "Fix the build");
        assert_eq!(s.meta.started, "C:/proj");
        assert_eq!(s.meta.first, "please fix the build");
        assert_eq!(s.meta.last, "thanks, ship it");
        let folders: Vec<&str> = s.meta.folders.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(folders, vec!["C:/proj", "C:/proj/sub"], "git-bash spelling merged; the one-line cd is dropped as noise");
        assert!(s.text.contains("working on step 3") && s.text.contains("ship it"));
        assert!(!s.text.contains("secret-tool"), "tool input/output is not searchable");
        assert_eq!(s.meta.msgs, 10);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn snippet_centres_on_the_match() {
        let t = "the quick brown fox jumps over the lazy dog and keeps running far away";
        let l = t.to_lowercase();
        let s = snippet(t, &l, "lazy");
        assert!(s.contains("lazy dog"));
        assert!(snippet(t, &l, "absent").is_empty());
    }

    /// `cargo test --release bench_index_all -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_index_all() {
        let files: Vec<SessionFile> = ["main", "main2", "work", "work2"].iter().flat_map(|n| session::session_files(n)).collect();
        let t = std::time::Instant::now();
        let results: Vec<Option<Scanned>> = std::thread::scope(|scope| {
            let handles: Vec<_> = files.chunks(files.len().div_ceil(4).max(1)).map(|c| scope.spawn(move || c.iter().map(|f| scan_file(&f.file, None)).collect::<Vec<_>>())).collect();
            handles.into_iter().flat_map(|h| h.join().unwrap()).collect()
        });
        let ok: Vec<&Scanned> = results.iter().flatten().collect();
        let multi = ok.iter().filter(|s| s.meta.folders.len() > 1).count();
        let text: usize = ok.iter().map(|s| s.text.len()).sum();
        println!("indexed {} sessions in {:?}: {} touch >1 folder, {:.1} MB of searchable text", ok.len(), t.elapsed(), multi, text as f64 / 1e6);
        let titled = ok.iter().filter(|s| !s.meta.title.is_empty()).count();
        println!("with a title: {titled}; with a started folder: {}", ok.iter().filter(|s| !s.meta.started.is_empty()).count());
    }

    /// Timings for everything the browser does on real data:
    /// `cargo test --release bench_sessions -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_sessions() {
        use std::time::Instant;
        let profiles = crate::profiles::list_profiles().unwrap_or_default();
        let names: Vec<String> = profiles.iter().map(|p| p.name.clone()).collect();
        let t = Instant::now();
        let meta_json = std::fs::read_to_string(index_path()).unwrap_or_default();
        let metas: HashMap<String, Meta> = serde_json::from_str(&meta_json).unwrap_or_default();
        println!("load session-index.json ({:.0} KB, {} entries): {:?}", meta_json.len() as f64 / 1e3, metas.len(), t.elapsed());
        let t = Instant::now();
        let text_json = std::fs::read_to_string(text_path()).unwrap_or_default();
        println!("read session-text.json ({:.1} MB): {:?}", text_json.len() as f64 / 1e6, t.elapsed());
        let t = Instant::now();
        let texts: HashMap<String, (u64, u64, String)> = serde_json::from_str(&text_json).unwrap_or_default();
        println!("  parse it: {:?} ({} entries)", t.elapsed(), texts.len());
        let t = Instant::now();
        let lowers: HashMap<String, String> = texts.iter().map(|(k, v)| (k.clone(), v.2.to_lowercase())).collect();
        println!("  lowercase copies: {:?}", t.elapsed());
        drop((texts, lowers, text_json));

        // a full incremental pass (nothing stale if the files are current)
        { let mut g = idx().lock().unwrap(); g.running = true; }
        let t = Instant::now();
        index_worker(names.clone());
        println!("index_worker (up to date): {:?}", t.elapsed());

        // the two costs behind "one session changed": re-scan it, rewrite the stores
        let mut files: Vec<SessionFile> = names.iter().flat_map(|n| session::session_files(n)).collect();
        files.sort_by_key(|f| std::cmp::Reverse(std::fs::metadata(&f.file).map(|m| m.len()).unwrap_or(0)));
        if let Some(big) = files.first() {
            let len = std::fs::metadata(&big.file).map(|m| m.len()).unwrap_or(0);
            let t = Instant::now();
            let _ = scan_file(&big.file, None);
            let e = t.elapsed();
            println!("scan_file biggest ({:.0} MB) single thread: {:?} = {:.0} MB/s", len as f64 / 1e6, e, len as f64 / 1e6 / e.as_secs_f64());
        }
        {
            let g = idx().lock().unwrap();
            let t = Instant::now();
            let j = serde_json::to_string(&g.text).unwrap();
            let ser = t.elapsed();
            let t2 = Instant::now();
            write_atomic(&text_path(), &j);
            println!("save text store: serialize {:?} + write {:?} ({:.1} MB)", ser, t2.elapsed(), j.len() as f64 / 1e6);
        }
        // whole-corpus indexing with the real worker pool shape (cold: every file)
        let t = Instant::now();
        let total: usize = files.iter().filter_map(|f| scan_file(&f.file, None)).map(|s| s.text.len()).sum();
        println!("scan every session serially: {:?} ({:.1} MB of text)", t.elapsed(), total as f64 / 1e6);

        let t = Instant::now();
        let m = Model::load(&profiles, &Scope::All);
        println!("Model::load: {:?} ({} sessions, {} folders)", t.elapsed(), m.sessions.len(), m.folders.len());

        let pool: Vec<usize> = (0..m.sessions.len()).collect();
        for q in ["stego", "the", "rust src", "fix build error", "cwd", "claude-cockpit", "zzzznotfound", "in:rust/src", "acct:work build", "/cargo (build|test)/", "~stgsr"] {
            let t = Instant::now();
            let hits = find(&m, &pool, q);
            println!("find {:>16?}: {:>3} hits in {:?}", q, hits.len(), t.elapsed());
        }
        let mut u = Ui { screen: Screen::Home, folder: None, sel: 0, top: 0, filter: String::new(), filtering: false, q: String::new(), ssel: 0, stop: 0, scope: 0, open_here: 5, open_else: 5, flash: String::new(), expanded: HashSet::new(), home_pos: (0, 0), picked: HashSet::new(), ask: None, ask_buf: String::new() };
        let t = Instant::now();
        for _ in 0..200 {
            let _ = draw(&m, &mut u, &[], 120, 39);
        }
        println!("draw home x200: {:?} per frame", t.elapsed() / 200);
        u.screen = Screen::Search;
        u.q = "the".to_string();
        let t = Instant::now();
        for _ in 0..50 {
            let _ = draw(&m, &mut u, &[], 120, 39);
        }
        println!("draw search('the') x50: {:?} per frame", t.elapsed() / 50);
    }

    fn test_model() -> Model {
        let mk = |acct: &str, title: &str, folders: &[&str], first: &str| {
            let mut s = Sess {
                acct: acct.into(),
                id: title.into(),
                file: PathBuf::from(format!("/nonexistent/{title}.jsonl")),
                modified: SystemTime::now(),
                started: folders[0].into(),
                folders: folders.iter().map(|f| (f.to_string(), 0.5)).collect(),
                title: title.into(),
                first: first.into(),
                last: first.into(),
                msgs: 1,
                indexed: true,
                head_lc: String::new(),
                meta_lc: String::new(),
            };
            let f = s.folders.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>().join(" ");
            s.head_lc = format!("{} {}", s.title, f).to_lowercase();
            s.meta_lc = format!("{} {} {} {}", s.head_lc, s.first, s.last, s.acct).to_lowercase();
            s
        };
        Model {
            pins: Vec::new(),
            sessions: vec![
                mk("work", "Fix build failure", &["C:/work/cockpit", "C:/work/cockpit/rust/src"], "cargo build fails"),
                mk("main2", "Hiding data in files", &["C:/work/stegosaur"], "steganography ideas"),
                mk("work2", "Obsidian font", &["D:/MEGA/obsidian"], "change the default font"),
            ],
            folders: vec![],
            cwd: String::new(),
            scope_label: String::new(),
        }
    }

    fn titles(m: &Model, q: &str) -> Vec<String> {
        let pool: Vec<usize> = (0..m.sessions.len()).collect();
        find(m, &pool, q).into_iter().map(|h| m.sessions[h.s].title.clone()).collect()
    }

    #[test]
    fn search_syntax() {
        let m = test_model();
        assert_eq!(titles(&m, "build"), vec!["Fix build failure"]);
        assert_eq!(titles(&m, "in:rust/src"), vec!["Fix build failure"], "a subfolder it touched is searchable");
        assert_eq!(titles(&m, "in:stegosaur"), vec!["Hiding data in files"]);
        assert_eq!(titles(&m, "acct:work2"), vec!["Obsidian font"]);
        assert_eq!(titles(&m, "acct:work").len(), 2, "substring on the account name");
        assert_eq!(titles(&m, "/^fix .*fail/"), vec!["Fix build failure"]);
        assert_eq!(titles(&m, "/fix b/"), vec!["Fix build failure"], "slashes keep a regex with spaces together");
        assert_eq!(titles(&m, "~obsdn"), vec!["Obsidian font"], "fuzzy: letters in order");
        assert_eq!(titles(&m, "~xyz").len(), 0);
        assert_eq!(titles(&m, "build acct:work"), vec!["Fix build failure"], "terms combine with AND");
        assert_eq!(titles(&m, "/[unclosed").len(), 0, "an invalid regex is searched as plain text");
    }

    #[test]
    fn subfolders_roll_up_into_parents() {
        let t = SystemTime::now();
        let f = |p: &str, sess: Vec<usize>| Folder { path: p.into(), sess, last: t, depth: 0 };
        let out = nest(vec![f("C:/w/cockpit/rust/src", vec![2]), f("C:/w/cockpit", vec![0, 2]), f("C:/w/cockpit/rust", vec![1]), f("C:", vec![3]), f("C:/w/cockpitx", vec![4])]);
        let get = |p: &str| out.iter().find(|x| x.path == p).unwrap();
        assert_eq!(get("C:/w/cockpit").sess, vec![0, 1, 2], "own + rust + rust/src, once each");
        assert_eq!(get("C:/w/cockpit/rust").sess, vec![1, 2]);
        assert_eq!(get("C:/w/cockpit/rust/src").sess, vec![2]);
        assert_eq!(get("C:").sess, vec![0, 1, 2, 3, 4], "a parent includes everything below it");
        assert_eq!(get("C:/w/cockpitx").depth, 1, "sibling with a shared prefix is not a child of cockpit");
        assert_eq!(get("C:/w/cockpit/rust/src").depth, 3);
        assert!(under("C:/w/cockpit/x", "C:/w/cockpit") && !under("C:/w/cockpitx", "C:/w/cockpit"));
    }

    #[test]
    fn incremental_scan_matches_a_full_scan() {
        let dir = std::env::temp_dir().join(format!("cockpit-incr-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s.jsonl");
        let user = |cwd: &str, t: &str| format!("{{\"type\":\"user\",\"cwd\":\"{cwd}\",\"message\":{{\"role\":\"user\",\"content\":\"{t}\"}}}}\n");
        let asst = |cwd: &str, t: &str| format!("{{\"type\":\"assistant\",\"cwd\":\"{cwd}\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"{t}\"}}]}}}}\n");
        std::fs::write(&p, user("C:\\\\proj", "first question") + &asst("C:\\\\proj", "first answer")).unwrap();
        let a = scan_file(&p, None).unwrap();
        // the session grows (and ends with a line that is still being written)
        let mut grown = std::fs::read_to_string(&p).unwrap();
        for i in 0..6 {
            grown += &asst("C:\\\\proj\\\\sub", &format!("step {i}"));
        }
        grown += &user("C:\\\\proj\\\\sub", "last question");
        std::fs::write(&p, &grown).unwrap();
        let inc = scan_file(&p, Some((&a.meta, &a.text))).unwrap();
        let full = scan_file(&p, None).unwrap();
        assert_eq!(inc.text, full.text);
        assert_eq!(inc.meta.msgs, full.meta.msgs);
        assert_eq!(inc.meta.folders, full.meta.folders);
        assert_eq!((inc.meta.first.as_str(), inc.meta.last.as_str()), ("first question", "last question"));
        assert_eq!(inc.meta.scanned_to, full.meta.scanned_to);
        // a half-written trailing line is left for the next pass
        let partial = format!("{grown}{{\"type\":\"user\",\"message\":{{\"content\":\"half wri");
        std::fs::write(&p, &partial).unwrap();
        let with_partial = scan_file(&p, Some((&inc.meta, &inc.text))).unwrap();
        assert_eq!(with_partial.meta.msgs, full.meta.msgs);
        assert_eq!(with_partial.meta.scanned_to as usize, grown.len());
        // the file was rewritten (not appended to): the old position no longer matches, so it is re-read
        std::fs::write(&p, user("C:\\\\other", "rewritten") + &asst("C:\\\\other", "different")).unwrap();
        let re = scan_file(&p, Some((&inc.meta, &inc.text))).unwrap();
        assert_eq!(re.meta.started, "C:/other");
        assert!(re.text.contains("rewritten") && !re.text.contains("step 3"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn folding_hides_descendants() {
        let t = SystemTime::now();
        let f = |p: &str, d: usize| Folder { path: p.into(), sess: vec![0], last: t, depth: d };
        // pre-order: A, A/b, A/b/c, A/d, E
        let fs = vec![f("A", 0), f("A/b", 1), f("A/b/c", 2), f("A/d", 1), f("E", 0)];
        let mut open: HashSet<String> = HashSet::new();
        assert_eq!(visible_folders(&fs, &open), vec![0, 4], "everything folded: top level only");
        open.insert("a".to_string());
        assert_eq!(visible_folders(&fs, &open), vec![0, 1, 3, 4], "A open, A/b still folded hides A/b/c");
        open.insert("a/b".to_string());
        assert_eq!(visible_folders(&fs, &open), vec![0, 1, 2, 3, 4]);
        assert!(has_children(&fs, 0) && has_children(&fs, 1) && !has_children(&fs, 2) && !has_children(&fs, 4));
    }
}
