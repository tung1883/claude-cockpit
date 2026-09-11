#!/usr/bin/env node
'use strict';
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const readline = require('node:readline');
const { spawn, spawnSync } = require('node:child_process');
const { addProfile, getProfile, listProfiles, removeProfile } = require('./profiles');
const paths = require('./paths');
const { copySession } = require('./handoff');
const { color, glyph, clearScreen, homeClear, enterAlt, leaveAlt, hideCursor, showCursor, resetStdin, banner, rule, makeRepainter } = require('./ui');

function usage() {
  const c = color;
  console.log(`${banner()}

${c.bold('Usage')}
  ${c.orange('claude-cockpit')}            Open the interactive cockpit (master command)
  ${c.orange('ccpit')}                     Short alias for the same thing

${c.bold('Commands')}
  add <name>                    Create an isolated account profile
  import <name> [dir]           Copy an existing login (default ~/.claude) into a profile
  sync <from> <to> [what]       Copy plugins/skills/mcp between profiles (what: plugins|skills|mcp|all)
  delete <name>                 Delete a profile and all its data
  list                          List profiles
  login <name>                  Launch Claude to log that profile in
  run <name> [claude args...]   Run Claude using that profile
  sessions [name|--all]         Show transcripts for a profile
  handoff <id> <from> <to>      Copy a transcript to another profile
  install-statusline <name>     Install the cockpit statusline into a profile
  statusline                    Read Claude status JSON from stdin

${c.bold('Inside the cockpit')}
  ${c.dim('↑/↓ or k/j')} move   ${c.dim('Enter')} launch   ${c.dim('a')} add   ${c.dim('i')} import login   ${c.dim('r')} refresh   ${c.dim('q / Esc')} quit
  When you exit Claude (${c.dim('/exit')} or Ctrl+C twice) you land back on this menu.`);
}

function quote(value) {
  return `"${String(value).replaceAll('"', '\\"')}"`;
}

function installStatusline(name) {
  const profile = getProfile(name);
  const settingsPath = path.join(paths.profileDir(profile.name), 'settings.json');
  let settings = {};
  if (fs.existsSync(settingsPath)) {
    try { settings = JSON.parse(fs.readFileSync(settingsPath, 'utf8')); }
    catch (error) { throw new Error(`Cannot parse ${settingsPath}: ${error.message}`); }
    const backup = `${settingsPath}.backup-${Date.now()}`;
    fs.copyFileSync(settingsPath, backup);
    console.log(`Backed up settings to ${backup}`);
  }
  settings.statusLine = {
    type: 'command',
    command: `${quote(process.execPath)} ${quote(path.join(__dirname, 'statusline.js'))}`,
  };
  fs.mkdirSync(path.dirname(settingsPath), { recursive: true });
  fs.writeFileSync(settingsPath, `${JSON.stringify(settings, null, 2)}\n`);
  console.log(`Installed statusline for '${profile.name}'. Restart Claude Code to see it.`);
}

function readJson(file) {
  if (!fs.existsSync(file)) return {};
  const raw = fs.readFileSync(file, 'utf8');
  if (!raw.trim()) return {};
  try { return JSON.parse(raw); }
  catch { throw new Error(`Refusing to sync: ${file} is not valid JSON.`); }
}

function writeJson(file, value) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, `${JSON.stringify(value, null, 2)}\n`);
}

function copyDir(src, dst) {
  if (!fs.existsSync(src)) return false;
  // dereference: Windows blocks symlink creation without elevation.
  fs.cpSync(src, dst, { recursive: true, force: true, dereference: true });
  return true;
}

// Copy plugins / skills / MCP servers from one profile into another.
// `spec` may be:
//   'all'                        every kind, every item
//   'plugins' | 'skills' | 'mcp' that whole kind
//   ['skills', 'mcp']            those whole kinds
//   { plugins: 'all', skills: ['a', 'b'], mcp: [...] }  per-item selection
function syncProfiles(fromName, toName, spec) {
  const from = getProfile(fromName);
  const to = getProfile(toName);
  if (from.name === to.name) throw new Error('Source and target are the same profile.');

  const valid = ['plugins', 'skills', 'mcp'];
  if (typeof spec === 'string') {
    spec = spec === 'all' ? { plugins: 'all', skills: 'all', mcp: 'all' } : { [spec]: 'all' };
  } else if (Array.isArray(spec)) {
    spec = Object.fromEntries(spec.map(kind => [kind, 'all']));
  }
  spec = spec || {};
  if (!valid.some(kind => spec[kind])) throw new Error('Nothing selected to sync.');

  const inv = profileInventory(from.name);
  const src = paths.profileDir(from.name);
  const dst = paths.profileDir(to.name);
  const srcSettings = path.join(src, 'settings.json');
  const dstSettings = path.join(dst, 'settings.json');
  const pick = (sel, available) => (sel === 'all' ? available : available.filter(x => sel.includes(x)));

  if (spec.plugins) {
    const ids = pick(spec.plugins, inv.plugins);
    copyDir(path.join(src, 'plugins'), path.join(dst, 'plugins'));
    const s = readJson(srcSettings);
    const d = readJson(dstSettings);
    d.enabledPlugins = d.enabledPlugins || {};
    for (const id of ids) d.enabledPlugins[id] = (s.enabledPlugins || {})[id] ?? true;
    d.extraKnownMarketplaces = { ...d.extraKnownMarketplaces, ...s.extraKnownMarketplaces };
    writeJson(dstSettings, d);
    console.log(color.green(`Synced ${ids.length} plugin${ids.length === 1 ? '' : 's'}`));
  }

  if (spec.skills) {
    const names = pick(spec.skills, inv.skills);
    let n = 0;
    for (const name of names) {
      if (copyDir(path.join(src, 'skills', name), path.join(dst, 'skills', name))) n++;
    }
    console.log(color.green(`Synced ${n} skill${n === 1 ? '' : 's'}`));
  }

  if (spec.mcp) {
    const names = pick(spec.mcp, inv.mcp);
    const sc = readJson(path.join(src, '.claude.json'));
    const dc = readJson(path.join(dst, '.claude.json'));
    const s = readJson(srcSettings);
    const d = readJson(dstSettings);
    dc.mcpServers = dc.mcpServers || {};
    d.mcpServers = d.mcpServers || {};
    for (const name of names) {
      if ((sc.mcpServers || {})[name]) dc.mcpServers[name] = sc.mcpServers[name];
      if ((s.mcpServers || {})[name]) d.mcpServers[name] = s.mcpServers[name];
    }
    writeJson(path.join(dst, '.claude.json'), dc);
    writeJson(dstSettings, d);
    console.log(color.green(`Synced ${names.length} MCP server${names.length === 1 ? '' : 's'}`));
  }
  console.log(color.dim('Restart Claude Code in the target profile to pick these up.'));
}

