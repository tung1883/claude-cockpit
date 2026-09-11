'use strict';
// Per-project TODO.md / PLAN.md, auto-created and .gitignore'd, mirrored into
// one global master TODO.md / PLAN.md so every project's notes show up in one
// place. Sync is bidirectional and live while a cockpit-launched Claude
// session is running: editing either the project file or the project's
// section in the master file propagates to the other side.
//
// Conflict rule: last write wins. A per-project content hash (notes-state.json)
// tells us which side actually changed since the last sync, so an untouched
// side never clobbers a genuinely edited one; if both changed, whichever file
// has the newer mtime wins.
const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');
const { spawnSync } = require('node:child_process');
const paths = require('./paths');

const FILES = { todo: 'TODO.md', plan: 'PLAN.md' };
const SKELETON = { todo: '# TODO\n\n', plan: '# PLAN\n\n' };
const GITIGNORE_MARKER = '# ccpit';
const DEBOUNCE_MS = 300;

function enabled() {
  return process.env.CLAUDE_COCKPIT_NOTES !== '0';
}

// Breadcrumb trail for tracking down a freeze: enable with
// CLAUDE_COCKPIT_NOTES_DEBUG=1, then check the tail of the log after a hang —
// the last line printed is the call that never returned.
function debugLog(msg) {
  if (!process.env.CLAUDE_COCKPIT_NOTES_DEBUG) return;
  try { fs.appendFileSync(path.join(paths.root, 'notes-debug.log'), `${new Date().toISOString()} ${msg}\n`); }
  catch { /* logging must never be the thing that breaks */ }
}

function notesDir() {
  return process.env.CLAUDE_COCKPIT_NOTES_DIR || paths.root;
}

function masterPaths() {
  const dir = notesDir();
  return { todo: path.join(dir, 'TODO.md'), plan: path.join(dir, 'PLAN.md') };
}

function statePath() {
  return path.join(paths.root, 'notes-state.json');
}

function hash(content) {
  return crypto.createHash('sha1').update(String(content).trim()).digest('hex');
}

function readSafe(file) {
  try { return fs.readFileSync(file, 'utf8'); } catch { return ''; }
}

function mtimeOf(file) {
  try { return fs.statSync(file).mtimeMs; } catch { return 0; }
}

// Never throws: a concurrently-open file (another agent/editor/AV scanner
// mid-read on Windows) can make writeFileSync/renameSync fail transiently.
// Returns false on failure so the caller can skip updating sync state and
// just retry on the next reconcile instead of crashing or wedging the UI.
function writeAtomic(file, content) {
  try {
    fs.mkdirSync(path.dirname(file), { recursive: true });
    const tmp = `${file}.tmp-${process.pid}-${Date.now()}`;
    fs.writeFileSync(tmp, content);
    fs.renameSync(tmp, file);
    return true;
  } catch { return false; }
}

function readState() {
  try { return JSON.parse(fs.readFileSync(statePath(), 'utf8')); }
  catch { return {}; }
}

function writeState(state) {
  try { writeAtomic(statePath(), JSON.stringify(state, null, 2)); }
  catch { /* best effort — a lost sync-state entry just means one extra reconcile */ }
}

// A bounded timeout matters here specifically: this runs synchronously on
// every notes action (once for projectRoot, once per file for
// alreadyTracked), and it's exactly the kind of call that can hang the whole
// process if another git operation is holding a lock at that moment — with no
// timeout, that hang is indistinguishable from the app freezing.
function git(args, cwd) {
  debugLog(`git start: ${args.join(' ')} (cwd=${cwd})`);
  try {
    const result = spawnSync('git', args, { cwd, encoding: 'utf8', timeout: 3000 });
    debugLog(`git done: ${args.join(' ')} status=${result.status} error=${result.error?.code || 'none'}`);
    return result.status === 0 ? result.stdout.trim() : '';
  } catch (error) { debugLog(`git threw: ${args.join(' ')} ${error.message}`); return ''; }
}

// The project root is the git top-level if `cwd` is inside a repo, so one
// TODO/PLAN pair covers the whole project instead of one per subdirectory.
// Memoized per cwd — a directory's git root doesn't change mid-session, and
// this avoids re-shelling out to git on every single notes action.
const projectRootCache = new Map();
function projectRoot(cwd) {
  if (!projectRootCache.has(cwd)) projectRootCache.set(cwd, git(['rev-parse', '--show-toplevel'], cwd) || cwd);
  return projectRootCache.get(cwd);
}

