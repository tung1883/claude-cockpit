// The Claude Code statusline renderer. Claude Code pipes a JSON payload on
// stdin and expects a two-line string back. Ported from src/statusline.js —
// see that file's comments for the design rationale (why fields go where).
use chrono::{Offset, TimeZone};
use serde_json::Value;
use std::process::{Command, Stdio};

fn number(values: &[Option<f64>]) -> Option<f64> {
    values.iter().flatten().find(|v| v.is_finite()).copied()
}

fn git(args: &[&str], cwd: &str) -> String {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

struct GitInfo {
    project: String,
    branch: String,
    changes: u32,
    added: u32,
    removed: u32,
}

fn git_stats(cwd: &str) -> GitInfo {
    let root = git(&["rev-parse", "--show-toplevel"], cwd);
    if root.is_empty() {
        let project = std::path::Path::new(cwd).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        return GitInfo { project, branch: String::new(), changes: 0, added: 0, removed: 0 };
    }
    let status = git(&["status", "--porcelain"], cwd);
    let (mut added, mut removed) = (0u32, 0u32);
    for line in git(&["diff", "--numstat", "HEAD"], cwd).lines() {
        let mut parts = line.split_whitespace();
        if let (Some(a), Some(d)) = (parts.next(), parts.next()) {
            if let Ok(n) = a.parse::<u32>() {
                added += n;
            }
            if let Ok(n) = d.parse::<u32>() {
                removed += n;
            }
        }
    }
    let project = std::path::Path::new(&root).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let changes = if status.is_empty() { 0 } else { status.lines().count() as u32 };
    GitInfo { project, branch: git(&["branch", "--show-current"], cwd), changes, added, removed }
}

fn format_age(ms: f64) -> String {
    if !ms.is_finite() || ms < 0.0 {
        return String::new();
    }
    let days = (ms / 86_400_000.0).floor();
    if days >= 1.0 {
        return format!("{days:.0}d");
    }
    let hours = (ms / 3_600_000.0).floor();
    if hours >= 1.0 {
        return format!("{hours:.0}h");
    }
    format!("{:.0}m", (ms / 60_000.0).floor().max(1.0))
}

fn duration_since(start: Option<&str>) -> String {
    let Some(start) = start.filter(|s| !s.is_empty()) else { return String::new() };
    let ms = chrono::DateTime::parse_from_rfc3339(start).map(|d| d.timestamp_millis() as f64).ok().or_else(|| {
        start.parse::<f64>().ok().map(|n| if start.len() < 13 { n * 1000.0 } else { n })
    });
    match ms {
        Some(ms) => format_age(now_ms() - ms),
        None => String::new(),
    }
}

fn now_ms() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as f64
}

fn pct(value: Option<f64>) -> String {
    match value {
        Some(n) => format!("{:.0}%", n.round()),
        None => "--".to_string(),
    }
}

fn compact_tokens(value: Option<f64>) -> String {
    match value {
        None => "--".to_string(),
        Some(n) if n >= 1_000_000.0 => format!("{:.1}m", n / 1_000_000.0),
        Some(n) if n >= 1_000.0 => format!("{:.1}k", n / 1_000.0),
        Some(n) => format!("{:.0}", n.round()),
    }
}

struct Ansi {
    reset: &'static str,
    gray: &'static str,
    cyan: &'static str,
    blue: &'static str,
    green: &'static str,
    yellow: &'static str,
    red: &'static str,
    magenta: &'static str,
    orange: &'static str,
}

fn orange_code() -> &'static str {
    let truecolor = matches!(std::env::var("COLORTERM").unwrap_or_default().to_lowercase().as_str(), "truecolor" | "24bit");
    if truecolor { "\x1b[38;2;217;119;87m" } else { "\x1b[38;5;173m" }
}

