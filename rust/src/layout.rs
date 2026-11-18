use crate::profiles::Profile;
use crate::session;
use crate::ui::{self, color};

#[derive(Clone)]
pub struct Stat {
    pub count: usize,
    pub age: String,
}

// Nearly every screen starts by asking for every account's stats, and a screen
// change usually follows another within a second or two. A few seconds of
// cache turns those repeated directory scans into one; anything that adds or
// removes a session calls `invalidate_stats`.
static STATS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, Stat)>>> = std::sync::OnceLock::new();
const STATS_TTL: std::time::Duration = std::time::Duration::from_secs(3);

pub fn invalidate_stats() {
    if let Some(m) = STATS.get() {
        if let Ok(mut m) = m.lock() {
            m.clear();
        }
    }
}

pub fn account_stat(name: &str) -> Stat {
    let cache = STATS.get_or_init(Default::default);
    if let Ok(m) = cache.lock() {
        if let Some((at, stat)) = m.get(name) {
            if at.elapsed() < STATS_TTL {
                return stat.clone();
            }
        }
    }
    let (count, newest) = session::session_summary(name);
    let stat = Stat { count, age: session::relative_age(newest) };
    if let Ok(mut m) = cache.lock() {
        m.insert(name.to_string(), (std::time::Instant::now(), stat.clone()));
    }
    stat
}

/// Visible width — ANSI escapes (\x1b[...m) take no columns.
pub fn vis_len(s: &str) -> usize {
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

pub fn pad_to(s: &str, width: usize) -> String {
    let len = vis_len(s);
    if len >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - len))
    }
}

/// The account list — shown at the top of the picker and every sub-screen.
pub fn account_lines(profiles: &[Profile], selected: i64, stats: &[Stat]) -> Vec<String> {
    let mut lines = vec![ui::banner(), String::new()];
    if !profiles.is_empty() {
        lines.push(color::dim(" Each account is an isolated Claude Code login."));
        lines.push(String::new());
    }
    let max_name = profiles.iter().map(|p| p.name.chars().count()).max().unwrap_or(0);
    let name_width = (max_name.max(12) + 2).min(24);
    for (i, p) in profiles.iter().enumerate() {
        let active = i as i64 == selected;
        let marker = if active { color::orange(crate::ui::glyph::pointer()) } else { " ".to_string() };
        let name = if active { color::orange_bold(&p.name) } else { p.name.clone() };
        let st = &stats[i];
        lines.push(format!(
            " {marker} {}{}{}",
            pad_to(&name, name_width),
            color::dim(&pad_to(&format!("{:>3} sessions", st.count), 14)),
            color::dim(&st.age),
        ));
    }
    lines.push(String::new());
    lines.push(ui::rule(None));
    lines
}

/// Rows of key hints laid out on an aligned grid, e.g. `[("↑/↓","move"), ...]`.
pub fn hint_grid(rows: &[&[(&str, &str)]], cell: usize) -> Vec<String> {
    rows.iter()
        .map(|cells| {
            let joined: String = cells
                .iter()
                .map(|(k, v)| pad_to(&format!("{} {v}", color::dim(k)), cell))
                .collect();
            format!(" {}", joined.trim_end())
        })
        .collect()
}
