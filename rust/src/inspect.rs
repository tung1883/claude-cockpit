// Read-only inspection of a profile: account identity, activity/storage, and
// per-item detail for plugins, skills, and MCP servers. Token figures are
// rough estimates (bytes / 4) of the on-disk instruction text, not live tool
// budgets. Ported from src/inspect.js.
use serde_json::Value;
use std::path::Path;

fn read_json(file: &Path) -> Value {
    std::fs::read_to_string(file)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null)
}

fn s(v: &Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

pub fn human_bytes(n: f64) -> String {
    if !n.is_finite() || n <= 0.0 {
        return "0 B".to_string();
    }
    let units = ["B", "KB", "MB", "GB"];
    let mut n = n;
    let mut i = 0;
    while n >= 1024.0 && i < units.len() - 1 {
        n /= 1024.0;
        i += 1;
    }
    if n < 10.0 && i > 0 {
        format!("{n:.1} {}", units[i])
    } else {
        format!("{:.0} {}", n.round(), units[i])
    }
}

pub fn est_tokens(bytes: u64) -> String {
    let t = (bytes as f64 / 4.0).round();
    if t >= 1e6 {
        format!("~{:.1}M tok", t / 1e6)
    } else if t >= 1000.0 {
        if t < 10000.0 {
            format!("~{:.1}k tok", t / 1000.0)
        } else {
            format!("~{:.0}k tok", t / 1000.0)
        }
    } else {
        format!("~{t:.0} tok")
    }
}

fn fmt_date(value: Option<&str>) -> String {
    let Some(value) = value.filter(|v| !v.is_empty()) else { return "—".to_string() };
    // Accept full ISO timestamps; just take the date portion, same as JS's toISOString().slice(0, 10).
    value.get(0..10).filter(|d| d.len() == 10 && d.as_bytes()[4] == b'-').unwrap_or("—").to_string()
}

fn rel_future(epoch_secs: Option<f64>) -> String {
    let Some(epoch_secs) = epoch_secs.filter(|&v| v != 0.0) else { return "—".to_string() };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64();
    let secs = epoch_secs - now;
    if secs <= 0.0 {
        "expired".to_string()
    } else if secs < 3600.0 {
        format!("in {}m", (secs / 60.0).round())
    } else if secs < 86400.0 {
        format!("in {}h", (secs / 3600.0).round())
    } else {
        format!("in {}d", (secs / 86400.0).round())
    }
}

pub struct ScanResult {
    pub bytes: u64,
    pub files: u64,
    pub md_bytes: u64,
    pub newest_ms: u128,
    pub jsonl: u64,
}

/// One recursive walk, everything the screens need in a single pass. Skips
/// node_modules/.git. This is the only expensive call; callers cache it.
pub fn scan_tree(dir: &Path) -> ScanResult {
    let mut r = ScanResult { bytes: 0, files: 0, md_bytes: 0, newest_ms: 0, jsonl: 0 };
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() {
                if name != "node_modules" && name != ".git" {
                    stack.push(path);
                }
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            r.bytes += meta.len();
            r.files += 1;
            if let Ok(modified) = meta.modified() {
                if let Ok(ms) = modified.duration_since(std::time::UNIX_EPOCH) {
                    r.newest_ms = r.newest_ms.max(ms.as_millis());
                }
            }
            let lower = name.to_lowercase();
            if lower.ends_with(".md") || lower.ends_with(".txt") || lower.ends_with(".mdx") {
                r.md_bytes += meta.len();
            }
            if lower.ends_with(".jsonl") {
                r.jsonl += 1;
            }
        }
    }
    r
}

pub struct AccountInfo {
    pub email: String,
    pub name: String,
    pub plan: String,
    pub org: String,
    pub rate_tier: String,
    pub created: String,
    pub sub_created: String,
    pub token_expiry: String,
}