fn theme() -> Ansi {
    // Orange stays the identity accent (model, project); everything else
    // gets its own hue so the line reads as a small dashboard, not grey.
    match std::env::var("CLAUDE_COCKPIT_THEME").unwrap_or_default().as_str() {
        "ocean" => Ansi { reset: "\x1b[0m", gray: "\x1b[90m", cyan: "\x1b[96m", blue: "\x1b[94m", green: "\x1b[92m", yellow: "\x1b[93m", red: "\x1b[91m", magenta: "\x1b[95m", orange: "\x1b[96m" },
        "dracula" => Ansi { reset: "\x1b[0m", gray: "\x1b[90m", cyan: "\x1b[96m", blue: "\x1b[95m", green: "\x1b[92m", yellow: "\x1b[93m", red: "\x1b[91m", magenta: "\x1b[95m", orange: "\x1b[96m" },
        "nord" => Ansi { reset: "\x1b[0m", gray: "\x1b[37m", cyan: "\x1b[96m", blue: "\x1b[94m", green: "\x1b[92m", yellow: "\x1b[93m", red: "\x1b[91m", magenta: "\x1b[95m", orange: "\x1b[96m" },
        "mono" => Ansi { reset: "\x1b[0m", gray: "\x1b[90m", cyan: "\x1b[37m", blue: "\x1b[37m", green: "\x1b[37m", yellow: "\x1b[97m", red: "\x1b[97m", magenta: "\x1b[97m", orange: "\x1b[37m" },
        _ => Ansi { reset: "\x1b[0m", gray: "\x1b[90m", cyan: "\x1b[96m", blue: "\x1b[94m", green: "\x1b[92m", yellow: "\x1b[93m", red: "\x1b[91m", magenta: "\x1b[95m", orange: orange_code() },
    }
}

fn color(value: &str, code: &str, ansi: &Ansi) -> String {
    if std::env::var("NO_COLOR").is_ok() || value.is_empty() {
        value.to_string()
    } else {
        format!("{code}{value}{}", ansi.reset)
    }
}

fn level<'a>(value: Option<f64>, ansi: &'a Ansi) -> &'a str {
    match value {
        None => ansi.gray,
        Some(n) if n >= 80.0 => ansi.red,
        Some(n) if n >= 50.0 => ansi.yellow,
        _ => ansi.green,
    }
}

fn tz() -> chrono_tz::Tz {
    std::env::var("CLAUDE_COCKPIT_TZ").ok().and_then(|s| s.parse().ok()).unwrap_or(chrono_tz::Asia::Bangkok)
}

/// with_date: the 7-day reset can be days out, so a bare time is ambiguous —
/// prefix the date. The 5-hour reset is always same-day, so it skips this.
fn reset_clock(epoch_secs: Option<f64>, with_date: bool) -> String {
    let Some(secs) = epoch_secs else { return String::new() };
    let dt = tz().timestamp_opt(secs as i64, 0).single();
    let Some(dt) = dt else { return String::new() };
    if with_date {
        // en-GB's Intl.DateTimeFormat (what the JS version uses) abbreviates
        // September as "Sept", unlike every other month's standard 3-letter
        // form — matched here so the two versions render identically.
        let month = if dt.format("%m").to_string() == "09" { "Sept".to_string() } else { dt.format("%b").to_string() };
        format!("{} {month} {}", dt.format("%e").to_string().trim(), dt.format("%H:%M"))
    } else {
        dt.format("%H:%M").to_string()
    }
}

fn format_duration_ms(value: Option<f64>) -> String {
    let Some(n) = value.filter(|&n| n >= 0.0) else { return String::new() };
    let total_minutes = (n / 60_000.0).floor() as i64;
    format!("{:02}:{:02}", total_minutes / 60, total_minutes % 60)
}

fn vis_len(s: &str) -> usize {
    let mut len = 0;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for c2 in chars.by_ref() {
                if c2 == 'm' {
                    break;
                }
            }
        } else {
            len += 1;
        }
    }
    len
}

fn pad_to(s: &str, w: usize) -> String {
    let len = vis_len(s);
    if len >= w { s.to_string() } else { format!("{s}{}", " ".repeat(w - len)) }
}

/// A long session name or branch shouldn't blow out every column's width —
/// cap it before it enters the alignment math.
fn truncate(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() > max {
        let head: String = chars[..max - 1].iter().collect();
        format!("{head}…")
    } else {
        s.to_string()
    }
}

fn get_f64(v: &Value, keys: &[&str]) -> Option<f64> {
    let mut cur = v;
    for k in keys {
        cur = cur.get(k)?;
    }
    cur.as_f64().or_else(|| cur.as_str().and_then(|s| s.parse().ok()))
}

