//! The Claude Code settings catalog: parsed from the official "All settings"
//! reference page, shipped as a snapshot (`settings_catalog.json`) and refreshed
//! into a cache file in the background so new Claude Code releases show up
//! without a cockpit update.

use crate::paths;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

pub const REFERENCE_URL: &str = "https://code.claude.com/docs/en/settings-reference.md";

/// One setting. Short field names keep the snapshot small.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct CatEntry {
    pub k: String,
    /// bool | enum | int | num | str | json
    pub t: String,
    pub g: String,
    pub desc: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub e: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub d: Option<Value>,
}

/// Bump when the parser changes what it produces, so an old cache is not used.
const CACHE_V: u32 = 2;

#[derive(Serialize, Deserialize)]
struct Cache {
    #[serde(default)]
    v: u32,
    fetched_ms: u64,
    items: Vec<CatEntry>,
}

fn strip_md(s: &str) -> String {
    use std::sync::OnceLock;
    static LINK: OnceLock<Regex> = OnceLock::new();
    let link = LINK.get_or_init(|| Regex::new(r"\[([^\]]*)\]\([^)]*\)").unwrap());
    link.replace_all(s, "$1").replace(['`', '*'], "").replace("\\|", "|").trim().to_string()
}

fn first_tick(s: &str) -> Option<&str> {
    let a = s.find('`')?;
    let rest = &s[a + 1..];
    let b = rest.find('`')?;
    Some(&rest[..b])
}

fn unquote(s: &str) -> String {
    s.trim().trim_matches('"').to_string()
}

/// Many on/off keys give their default as prose ("unset, so thinking is on").
/// Reads the value the key effectively has when unset, only for clear cases:
/// "disable…/skip…/block…/deny…/isolate…" keys are off when unset; others say
/// "is on" / "is off", or that sessions sync, or that Claude asks first.
fn infer_bool_default(key: &str, line: &str) -> Option<Value> {
    let last = key.rsplit('.').next().unwrap_or(key);
    if ["disable", "skip", "block", "deny", "isolate"].iter().any(|p| last.starts_with(p)) {
        return Some(Value::Bool(false));
    }
    let t = line.to_lowercase();
    if t.contains("unless") {
        return None; // conditional ("on unless you are on the Pro plan")
    }
    if t.contains(" is on") || t.contains(" are on") || t.contains(" sync ") {
        Some(Value::Bool(true))
    } else if t.contains(" is off") || t.contains("asks you") || t.contains("prompts you") {
        Some(Value::Bool(false))
    } else {
        None
    }
}

#[derive(Default, Clone)]
struct Section {
    type_line: String,
    bullets: Vec<String>,
    default_line: String,
}