// Copy an existing Claude Code config (default: ~/.claude + ~/.claude.json)
// into a new isolated profile, so it keeps that account's login, sessions,
// plugins, skills, and settings.
function importProfile(name, source) {
  const src = source ? path.resolve(source) : path.join(os.homedir(), '.claude');
  if (!fs.existsSync(src)) throw new Error(`Source config dir not found: ${src}`);
  const profile = addProfile(name);
  const dst = paths.profileDir(profile.name);
  const skip = new Set(['node_modules', '.git', 'cache', 'shell-snapshots']);
  fs.cpSync(src, dst, {
    recursive: true,
    force: true,
    // Windows blocks symlink creation without elevation; copy link targets instead.
    dereference: true,
    filter: entry => !skip.has(path.basename(entry)),
  });
  const homeJson = path.join(path.dirname(src), '.claude.json');
  if (fs.existsSync(homeJson)) fs.copyFileSync(homeJson, path.join(dst, '.claude.json'));
  console.log(color.green(`Imported '${src}' into profile '${profile.name}'.`));
  console.log(color.dim('Close any running Claude Code first if the copy looks incomplete, then re-run.'));
  console.log(`Launch it with: ${color.orange('ccpit ' + profile.name)}`);
}

function sessionFiles(profile) {
  const root = path.join(paths.profileDir(profile), 'projects');
  const result = [];
  if (!fs.existsSync(root)) return result;
  const queue = [root];
  while (queue.length) {
    const current = queue.shift();
    for (const entry of fs.readdirSync(current, { withFileTypes: true })) {
      const full = path.join(current, entry.name);
      if (entry.isDirectory()) queue.push(full);
      else if (entry.isFile() && entry.name.endsWith('.jsonl')) {
        result.push({ id: entry.name.slice(0, -6), file: full, modified: fs.statSync(full).mtime });
      }
    }
  }
  return result.sort((a, b) => b.modified - a.modified);
}

function sessionDetails(session) {
  let sessionName = '';
  let recap = '';
  let lastPrompt = '';
  let projectPath = '';
  try {
    const lines = fs.readFileSync(session.file, 'utf8').split(/\r?\n/);
    for (const line of lines) {
      if (!line) continue;
      let entry;
      try { entry = JSON.parse(line); } catch { continue; }
      sessionName = entry.session_name || entry.sessionName || entry.name || sessionName;
      projectPath = entry.cwd || entry.workspace?.current_dir || projectPath;
      if (entry.type === 'summary' || entry.type === 'compact_summary') {
        recap = typeof entry.summary === 'string' ? entry.summary : typeof entry.content === 'string' ? entry.content : recap;
      }
      if (entry.type === 'last-prompt' && typeof entry.lastPrompt === 'string') lastPrompt = entry.lastPrompt;
      if (entry.type === 'user' && typeof entry.message?.content === 'string') lastPrompt = entry.message.content;
    }
  } catch { /* A deleted or partially-written session should still be listed. */ }
  return { ...session, sessionName, recap, lastPrompt, projectPath };
}

function relativeAge(date) {
  if (!date) return 'never';
  const seconds = Math.max(0, (Date.now() - date.getTime()) / 1000);
  if (seconds < 60) return 'just now';
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m ago`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)}h ago`;
  return `${Math.floor(seconds / 86400)}d ago`;
}

function dashboard() {
  const profiles = listProfiles();
  console.log(`${banner()}\n`);
  if (!profiles.length) {
    console.log(`No profiles yet. Create one with: ${color.orange('claude-cockpit add personal')}`);
    return;
  }
  for (const profile of profiles) {
    const sessions = sessionFiles(profile.name);
    const latest = relativeAge(sessions[0]?.modified);
    console.log(
      `  ${color.bold(profile.name.padEnd(16))} ` +
      `${color.orange(String(sessions.length).padStart(3))} sessions  ` +
      `${color.dim(latest)}`,
    );
  }
  console.log(`\n${color.dim('Commands: claude-cockpit | sessions <profile> | handoff <id> <from> <to>')}`);
}

