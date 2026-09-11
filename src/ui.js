'use strict';

// Minimal ANSI helpers. Disabled when stdout is not a TTY or NO_COLOR is set.
const enabled = Boolean(process.stdout.isTTY) && !process.env.NO_COLOR;
const truecolor = /^(truecolor|24bit)$/i.test(process.env.COLORTERM || '');

function wrap(open, close) {
  return text => (enabled ? `\x1b[${open}m${text}\x1b[${close}m` : String(text));
}

// Claude's signature orange (#D97757), with a 256-color fallback.
const ORANGE_OPEN = truecolor ? '38;2;217;119;87' : '38;5;173';

const color = {
  reset: enabled ? '\x1b[0m' : '',
  bold: wrap(1, 22),
  dim: wrap(2, 22),
  orange: wrap(ORANGE_OPEN, 39),
  orangeBold: text => (enabled ? `\x1b[1;${ORANGE_OPEN}m${text}\x1b[0m` : String(text)),
  red: wrap(31, 39),
  green: wrap(32, 39),
  yellow: wrap(33, 39),
  cyan: wrap(36, 39),
  gray: wrap(90, 39),
};

const glyph = {
  pointer: enabled ? '❯' : '>',
  spark: enabled ? '✻' : '*',
  dot: enabled ? '·' : '-',
  back: enabled ? '←' : '<-',
};

// Hand stdin back to a child process cleanly: drop our keypress listeners,
// leave raw mode, stop reading, restore the cursor. Without this a spawned
// interactive program (Claude Code) can appear frozen because this process is
// still holding stdin in raw/flowing mode.
function resetStdin() {
  const s = process.stdin;
  try { s.removeAllListeners('keypress'); } catch { /* ignore */ }
  try { if (s.isTTY && s.isRaw) s.setRawMode(false); } catch { /* ignore */ }
  try { s.pause(); } catch { /* ignore */ }
  // Full SGR reset + show cursor so no lingering color/attribute bleeds into
  // the child program's output.
  if (enabled) process.stdout.write('\x1b[0m\x1b[?25h');
}

// Alternate screen buffer: a fixed-size page with no scrollback and no
// scrolling, so every repaint (home + overwrite + erase-below) is stable and
// never makes the terminal scroll or flash. Enter once on start, leave on exit
// and before handing the terminal to Claude.
let inAlt = false;
function enterAlt() {
  if (enabled && !inAlt) { process.stdout.write('\x1b[?1049h\x1b[H'); inAlt = true; }
}
function leaveAlt() {
  if (enabled && inAlt) { process.stdout.write('\x1b[?25h\x1b[?1049l'); inAlt = false; }
}
process.on('exit', leaveAlt);
// Belt-and-suspenders: whatever caused the process to exit — quit, an
// uncaught error, Ctrl+C — the terminal must get raw mode back. Screens
// swapping between each other deliberately leave stdin in raw mode (that's
// the fix for the stdin-wedging bug), so something has to release it on the
// way out or the shell you return to is left broken (no echo/line editing).
process.on('exit', resetStdin);

// Hard wipe — use only for a real reset (startup, returning from Claude).
function clearScreen() {
  if (enabled) process.stdout.write('\x1b[2J\x1b[3J\x1b[H');
}
// Soft reset for switching between cockpit screens: home + erase below, no flash.
function homeClear() {
  if (enabled) process.stdout.write('\x1b[H\x1b[0J');
}
function hideCursor() {
  if (enabled) process.stdout.write('\x1b[?25l');
}
function showCursor() {
  if (enabled) process.stdout.write('\x1b[?25h');
}

function banner() {
  return `${color.orangeBold(glyph.spark)} ${color.bold('Claude Cockpit')}`;
}

function rule(width) {
  const cols = width || Math.min(process.stdout.columns || 80, 100);
  return color.gray('─'.repeat(Math.max(8, cols)));
}

// Repaint a block of lines anchored to the top of the screen: home the cursor,
// overwrite each line, then erase anything below. No full-screen wipe, so there
// is no flash — not between keystrokes and not between screens. Every cockpit
// screen paints from row 1, so switching screens just overwrites in place.
function makeRepainter() {
  return lines => {
    if (!enabled) { process.stdout.write(lines.join('\n') + '\n'); return; }
    // Home, clear+write each line, join with CR+LF, then erase below. No
    // trailing newline — a newline on the last row scrolls the page and the
    // next paint lands one row higher: that one-row jitter is the "flicker".
    const body = lines.map(line => `\x1b[2K${line}`).join('\r\n');
    process.stdout.write(`\x1b[H${body}\x1b[0J`);
  };
}

module.exports = {
  enabled, color, glyph, clearScreen, homeClear, enterAlt, leaveAlt, hideCursor, showCursor, resetStdin, banner, rule, makeRepainter,
};