/// Parses the reference page: the index table gives key, description, topic and
/// where the key may live; each `### \`key\`` section gives type, allowed
/// values and default. Managed-only keys and `~/.claude.json` keys are left out
/// (they do not belong in an account's `settings.json`).
pub fn parse_reference(md: &str) -> Vec<CatEntry> {
    struct Row {
        key: String,
        desc: String,
        topic: String,
    }
    let mut rows: Vec<Row> = Vec::new();
    let mut sections: std::collections::HashMap<String, Section> = std::collections::HashMap::new();
    let mut cur: Option<String> = None;
    let mut in_bullets = false;

    for line in md.lines() {
        if let Some(rest) = line.strip_prefix("| [`") {
            // | [`key`](#anchor) | description | topic | scope |
            let Some(end) = rest.find("`]") else { continue };
            let key = rest[..end].to_string();
            let cols: Vec<&str> = line.trim().trim_matches('|').split(" | ").collect();
            if cols.len() < 4 {
                continue;
            }
            let scope = cols[cols.len() - 1].trim();
            if scope == "Managed" || scope.contains("Global config") {
                continue;
            }
            let topic = cols[cols.len() - 2].trim().to_string();
            let desc = strip_md(&cols[1..cols.len() - 2].join(" | "));
            rows.push(Row { key, desc, topic });
            continue;
        }
        if let Some(h) = line.strip_prefix("### `") {
            cur = h.strip_suffix('`').map(|k| k.to_string());
            in_bullets = false;
            continue;
        }
        if line.starts_with("## ") || line.starts_with("### ") || line.starts_with("#### ") {
            cur = None;
            in_bullets = false;
            continue;
        }
        let Some(key) = &cur else { continue };
        if let Some(t) = line.strip_prefix("* **Type**: ") {
            sections.entry(key.clone()).or_default().type_line = t.to_string();
            in_bullets = true;
        } else if let Some(d) = line.strip_prefix("* **Default**: ") {
            sections.entry(key.clone()).or_default().default_line = d.to_string();
            in_bullets = false;
        } else if in_bullets && line.starts_with("  * ") {
            sections.entry(key.clone()).or_default().bullets.push(line[4..].to_string());
        } else if !line.starts_with("  ") {
            in_bullets = false;
        }
    }

    rows.into_iter()
        .map(|r| {
            let sec = sections.get(&r.key).cloned().unwrap_or_default();
            let ty = sec.type_line.to_lowercase();
            let ticks: Vec<String> = {
                let mut v = Vec::new();
                let mut rest = sec.type_line.as_str();
                while let Some(s) = first_tick(rest) {
                    v.push(unquote(s));
                    let cut = rest.find('`').unwrap() + 1 + s.len() + 1;
                    rest = &rest[cut.min(rest.len())..];
                }
                v
            };
            let (kind, opts) = if ty.starts_with("boolean") {
                ("bool", Vec::new())
            } else if ty.contains("one of") {
                let o: Vec<String> = sec.bullets.iter().filter_map(|b| first_tick(b)).map(unquote).filter(|o| !o.contains(['<', '%'])).collect();
                if o.is_empty() { ("str", Vec::new()) } else { ("enum", o) }
            } else if ty.starts_with("the string") && !ticks.is_empty() {
                ("enum", vec![ticks[0].clone()])
            } else if ty.starts_with("string") {
                ("str", Vec::new())
            } else if ty.starts_with("number") || ty.starts_with("integer") || ty.starts_with("a whole number") {
                (if ty.contains("whole") || ty.contains("integer") { "int" } else { "num" }, Vec::new())
            } else {
                ("json", Vec::new())
            };
            let d = sec.default_line.strip_prefix('`').and_then(|_| first_tick(&sec.default_line)).and_then(|t| serde_json::from_str::<Value>(t).ok());
            let d = d.or_else(|| if kind == "bool" { infer_bool_default(&r.key, &sec.default_line) } else { None });
            let g = if r.desc.to_lowercase().starts_with("deprecated") { "Deprecated".to_string() } else { r.topic };
            CatEntry { k: r.key, t: kind.to_string(), g, desc: r.desc, e: opts, d }
        })
        .collect()
}

// ---------- cache + refresh ----------

fn cache_path() -> PathBuf {
    paths::root().join("settings-catalog.json")
}

fn attempt_path() -> PathBuf {
    paths::root().join("settings-catalog.attempt")
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// The refreshed catalog and its age in days, if a usable cache exists.
pub fn load_cache() -> Option<(Vec<CatEntry>, u64)> {
    let c: Cache = serde_json::from_str(&std::fs::read_to_string(cache_path()).ok()?).ok()?;
    if c.v != CACHE_V || c.items.len() < 50 {
        return None;
    }
    Some((c.items, now_ms().saturating_sub(c.fetched_ms) / 86_400_000))
}

fn fetch() -> Result<String, String> {
    let mut cmd = std::process::Command::new("curl");
    cmd.args(["-fsSL", "--max-time", "25", REFERENCE_URL]).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // no console window
    }
    let out = cmd.output().map_err(|e| format!("curl not available: {e}"))?;
    if !out.status.success() {
        return Err("could not reach code.claude.com".to_string());
    }
    String::from_utf8(out.stdout).map_err(|_| "unexpected response".to_string())
}

/// Downloads and parses the reference, then replaces the cache. Returns the
/// number of settings found. A page that does not parse to a plausible catalog
/// is rejected, so a layout change upstream cannot wipe a good cache.
pub fn refresh() -> Result<usize, String> {
    let _ = std::fs::write(attempt_path(), "");
    let items = parse_reference(&fetch()?);
    if items.len() < 80 || !items.iter().any(|i| i.k == "model") {
        return Err("the settings page changed shape — keeping the current catalog".to_string());
    }
    let n = items.len();
    let json = serde_json::to_string(&Cache { v: CACHE_V, fetched_ms: now_ms(), items }).map_err(|e| e.to_string())?;
    let tmp = cache_path().with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, cache_path()).map_err(|e| e.to_string())?;
    Ok(n)
}