function showSessions(name) {
  const profiles = name === '--all' || !name ? listProfiles().map(profile => profile.name) : [name];
  for (const profile of profiles) {
    getProfile(profile);
    const sessions = sessionFiles(profile);
    console.log(`\n${color.bold(`[${profile}]`)} ${sessions.length} session${sessions.length === 1 ? '' : 's'}`);
    for (const rawSession of sessions) {
      const session = sessionDetails(rawSession);
      console.log(`  ${color.orange(session.id)}  ${color.dim(session.modified.toLocaleString())}`);
      if (session.projectPath) console.log(`    ${color.dim('project:')} ${session.projectPath.replaceAll('\\', '/')}`);
      if (session.sessionName) console.log(`    ${color.dim('name:')} ${session.sessionName}`);
      if (session.recap) console.log(`    ${color.dim('last recap:')} ${session.recap.replace(/\s+/g, ' ').slice(0, 240)}`);
      if (session.lastPrompt) console.log(`    ${color.dim('last prompt:')} ${session.lastPrompt.replace(/\s+/g, ' ').slice(0, 160)}`);
    }
  }
}

// Session counts require walking each profile's projects tree. accountLines is
// rebuilt on every keystroke, so cache the result and only rescan when the
// cockpit loop clears it (a new menu iteration or an explicit refresh).
let statCache = new Map();
function invalidateStats() { statCache = new Map(); }
function accountStat(name) {
  if (!statCache.has(name)) {
    const sessions = sessionFiles(name);
    statCache.set(name, { count: sessions.length, age: relativeAge(sessions[0]?.modified) });
  }
  return statCache.get(name);
}