function ensureFile(file, skeleton) {
  try { if (!fs.existsSync(file)) fs.writeFileSync(file, skeleton); }
  catch { /* best effort — a locked/missing-permission path just skips creation this time */ }
}

// Always adds the .gitignore block, git repo or not — harmless outside one,
// and it's there ready if the directory is ever git-inited later.
function ensureGitignore(root) {
  try {
    const file = path.join(root, '.gitignore');
    const existing = readSafe(file);
    if (existing.includes(GITIGNORE_MARKER)) return;
    const block = `${GITIGNORE_MARKER}\n${FILES.todo}\n${FILES.plan}\n`;
    const sep = existing && !existing.endsWith('\n') ? '\n' : '';
    fs.writeFileSync(file, existing ? `${existing}${sep}\n${block}` : block);
  } catch { /* best effort */ }
}

// If TODO.md/PLAN.md were already tracked by git before we got here, adding
// them to .gitignore doesn't untrack them — that needs `git rm --cached`,
// which rewrites history the user might not want done automatically.
// Memoized per root for the same reason as projectRoot: this otherwise reruns
// on every single notes action (ensureProjectNotes is called both directly
// and again inside createSession).
const trackedCache = new Map();
function alreadyTracked(root) {
  if (!trackedCache.has(root)) {
    trackedCache.set(root, Object.values(FILES).filter(name => git(['ls-files', name], root) === name));
  }
  return trackedCache.get(root);
}

// Create the project's files/.gitignore if missing. Returns any files that
// were already git-tracked, so the caller can warn instead of silently doing
// nothing about it.
function ensureProjectNotes(root) {
  debugLog(`ensureProjectNotes start: ${root}`);
  if (!enabled()) return { tracked: [] };
  ensureFile(path.join(root, FILES.todo), SKELETON.todo);
  debugLog('ensureProjectNotes: todo file ensured');
  ensureFile(path.join(root, FILES.plan), SKELETON.plan);
  debugLog('ensureProjectNotes: plan file ensured');
  ensureGitignore(root);
  debugLog('ensureProjectNotes: gitignore ensured');
  let tracked = [];
  try { tracked = alreadyTracked(root); } catch { /* not fatal */ }
  debugLog(`ensureProjectNotes done: tracked=${JSON.stringify(tracked)}`);
  return { tracked };
}

// --- Master file section format -------------------------------------------
// ## <project name>
// <!-- ccpit:path=<absolute project path> -->
// <body>
//
// The marker is the real key (a directory can be renamed/moved and matching
// still works as long as the path is unchanged); the heading above it is
// cosmetic and gets refreshed on every sync. A heading is only treated as a
// section boundary when a marker immediately follows it, so a project's own
// "## something" inside its body text does not get mistaken for one.

const MARKER_RE = /<!-- ccpit:path=(.*?) -->/g;

function findSections(content) {
  const markers = [];
  let m;
  MARKER_RE.lastIndex = 0;
  while ((m = MARKER_RE.exec(content))) markers.push({ key: m[1], start: m.index, end: MARKER_RE.lastIndex });
  // Each section's heading line is one line above its marker; compute all of
  // them up front so section i's end is exactly section (i+1)'s heading start
  // — not the (i+1) marker's own start, which is one line further down.
  // marker.start - 1 is itself the newline that ends the heading line right
  // above the marker, so lastIndexOf must search strictly before that (- 2),
  // or it just finds that same newline and "heading start" collapses onto
  // the marker line instead of the line above it.
  const headingStarts = markers.map(marker => content.lastIndexOf('\n', marker.start - 2) + 1);
  return markers.map((marker, i) => {
    const sectionEnd = i + 1 < markers.length ? headingStarts[i + 1] : content.length;
    const nl = content.indexOf('\n', marker.end);
    const bodyStart = nl === -1 ? content.length : nl + 1;
    return {
      key: marker.key,
      start: headingStarts[i],
      end: sectionEnd,
      body: content.slice(bodyStart, sectionEnd).trimEnd(),
    };
  });
}

function getSectionBody(content, key) {
  const section = findSections(content).find(s => s.key === key);
  return section ? section.body : null;
}

function upsertSection(content, key, name, body) {
  const block = `## ${name}\n<!-- ccpit:path=${key} -->\n${body.trim()}\n`;
  const sections = findSections(content);
  const existing = sections.find(s => s.key === key);
  if (existing) return `${content.slice(0, existing.start)}${block}${content.slice(existing.end)}`;
  const trimmed = content.trimEnd();
  return trimmed ? `${trimmed}\n\n${block}` : block;
}