/// Refreshes in the background when the cache is missing or a week old (and no
/// attempt was made in the last hour). Never blocks, never reports.
pub fn refresh_in_background_if_stale() {
    let stale = load_cache().map(|(_, days)| days >= 7).unwrap_or(true);
    let tried_recently = std::fs::metadata(attempt_path()).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|e| e.as_secs() < 3600);
    if stale && !tried_recently {
        std::thread::spawn(|| {
            let _ = refresh();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
## Settings index

| Key | Description | Topic | Scope |
| :- | :- | :- | :- |
| [`idleCompaction`](#idlecompaction) | Stop [compacting](https://x.y/z) a long conversation | Memory and context | Any file |
| [`effortLevel`](#effortlevel) | Set a default effort | Model and responses | Any file |
| [`policyHelper.path`](#policyhelperpath) | Admin only | Enterprise | Managed |
| [`cleanupPeriodDays`](#cleanupperioddays) | Days to keep | Memory and context | Any file |
| [`includeCoAuthoredBy`](#includecoauthoredby) | Deprecated; use `attribution` | Git and attribution | Any file |
| [`respectGitignore`](#respectgitignore) | Picker setting | Interface | Global config |

## Memory and context

### `idleCompaction`

Prose.

* **Scope**: [`Any file`](#scopes)
* **Type**: Boolean
  * `true`: compacts
  * `false`: does not
* **Default**: `true`

### `effortLevel`

* **Type**: string, one of:
  * `"low"`: least
  * `"high"`: most
* **Default**: unset

### `cleanupPeriodDays`

* **Type**: number of days, a whole number, minimum `1`
* **Default**: `30`
"#;

    #[test]
    fn parses_index_types_enums_defaults_and_skips_managed_and_global() {
        let c = parse_reference(SAMPLE);
        let get = |k: &str| c.iter().find(|e| e.k == k);
        assert_eq!(c.len(), 4, "managed and global-config rows are left out");
        let idle = get("idleCompaction").unwrap();
        assert_eq!((idle.t.as_str(), idle.d.clone(), idle.g.as_str()), ("bool", Some(Value::Bool(true)), "Memory and context"));
        assert_eq!(idle.desc, "Stop compacting a long conversation");
        let eff = get("effortLevel").unwrap();
        assert_eq!((eff.t.as_str(), eff.e.clone(), eff.d.clone()), ("enum", vec!["low".to_string(), "high".to_string()], None));
        let days = get("cleanupPeriodDays").unwrap();
        assert_eq!((days.t.as_str(), days.d.clone()), ("int", Some(serde_json::json!(30))));
        assert_eq!(get("includeCoAuthoredBy").unwrap().g, "Deprecated");
    }

    #[test]
    fn bool_defaults_given_as_prose_are_read_when_clear() {
        let b = |k: &str, l: &str| infer_bool_default(k, l);
        assert_eq!(b("alwaysThinkingEnabled", "unset, so thinking is on for models that support it"), Some(Value::Bool(true)));
        assert_eq!(b("axScreenReader", "unset, so screen-reader mode is off"), Some(Value::Bool(false)));
        assert_eq!(b("disableAllHooks", "unset, so hooks run"), Some(Value::Bool(false)));
        assert_eq!(b("permissions.denyInsteadOfPrompting", "unset, so a call prompts you"), Some(Value::Bool(false)));
        assert_eq!(b("syncClaudeAiPlugins", "unset, so sessions signed in sync your plugins"), Some(Value::Bool(true)));
        assert_eq!(b("voiceEnabled", "unset"), None);
        assert_eq!(b("enableWorkflows", "unset, so workflows are on unless you're on the Pro plan"), None);
    }

    /// Regenerates the shipped snapshot from a downloaded copy of the page:
    /// `CPIT_REF_MD=ref.md cargo test regen_catalog_snapshot -- --ignored`
    #[test]
    #[ignore]
    fn regen_catalog_snapshot() {
        let md = std::fs::read_to_string(std::env::var("CPIT_REF_MD").expect("CPIT_REF_MD=path to settings-reference.md")).unwrap();
        let items = parse_reference(&md);
        assert!(items.len() > 80 && items.iter().any(|i| i.k == "idleCompaction"));
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/settings_catalog.json");
        std::fs::write(path, serde_json::to_string(&items).unwrap()).unwrap();
        println!("wrote {} settings", items.len());
    }
}
