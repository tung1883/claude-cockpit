#!/usr/bin/env node
'use strict';
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const readline = require('node:readline');
const { spawn } = require('node:child_process');
const { addProfile, getProfile, listProfiles, removeProfile } = require('./profiles');
const paths = require('./paths');
const { copySession } = require('./handoff');
const { color, glyph, clearScreen, hideCursor, showCursor, banner, rule, makeRepainter } = require('./ui');

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
// `what` is 'plugins' | 'skills' | 'mcp' | 'all'.
function syncProfiles(fromName, toName, what = 'all') {
  const from = getProfile(fromName);
  const to = getProfile(toName);
  if (from.name === to.name) throw new Error('Source and target are the same profile.');
  const valid = ['plugins', 'skills', 'mcp'];
  const want = what === 'all' ? valid : [what];
  if (!want.every(w => valid.includes(w))) throw new Error('what must be one of: plugins, skills, mcp, all');
  const src = paths.profileDir(from.name);
  const dst = paths.profileDir(to.name);
  const srcSettings = path.join(src, 'settings.json');
  const dstSettings = path.join(dst, 'settings.json');

  if (want.includes('plugins')) {
    copyDir(path.join(src, 'plugins'), path.join(dst, 'plugins'));
    const s = readJson(srcSettings);
    const d = readJson(dstSettings);
    d.enabledPlugins = { ...d.enabledPlugins, ...s.enabledPlugins };
    d.extraKnownMarketplaces = { ...d.extraKnownMarketplaces, ...s.extraKnownMarketplaces };
    writeJson(dstSettings, d);
    console.log(color.green(`Synced plugins ${from.name} -> ${to.name}`));
  }

  if (want.includes('skills')) {
    const copied = copyDir(path.join(src, 'skills'), path.join(dst, 'skills'));
    console.log(color.green(`Synced skills ${from.name} -> ${to.name}${copied ? '' : ' (none found)'}`));
  }

  if (want.includes('mcp')) {
    const sc = readJson(path.join(src, '.claude.json'));
    const dc = readJson(path.join(dst, '.claude.json'));
    dc.mcpServers = { ...dc.mcpServers, ...sc.mcpServers };
    writeJson(path.join(dst, '.claude.json'), dc);
    const s = readJson(srcSettings);
    const d = readJson(dstSettings);
    d.mcpServers = { ...d.mcpServers, ...s.mcpServers };
    if (s.enabledMcpjsonServers) {
      d.enabledMcpjsonServers = [...new Set([...(d.enabledMcpjsonServers || []), ...s.enabledMcpjsonServers])];
    }
    writeJson(dstSettings, d);
    const count = Object.keys({ ...sc.mcpServers, ...s.mcpServers }).length;
    console.log(color.green(`Synced ${count} MCP server${count === 1 ? '' : 's'} ${from.name} -> ${to.name}`));
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

function accountStat(name) {
  const sessions = sessionFiles(name);
  return { count: sessions.length, age: relativeAge(sessions[0]?.modified) };
}

// The account list. Shown at all times — in the picker and above every sub-prompt.
function accountLines(profiles, selected = -1, stats = null) {
  const lines = [
    banner(),
    '',
    color.dim('  Pick an account to launch Claude with an isolated profile.'),
    '',
  ];
  profiles.forEach((profile, index) => {
    const active = index === selected;
    const marker = active ? color.orange(glyph.pointer) : ' ';
    const name = active ? color.orangeBold(profile.name) : profile.name;
    const pad = ' '.repeat(Math.max(0, 18 - profile.name.length));
    const st = stats ? stats[index] : accountStat(profile.name);
    lines.push(
      ` ${marker} ${name}${pad} ` +
      `${color.dim(String(st.count).padStart(3) + ' sessions')}   ${color.dim(st.age)}`,
    );
  });
  lines.push('');
  lines.push(rule());
  return lines;
}

// Returned by promptLine when the user asks to go back (`<-` or Esc).
const BACK = Symbol('back');

// Repaint the persistent account list, then a feature heading with a back hint.
function subScreen(profiles, heading) {
  clearScreen();
  const lines = accountLines(profiles);
  lines.push(`${color.dim('<-')} ${color.bold(heading)}`);
  lines.push(color.dim('   type <- or press Esc to go back'));
  lines.push('');
  process.stdout.write(lines.join('\n') + '\n');
}

// A line prompt that also resolves to BACK on `<-` (then Enter) or a lone Esc.
function promptLine(question) {
  return new Promise(resolve => {
    const stdin = process.stdin;
    let buf = '';
    process.stdout.write(`${color.dim('<-')} ${question}`);
    readline.emitKeypressEvents(stdin);
    const wasRaw = Boolean(stdin.isRaw);
    if (stdin.isTTY) stdin.setRawMode(true);
    stdin.resume();
    const done = value => {
      stdin.removeListener('keypress', onKey);
      if (stdin.isTTY) stdin.setRawMode(wasRaw);
      process.stdout.write('\n');
      resolve(value);
    };
    const onKey = (str, key) => {
      key = key || {};
      const name = key.name || '';
      if (name === 'escape') return done(BACK);
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

// Interactive account picker. Resolves to an action the caller loop acts on.
function pickProfile(profiles) {
  return new Promise(resolve => {
    let selected = 0;
    const stdin = process.stdin;
    // Read session stats once — the picker list is static while it is open.
    const stats = profiles.map(p => accountStat(p.name));
    const repaint = makeRepainter();
    let firstRender = true;

    const render = () => {
      if (firstRender) { clearScreen(); hideCursor(); firstRender = false; }
      const lines = accountLines(profiles, selected, stats);
      lines.push(` ${color.dim('↑/↓')} move    ${color.dim('Enter')} launch    ${color.dim('r')} refresh    ${color.dim('q')} quit`);
      lines.push(` ${color.dim('a')} add    ${color.dim('i')} import    ${color.dim('s')} sync plugins/skills/mcp    ${color.dim('d')} delete`);
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

  for (;;) {
    let profiles = listProfiles();
    if (!profiles.length) {
      console.log(`${banner()}\n`);
      console.log(color.dim('  No accounts yet.'));
      console.log(`  ${color.dim('1')} import this machine's existing login (${color.dim('~/.claude')})`);
      console.log(`  ${color.dim('2')} create a fresh empty account`);
      const pick = (await ask('  choice (blank to quit): ')).trim();
      if (pick === '1') {
        const name = (await ask('  profile name [main]: ')).trim() || 'main';
        try { importProfile(name); } catch (error) { console.error(color.red(`Error: ${error.message}`)); await ask('Press Enter...'); }
        continue;
      }
      if (pick === '2') {
        const name = (await ask('  account name: ')).trim();
        if (!name) return;
        try { addProfile(name); console.log(color.green(`Created '${name}'.`)); }
        catch (error) { console.error(color.red(`Error: ${error.message}`)); }
        continue;
      }
      return;
    }

    const action = await pickProfile(profiles);
    if (action.type === 'quit') { clearScreen(); return; }
    if (action.type === 'refresh') continue;
    if (action.type === 'import') {
      subScreen(profiles, 'Import an existing login into a new profile');
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
        subScreen(profiles, `Sync from '${action.name}'`);
        console.log(color.dim('Need a second profile to sync into.'));
        await ask('Press Enter...');
        continue;
      }
      subScreen(profiles, `Copy plugins/skills/mcp from '${action.name}' into which profile?`);
      others.forEach((p, i) => console.log(`  ${color.dim(String(i + 1))} ${p.name}`));
      console.log('');
      const target = await promptLine('target #: ');
      if (target === BACK) continue;
      const pick = others[Number(target.trim()) - 1];
      if (!pick) { console.log(color.dim('Cancelled.')); await ask('Press Enter...'); continue; }
      const what = await promptLine('what [all/plugins/skills/mcp]: ');
      if (what === BACK) continue;
      try { syncProfiles(action.name, pick.name, what.trim() || 'all'); }
      catch (error) { console.error(color.red(`Error: ${error.message}`)); }
      await ask('Press Enter to return to the menu...');
      continue;
    }
    if (action.type === 'delete') {
      subScreen(profiles, `Delete profile '${action.name}'`);
      console.log(color.red(`Everything under ${paths.profileDir(action.name)} will be removed.`));
      console.log(color.dim('That account\'s login, sessions, plugins, and settings. Cannot be undone.'));
      console.log('');
      const confirm = await promptLine('Type the profile name to confirm: ');
      if (confirm === BACK) continue;
      if (confirm.trim() === action.name) {
        try { removeProfile(action.name); console.log(color.green(`Deleted '${action.name}'.`)); }
        catch (error) { console.error(color.red(`Error: ${error.message}`)); }
      } else {
        console.log(color.dim('Cancelled.'));
      }
      await ask('Press Enter to return to the menu...');
      continue;
    }
    if (action.type === 'add') {
      subScreen(profiles, 'Create a fresh account');
      const name = await promptLine('New account name: ');
      if (name === BACK || !name.trim()) continue;
      try {
        const profile = addProfile(name.trim());
        console.log(color.green(`Created '${profile.name}'. Launching Claude — run /login inside it.`));
        await launch(profile.name, [], { interactive: true });
      } catch (error) { console.error(color.red(`Error: ${error.message}`)); await ask('Press Enter...'); }
      continue;
    }
    if (action.type === 'open') {
      clearScreen();
      console.log(color.dim(`${glyph.spark} launching Claude as '${action.name}' — exit Claude to return here\n`));
      await launch(action.name, [], { interactive: true });
      // fall through: loop repaints the menu
    }
  }
}

// Spawn Claude with the profile's isolated config dir.
// interactive:true resolves when Claude exits (so the cockpit menu can resume);
// otherwise the process exits with Claude's code.
function launch(profile, args, { interactive = false } = {}) {
  return new Promise(resolve => {
    const command = process.env.CLAUDE_BIN || (process.platform === 'win32' ? 'claude.cmd' : 'claude');
    const child = spawn(command, args, {
      stdio: 'inherit',
      env: { ...process.env, CLAUDE_CONFIG_DIR: paths.profileDir(profile) },
      windowsHide: false,
      shell: process.platform === 'win32',
    });
    child.on('exit', code => {
      if (interactive) resolve(code ?? 0);
      else process.exit(code ?? 0);
    });
    child.on('error', error => {
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
  } catch (error) { console.error(color.red(`Error: ${error.message}`)); process.exitCode = 1; }
}

main(process.argv.slice(2));
