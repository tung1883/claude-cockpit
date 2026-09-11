'use strict';
// Read-only inspection of a profile: account identity, activity/storage, and
// per-item detail for plugins, skills, and MCP servers. Token figures are rough
// estimates (bytes / 4) of the on-disk instruction text, not live tool budgets.
const fs = require('node:fs');
const path = require('node:path');

function readJson(file) {
  try {
    const raw = fs.readFileSync(file, 'utf8');
    return raw.trim() ? JSON.parse(raw) : {};
  } catch { return {}; }
}

function humanBytes(n) {
  if (!Number.isFinite(n) || n <= 0) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB'];
  let i = 0;
  while (n >= 1024 && i < units.length - 1) { n /= 1024; i++; }
  return `${n < 10 && i ? n.toFixed(1) : Math.round(n)} ${units[i]}`;
}

function estTokens(bytes) {
  const t = Math.round(bytes / 4);
  if (t >= 1e6) return `~${(t / 1e6).toFixed(1)}M tok`;
  if (t >= 1000) return `~${(t / 1000).toFixed(t < 10000 ? 1 : 0)}k tok`;
  return `~${t} tok`;
}

function fmtDate(value) {
  if (!value) return '—';
  const d = new Date(value);
  return Number.isNaN(d.getTime()) ? '—' : d.toISOString().slice(0, 10);
}

function relFuture(ms) {
  if (!ms) return '—';
  const s = (ms - Date.now()) / 1000;
  if (s <= 0) return 'expired';
  if (s < 3600) return `in ${Math.round(s / 60)}m`;
  if (s < 86400) return `in ${Math.round(s / 3600)}h`;
  return `in ${Math.round(s / 86400)}d`;
}

// One recursive walk, everything the screens need in a single pass:
//   { bytes, files, mdBytes, newestMs, jsonl }
// mdBytes counts only .md / .txt — the instruction text a plugin or skill
// feeds the model, which is what the token estimate should reflect.
// Skips node_modules/.git. This is the only expensive call; callers cache it.
function scanTree(dir) {
  let bytes = 0, files = 0, mdBytes = 0, newestMs = 0, jsonl = 0;
  const stack = [dir];
  while (stack.length) {
    const current = stack.pop();
    let entries;
    try { entries = fs.readdirSync(current, { withFileTypes: true }); }
    catch { continue; }
    for (const entry of entries) {
      const full = path.join(current, entry.name);
      if (entry.isDirectory()) {
        if (entry.name !== 'node_modules' && entry.name !== '.git') stack.push(full);
        continue;
      }
      try {
        const st = fs.statSync(full);
        bytes += st.size;
        files++;
        if (st.mtimeMs > newestMs) newestMs = st.mtimeMs;
        if (/\.(md|txt|mdx)$/i.test(entry.name)) mdBytes += st.size;
        if (entry.name.endsWith('.jsonl')) jsonl++;
      } catch { /* skip */ }
    }
  }
  return { bytes, files, mdBytes, newestMs, jsonl };
}
const dirSize = scanTree;

function accountInfo(profileDir) {
  const cj = readJson(path.join(profileDir, '.claude.json'));
  const cred = readJson(path.join(profileDir, '.credentials.json'));
  const acc = cj.oauthAccount || {};
  const oauth = cred.claudeAiOauth || {};
  return {
    email: acc.emailAddress || '—',
    name: acc.fullName || acc.displayName || '',
    plan: [oauth.subscriptionType, acc.organizationType, acc.organizationRole].filter(Boolean).join(' · ') || '—',
    org: acc.organizationName || '—',
    rateTier: oauth.rateLimitTier || acc.organizationRateLimitTier || '—',
    created: fmtDate(acc.accountCreatedAt),
    subCreated: fmtDate(acc.subscriptionCreatedAt),
    tokenExpiry: relFuture(oauth.expiresAt),
    scopes: Array.isArray(oauth.scopes) ? oauth.scopes.join(' ') : '—',
    userId: cj.userID || '—',
  };
}

