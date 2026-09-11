const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

function tmpDir(name) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), `ccpit-notes-${name}-`));
  return dir;
}

test('upsertSection inserts then replaces a project section without disturbing others', () => {
  const notes = require('../src/notes');
  let content = '';
  content = notes.upsertSection(content, '/a', 'proj-a', '- [ ] first\n## not a boundary\nstill body');
  content = notes.upsertSection(content, '/b', 'proj-b', '- [ ] other');
  assert.equal(notes.getSectionBody(content, '/a'), '- [ ] first\n## not a boundary\nstill body');
  assert.equal(notes.getSectionBody(content, '/b'), '- [ ] other');

  content = notes.upsertSection(content, '/a', 'proj-a', '- [x] first (done)');
  assert.equal(notes.getSectionBody(content, '/a'), '- [x] first (done)');
  assert.equal(notes.getSectionBody(content, '/b'), '- [ ] other', 'unrelated section untouched');
  assert.equal(notes.getSectionBody(content, '/missing'), null);
});

test('ensureProjectNotes creates skeleton files and a .gitignore block, idempotently', () => {
  process.env.CLAUDE_COCKPIT_NOTES_DIR = tmpDir('master');
  const notes = require('../src/notes');
  const root = tmpDir('project');
  notes.ensureProjectNotes(root);
  assert.match(fs.readFileSync(path.join(root, 'TODO.md'), 'utf8'), /^# TODO/);
  assert.match(fs.readFileSync(path.join(root, 'PLAN.md'), 'utf8'), /^# PLAN/);
  const gitignore1 = fs.readFileSync(path.join(root, '.gitignore'), 'utf8');
  assert.match(gitignore1, /TODO\.md/);
  assert.match(gitignore1, /PLAN\.md/);

  fs.writeFileSync(path.join(root, 'TODO.md'), '# TODO\n\n- keep me\n');
  notes.ensureProjectNotes(root); // second call must not clobber existing content or duplicate .gitignore
  assert.match(fs.readFileSync(path.join(root, 'TODO.md'), 'utf8'), /keep me/);
  const gitignore2 = fs.readFileSync(path.join(root, '.gitignore'), 'utf8');
  assert.equal(gitignore1, gitignore2, '.gitignore is not duplicated on a second run');
});

test('createSession mirrors a fresh project file into the master file', async () => {
  process.env.CLAUDE_COCKPIT_NOTES_DIR = tmpDir('master');
  delete require.cache[require.resolve('../src/notes')];
  const notes = require('../src/notes');
  const root = tmpDir('project');
  fs.writeFileSync(path.join(root, 'TODO.md'), '# TODO\n\n- [ ] ship it\n');

  const session = notes.createSession(root);
  try {
    const masterContent = fs.readFileSync(notes.masterPaths().todo, 'utf8');
    assert.match(masterContent, /ship it/);
    assert.match(masterContent, new RegExp(`ccpit:path=${root.replace(/\\/g, '\\\\')}`));
  } finally {
    session.stop(); // must run even on assertion failure — a live fs.watchFile hangs the process
  }
});

test('createSession pulls a master-side edit down into the project file on reconcile', async () => {
  process.env.CLAUDE_COCKPIT_NOTES_DIR = tmpDir('master');
  delete require.cache[require.resolve('../src/notes')];
  const notes = require('../src/notes');
  const root = tmpDir('project');
  fs.writeFileSync(path.join(root, 'TODO.md'), '# TODO\n\n- [ ] original\n');

  let session = notes.createSession(root);
  session.stop();

  // Simulate editing the master file directly while no session is running.
  const master = notes.masterPaths().todo;
  const edited = notes.upsertSection(fs.readFileSync(master, 'utf8'), root, 'project', '- [ ] edited from master');
  fs.writeFileSync(master, edited);

  session = notes.createSession(root); // reconcile-on-start should pull the master's edit down
  try {
    const local = fs.readFileSync(path.join(root, 'TODO.md'), 'utf8');
    assert.match(local, /edited from master/);
    assert.doesNotMatch(local, /original/);
  } finally {
    session.stop();
  }
});

test('respects CLAUDE_COCKPIT_NOTES=0', () => {
  process.env.CLAUDE_COCKPIT_NOTES = '0';
  delete require.cache[require.resolve('../src/notes')];
  const notes = require('../src/notes');
  const root = tmpDir('project');
  const session = notes.createSession(root);
  try {
    assert.equal(fs.existsSync(path.join(root, 'TODO.md')), false, 'disabled — no files created');
  } finally {
    session.stop();
    delete process.env.CLAUDE_COCKPIT_NOTES;
  }
});