pub fn account_info(profile_dir: &Path) -> AccountInfo {
    let cj = read_json(&profile_dir.join(".claude.json"));
    let cred = read_json(&profile_dir.join(".credentials.json"));
    let acc = cj.get("oauthAccount").cloned().unwrap_or(Value::Null);
    let oauth = cred.get("claudeAiOauth").cloned().unwrap_or(Value::Null);
    let plan_parts: Vec<String> = [s(&oauth, "subscriptionType"), s(&acc, "organizationType"), s(&acc, "organizationRole")]
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect();
    AccountInfo {
        email: nz(s(&acc, "emailAddress")),
        name: {
            let n = s(&acc, "fullName");
            if n.is_empty() { s(&acc, "displayName") } else { n }
        },
        plan: if plan_parts.is_empty() { "—".to_string() } else { plan_parts.join(" · ") },
        org: nz(s(&acc, "organizationName")),
        rate_tier: {
            let t = s(&oauth, "rateLimitTier");
            nz(if t.is_empty() { s(&acc, "organizationRateLimitTier") } else { t })
        },
        created: fmt_date(acc.get("accountCreatedAt").and_then(|v| v.as_str())),
        sub_created: fmt_date(acc.get("subscriptionCreatedAt").and_then(|v| v.as_str())),
        token_expiry: rel_future(oauth.get("expiresAt").and_then(|v| v.as_f64()).map(|ms| ms / 1000.0)),
    }
}

fn nz(s: String) -> String {
    if s.is_empty() { "—".to_string() } else { s }
}

pub struct ActivityInfo {
    pub startups: u64,
    pub first_start: String,
    pub projects: usize,
    pub session_count: u64,
    pub session_bytes: u64,
    pub last_active_ms: u128,
    pub history_bytes: u64,
    pub total_bytes: u64,
}

pub fn activity_info(profile_dir: &Path, extra_bytes: u64) -> ActivityInfo {
    let cj = read_json(&profile_dir.join(".claude.json"));
    let sessions = scan_tree(&profile_dir.join("projects"));
    let history_bytes = std::fs::metadata(profile_dir.join("history.jsonl")).map(|m| m.len()).unwrap_or(0);
    ActivityInfo {
        startups: cj.get("numStartups").and_then(|v| v.as_u64()).unwrap_or(0),
        first_start: fmt_date(cj.get("firstStartTime").and_then(|v| v.as_str())),
        projects: cj.get("projects").and_then(|v| v.as_object()).map(|o| o.len()).unwrap_or(0),
        session_count: sessions.jsonl,
        session_bytes: sessions.bytes,
        last_active_ms: sessions.newest_ms,
        history_bytes,
        total_bytes: sessions.bytes + history_bytes + extra_bytes,
    }
}

fn count_kind(dir: &Path, kind: &str) -> usize {
    std::fs::read_dir(dir.join(kind))
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().is_dir() || e.file_name().to_string_lossy().ends_with(".md"))
                .count()
        })
        .unwrap_or(0)
}

pub struct PluginContents {
    pub skills: usize,
    pub commands: usize,
    pub agents: usize,
}

pub struct PluginInfo {
    pub id: String,
    pub marketplace: String,
    pub version: String,
    pub scope: String,
    pub installed_at: String,
    pub last_updated: String,
    pub sha: String,
    pub path: String,
    pub bytes: u64,
    pub files: u64,
    pub md_bytes: u64,
    pub contents: Option<PluginContents>,
    pub enabled: bool,
}

pub fn plugin_info(profile_dir: &Path) -> Vec<PluginInfo> {
    let installed = read_json(&profile_dir.join("plugins").join("installed_plugins.json"));
    let plugins = installed.get("plugins").and_then(|v| v.as_object()).cloned().unwrap_or_default();
    let settings = read_json(&profile_dir.join("settings.json"));
    let enabled_map = settings.get("enabledPlugins").and_then(|v| v.as_object()).cloned().unwrap_or_default();

    let mut ids: Vec<String> = plugins.keys().cloned().collect();
    ids.sort();
    ids.into_iter()
        .map(|id| {
            let meta = plugins.get(&id).and_then(|v| v.as_array()).and_then(|a| a.first()).cloned().unwrap_or(Value::Null);
            let install_path = s(&meta, "installPath");
            let (bytes, files, md_bytes, contents) = if !install_path.is_empty() && Path::new(&install_path).exists() {
                let size = scan_tree(Path::new(&install_path));
                (
                    size.bytes,
                    size.files,
                    size.md_bytes,
                    Some(PluginContents {
                        skills: count_kind(Path::new(&install_path), "skills"),
                        commands: count_kind(Path::new(&install_path), "commands"),
                        agents: count_kind(Path::new(&install_path), "agents"),
                    }),
                )
            } else {
                (0, 0, 0, None)
            };
            let sha = s(&meta, "gitCommitSha");
            PluginInfo {
                marketplace: id.split('@').nth(1).unwrap_or("—").to_string(),
                version: nz(s(&meta, "version")),
                scope: nz(s(&meta, "scope")),
                installed_at: fmt_date(meta.get("installedAt").and_then(|v| v.as_str())),
                last_updated: fmt_date(meta.get("lastUpdated").and_then(|v| v.as_str())),
                sha: if sha.is_empty() { "—".to_string() } else { sha.chars().take(12).collect() },
                path: if install_path.is_empty() { "—".to_string() } else { install_path },
                bytes,
                files,
                md_bytes,
                contents,
                enabled: enabled_map.get(&id).and_then(|v| v.as_bool()).unwrap_or(true),
                id,
            }
        })
        .collect()
}

