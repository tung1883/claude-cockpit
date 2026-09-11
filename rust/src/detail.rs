// Plugin/skill/MCP detail view: an overview screen (account, activity,
// pickable plugin/skill/mcp list) plus a per-item screen. Ported from the
// profileOverview/itemDetail/kv/kvWrap/etc. helpers in src/cli.js.
use crate::inspect::{self, Inspection};
use crate::layout::{self, pad_to};
use crate::picker::ScrollRow;
use crate::profiles::Profile;
use crate::ui::{self, color};
use crossterm::terminal;

fn term_width() -> usize {
    terminal::size().map(|(w, _)| w as usize).unwrap_or(80).min(100)
}

/// Shorten a plain string to `max` visible chars, keeping both ends (best
/// for file paths — the tail says which plugin/version it is).
fn clip_mid(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max || max < 8 {
        return text.to_string();
    }
    let keep = max - 1;
    let head = keep.div_ceil(2);
    let tail = keep / 2;
    let head_s: String = chars[..head].iter().collect();
    let tail_s: String = chars[chars.len() - tail..].iter().collect();
    format!("{head_s}…{tail_s}")
}

/// A two-column "key   value" line, key dimmed, aligned to `w`.
fn kv(key: &str, value: &str, w: usize) -> String {
    format!("    {}{value}", color::dim(&pad_to(key, w)))
}

/// Break a path/command into terminal-width chunks, preferring a break
/// right after a path separator so each piece still reads like a fragment.
fn wrap_hard(text: &str, width: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= width {
        return vec![text.to_string()];
    }
    let mut parts = Vec::new();
    let mut rest = chars.as_slice();
    while rest.len() > width {
        let window = &rest[..width];
        let sep = window.iter().rposition(|&c| c == '\\' || c == '/');
        let cut = match sep {
            Some(i) if (i as f64) > width as f64 * 0.4 => i + 1,
            _ => width,
        };
        parts.push(rest[..cut].iter().collect::<String>());
        rest = &rest[cut..];
    }
    if !rest.is_empty() {
        parts.push(rest.iter().collect());
    }
    parts
}

/// Dimmed value wrapped onto continuation lines (indented under the value
/// column) instead of being cut off — for paths / long commands.
fn kv_wrap(key: &str, text: &str, w: usize) -> Vec<String> {
    let indent = " ".repeat(4 + w);
    let parts = wrap_hard(text, term_width().saturating_sub(4 + w));
    parts
        .iter()
        .enumerate()
        .map(|(i, part)| if i == 0 { kv(key, &color::dim(part), w) } else { format!("{indent}{}", color::dim(part)) })
        .collect()
}

fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