fn get_str<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    let mut cur = v;
    for k in keys {
        cur = cur.get(k)?;
    }
    cur.as_str()
}

fn first_object<'a>(v: &'a Value, keys: &[&str]) -> Value {
    for k in keys {
        if let Some(obj) = v.get(k) {
            if !obj.is_null() {
                return obj.clone();
            }
        }
    }
    Value::Null
}

pub fn render(input: &Value) -> String {
    let ansi = theme();
    let cwd = get_str(input, &["workspace", "current_dir"])
        .or_else(|| get_str(input, &["cwd"]))
        .map(String::from)
        .unwrap_or_else(|| std::env::current_dir().map(|p| p.to_string_lossy().to_string()).unwrap_or_default());
    let git_info = git_stats(&cwd);
    let model = get_str(input, &["model", "display_name"])
        .or_else(|| get_str(input, &["model", "name"]))
        .or_else(|| input.get("model").and_then(|v| v.as_str()))
        .unwrap_or("--")
        .to_string();
    let context = first_object(input, &["context_window", "context"]);
    let cost = input.get("cost").cloned().unwrap_or(Value::Null);
    let rate = first_object(input, &["rate_limits", "rateLimits"]);
    let five = first_object(&rate, &["five_hour", "5h", "fiveHour"]);
    let seven = first_object(&rate, &["seven_day", "7d", "sevenDay"]);
    let tokens = number(&[
        get_f64(input, &["total_tokens"]),
        get_f64(input, &["tokens"]),
        get_f64(&cost, &["total_tokens"]),
        {
            let a = get_f64(&context, &["total_input_tokens"]);
            let b = get_f64(&context, &["total_output_tokens"]);
            match (a, b) { (Some(a), Some(b)) => Some(a + b), _ => None }
        },
        {
            let a = get_f64(input, &["usage", "input_tokens"]);
            let b = get_f64(input, &["usage", "output_tokens"]);
            match (a, b) { (Some(a), Some(b)) => Some(a + b), _ => None }
        },
    ]);
    let session_start = get_str(input, &["session_start_time"])
        .or_else(|| get_str(input, &["session_start_timestamp"]))
        .or_else(|| get_str(input, &["sessionStartTime"]));
    let clock = {
        let now = tz().from_utc_datetime(&chrono::Utc::now().naive_utc());
        let offset_secs = now.offset().fix().local_minus_utc();
        let sign = if offset_secs < 0 { '-' } else { '+' };
        let h = offset_secs.abs() / 3600;
        let m = (offset_secs.abs() % 3600) / 60;
        let gmt = if m == 0 { format!("GMT{sign}{h}") } else { format!("GMT{sign}{h}:{m:02}") };
        format!("{} {gmt}", now.format("%H:%M"))
    };
    let elapsed = {
        let d = format_duration_ms(get_f64(&cost, &["total_duration_ms"]));
        if !d.is_empty() { d } else { duration_since(session_start) }
    };
    let sep = color(" · ", ansi.gray, &ansi);
    let added = number(&[get_f64(&cost, &["total_lines_added"]), Some(git_info.added as f64)]).unwrap_or(0.0) as u32;
    let removed = number(&[get_f64(&cost, &["total_lines_removed"]), Some(git_info.removed as f64)]).unwrap_or(0.0) as u32;
    let five_reset = reset_clock(number(&[get_f64(&five, &["resets_at"]), get_f64(&five, &["reset_at"])]), false);
    let seven_reset = reset_clock(number(&[get_f64(&seven, &["resets_at"]), get_f64(&seven, &["reset_at"])]), true);
    let context_pct = number(&[get_f64(&context, &["used_percentage"]), get_f64(&context, &["usedPercent"]), get_f64(&context, &["percentage"])]);
    let five_pct = number(&[get_f64(&five, &["used_percentage"]), get_f64(&five, &["usedPercent"]), get_f64(&five, &["percentage"])]);
    let seven_pct = number(&[get_f64(&seven, &["used_percentage"]), get_f64(&seven, &["usedPercent"]), get_f64(&seven, &["percentage"])]);
    let cost_val = number(&[get_f64(&cost, &["total_cost_usd"]), get_f64(&cost, &["totalCostUsd"])]).unwrap_or(0.0);
    let cost_text = format!("${cost_val:.2}");
    let meter = |label: &str, value: Option<f64>, label_color: &str| format!("{} {}", color(label, label_color, &ansi), color(&pct(value), level(value, &ansi), &ansi));

    let session_name = get_str(input, &["session_name"]);
    let project_field = match session_name {
        Some(name) => format!("{}{}", color(&git_info.project, ansi.orange, &ansi), color(&format!("@{name}"), ansi.magenta, &ansi)),
        None => color(&git_info.project, ansi.orange, &ansi),
    };
    let branch_field = if !git_info.branch.is_empty() {
        let changes = if git_info.changes > 0 { format!(" {}", color(&format!("({}±)", git_info.changes), ansi.yellow, &ansi)) } else { String::new() };
        format!("{}{changes}", color(&truncate(&git_info.branch, 16), ansi.orange, &ansi))
    } else {
        color("--", ansi.gray, &ansi)
    };
    let cols1 = [
        branch_field,
        format!("{} {}", color(&format!("+{added}"), ansi.green, &ansi), color(&format!("-{removed}"), ansi.red, &ansi)),
        color(&truncate(&model, 16), ansi.orange, &ansi),
        color(&format!("{clock}{}", if elapsed.is_empty() { String::new() } else { format!(" {elapsed}") }), ansi.blue, &ansi),
        project_field,
    ];
    let cols2 = [
        meter("ctx", context_pct, ansi.cyan),
        color(&compact_tokens(tokens), ansi.cyan, &ansi),
        format!("{}{}", meter("5h", five_pct, ansi.blue), if five_reset.is_empty() { String::new() } else { format!(" {}", color(&five_reset, ansi.blue, &ansi)) }),
        format!("{}{}", meter("7d", seven_pct, ansi.magenta), if seven_reset.is_empty() { String::new() } else { format!(" {}", color(&seven_reset, ansi.magenta, &ansi)) }),
        color(&cost_text, ansi.green, &ansi),
    ];
    // Cap each column so one unusually long field can't stretch every other
    // column with it. The two rows have different field counts on purpose;
    // widths just covers whichever is longer at each index.
    let n = cols1.len().max(cols2.len());
    let widths: Vec<usize> = (0..n)
        .map(|i| vis_len(cols1.get(i).map(String::as_str).unwrap_or("")).max(vis_len(cols2.get(i).map(String::as_str).unwrap_or(""))).min(32))
        .collect();
    let align = |cols: &[String]| -> String {
        let last = cols.len() - 1;
        cols.iter().enumerate().map(|(i, c)| if i == last { c.clone() } else { pad_to(c, widths[i]) }).collect::<Vec<_>>().join(&sep)
    };
    format!("{}\n{}", align(&cols1), align(&cols2))
}

