const fs = require('node:fs');
const path = require('node:path');
const paths = require('./paths');

function ensureStore() {
  fs.mkdirSync(paths.profilesDir, { recursive: true });
  if (!fs.existsSync(paths.profilesFile)) {
    fs.writeFileSync(paths.profilesFile, JSON.stringify({ profiles: {} }, null, 2));
  }
}

function readStore() {
  ensureStore();
  try {
    const value = JSON.parse(fs.readFileSync(paths.profilesFile, 'utf8'));
    return value && value.profiles ? value : { profiles: {} };
  } catch (error) {
    throw new Error(`Cannot read ${paths.profilesFile}: ${error.message}`);
  }
}

function writeStore(store) {
  ensureStore();
  const temp = `${paths.profilesFile}.tmp-${process.pid}`;
  fs.writeFileSync(temp, JSON.stringify(store, null, 2));
  fs.renameSync(temp, paths.profilesFile);
}

function validateName(name) {
  if (!name || !/^[a-zA-Z0-9][a-zA-Z0-9_-]{0,48}$/.test(name)) {
    throw new Error('Profile names must be 1-49 characters: letters, numbers, _ or -');
  }
}

function addProfile(name) {
  validateName(name);
  const store = readStore();
  if (!store.profiles[name]) store.profiles[name] = { name, createdAt: new Date().toISOString() };
  fs.mkdirSync(paths.profileDir(name), { recursive: true });
  writeStore(store);
  return store.profiles[name];
}

function listProfiles() {
  return Object.values(readStore().profiles).sort((a, b) => a.name.localeCompare(b.name));
}

function getProfile(name) {
  const profile = readStore().profiles[name];
  if (!profile) throw new Error(`Unknown profile '${name}'. Run: claude-cockpit add ${name}`);
  return profile;
}

function removeProfile(name) {
  const store = readStore();
  if (!store.profiles[name]) throw new Error(`Unknown profile '${name}'.`);
  delete store.profiles[name];
  writeStore(store);
  const dir = paths.profileDir(name);
  if (fs.existsSync(dir)) fs.rmSync(dir, { recursive: true, force: true });
  return { name, dir };
}

module.exports = { addProfile, getProfile, listProfiles, removeProfile, validateName };