fn short_id(id: &str) -> String {
    id.replace("@claude-plugins-official", "@official")
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in words {
        let candidate = if cur.is_empty() { word.to_string() } else { format!("{cur} {word}") };
        if candidate.chars().count() > width && !cur.is_empty() {
            lines.push(cur);
            cur = word.to_string();
        } else {
            cur = candidate;
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() { vec!["—".to_string()] } else { lines }
}

fn ms_to_relative_age(ms: u128) -> String {
    if ms == 0 {
        return "never".to_string();
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis();
    let secs = now.saturating_sub(ms) as f64 / 1000.0;
    if secs < 60.0 {
        "just now".to_string()
    } else if secs < 3600.0 {
        format!("{}m ago", (secs / 60.0) as u64)
    } else if secs < 86400.0 {
        format!("{}h ago", (secs / 3600.0) as u64)
    } else {
        format!("{}d ago", (secs / 86400.0) as u64)
    }
}

/// Overview screen: account, activity/storage, then a pickable list of
/// plugins/skills/MCP servers. Row values are "plugin:i" / "skill:i" / "mcp:i".
pub fn profile_overview(snap: &Inspection, name: &str, profiles: &[Profile], mark: i64) -> (Vec<String>, Vec<ScrollRow>) {
    let stats: Vec<layout::Stat> = profiles.iter().map(|p| layout::account_stat(&p.name)).collect();
    let mut header = layout::account_lines(profiles, mark, &stats);
    header.push(format!("{} {}  {}", color::orange(ui::glyph::back()), color::orange(name), color::dim("— details")));
    header.push(String::new());

    let mut rows: Vec<ScrollRow> = Vec::new();
    let acc = &snap.account;
    let act = &snap.activity;

    rows.push(ScrollRow::line(format!("  {}", color::orange_bold("Account"))));
    rows.push(ScrollRow::line(kv("email", &format!("{}{}", acc.email, if !acc.name.is_empty() { color::dim(&format!("  ({})", acc.name)) } else { String::new() }), 14)));
    rows.push(ScrollRow::line(kv("plan", &acc.plan, 14)));
    rows.push(ScrollRow::line(kv("org", &acc.org, 14)));
    rows.push(ScrollRow::line(kv("rate tier", &acc.rate_tier, 14)));
    rows.push(ScrollRow::line(kv(
        "created",
        &format!("{}{}", acc.created, if acc.sub_created != "—" { color::dim(&format!("  · sub {}", acc.sub_created)) } else { String::new() }),
        14,
    )));
    rows.push(ScrollRow::line(kv("token", &format!("expires {}", acc.token_expiry), 14)));
    rows.push(ScrollRow::line(String::new()));

    rows.push(ScrollRow::line(format!("  {}", color::orange_bold("Activity"))));
    rows.push(ScrollRow::line(kv(
        "startups",
        &format!("{}{}", act.startups, if act.first_start != "—" { color::dim(&format!("  · first {}", act.first_start)) } else { String::new() }),
        14,
    )));
    rows.push(ScrollRow::line(kv("projects", &act.projects.to_string(), 14)));
    rows.push(ScrollRow::line(kv(
        "sessions",
        &format!("{}  {}", act.session_count, color::dim(&format!("{} · last {}", inspect::human_bytes(act.session_bytes as f64), ms_to_relative_age(act.last_active_ms)))),
        14,
    )));
    rows.push(ScrollRow::line(kv("history", &inspect::human_bytes(act.history_bytes as f64), 14)));
    rows.push(ScrollRow::line(kv("disk total", &inspect::human_bytes(act.total_bytes as f64), 14)));
    rows.push(ScrollRow::line(String::new()));

    rows.push(ScrollRow::line(format!("  {} {}", color::orange_bold("Plugins"), color::dim(&format!("({})", snap.plugins.len())))));
    if snap.plugins.is_empty() {
        rows.push(ScrollRow::line(format!("    {}", color::dim("none"))));
    } else {
        for (i, p) in snap.plugins.iter().enumerate() {
            let label = format!("{}{}", short_id(&p.id), if p.enabled { "" } else { " (off)" });
            let tok = if p.md_bytes > 0 { inspect::est_tokens(p.md_bytes) } else { String::new() };
            let text = format!("{}{}", pad_to(&label, 32), color::dim(&format!("{}{tok}", pad_to(&inspect::human_bytes(p.bytes as f64), 9))));
            rows.push(ScrollRow::pick(format!("  {text}"), format!("plugin:{i}")));
        }
    }
    rows.push(ScrollRow::line(String::new()));

    rows.push(ScrollRow::line(format!("  {} {}", color::orange_bold("Skills"), color::dim(&format!("({})", snap.skills.len())))));
    if snap.skills.is_empty() {
        rows.push(ScrollRow::line(format!("    {}", color::dim("none"))));
    } else {
        for (i, sk) in snap.skills.iter().enumerate() {
            let tok = inspect::est_tokens(if sk.md_bytes > 0 { sk.md_bytes } else { sk.bytes });
            let text = format!("{}{}", pad_to(&sk.name, 32), color::dim(&format!("{}{tok}", pad_to(&inspect::human_bytes(sk.bytes as f64), 9))));
            rows.push(ScrollRow::pick(format!("  {text}"), format!("skill:{i}")));
        }
    }
    rows.push(ScrollRow::line(String::new()));

    rows.push(ScrollRow::line(format!("  {} {}", color::orange_bold("MCP servers"), color::dim(&format!("({})", snap.mcps.len())))));
    if snap.mcps.is_empty() {
        rows.push(ScrollRow::line(format!("    {}", color::dim("none"))));
    } else {
        for (i, m) in snap.mcps.iter().enumerate() {
            let text = format!("{}{}", pad_to(&m.name, 24), color::dim(&m.kind));
            rows.push(ScrollRow::pick(format!("  {text}"), format!("mcp:{i}")));
        }
    }
    rows.push(ScrollRow::line(String::new()));

    (header, rows)
}

/// Per-item detail screen. `pick` is "kind:index" from profile_overview rows.
pub fn item_detail(snap: &Inspection, pick: &str, profiles: &[Profile], mark: i64) -> (Vec<String>, Vec<ScrollRow>) {
    let stats: Vec<layout::Stat> = profiles.iter().map(|p| layout::account_stat(&p.name)).collect();
    let mut header = layout::account_lines(profiles, mark, &stats);
    let mut rows: Vec<ScrollRow> = Vec::new();
    let (kind, idx_raw) = pick.split_once(':').unwrap_or((pick, "0"));
    let idx: usize = idx_raw.parse().unwrap_or(0);

    match kind {
        "plugin" => {
            let p = &snap.plugins[idx];
            header.push(format!("{} {}  {}", color::orange(ui::glyph::back()), color::orange(&clip_mid(&p.id, term_width().saturating_sub(12))), color::dim("— plugin")));
            header.push(String::new());
            rows.push(ScrollRow::line(kv("marketplace", &p.marketplace, 14)));
            rows.push(ScrollRow::line(kv("version", &p.version, 14)));
            rows.push(ScrollRow::line(kv("enabled", &if p.enabled { color::green("yes") } else { color::dim("no") }, 14)));
            rows.push(ScrollRow::line(kv("scope", &p.scope, 14)));
            rows.push(ScrollRow::line(kv("installed", &p.installed_at, 14)));
            rows.push(ScrollRow::line(kv("updated", &p.last_updated, 14)));
            rows.push(ScrollRow::line(kv("commit", &p.sha, 14)));
            let size_val = if p.bytes > 0 {
                format!("{}  {}", inspect::human_bytes(p.bytes as f64), color::dim(&plural(p.files as usize, "file")))
            } else {
                color::dim("not on disk")
            };
            rows.push(ScrollRow::line(kv("size", &size_val, 14)));
            if p.md_bytes > 0 {
                rows.push(ScrollRow::line(kv(
                    "instructions",
                    &format!("{} of markdown  {}", inspect::human_bytes(p.md_bytes as f64), color::dim(&format!("{} if all loaded", inspect::est_tokens(p.md_bytes)))),
                    14,
                )));
            }
            if let Some(c) = &p.contents {
                rows.push(ScrollRow::line(kv(
                    "contents",
                    &format!("{} · {} · {}", plural(c.skills, "skill"), plural(c.commands, "command"), plural(c.agents, "agent")),
                    14,
                )));
            }
            rows.push(ScrollRow::line(String::new()));
            let home = dirs::home_dir().map(|h| h.to_string_lossy().to_string()).unwrap_or_default();
            let display_path = if !home.is_empty() { p.path.replacen(&home, "~", 1) } else { p.path.clone() };
            for l in kv_wrap("path", &display_path, 14) {
                rows.push(ScrollRow::line(l));
            }
        }
        "skill" => {
            let sk = &snap.skills[idx];
            header.push(format!("{} {}  {}", color::orange(ui::glyph::back()), color::orange(&sk.name), color::dim("— skill")));
            header.push(String::new());
            rows.push(ScrollRow::line(kv("size", &format!("{}  {}", inspect::human_bytes(sk.bytes as f64), color::dim(&plural(sk.file_count as usize, "file"))), 14)));
            let tok = inspect::est_tokens(if sk.md_bytes > 0 { sk.md_bytes } else { sk.bytes });
            rows.push(ScrollRow::line(kv("tokens", &format!("{tok}  {}", color::dim("(SKILL.md loads on trigger; the rest on demand)")), 14)));
            let files = if sk.files.is_empty() { "—".to_string() } else { sk.files.join(", ") };
            rows.push(ScrollRow::line(kv("files", &files, 14)));
            rows.push(ScrollRow::line(String::new()));
            rows.push(ScrollRow::line(format!("  {}", color::dim("description"))));
            let desc = if sk.description.is_empty() { "—" } else { &sk.description };
            for chunk in wrap_text(desc, 72) {
                rows.push(ScrollRow::line(format!("    {chunk}")));
            }
        }
        _ => {
            let m = &snap.mcps[idx];
            header.push(format!("{} {}  {}", color::orange(ui::glyph::back()), color::orange(&m.name), color::dim("— MCP server")));
            header.push(String::new());
            rows.push(ScrollRow::line(kv("type", &m.kind, 14)));
            for l in kv_wrap("command", &m.command, 14) {
                rows.push(ScrollRow::line(l));
            }
            for l in kv_wrap("url", &m.url, 14) {
                rows.push(ScrollRow::line(l));
            }
            let env_val = if m.env_keys.is_empty() { "—".to_string() } else { format!("{}  {}", m.env_keys.join(", "), color::dim("(values hidden)")) };
            rows.push(ScrollRow::line(kv("env", &env_val, 14)));
            rows.push(ScrollRow::line(String::new()));
            rows.push(ScrollRow::line(format!("  {}", color::dim("Token cost is decided at runtime by the tool definitions this"))));
            rows.push(ScrollRow::line(format!("  {}", color::dim("server returns — it cannot be measured from disk."))));
        }
    }
    (header, rows)
}
