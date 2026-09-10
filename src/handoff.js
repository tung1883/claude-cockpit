const fs = require('node:fs');
const path = require('node:path');
const paths = require('./paths');

function findSession(profileDir, sessionId) {
  const projectRoot = path.join(profileDir, 'projects');
  if (!fs.existsSync(projectRoot)) return null;
  const queue = [projectRoot];
  while (queue.length) {
    const current = queue.shift();
    for (const entry of fs.readdirSync(current, { withFileTypes: true })) {
      const full = path.join(current, entry.name);
      if (entry.isDirectory()) queue.push(full);
      else if (entry.isFile() && entry.name === `${sessionId}.jsonl`) return full;
    }
  }
  return null;
}

function copySession(from, to, sessionId) {
  const source = findSession(paths.profileDir(from), sessionId);
  if (!source) throw new Error(`Session ${sessionId} was not found in profile '${from}'.`);
  const relative = path.relative(paths.profileDir(from), source);
  const target = path.join(paths.profileDir(to), relative);
  if (fs.existsSync(target)) throw new Error(`Target session already exists: ${target}`);
  fs.mkdirSync(path.dirname(target), { recursive: true });
  fs.copyFileSync(source, target, fs.constants.COPYFILE_EXCL);
  return { source, target };
}

module.exports = { copySession, findSession };