pub struct SkillInfo {
    pub name: String,
    pub description: String,
    pub files: Vec<String>,
    pub bytes: u64,
    pub md_bytes: u64,
    pub file_count: u64,
}

pub fn skill_info(profile_dir: &Path) -> Vec<SkillInfo> {
    let root = profile_dir.join("skills");
    let Ok(entries) = std::fs::read_dir(&root) else { return vec![] };
    let mut names: Vec<String> =
        entries.flatten().filter(|e| e.path().is_dir()).map(|e| e.file_name().to_string_lossy().to_string()).collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let dir = root.join(&name);
            let size = scan_tree(&dir);
            let files: Vec<String> = std::fs::read_dir(&dir)
                .map(|e| e.flatten().map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| !n.starts_with('.')).collect())
                .unwrap_or_default();
            let description = std::fs::read_to_string(dir.join("SKILL.md"))
                .ok()
                .and_then(|md| {
                    md.lines().find_map(|line| line.strip_prefix("description:").map(|d| d.trim().to_string()))
                })
                .unwrap_or_default();
            SkillInfo { name, description, files, bytes: size.bytes, md_bytes: size.md_bytes, file_count: size.files }
        })
        .collect()
}

pub struct McpInfo {
    pub name: String,
    pub kind: String,
    pub command: String,
    pub url: String,
    pub env_keys: Vec<String>,
}

pub fn mcp_info(profile_dir: &Path) -> Vec<McpInfo> {
    let cj = read_json(&profile_dir.join(".claude.json"));
    let settings = read_json(&profile_dir.join("settings.json"));
    let mut merged = serde_json::Map::new();
    if let Some(m) = cj.get("mcpServers").and_then(|v| v.as_object()) {
        merged.extend(m.clone());
    }
    if let Some(m) = settings.get("mcpServers").and_then(|v| v.as_object()) {
        merged.extend(m.clone());
    }
    let mut names: Vec<String> = merged.keys().cloned().collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let server = merged.get(&name).cloned().unwrap_or(Value::Null);
            let url = s(&server, "url");
            let kind = {
                let t = s(&server, "type");
                if !t.is_empty() { t } else if !url.is_empty() { "http".to_string() } else { "stdio".to_string() }
            };
            let mut parts = vec![s(&server, "command")];
            if let Some(args) = server.get("args").and_then(|v| v.as_array()) {
                parts.extend(args.iter().filter_map(|a| a.as_str()).map(String::from));
            }
            let command = parts.into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" ");
            let env_keys: Vec<String> =
                server.get("env").and_then(|v| v.as_object()).map(|o| o.keys().cloned().collect()).unwrap_or_default();
            McpInfo { name, kind, command: nz(command), url: nz(url), env_keys }
        })
        .collect()
}

pub struct Inspection {
    pub account: AccountInfo,
    pub activity: ActivityInfo,
    pub plugins: Vec<PluginInfo>,
    pub skills: Vec<SkillInfo>,
    pub mcps: Vec<McpInfo>,
}

/// Everything the detail screens need, gathered in one pass. Callers run
/// this ONCE when the detail view opens and reuse the result.
pub fn build_inspection(profile_dir: &Path) -> Inspection {
    let plugins = plugin_info(profile_dir);
    let skills = skill_info(profile_dir);
    let mcps = mcp_info(profile_dir);
    let extra_bytes: u64 = plugins.iter().map(|p| p.bytes).sum::<u64>() + skills.iter().map(|s| s.bytes).sum::<u64>();
    Inspection { account: account_info(profile_dir), activity: activity_info(profile_dir, extra_bytes), plugins, skills, mcps }
}