function activityInfo(profileDir, extraBytes = 0) {
  const cj = readJson(path.join(profileDir, '.claude.json'));
  const sessions = scanTree(path.join(profileDir, 'projects'));
  let history = 0;
  try { history = fs.statSync(path.join(profileDir, 'history.jsonl')).size; } catch { /* none */ }
  return {
    startups: cj.numStartups || 0,
    firstStart: fmtDate(cj.firstStartTime),
    projects: Object.keys(cj.projects || {}).length,
    sessionCount: sessions.jsonl,
    sessionBytes: sessions.bytes,
    lastActive: sessions.newestMs,
    historyBytes: history,
    // Sum of the parts we already measured — avoids a second full-profile walk.
    totalBytes: sessions.bytes + history + extraBytes,
  };
}

function countKind(dir, kind) {
  try {
    return fs.readdirSync(path.join(dir, kind), { withFileTypes: true }).filter(e => e.isDirectory() || e.name.endsWith('.md')).length;
  } catch { return 0; }
}

function pluginInfo(profileDir) {
  const installed = readJson(path.join(profileDir, 'plugins', 'installed_plugins.json')).plugins || {};
  const settings = readJson(path.join(profileDir, 'settings.json'));
  const enabled = settings.enabledPlugins || {};
  return Object.keys(installed).sort().map(id => {
    const meta = (installed[id] || [])[0] || {};
    let size = { bytes: 0, files: 0 };
    let contents = null;
    if (meta.installPath && fs.existsSync(meta.installPath)) {
      size = dirSize(meta.installPath);
      contents = {
        skills: countKind(meta.installPath, 'skills'),
        commands: countKind(meta.installPath, 'commands'),
        agents: countKind(meta.installPath, 'agents'),
      };
    }
    return {
      id,
      marketplace: id.split('@')[1] || '—',
      version: meta.version || '—',
      scope: meta.scope || '—',
      installedAt: fmtDate(meta.installedAt),
      lastUpdated: fmtDate(meta.lastUpdated),
      sha: meta.gitCommitSha ? meta.gitCommitSha.slice(0, 12) : '—',
      path: meta.installPath || '—',
      size,
      contents,
      enabled: enabled[id] !== false,
    };
  });
}

function skillInfo(profileDir) {
  const root = path.join(profileDir, 'skills');
  let names;
  try { names = fs.readdirSync(root, { withFileTypes: true }).filter(e => e.isDirectory()).map(e => e.name).sort(); }
  catch { return []; }
  return names.map(name => {
    const dir = path.join(root, name);
    const size = dirSize(dir);
    let description = '';
    let files = [];
    try { files = fs.readdirSync(dir).filter(f => !f.startsWith('.')); } catch { /* none */ }
    try {
      const md = fs.readFileSync(path.join(dir, 'SKILL.md'), 'utf8');
      const m = md.match(/^description:\s*(.+)$/m);
      if (m) description = m[1].trim();
    } catch { /* no SKILL.md */ }
    return { name, description, files, bytes: size.bytes, mdBytes: size.mdBytes, fileCount: size.files };
  });
}

function mcpInfo(profileDir) {
  const cj = readJson(path.join(profileDir, '.claude.json'));
  const settings = readJson(path.join(profileDir, 'settings.json'));
  const merged = { ...cj.mcpServers, ...settings.mcpServers };
  return Object.keys(merged).sort().map(name => {
    const s = merged[name] || {};
    return {
      name,
      type: s.type || (s.url ? 'http' : 'stdio'),
      command: [s.command, ...(s.args || [])].filter(Boolean).join(' ') || '—',
      url: s.url || '—',
      envKeys: Object.keys(s.env || {}),
    };
  });
}

// Everything the detail screens need, gathered in one pass. Callers run this
// ONCE when the detail view opens and reuse the result — the screens
// themselves must not touch the filesystem again.
function buildInspection(profileDir) {
  const plugins = pluginInfo(profileDir);
  const skills = skillInfo(profileDir);
  const mcps = mcpInfo(profileDir);
  const extraBytes = plugins.reduce((n, p) => n + p.size.bytes, 0)
    + skills.reduce((n, s) => n + s.bytes, 0);
  return {
    account: accountInfo(profileDir),
    activity: activityInfo(profileDir, extraBytes),
    plugins,
    skills,
    mcps,
  };
}

module.exports = {
  humanBytes, estTokens, dirSize,
  accountInfo, activityInfo, pluginInfo, skillInfo, mcpInfo, buildInspection,
};
