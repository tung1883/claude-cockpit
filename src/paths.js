const os = require('node:os');
const path = require('node:path');

const root = path.join(os.homedir(), '.claude-multi-cockpit');

module.exports = {
  root,
  profilesFile: path.join(root, 'profiles.json'),
  profilesDir: path.join(root, 'profiles'),
  profileDir(name) {
    return path.join(root, 'profiles', name);
  },
};
