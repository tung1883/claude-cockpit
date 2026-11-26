const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

// Falls back to the pre-rename folder while it is the only one on disk
// (see scripts/migrate-data-dir.ps1).
const newRoot = path.join(os.homedir(), '.claude-cockpit');
const oldRoot = path.join(os.homedir(), '.claude-multi-cockpit');
const root = !fs.existsSync(newRoot) && fs.existsSync(oldRoot) ? oldRoot : newRoot;

module.exports = {
  root,
  profilesFile: path.join(root, 'profiles.json'),
  profilesDir: path.join(root, 'profiles'),
  profileDir(name) {
    return path.join(root, 'profiles', name);
  },
};
