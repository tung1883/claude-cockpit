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
const inspect = require('./inspect');
const notes = require('./notes');
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
  notes [dir]                   Sync a project's TODO.md/PLAN.md with the master files right now
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

// quiet: skip the console messages (used when this runs automatically as part
// of creating/importing a profile). onlyIfMissing: leave an existing
// statusLine alone instead of overwriting it (used on import, so we don't
// clobber a statusline the imported config already had configured).
function installStatusline(name, { quiet = false, onlyIfMissing = false } = {}) {
  const profile = getProfile(name);
  const settingsPath = path.join(paths.profileDir(profile.name), 'settings.json');
  let settings = {};
  if (fs.existsSync(settingsPath)) {
    try { settings = JSON.parse(fs.readFileSync(settingsPath, 'utf8')); }
    catch (error) { throw new Error(`Cannot parse ${settingsPath}: ${error.message}`); }
    if (onlyIfMissing && settings.statusLine) return false;
    const backup = `${settingsPath}.backup-${Date.now()}`;
    fs.copyFileSync(settingsPath, backup);
    if (!quiet) console.log(`Backed up settings to ${backup}`);
  }
  settings.statusLine = {
    type: 'command',
    command: `${quote(process.execPath)} ${quote(path.join(__dirname, 'statusline.js'))}`,
  };
  fs.mkdirSync(path.dirname(settingsPath), { recursive: true });
  fs.writeFileSync(settingsPath, `${JSON.stringify(settings, null, 2)}\n`);
  if (!quiet) console.log(`Installed statusline for '${profile.name}'. Restart Claude Code to see it.`);
  return true;
}