// --- Sync session ------------------------------------------------------
// One instance per cockpit-launched Claude session. Reconciles once
// immediately (catches edits made while nothing was watching), then watches
// both sides live until stop() is called.
function createSession(root) {
  debugLog(`createSession start: ${root}`);
  if (!enabled()) return { stop() {}, tracked: [] };

  const { tracked } = ensureProjectNotes(root);
  const master = masterPaths();
  ensureFile(master.todo, '');
  ensureFile(master.plan, '');
  debugLog('createSession: master files ensured');

  const state = readState();
  debugLog('createSession: state read');
  state[root] = state[root] || {};
  const name = path.basename(root);
  const localPath = kind => path.join(root, FILES[kind]);
  const masterPath = kind => master[kind];

  // Every one of these is best-effort: a concurrently-open file (another
  // agent, an editor, an AV scanner) must never throw out of here — worst
  // case we just skip updating sync state and pick it up on the next pass.
  function pushLocalToMaster(kind, content) {
    const ok = writeAtomic(masterPath(kind), upsertSection(readSafe(masterPath(kind)), root, name, content));
    if (ok) { state[root][kind] = hash(content); writeState(state); }
  }

  function pullMasterToLocal(kind, content) {
    try {
      fs.writeFileSync(localPath(kind), content.endsWith('\n') || !content ? content : `${content}\n`);
      state[root][kind] = hash(content);
      writeState(state);
    } catch { /* best effort */ }
  }

  function reconcile(kind) {
    debugLog(`reconcile(${kind}) start`);
    try {
      const local = readSafe(localPath(kind));
      const remote = getSectionBody(readSafe(masterPath(kind)), root) ?? '';
      const localHash = hash(local);
      const remoteHash = hash(remote);
      if (localHash === remoteHash) { state[root][kind] = localHash; return; }
      const last = state[root][kind];
      let winner;
      if (last && localHash === last) winner = 'master';
      else if (last && remoteHash === last) winner = 'local';
      // No sync history for this project yet (first time, or state was lost):
      // an empty side just means "nothing written there yet", not "changed to
      // empty" — the side with real content wins outright rather than by mtime,
      // otherwise a master file freshly created a moment ago (so it looks
      // "newer") would wipe out real local content on the very first sync.
      else if (!local.trim() && remote.trim()) winner = 'master';
      else if (!remote.trim() && local.trim()) winner = 'local';
      else winner = mtimeOf(localPath(kind)) >= mtimeOf(masterPath(kind)) ? 'local' : 'master';
      if (winner === 'local') pushLocalToMaster(kind, local);
      else pullMasterToLocal(kind, remote);
    } catch (error) { debugLog(`reconcile(${kind}) threw: ${error.message}`); }
    debugLog(`reconcile(${kind}) done`);
  }

  Object.keys(FILES).forEach(reconcile);
  writeState(state);
  debugLog('createSession: initial reconcile pass done');

  // Live watch: fs.watchFile polls the path itself, which (unlike fs.watch)
  // keeps working through editors that save via write-temp-then-rename.
  const timers = {};
  const debounce = (key, fn) => {
    clearTimeout(timers[key]);
    timers[key] = setTimeout(fn, DEBOUNCE_MS);
  };
  const watched = [];
  const watch = (file, onChange) => {
    const listener = () => onChange();
    fs.watchFile(file, { interval: 500 }, listener);
    watched.push(() => fs.unwatchFile(file, listener));
  };

  for (const kind of Object.keys(FILES)) {
    watch(localPath(kind), () => debounce(`local:${kind}`, () => {
      const content = readSafe(localPath(kind));
      if (hash(content) !== state[root][kind]) pushLocalToMaster(kind, content);
    }));
    watch(masterPath(kind), () => debounce(`master:${kind}`, () => {
      const content = getSectionBody(readSafe(masterPath(kind)), root) ?? '';
      if (hash(content) !== state[root][kind]) pullMasterToLocal(kind, content);
    }));
  }

  debugLog('createSession: watchers set up, returning');
  return {
    tracked,
    stop() {
      debugLog(`session.stop start: ${root}`);
      watched.forEach(unwatch => unwatch());
      debugLog('session.stop: watchers removed');
      Object.keys(timers).forEach(key => clearTimeout(timers[key]));
      Object.keys(FILES).forEach(reconcile); // catch a debounced write that hadn't landed yet
      writeState(state);
      debugLog('session.stop done');
    },
  };
}

module.exports = {
  enabled, notesDir, masterPaths, projectRoot, ensureProjectNotes, createSession, debugLog,
  findSections, getSectionBody, upsertSection, // exported for tests
};