/// Pulls just the two rate-limit windows out of the payload, for the quota
/// cache — kept separate from `render`'s own parsing so the display logic
/// above stays untangled from the caching side effect below.
fn extract_quota(input: &Value) -> crate::quota::Quota {
    let rate = first_object(input, &["rate_limits", "rateLimits"]);
    let five = first_object(&rate, &["five_hour", "5h", "fiveHour"]);
    let seven = first_object(&rate, &["seven_day", "7d", "sevenDay"]);
    crate::quota::Quota {
        five_pct: number(&[get_f64(&five, &["used_percentage"]), get_f64(&five, &["usedPercent"]), get_f64(&five, &["percentage"])]),
        seven_pct: number(&[get_f64(&seven, &["used_percentage"]), get_f64(&seven, &["usedPercent"]), get_f64(&seven, &["percentage"])]),
        five_resets_at: number(&[get_f64(&five, &["resets_at"]), get_f64(&five, &["reset_at"])]),
        seven_resets_at: number(&[get_f64(&seven, &["resets_at"]), get_f64(&seven, &["reset_at"])]),
        updated_ms: now_ms(),
    }
}

pub fn main() {
    let mut raw = String::new();
    use std::io::Read;
    let _ = std::io::stdin().read_to_string(&mut raw);
    let input: Value = if raw.trim().is_empty() { Value::Null } else { serde_json::from_str(&raw).unwrap_or(Value::Null) };
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        crate::quota::save(std::path::Path::new(&dir), &extract_quota(&input));
    }
    print!("{}", render(&input));
}