// Create a profile and wire up the cockpit statusline in one step, so a new
// account never sits there silently missing stats.
function createAccount(name) {
  const profile = addProfile(name);
  try { installStatusline(profile.name, { quiet: true }); } catch { /* best effort */ }
  invalidateInspection(profile.name);
  return profile;
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
  // The imported settings.json rarely has our statusline wired up — add it,
  // but leave alone whatever statusline the imported config already had.
  try { installStatusline(profile.name, { quiet: true, onlyIfMissing: true }); } catch { /* best effort */ }
  invalidateInspection(profile.name);
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

// The full profile inspection (plugins/skills/sessions scan) is the slow
// part of opening the detail view — a few hundred ms on a big profile. We
// pre-build it in the background for whichever row is highlighted, so by the
// time you press → it's usually already sitting in cache and the view opens
// instantly. `inFlight` guards against scheduling the same scan twice while
// the user is still holding an arrow key.
let inspectionCache = new Map();
const inFlight = new Set();
function invalidateInspection(name) {
  if (name) inspectionCache.delete(name);
  else inspectionCache = new Map();
}
function prefetchInspection(name) {
  if (inspectionCache.has(name) || inFlight.has(name)) return;
  inFlight.add(name);
  notes.debugLog(`prefetchInspection scheduled for ${name}`);
  setImmediate(() => {
    notes.debugLog(`prefetchInspection scan start for ${name}`);
    try { inspectionCache.set(name, inspect.buildInspection(paths.profileDir(name))); }
    catch { /* best effort — detail view will retry synchronously */ }
    finally { inFlight.delete(name); notes.debugLog(`prefetchInspection scan done for ${name}`); }
  });
}
function getInspection(name) {
  if (!inspectionCache.has(name)) inspectionCache.set(name, inspect.buildInspection(paths.profileDir(name)));
  return inspectionCache.get(name);
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

// Raw mode + flowing stdin persist for the whole interactive cockpit session
// — only the keypress LISTENER changes as screens come and go. Toggling
// setRawMode/pause/resume on every single screen transition (which every
// screen used to do independently) is what would silently stop stdin from
// ever delivering another keypress after a few cycles on Windows — no error,
// it just goes dead, which looks exactly like the whole app freezing.
// resetStdin() (used when handing off to Claude or an external editor) is
// still the one legitimate place raw mode actually gets released.
function beginKeys(stdin, onKey) {
  readline.emitKeypressEvents(stdin);
  if (stdin.isTTY && !stdin.isRaw) stdin.setRawMode(true);
  if (stdin.isPaused()) stdin.resume();
  stdin.on('keypress', onKey);
}
function endKeys(stdin, onKey) {
  stdin.removeListener('keypress', onKey);
}

// Scrollable read-only list. `rows` are { text, selectable, value? }. ↑/↓ or
// k/j move the highlight over selectable rows (scrolling the viewport).
// Enter/→ on a row that has a `value` resolves { pick: value }; ←/Esc/q resolve
// null. `footerHint` overrides the default key hint line.
function scrollScreen({ header, rows, footerHint, start }) {
  return new Promise(resolve => {
    const stdin = process.stdin;
    const repaint = makeRepainter();
    let firstRender = true;
    const selectableIdx = rows.map((r, i) => (r.selectable ? i : -1)).filter(i => i >= 0);
    const anyPickable = rows.some(r => r.value !== undefined);
    // Start on the row whose value matches `start` (so returning from a
    // sub-screen keeps your place), else the first selectable row.
    let pos = Math.max(0, selectableIdx.findIndex(i => rows[i].value === start));
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
      const hint = footerHint || ` ${color.dim('↑/↓')} move${anyPickable ? `    ${color.dim('Enter')} open` : ''}    ${color.dim(`${glyph.back}/Esc`)} back`;
      lines.push(hint);
      repaint(lines);
    };

    const finish = value => {
      endKeys(stdin, onKey);
      showCursor();
      resolve(value);
    };

    const onKey = (str, key) => {
      key = key || {};
      const name = key.name || str;
      if (key.ctrl && name === 'c') { showCursor(); process.exit(130); }
      if (['escape', 'left', 'h', 'q'].includes(name)) return finish(null);
      if (name === 'return' || name === 'enter' || name === 'right' || name === 'l') {
        const row = rows[selectableIdx[pos]];
        if (row && row.value !== undefined) return finish({ pick: row.value });
        if (!anyPickable) return finish(null);
        return;
      }
      if (!selectableIdx.length) return;
      if (name === 'up' || name === 'k') { pos = (pos + selectableIdx.length - 1) % selectableIdx.length; return render(); }
      if (name === 'down' || name === 'j') { pos = (pos + 1) % selectableIdx.length; return render(); }
      if (name === 'pageup') { pos = Math.max(0, pos - 5); return render(); }
      if (name === 'pagedown') { pos = Math.min(selectableIdx.length - 1, pos + 5); return render(); }
    };

    beginKeys(stdin, onKey);
    render();
  });
}

// A small in-terminal text editor — no shelling out to notepad/vim. Loads
// `file` (or starts empty if it doesn't exist yet), lets you move around and
// type, and writes it back on save. Unlike the other screens here it shows
// the real terminal cursor (positioned with a raw ANSI move after each
// repaint) instead of drawing a `❯` pointer — that's what makes it feel like
// an editor rather than a menu.
function editFile(file, { profiles = [], mark = -1, title } = {}) {
  return new Promise(resolve => {
    let lines = readSafeLines(file);
    let row = 0;
    let col = 0;
    let top = 0;
    let dirty = false;
    let saveError = null; // set when a save fails, shown until the next keypress
    const stdin = process.stdin;
    const repaint = makeRepainter();
    let firstRender = true;

    const content = () => lines.join('\n');
    // Never throws: another process (agent, editor, AV scanner) can hold the
    // file at the wrong moment. A failed save keeps your edits in the buffer
    // and reports it instead of crashing out of the keypress handler, which
    // would leave stdin's raw mode/listener stranded for whatever screen
    // comes next.
    const save = () => {
      try { fs.writeFileSync(file, `${content()}\n`); dirty = false; saveError = null; return true; }
      catch (error) { saveError = error.message; return false; }
    };
    const clamp = () => {
      row = Math.max(0, Math.min(row, lines.length - 1));
      col = Math.max(0, Math.min(col, lines[row].length));
    };

    const render = () => {
      if (firstRender) { showCursor(); firstRender = false; }
      clamp();
      const header = accountLines(profiles, mark);
      header.push(`${color.orange(glyph.back)} ${color.bold(title || path.basename(file))}${dirty ? color.yellow(' •  unsaved') : ''}`);
      header.push(color.dim(`   ${file}`));
      header.push('');
      const viewport = Math.max(5, (process.stdout.rows || 24) - header.length - 5);
      if (row < top) top = row;
      if (row >= top + viewport) top = row - viewport + 1;
      top = Math.max(0, Math.min(top, Math.max(0, lines.length - viewport)));
      const out = [...header];
      const end = Math.min(lines.length, top + viewport);
      if (top > 0) out.push(color.dim(`    ↑ ${top} more`));
      for (let i = top; i < end; i++) out.push(`${color.dim(String(i + 1).padStart(3))} ${lines[i]}`);
      if (end < lines.length) out.push(color.dim(`    ↓ ${lines.length - end} more`));
      out.push('');
      out.push(rule());
      if (saveError) out.push(` ${color.red(`Save failed: ${saveError}`)}`);
      out.push(` ${color.dim('Ctrl+S')} save   ${color.dim('Esc')} save & exit   ${color.dim('↑/↓/←/→')} move   ${color.dim('Ctrl+C')} quit without saving`);
      repaint(out);
      const gutter = 4; // "NNN " line-number column
      const termRow = header.length + 1 + (row - top) + (top > 0 ? 1 : 0);
      const termCol = 1 + gutter + col;
      process.stdout.write(`\x1b[${termRow};${termCol}H`);
    };

    const cleanup = () => { endKeys(stdin, onKey); };
    const finish = () => { cleanup(); resolve(); };

    const insert = text => {
      const line = lines[row];
      lines[row] = line.slice(0, col) + text + line.slice(col);
      col += text.length;
      dirty = true;
    };

    const onKey = (str, key) => {
      key = key || {};
      const name = key.name || '';
      if (key.ctrl && name === 'c') { showCursor(); process.exit(130); }

      try { handleKey(str, key, name); }
      catch (error) {
        // A bug here must never crash out of the listener with stdin still in
        // raw mode — that strands a dead listener for whatever screen comes
        // next, which then looks like the whole app has frozen.
        saveError = error.message;
        render();
      }
    };

    const handleKey = (str, key, name) => {
      if (key.ctrl && name === 's') { save(); return render(); }
      // Esc always saves & exits — matches the footer hint. A failed save
      // (locked file, etc.) keeps you in the editor with the error shown
      // instead of losing the buffer; discarding on purpose is Ctrl+C.
      if (name === 'escape') { if (!dirty || save()) return finish(); return render(); }
      if (name === 'up') { row--; return render(); }
      if (name === 'down') { row++; return render(); }
      if (name === 'left') {
        if (col > 0) col--;
        else if (row > 0) { row--; col = lines[row].length; }
        return render();
      }
      if (name === 'right') {
        if (col < lines[row].length) col++;
        else if (row < lines.length - 1) { row++; col = 0; }
        return render();
      }
      if (name === 'home') { col = 0; return render(); }
      if (name === 'end') { col = lines[row].length; return render(); }
      if (name === 'pageup') { row -= 10; return render(); }
      if (name === 'pagedown') { row += 10; return render(); }
      if (name === 'return' || name === 'enter') {
        const rest = lines[row].slice(col);
        lines[row] = lines[row].slice(0, col);
        lines.splice(row + 1, 0, rest);
        row++; col = 0; dirty = true;
        return render();
      }
      if (name === 'backspace') {
        if (col > 0) { lines[row] = lines[row].slice(0, col - 1) + lines[row].slice(col); col--; dirty = true; }
        else if (row > 0) { col = lines[row - 1].length; lines[row - 1] += lines[row]; lines.splice(row, 1); row--; dirty = true; }
        return render();
      }
      if (name === 'delete') {
        if (col < lines[row].length) { lines[row] = lines[row].slice(0, col) + lines[row].slice(col + 1); dirty = true; }
        else if (row < lines.length - 1) { lines[row] += lines[row + 1]; lines.splice(row + 1, 1); dirty = true; }
        return render();
      }
      if (name === 'tab') { insert('  '); return render(); }
      if (str && !key.ctrl && !key.meta && str >= ' ') { insert(str); return render(); }
    };

    beginKeys(stdin, onKey);
    render();
  });
}

function readSafeLines(file) {
  let content = '';
  try { content = fs.readFileSync(file, 'utf8'); } catch { /* new file */ }
  const lines = content.split(/\r?\n/);
  return lines.length ? lines : [''];
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

function termWidth() {
  return Math.min(process.stdout.columns || 80, 100);
}

// Shorten a plain string to `max` visible chars, keeping both ends (best for
// file paths — the tail says which plugin/version it is).
function clipMid(text, max) {
  const s = String(text);
  if (s.length <= max || max < 8) return s;
  const keep = max - 1;
  return s.slice(0, Math.ceil(keep / 2)) + '…' + s.slice(s.length - Math.floor(keep / 2));
}

// A two-column "key   value" line, key dimmed, aligned to `w`.
function kv(key, value, w = 14) {
  return `    ${color.dim(padTo(key, w))}${value}`;
}

// Break a path/command into terminal-width chunks, preferring a break right
// after a path separator so each piece still reads like a path fragment.
function wrapHard(text, width) {
  const s = String(text);
  if (s.length <= width) return [s];
  const parts = [];
  let rest = s;
  while (rest.length > width) {
    const window = rest.slice(0, width);
    const sep = Math.max(window.lastIndexOf('\\'), window.lastIndexOf('/'));
    const cut = sep > width * 0.4 ? sep + 1 : width;
    parts.push(rest.slice(0, cut));
    rest = rest.slice(cut);
  }
  if (rest) parts.push(rest);
  return parts;
}

// Dimmed value wrapped onto continuation lines (indented under the value
// column) instead of being cut off — for paths / long commands.
function kvWrap(key, text, w = 14) {
  const indent = ' '.repeat(4 + w);
  const parts = wrapHard(text, termWidth() - 4 - w);
  return parts.map((part, i) => (i === 0 ? kv(key, color.dim(part), w) : `${indent}${color.dim(part)}`));
}

const plural = (n, word) => `${n} ${word}${n === 1 ? '' : 's'}`;
const shortId = id => id.replace('@claude-plugins-official', '@official');

// Overview screen for a profile: account, activity/storage, then a pickable
// list of plugins / skills / MCP servers (Enter opens per-item detail).
// `snap` is a cached inspect.buildInspection() result — no filesystem work here.
function profileOverview(snap, name, profiles, mark) {
  const { account: acc, activity: act, plugins, skills, mcps } = snap;

  const header = accountLines(profiles, mark);
  header.push(`${color.orange(glyph.back)} ${color.orange(name)}  ${color.dim('— details')}`);
  header.push('');

  const rows = [];
  const head = title => rows.push({ text: `  ${color.orangeBold(title)}`, selectable: false });
  const line = text => rows.push({ text, selectable: false });
  const gap = () => rows.push({ text: '', selectable: false });

  head('Account');
  line(kv('email', `${acc.email}${acc.name ? color.dim(`  (${acc.name})`) : ''}`));
  line(kv('plan', acc.plan));
  line(kv('org', acc.org));
  line(kv('rate tier', acc.rateTier));
  line(kv('created', `${acc.created}${acc.subCreated !== '—' ? color.dim(`  · sub ${acc.subCreated}`) : ''}`));
  line(kv('token', `expires ${acc.tokenExpiry}`));
  gap();
  head('Activity');
  line(kv('startups', `${act.startups}${act.firstStart !== '—' ? color.dim(`  · first ${act.firstStart}`) : ''}`));
  line(kv('projects', String(act.projects)));
  line(kv('sessions', `${act.sessionCount}  ${color.dim(`${inspect.humanBytes(act.sessionBytes)} · last ${relativeAge(act.lastActive ? new Date(act.lastActive) : null)}`)}`));
  line(kv('history', inspect.humanBytes(act.historyBytes)));
  line(kv('disk total', inspect.humanBytes(act.totalBytes)));
  gap();

  const section = (title, items, render, kind) => {
    head(`${title} ${color.dim(`(${items.length})`)}`);
    if (!items.length) { line(`    ${color.dim('none')}`); gap(); return; }
    items.forEach((item, i) => rows.push({ text: `  ${render(item)}`, selectable: true, value: `${kind}:${i}` }));
    gap();
  };
  section('Plugins', plugins, p =>
    `${padTo(shortId(p.id) + (p.enabled ? '' : ' (off)'), 32)}${color.dim(`${padTo(inspect.humanBytes(p.size.bytes), 9)}${p.size.mdBytes ? inspect.estTokens(p.size.mdBytes) : ''}`)}`, 'plugin');
  section('Skills', skills, s =>
    `${padTo(s.name, 32)}${color.dim(`${padTo(inspect.humanBytes(s.bytes), 9)}${inspect.estTokens(s.mdBytes || s.bytes)}`)}`, 'skill');
  section('MCP servers', mcps, m => `${padTo(m.name, 24)}${color.dim(m.type)}`, 'mcp');

  return { header, rows };
}

// Per-item detail screen. `pick` is "kind:index" from profileOverview rows.
function itemDetail(snap, pick, profiles, mark) {
  const [kind, idxRaw] = pick.split(':');
  const idx = Number(idxRaw);
  const header = accountLines(profiles, mark);
  const rows = [];
  const line = text => rows.push({ text, selectable: false });

  if (kind === 'plugin') {
    const p = snap.plugins[idx];
    header.push(`${color.orange(glyph.back)} ${color.orange(clipMid(p.id, termWidth() - 12))}  ${color.dim('— plugin')}`);
    header.push('');
    line(kv('marketplace', p.marketplace));
    line(kv('version', p.version));
    line(kv('enabled', p.enabled ? color.green('yes') : color.dim('no')));
    line(kv('scope', p.scope));
    line(kv('installed', p.installedAt));
    line(kv('updated', p.lastUpdated));
    line(kv('commit', p.sha));
    line(kv('size', p.size.bytes ? `${inspect.humanBytes(p.size.bytes)}  ${color.dim(plural(p.size.files, 'file'))}` : color.dim('not on disk')));
    if (p.size.mdBytes) line(kv('instructions', `${inspect.humanBytes(p.size.mdBytes)} of markdown  ${color.dim(`${inspect.estTokens(p.size.mdBytes)} if all loaded`)}`));
    if (p.contents) line(kv('contents', `${plural(p.contents.skills, 'skill')} · ${plural(p.contents.commands, 'command')} · ${plural(p.contents.agents, 'agent')}`));
    line('');
    for (const l of kvWrap('path', p.path.replace(os.homedir(), '~'))) line(l);
  } else if (kind === 'skill') {
    const s = snap.skills[idx];
    header.push(`${color.orange(glyph.back)} ${color.orange(s.name)}  ${color.dim('— skill')}`);
    header.push('');
    line(kv('size', `${inspect.humanBytes(s.bytes)}  ${color.dim(plural(s.fileCount, 'file'))}`));
    line(kv('tokens', `${inspect.estTokens(s.mdBytes || s.bytes)}  ${color.dim('(SKILL.md loads on trigger; the rest on demand)')}`));
    line(kv('files', s.files.join(', ') || '—'));
    line('');
    line(`  ${color.dim('description')}`);
    for (const chunk of wrapText(s.description || '—', 72)) line(`    ${chunk}`);
  } else {
    const m = snap.mcps[idx];
    header.push(`${color.orange(glyph.back)} ${color.orange(m.name)}  ${color.dim('— MCP server')}`);
    header.push('');
    line(kv('type', m.type));
    for (const l of kvWrap('command', m.command)) line(l);
    for (const l of kvWrap('url', m.url)) line(l);
    line(kv('env', m.envKeys.length ? `${m.envKeys.join(', ')}  ${color.dim('(values hidden)')}` : '—'));
    line('');
    line(`  ${color.dim('Token cost is decided at runtime by the tool definitions this')}`);
    line(`  ${color.dim('server returns — it cannot be measured from disk.')}`);
  }
  return { header, rows, footerHint: ` ${color.dim(`${glyph.back}/Esc/Enter`)} back` };
}

function wrapText(text, width) {
  const words = String(text).split(/\s+/);
  const lines = [];
  let cur = '';
  for (const word of words) {
    if ((cur + ' ' + word).trim().length > width) { if (cur) lines.push(cur); cur = word; }
    else cur = (cur ? `${cur} ` : '') + word;
  }
  if (cur) lines.push(cur);
  return lines.length ? lines : ['—'];
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
    const done = value => {
      endKeys(stdin, onKey);
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
    beginKeys(stdin, onKey);
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
      endKeys(stdin, onKey);
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

    beginKeys(stdin, onKey);
    render();
  });
}

// Arrow-key list picker. Shows the account list for context, then `options`
// with a ❯ pointer. ↑/↓ or k/j move, Enter selects, ←/Esc returns BACK.
function chooseFromList({ profiles, heading, hint, options, mark = -1, externalKey }) {
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
      const externalHint = externalKey ? `    ${color.dim(externalKey)} external editor` : '';
      lines.push(` ${color.dim('↑/↓')} move    ${color.dim('Enter')} select${externalHint}    ${color.dim(`${glyph.back}/Esc`)} back`);
      repaint(lines);
    };

    const cleanup = () => {
      endKeys(stdin, onKey);
      showCursor();
    };
    const done = value => { cleanup(); resolve(value); };

    const onKey = (str, key) => {
      key = key || {};
      const name = key.name || str;
      notes.debugLog(`chooseFromList keypress: ${name}`);
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
      if (externalKey && name === externalKey) return done({ pick: options[selected].value, external: true });
    };

    beginKeys(stdin, onKey);
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
      prefetchInspection(profiles[selected].name); // so → usually opens instantly
      const lines = accountLines(profiles, selected, stats);
      lines.push(...hintGrid([
        [['↑/↓', 'move'], ['→', 'view'], ['n', 'notes'], ['r', 'refresh'], ['q', 'quit']],
        [['a', 'add'], ['i', 'import'], ['d', 'delete'], ['s', 'sync']],
      ]));
      repaint(lines);
    };

    const cleanup = () => {
      endKeys(stdin, onKey);
      showCursor();
    };

    const finish = action => {
      cleanup();
      resolve(action);
    };

    const onKey = (str, key) => {
      key = key || {};
      const name = key.name || str;
      notes.debugLog(`pickProfile keypress: ${name}`);
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
      if (name === 'n') return finish({ type: 'notes' });
      if (name === 'a') return finish({ type: 'add' });
      if (name === 'i') return finish({ type: 'import' });
      if (name === 's') return finish({ type: 'sync', name: profiles[selected].name });
      if (name === 'd') return finish({ type: 'delete', name: profiles[selected].name });
      if (name === 'r') return finish({ type: 'refresh' });
    };

    beginKeys(stdin, onKey);
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
      if (pick === BACK || pick === 'quit') { leaveAlt(); process.exit(0); }
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
        try { createAccount(name.trim()); console.log(color.green(`Created '${name.trim()}'.`)); }
        catch (error) { console.error(color.red(`Error: ${error.message}`)); await ask('Press Enter...'); }
        continue;
      }
      continue;
    }

    cursor = Math.max(0, Math.min(cursor, profiles.length - 1));
    const action = await pickProfile(profiles, cursor);
    const mark = profiles.findIndex(p => p.name === action.name);
    if (mark >= 0) cursor = mark;
    // Explicit process.exit: since screens now deliberately leave stdin
    // resumed between transitions (that's the fix for the stdin-wedging
    // bug), a resumed stream keeps the event loop alive forever — without
    // this, quitting would just leave the process sitting there doing
    // nothing instead of actually returning you to the shell.
    if (action.type === 'quit') { leaveAlt(); process.exit(0); }
    if (action.type === 'refresh') { invalidateInspection(); continue; }
    if (action.type === 'detail') {
      // No intermediate "loading" paint here — that would be a second screen
      // swap (picker -> loading -> overview) and read as a flicker. One
      // blocking build, then scrollScreen paints straight over the picker.
      const snap = getInspection(action.name);
      let at;
      for (;;) {
        const view = profileOverview(snap, action.name, profiles, mark);
        view.start = at;
        const res = await scrollScreen(view);
        if (!res) break;
        at = res.pick;
        await scrollScreen(itemDetail(snap, res.pick, profiles, mark));
      }
      continue;
    }
    if (action.type === 'notes') {
      notes.debugLog('cli: notes action start');
      const root = notes.projectRoot(process.cwd());
      notes.debugLog(`cli: projectRoot resolved to ${root}`);
      const master = notes.masterPaths();
      notes.ensureProjectNotes(root);
      notes.debugLog('cli: ensureProjectNotes returned, opening chooser');
      const choice = await chooseFromList({
        profiles,
        heading: `Notes for ${color.orange(path.basename(root))}`,
        hint: 'Enter edits it here — e for your $EDITOR instead',
        options: [
          { label: 'TODO.md', value: path.join(root, 'TODO.md'), note: 'this project' },
          { label: 'PLAN.md', value: path.join(root, 'PLAN.md'), note: 'this project' },
          { label: 'Master TODO.md', value: master.todo, note: 'every project' },
          { label: 'Master PLAN.md', value: master.plan, note: 'every project' },
        ],
        externalKey: 'e',
      });
      notes.debugLog(`cli: chooser resolved: ${JSON.stringify(choice)}`);
      if (choice === BACK) { notes.debugLog('cli: notes action back'); continue; }
      const file = choice.pick ?? choice;
      if (choice.external) await openInEditor(file);
      else await editFile(file, { profiles, title: path.basename(file) });
      notes.debugLog('cli: editor closed, syncing');
      const session = notes.createSession(root); // one-shot: reconcile both ways, then stop
      session.stop();
      notes.debugLog('cli: notes action done');
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
      try { syncProfiles(action.name, target, spec); invalidateInspection(target); }
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
        try { removeProfile(action.name); invalidateInspection(action.name); continue; } // straight back to the menu
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
        const profile = createAccount(name.trim());
        leaveAlt();
        clearScreen(); // primary buffer keeps scrolling across launches; wipe it first
        console.log(color.green(`Created '${profile.name}'. Launching Claude — run /login inside it.`));
        await launch(profile.name, [], { interactive: true });
        enterAlt();
      } catch (error) { enterAlt(); console.error(color.red(`Error: ${error.message}`)); await ask('Press Enter...'); }
      continue;
    }
    if (action.type === 'open') {
      leaveAlt(); // give Claude the real terminal (its own alt screen, scrollback)
      clearScreen(); // primary buffer keeps scrolling across launches; wipe it first
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

  const where = spawnSync('where', ['claude'], { encoding: 'utf8', timeout: 3000 });
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

// Open a file in the user's editor ($VISUAL / $EDITOR, else notepad/nano) and
// resolve once it's closed. Handles a multi-word editor command like "code
// --wait". SIGINT is ignored here the same way it is around a Claude launch,
// so Ctrl+C inside the editor doesn't take the cockpit down with it.
function openInEditor(file) {
  return new Promise(resolve => {
    resetStdin();
    const editorCmd = process.env.VISUAL || process.env.EDITOR || (process.platform === 'win32' ? 'notepad' : 'nano');
    const parts = editorCmd.match(/(?:[^\s"]+|"[^"]*")+/g) || [editorCmd];
    const strip = s => s.replace(/^"|"$/g, '');
    const cmd = strip(parts[0]);
    const cmdArgs = [...parts.slice(1).map(strip), file];
    const child = spawn(cmd, cmdArgs, { stdio: 'inherit', shell: process.platform === 'win32' });
    const ignore = () => {};
    process.on('SIGINT', ignore);
    process.on('SIGBREAK', ignore);
    const restore = () => { process.removeListener('SIGINT', ignore); process.removeListener('SIGBREAK', ignore); };
    child.on('exit', () => { restore(); resolve(); });
    child.on('error', error => {
      restore();
      console.error(color.red(`Could not open editor (${cmd}): ${error.message}`));
      console.error(color.dim('Set $env:EDITOR to your preferred editor.'));
      resolve();
    });
  });
}

// Spawn Claude with the profile's isolated config dir.
// interactive:true resolves when Claude exits (so the cockpit menu can resume);
// otherwise the process exits with Claude's code.
function launch(profile, args, { interactive = false } = {}) {
  return new Promise(resolve => {
    resetStdin(); // release stdin so Claude's own prompt gets the keyboard

    // TODO.md / PLAN.md: ensure they exist + are .gitignore'd in whatever
    // project Claude is about to run in, and keep them mirrored into the
    // master notes files for as long as this session runs.
    const projectRoot = notes.projectRoot(process.cwd());
    const notesSession = notes.createSession(projectRoot);
    if (notesSession.tracked.length) {
      console.log(color.dim(
        `Note: ${notesSession.tracked.join(', ')} already tracked by git — ` +
        `run 'git rm --cached ${notesSession.tracked.join(' ')}' if you want them untracked.`,
      ));
    }

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
    const cleanup = () => {
      process.removeListener('SIGINT', ignore);
      process.removeListener('SIGBREAK', ignore);
      notesSession.stop();
    };
    child.on('exit', code => {
      cleanup();
      if (interactive) resolve(code ?? 0);
      else process.exit(code ?? 0);
    });
    child.on('error', error => {
      cleanup();
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
    if (command === 'add') { const p = createAccount(args[0]); console.log(`Created profile '${p.name}' at ${paths.profileDir(p.name)}`); return; }
    if (command === 'import') return importProfile(args[0], args[1]);
    if (command === 'sync') { const [fromP, toP, what] = args; return syncProfiles(fromP, toP, what || 'all'); }
    if (command === 'notes') {
      const dir = args[0] ? path.resolve(args[0]) : process.cwd();
      const root = notes.projectRoot(dir);
      const session = notes.createSession(root);
      session.stop();
      console.log(`Synced ${color.orange(root)} <-> ${color.dim(notes.masterPaths().todo)}`);
      if (session.tracked.length) {
        console.log(color.dim(
          `Note: ${session.tracked.join(', ')} already tracked by git — ` +
          `run 'git rm --cached ${session.tracked.join(' ')}' if you want them untracked.`,
        ));
      }
      return;
    }
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

if (require.main === module) main(process.argv.slice(2));

module.exports = { profileOverview, itemDetail, syncProfiles, profileInventory, inspect, editFile, chooseFromList, pickProfile, cockpit };