// Visible width (ANSI escapes don't take columns) and a padder that uses it.
const visLen = s => s.replace(/\x1b\[[0-9;]*m/g, '').length;
const padTo = (s, width) => s + ' '.repeat(Math.max(0, width - visLen(s)));

// The account list. Shown at all times — in the picker and above every sub-prompt.
function accountLines(profiles, selected = -1, stats = null) {
  const lines = [banner(), ''];
  if (profiles.length) {
    lines.push(color.dim(' Each account is an isolated Claude Code login.'), '');
  }
  const nameWidth = Math.min(24, Math.max(12, ...profiles.map(p => p.name.length)) + 2);
  profiles.forEach((profile, index) => {
    const active = index === selected;
    const marker = active ? color.orange(glyph.pointer) : ' ';
    const name = active ? color.orangeBold(profile.name) : profile.name;
    const st = stats ? stats[index] : accountStat(profile.name);
    lines.push(
      ` ${marker} ${padTo(name, nameWidth)}` +
      `${color.dim(padTo(`${String(st.count).padStart(3)} sessions`, 14))}` +
      `${color.dim(st.age)}`,
    );
  });
  lines.push('');
  lines.push(rule());
  return lines;
}

// Two rows of key hints laid out on an aligned grid.
function hintGrid(rows, cell = 16) {
  return rows.map(cells =>
    ' ' + cells.map(c => padTo(`${color.dim(c[0])} ${c[1]}`, cell)).join('').replace(/\s+$/, ''));
}

// Returned by promptLine when the user asks to go back (`<-` or Esc).
const BACK = Symbol('back');

// Scrollable read-only list. `rows` are { text, selectable }. ↑/↓ or k/j move
// the highlight over selectable rows (scrolling the viewport); ←/Esc/Enter/q exit.
function scrollScreen({ header, rows, footerHint }) {
  return new Promise(resolve => {
    const stdin = process.stdin;
    const repaint = makeRepainter();
    let firstRender = true;
    const selectableIdx = rows.map((r, i) => (r.selectable ? i : -1)).filter(i => i >= 0);
    let pos = 0; // index into selectableIdx
    let top = 0; // first visible row
    const viewport = Math.max(5, (process.stdout.rows || 24) - header.length - 5);

    const render = () => {
      if (firstRender) { hideCursor(); firstRender = false; }
      const active = selectableIdx[pos] ?? -1;
      if (active >= 0 && active < top) top = active;
      if (active >= top + viewport) top = active - viewport + 1;
      top = Math.max(0, Math.min(top, Math.max(0, rows.length - viewport)));
      const lines = [...header];
      const end = Math.min(rows.length, top + viewport);
      if (top > 0) lines.push(color.dim(`    ↑ ${top} more`));
      for (let i = top; i < end; i++) {
        const row = rows[i];
        if (!row.selectable) { lines.push(row.text); continue; }
        const on = i === active;
        lines.push(` ${on ? color.orange(glyph.pointer) : ' '} ${on ? color.orangeBold(row.text) : row.text}`);
      }
      if (end < rows.length) lines.push(color.dim(`    ↓ ${rows.length - end} more`));
      lines.push('');
      lines.push(rule());
      lines.push(` ${color.dim('↑/↓')} move    ${color.dim(`${glyph.back}/Esc`)} back`);
      if (footerHint) lines.push(color.dim(`   ${footerHint}`));
      repaint(lines);
    };

    const onKey = (str, key) => {
      key = key || {};
      const name = key.name || str;
      if (key.ctrl && name === 'c') { showCursor(); process.exit(130); }
      if (['escape', 'left', 'h', 'return', 'enter', 'q'].includes(name)) {
        stdin.removeListener('keypress', onKey);
        if (stdin.isRaw) stdin.setRawMode(false);
        stdin.pause();
        showCursor();
        return resolve();
      }
      if (!selectableIdx.length) return;
      if (name === 'up' || name === 'k') { pos = (pos + selectableIdx.length - 1) % selectableIdx.length; return render(); }
      if (name === 'down' || name === 'j') { pos = (pos + 1) % selectableIdx.length; return render(); }
      if (name === 'pageup') { pos = Math.max(0, pos - 5); return render(); }
      if (name === 'pagedown') { pos = Math.min(selectableIdx.length - 1, pos + 5); return render(); }
    };

    readline.emitKeypressEvents(stdin);
    if (stdin.isTTY) stdin.setRawMode(true);
    stdin.resume();
    stdin.on('keypress', onKey);
    render();
  });
}

// What a profile has enabled: plugin ids, skill dirs, MCP server names.
function profileInventory(name) {
  const dir = paths.profileDir(name);
  const settings = readJson(path.join(dir, 'settings.json'));
  const claudeJson = readJson(path.join(dir, '.claude.json'));
  const plugins = Object.entries(settings.enabledPlugins || {}).filter(([, v]) => v).map(([k]) => k).sort();
  let skills = [];
  try {
    skills = fs.readdirSync(path.join(dir, 'skills'), { withFileTypes: true })
      .filter(entry => entry.isDirectory()).map(entry => entry.name).sort();
  } catch { /* no skills dir */ }
  const mcp = [...new Set([
    ...Object.keys(claudeJson.mcpServers || {}),
    ...Object.keys(settings.mcpServers || {}),
  ])].sort();
  return { plugins, skills, mcp };
}

// Repaint the persistent account list, then a feature heading with a back hint.
function subScreen(profiles, heading, mark = -1) {
  const lines = accountLines(profiles, mark);
  lines.push(`${color.orange(glyph.back)} ${color.bold(heading)}`);
  lines.push(color.dim(`   press ${glyph.back} or Esc to go back`));
  lines.push('');
  process.stdout.write(`\x1b[H${lines.join('\n')}\n\x1b[0J`);
}

// A line prompt that resolves to BACK on ←/Esc (or an empty Enter after typing nothing).
function promptLine(question) {
  return new Promise(resolve => {
    const stdin = process.stdin;
    let buf = '';
    process.stdout.write(`${color.orange(glyph.back)} ${question}`);
    readline.emitKeypressEvents(stdin);
    const wasRaw = Boolean(stdin.isRaw);
    if (stdin.isTTY) stdin.setRawMode(true);
    stdin.resume();
    const done = value => {
      stdin.removeListener('keypress', onKey);
      if (stdin.isTTY && !wasRaw) stdin.setRawMode(false);
      stdin.pause();
      process.stdout.write('\n');
      resolve(value);
    };
    const onKey = (str, key) => {
      key = key || {};
      const name = key.name || '';
      if (name === 'escape' || (name === 'left' && !buf)) return done(BACK);
      if (key.ctrl && name === 'c') { showCursor(); process.exit(130); }
      if (name === 'return' || name === 'enter') return done(buf.trim() === '<-' ? BACK : buf);
      if (name === 'backspace') {
        if (buf) { buf = buf.slice(0, -1); process.stdout.write('\b \b'); }
        return;
      }
      if (str && !key.ctrl && !key.meta && str >= ' ') { buf += str; process.stdout.write(str); }
    };
    stdin.on('keypress', onKey);
  });
}

// Arrow-key checklist. ↑/↓ move, Space/Enter toggle a row, move to "→ Go" and
// Enter to confirm. An option with `items: [...]` is drillable: → opens a
// sub-checklist for its individual items.
// Resolves to:
//   BACK
//   { go: true, checked, sub, selected }   sub[i] = null (all) | string[] (subset)
//   { drill: i, checked, sub, selected }   caller opens the sub-list for row i
function chooseMulti({ profiles, heading, hint, options, goLabel = 'Go', mark = -1, state }) {
  return new Promise(resolve => {
    const stdin = process.stdin;
    const checked = state?.checked ? state.checked.slice() : options.map(o => Boolean(o.checked));
    const sub = state?.sub ? state.sub.slice() : options.map(() => null);
    let selected = state?.selected ?? 0;
    const rows = options.length + 1;
    const repaint = makeRepainter();
    let firstRender = true;
    let top = 0;
    const viewport = Math.max(4, (process.stdout.rows || 24) - (profiles.length ? profiles.length + 6 : 4) - 8);

    const snapshot = () => ({ checked: checked.slice(), sub: sub.slice(), selected });

    const rowNote = (opt, i) => {
      if (opt.items && Array.isArray(sub[i])) return `${sub[i].length} of ${opt.items.length}`;
      if (opt.items && checked[i]) return `all ${opt.items.length}`;
      return opt.note || '';
    };

    const render = () => {
      if (firstRender) { hideCursor(); firstRender = false; }
      if (selected < top) top = selected;
      if (selected >= top + viewport) top = selected - viewport + 1;
      top = Math.max(0, Math.min(top, Math.max(0, options.length - viewport)));
      const lines = accountLines(profiles, mark);
      lines.push(`${color.orange(glyph.back)} ${color.bold(heading)}`);
      if (hint) lines.push(color.dim(`   ${hint}`));
      lines.push('');
      const end = Math.min(options.length, top + viewport);
      if (top > 0) lines.push(color.dim(`    ↑ ${top} more`));
      for (let i = top; i < end; i++) {
        const opt = options[i];
        const active = i === selected;
        const marker = active ? color.orange(glyph.pointer) : ' ';
        const box = checked[i] ? color.orange('[x]') : color.dim('[ ]');
        const label = active ? color.orangeBold(opt.label) : opt.label;
        const arrow = opt.items && opt.items.length ? color.dim(' →') : '';
        const note = rowNote(opt, i);
        lines.push(` ${marker} ${box} ${label}${arrow}${note ? `   ${color.dim(note)}` : ''}`);
      }
      if (end < options.length) lines.push(color.dim(`    ↓ ${options.length - end} more`));
      lines.push('');
      const goActive = selected === options.length;
      const count = checked.filter(Boolean).length;
      const go = `${color.orange(`${glyph.pointer} →`)} ${goActive ? color.orangeBold(goLabel) : goLabel}`;
      lines.push(` ${goActive ? go : `   ${color.dim('→')} ${goLabel}`}   ${color.dim(`${count} selected`)}`);
      lines.push('');
      lines.push(rule());
      const drillHint = options.some(o => o.items && o.items.length) ? `    ${color.dim('→')} choose items` : '';
      lines.push(` ${color.dim('↑/↓')} move    ${color.dim('Space')} toggle    ${color.dim('Enter')} toggle / go${drillHint}    ${color.dim(`${glyph.back}/Esc`)} back`);
      repaint(lines);
    };

    const cleanup = () => {
      stdin.removeListener('keypress', onKey);
      if (stdin.isRaw) stdin.setRawMode(false);
      stdin.pause();
      showCursor();
    };
    const done = value => { cleanup(); resolve(value); };

    const onKey = (str, key) => {
      key = key || {};
      const name = key.name || str;
      if (key.ctrl && name === 'c') { showCursor(); process.exit(130); }
      if (name === 'escape' || name === 'left' || name === 'h') return done(BACK);
      if (name === 'up' || name === 'k') { selected = (selected + rows - 1) % rows; return render(); }
      if (name === 'down' || name === 'j' || name === 'tab') { selected = (selected + 1) % rows; return render(); }
      const onGo = selected === options.length;
      if ((name === 'right' || name === 'l') && !onGo && options[selected].items && options[selected].items.length) {
        return done({ drill: selected, ...snapshot() });
      }
      if (name === 'space' || str === ' ') {
        if (!onGo) { checked[selected] = !checked[selected]; render(); }
        return;
      }
      if (name === 'return' || name === 'enter') {
        if (onGo) return done({ go: true, ...snapshot() });
        checked[selected] = !checked[selected];
        return render();
      }
    };

    readline.emitKeypressEvents(stdin);
    if (stdin.isTTY) stdin.setRawMode(true);
    stdin.resume();
    stdin.on('keypress', onKey);
    render();
  });
}

// Arrow-key list picker. Shows the account list for context, then `options`
// with a ❯ pointer. ↑/↓ or k/j move, Enter selects, ←/Esc returns BACK.
function chooseFromList({ profiles, heading, hint, options, mark = -1 }) {
  return new Promise(resolve => {
    const stdin = process.stdin;
    let selected = 0;
    const repaint = makeRepainter();
    let firstRender = true;

    const render = () => {
      if (firstRender) { hideCursor(); firstRender = false; }
      const lines = accountLines(profiles, mark);
      lines.push(`${color.orange(glyph.back)} ${color.bold(heading)}`);
      if (hint) lines.push(color.dim(`   ${hint}`));
      lines.push('');
      options.forEach((opt, index) => {
        const active = index === selected;
        const marker = active ? color.orange(glyph.pointer) : ' ';
        const label = active ? color.orangeBold(opt.label) : opt.label;
        lines.push(` ${marker} ${label}${opt.note ? `   ${color.dim(opt.note)}` : ''}`);
      });
      lines.push('');
      lines.push(rule());
      lines.push(` ${color.dim('↑/↓')} move    ${color.dim('Enter')} select    ${color.dim(`${glyph.back}/Esc`)} back`);
      repaint(lines);
    };

    const cleanup = () => {
      stdin.removeListener('keypress', onKey);
      if (stdin.isRaw) stdin.setRawMode(false);
      stdin.pause();
      showCursor();
    };
    const done = value => { cleanup(); resolve(value); };

    const onKey = (str, key) => {
      key = key || {};
      const name = key.name || str;
      if (key.ctrl && name === 'c') { showCursor(); process.exit(130); }
      if (name === 'escape' || name === 'left' || name === 'h') return done(BACK);
      if (name === 'up' || name === 'k' || (key.shift && name === 'tab')) {
        selected = (selected + options.length - 1) % options.length;
        return render();
      }
      if (name === 'down' || name === 'j' || name === 'tab') {
        selected = (selected + 1) % options.length;
        return render();
      }
      if (name === 'return' || name === 'enter') return done(options[selected].value);
    };

    readline.emitKeypressEvents(stdin);
    if (stdin.isTTY) stdin.setRawMode(true);
    stdin.resume();
    stdin.on('keypress', onKey);
    render();
  });
}

// Interactive account picker. Resolves to an action the caller loop acts on.
function pickProfile(profiles, start = 0) {
  return new Promise(resolve => {
    let selected = Math.max(0, Math.min(start, profiles.length - 1));
    const stdin = process.stdin;
    // Read session stats once — the picker list is static while it is open.
    const stats = profiles.map(p => accountStat(p.name));
    const repaint = makeRepainter();
    let firstRender = true;

    const render = () => {
      if (firstRender) { hideCursor(); firstRender = false; }
      const lines = accountLines(profiles, selected, stats);
      lines.push(...hintGrid([
        [['↑/↓', 'move'], ['→', 'view'], ['r', 'refresh'], ['q', 'quit']],
        [['a', 'add'], ['i', 'import'], ['d', 'delete'], ['s', 'sync']],
      ]));
      repaint(lines);
    };

    const cleanup = () => {
      stdin.removeListener('keypress', onKey);
      if (stdin.isRaw) stdin.setRawMode(false);
      stdin.pause();
      showCursor();
    };

    const finish = action => {
      cleanup();
      resolve(action);
    };

    const onKey = (str, key) => {
      key = key || {};
      const name = key.name || str;
      if ((key.ctrl && name === 'c') || name === 'q' || name === 'escape') return finish({ type: 'quit' });
      if (name === 'up' || name === 'k' || (key.shift && name === 'tab')) {
        selected = (selected + profiles.length - 1) % profiles.length;
        return render();
      }
      if (name === 'down' || name === 'j' || name === 'tab') {
        selected = (selected + 1) % profiles.length;
        return render();
      }
      if (name === 'return' || name === 'enter') return finish({ type: 'open', name: profiles[selected].name });
      if (name === 'right' || name === 'l') return finish({ type: 'detail', name: profiles[selected].name });
      if (name === 'a') return finish({ type: 'add' });
      if (name === 'i') return finish({ type: 'import' });
      if (name === 's') return finish({ type: 'sync', name: profiles[selected].name });
      if (name === 'd') return finish({ type: 'delete', name: profiles[selected].name });
      if (name === 'r') return finish({ type: 'refresh' });
    };

    readline.emitKeypressEvents(stdin);
    if (stdin.isTTY) stdin.setRawMode(true);
    stdin.resume();
    stdin.on('keypress', onKey);
    render();
  });
}

function ask(question) {
  return new Promise(resolve => {
    const rl = readline.createInterface({ input: process.stdin, output: process.stdout });
    rl.question(question, answer => { rl.close(); resolve(answer); });
  });
}

// The master command. Loops: pick account -> run Claude -> back to the menu on exit.
async function cockpit(args = []) {
  if (args[0]) return launch(args[0], args.slice(1));           // `claude-cockpit <profile> [args]`
  if (!process.stdin.isTTY || !process.stdout.isTTY) return dashboard();

  enterAlt(); // fixed page: screens repaint in place, terminal never scrolls
  let cursor = 0; // remembered account row, so sub-screens return you to it
  for (;;) {
    invalidateStats(); // rescan session counts once per menu visit, not per keypress
    let profiles = listProfiles();
    if (!profiles.length) {
      const pick = await chooseFromList({
        profiles: [],
        heading: 'No accounts yet — set one up',
        hint: 'each account is an isolated Claude Code login',
        options: [
          { label: 'Import this machine\'s existing login', value: 'import', note: '~/.claude — keeps sessions, plugins, skills' },
          { label: 'Create a fresh empty account', value: 'add', note: 'log in with /login afterward' },
          { label: 'Quit', value: 'quit' },
        ],
      });
      if (pick === BACK || pick === 'quit') { leaveAlt(); return; }
      if (pick === 'import') {
        subScreen(profiles, 'Import an existing login');
        const name = await promptLine('Profile name [main]: ');
        if (name === BACK) continue;
        try { importProfile(name.trim() || 'main'); }
        catch (error) { console.error(color.red(`Error: ${error.message}`)); }
        await ask('Press Enter...');
        continue;
      }
      if (pick === 'add') {
        subScreen(profiles, 'Create a fresh account');
        const name = await promptLine('Account name: ');
        if (name === BACK || !name.trim()) continue;
        try { addProfile(name.trim()); console.log(color.green(`Created '${name.trim()}'.`)); }
        catch (error) { console.error(color.red(`Error: ${error.message}`)); await ask('Press Enter...'); }
        continue;
      }
      continue;
    }

    cursor = Math.max(0, Math.min(cursor, profiles.length - 1));
    const action = await pickProfile(profiles, cursor);
    const mark = profiles.findIndex(p => p.name === action.name);
    if (mark >= 0) cursor = mark;
    if (action.type === 'quit') { leaveAlt(); return; }
    if (action.type === 'refresh') continue;
    if (action.type === 'detail') {
      const inv = profileInventory(action.name);
      const header = accountLines(profiles, mark);
      header.push(`${color.orange(glyph.back)} ${color.orange(action.name)}  ${color.dim('—  plugins · skills · MCP')}`);
      header.push('');
      const rows = [];
      const section = (title, items) => {
        rows.push({ text: `  ${color.orangeBold(title)} ${color.dim(`(${items.length})`)}`, selectable: false });
        if (!items.length) rows.push({ text: `    ${color.dim('none')}`, selectable: false });
        else for (const item of items) rows.push({ text: `  ${item}`, selectable: true });
        rows.push({ text: '', selectable: false });
      };
      section('Plugins', inv.plugins);
      section('Skills', inv.skills);
      section('MCP servers', inv.mcp);
      await scrollScreen({ header, rows });
      continue;
    }
    if (action.type === 'import') {
      subScreen(profiles, 'Import an existing login into a new profile', mark);
      const name = await promptLine('Profile name for the imported login [main]: ');
      if (name === BACK) continue;
      const dir = await promptLine('Source config dir [~/.claude]: ');
      if (dir === BACK) continue;
      try { importProfile(name.trim() || 'main', dir.trim() || undefined); }
      catch (error) { console.error(color.red(`Error: ${error.message}`)); }
      await ask('Press Enter to return to the menu...');
      continue;
    }
    if (action.type === 'sync') {
      const others = profiles.filter(p => p.name !== action.name);
      if (!others.length) {
        subScreen(profiles, `Sync from ${color.orange(action.name)}`, mark);
        console.log(color.dim('Need a second profile to sync into.'));
        await ask('Press Enter...');
        continue;
      }
      const target = await chooseFromList({
        profiles,
        mark,
        heading: `Sync plugins / skills / MCP from ${color.orange(action.name)} into…`,
        hint: 'choose the target account',
        options: others.map(p => ({
          label: p.name, value: p.name, note: `${accountStat(p.name).count} sessions`,
        })),
      });
      if (target === BACK) continue;

      const inv = profileInventory(action.name);
      const cats = [
        { label: 'Plugins', value: 'plugins', items: inv.plugins, note: `${inv.plugins.length} available`, checked: inv.plugins.length > 0 },
        { label: 'Skills', value: 'skills', items: inv.skills, note: `${inv.skills.length} available`, checked: inv.skills.length > 0 },
        { label: 'MCP servers', value: 'mcp', items: inv.mcp, note: `${inv.mcp.length} available`, checked: inv.mcp.length > 0 },
      ];
      let st;
      let result;
      for (;;) {
        const res = await chooseMulti({
          profiles,
          mark,
          heading: `Sync  ${color.orange(action.name)}  ${glyph.pointer}  ${color.orange(target)}`,
          hint: 'Space to tick a kind · → to pick individual items · then → Go',
          options: cats,
          state: st,
        });
        if (res === BACK) { result = BACK; break; }
        st = res;
        if (res.go) { result = res; break; }
        const cat = cats[res.drill];
        const pre = Array.isArray(res.sub[res.drill]) ? res.sub[res.drill] : cat.items;
        const subRes = await chooseMulti({
          profiles,
          mark,
          heading: `${cat.label}  —  copy which into ${color.orange(target)}?`,
          hint: 'Space to tick · → Go when done',
          options: cat.items.map(name => ({ label: name, value: name, checked: pre.includes(name) })),
        });
        if (subRes !== BACK && subRes.go) {
          const picks = cat.items.filter((_, i) => subRes.checked[i]);
          st.sub[res.drill] = picks.length === cat.items.length ? null : picks;
          st.checked[res.drill] = picks.length > 0;
        }
      }
      if (result === BACK) continue;
      const spec = {};
      cats.forEach((cat, i) => {
        if (result.checked[i]) spec[cat.value] = Array.isArray(result.sub[i]) ? result.sub[i] : 'all';
      });
      homeClear();
      if (!Object.keys(spec).length) { console.log(color.dim('Nothing selected.')); await ask('Press Enter...'); continue; }
      try { syncProfiles(action.name, target, spec); }
      catch (error) { console.error(color.red(`Error: ${error.message}`)); }
      await ask('Press Enter to return to the menu...');
      continue;
    }
    if (action.type === 'delete') {
      subScreen(profiles, `Delete profile ${color.orange(action.name)}`, mark);
      console.log(color.red(`Everything under ${paths.profileDir(action.name)} will be removed.`));
      console.log(color.dim('That account\'s login, sessions, plugins, and settings. Cannot be undone.'));
      console.log('');
      const confirm = await promptLine('Type the profile name to confirm: ');
      if (confirm === BACK) continue;
      if (confirm.trim() === action.name) {
        try { removeProfile(action.name); continue; } // straight back to the menu
        catch (error) { console.error(color.red(`Error: ${error.message}`)); await ask('Press Enter...'); }
      } else {
        console.log(color.dim('Cancelled.'));
        await ask('Press Enter...');
      }
      continue;
    }
    if (action.type === 'add') {
      subScreen(profiles, 'Create a fresh account');
      const name = await promptLine('New account name: ');
      if (name === BACK || !name.trim()) continue;
      try {
        const profile = addProfile(name.trim());
        leaveAlt();
        console.log(color.green(`Created '${profile.name}'. Launching Claude — run /login inside it.`));
        await launch(profile.name, [], { interactive: true });
        enterAlt();
      } catch (error) { enterAlt(); console.error(color.red(`Error: ${error.message}`)); await ask('Press Enter...'); }
      continue;
    }
    if (action.type === 'open') {
      leaveAlt(); // give Claude the real terminal (its own alt screen, scrollback)
      process.stdout.write(`${color.dim(`${glyph.spark} launching Claude as `)}${color.orange(action.name)}${color.dim(' — exit Claude to return here')}\n\n${color.reset}`);
      await launch(action.name, [], { interactive: true });
      enterAlt();
    }
  }
}

// Resolve how to start Claude. On Windows we avoid the `claude.cmd` shim so
// Ctrl+C inside Claude does not drop to cmd.exe's "Terminate batch job (Y/N)?"
// prompt — we run the real .exe (or cli.js via node) that the shim points at.
let claudeTargetCache;
function claudeTarget() {
  if (claudeTargetCache) return claudeTargetCache;
  const set = value => (claudeTargetCache = value);

  const bin = process.env.CLAUDE_BIN;
  if (bin) return set({ command: bin, prefix: [], shell: /\.(cmd|bat)$/i.test(bin) });
  if (process.platform !== 'win32') return set({ command: 'claude', prefix: [], shell: false });

  const where = spawnSync('where', ['claude'], { encoding: 'utf8' });
  const shims = where.status === 0 ? where.stdout.split(/\r?\n/).map(s => s.trim()).filter(Boolean) : [];
  for (const shim of shims) {
    const dir = path.dirname(shim);
    const guesses = [];
    if (/\.(cmd|bat|ps1)$/i.test(shim)) {
      try {
        const text = fs.readFileSync(shim, 'utf8');
        const m = text.match(/%[~a-z0-9]*dp0%[\\/]?([^"'\s]+?\.(?:exe|js))/i);
        if (m) guesses.push(path.join(dir, m[1]));
      } catch { /* unreadable shim */ }
    }
    guesses.push(
      path.join(dir, 'node_modules', '@anthropic-ai', 'claude-code', 'bin', 'claude.exe'),
      path.join(dir, 'node_modules', '@anthropic-ai', 'claude-code', 'cli.js'),
    );
    const hit = guesses.find(p => { try { return fs.existsSync(p); } catch { return false; } });
    if (hit && hit.toLowerCase().endsWith('.js')) return set({ command: process.execPath, prefix: [hit], shell: false });
    if (hit) return set({ command: hit, prefix: [], shell: false });
  }
  return set({ command: 'claude.cmd', prefix: [], shell: true });
}

// Spawn Claude with the profile's isolated config dir.
// interactive:true resolves when Claude exits (so the cockpit menu can resume);
// otherwise the process exits with Claude's code.
function launch(profile, args, { interactive = false } = {}) {
  return new Promise(resolve => {
    resetStdin(); // release stdin so Claude's own prompt gets the keyboard
    const target = claudeTarget();
    const command = target.command;
    const child = spawn(command, [...target.prefix, ...args], {
      stdio: 'inherit',
      env: { ...process.env, CLAUDE_CONFIG_DIR: paths.profileDir(profile) },
      windowsHide: false,
      shell: target.shell,
    });
    // Ctrl+C is delivered to the whole console group. Let Claude own it while it
    // runs — ignore it here so the cockpit survives and shows the menu again.
    const ignore = () => {};
    process.on('SIGINT', ignore);
    process.on('SIGBREAK', ignore);
    const restoreSignals = () => {
      process.removeListener('SIGINT', ignore);
      process.removeListener('SIGBREAK', ignore);
    };
    child.on('exit', code => {
      restoreSignals();
      if (interactive) resolve(code ?? 0);
      else process.exit(code ?? 0);
    });
    child.on('error', error => {
      restoreSignals();
      if (error.code === 'ENOENT') {
        console.error(color.red(`Claude CLI not found (${command}). Install Claude Code or set CLAUDE_BIN to its full path.`));
        console.error(color.dim('Example: $env:CLAUDE_BIN = "$env:APPDATA\\npm\\claude.cmd"'));
      } else {
        console.error(color.red(`Could not launch Claude CLI: ${error.message}`));
      }
      if (interactive) resolve(1);
      else process.exit(1);
    });
  });
}

async function main(argv) {
  const [command, ...args] = argv;
  try {
    if (!command) return await cockpit();
    if (command === '--help' || command === '-h' || command === 'help') return usage();
    if (command === 'dashboard' || command === 'accounts') return dashboard();
    if (command === 'open' || command === 'start') return await cockpit(args);
    if (command === 'add') { const p = addProfile(args[0]); console.log(`Created profile '${p.name}' at ${paths.profileDir(p.name)}`); return; }
    if (command === 'import') return importProfile(args[0], args[1]);
    if (command === 'sync') { const [fromP, toP, what] = args; return syncProfiles(fromP, toP, what || 'all'); }
    if (command === 'delete' || command === 'rm') {
      const target = getProfile(args[0]);
      if (args[1] !== '--yes' && args[1] !== '-y') throw new Error(`Pass --yes to confirm: claude-cockpit delete ${target.name} --yes`);
      const result = removeProfile(target.name);
      console.log(`Deleted profile '${result.name}' and ${result.dir}`);
      return;
    }
    if (command === 'list') return listProfiles().forEach(p => console.log(`${p.name}\t${paths.profileDir(p.name)}`));
    if (command === 'sessions') return showSessions(args[0]);
    if (command === 'install-statusline') return installStatusline(args[0]);
    if (command === 'login' || command === 'run') { const p = getProfile(args.shift()); return await launch(p.name, command === 'login' ? [] : args); }
    if (command === 'handoff') { const [session, from, to] = args; getProfile(from); getProfile(to); const result = copySession(from, to, session); console.log(`Copied session ${session} to '${to}'. Run: claude-cockpit run ${to} --resume ${session}`); return result; }
    if (command === 'statusline') return require('./statusline').main();
    // Unknown first arg is treated as a profile name: `claude-cockpit work`
    getProfile(command);
    return await cockpit([command, ...args]);
  } catch (error) { leaveAlt(); console.error(color.red(`Error: ${error.message}`)); process.exitCode = 1; }
}

main(process.argv.slice(2));
