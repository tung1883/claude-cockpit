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
};

function clearScreen() {
  if (enabled) process.stdout.write('\x1b[2J\x1b[3J\x1b[H');
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

function rule(width = 44) {
  return color.gray('─'.repeat(width));
}

// Repaint a block of lines in place — moves the cursor up over the previous
// block and overwrites each line instead of clearing the whole screen, so
// navigation does not flicker. Pass the same line count every call.
function makeRepainter() {
  let painted = 0;
  return lines => {
    if (!enabled) { process.stdout.write(lines.join('\n') + '\n'); return; }
    let out = '';
    if (painted) out += `\x1b[${painted}A`;
    for (const line of lines) out += `\r\x1b[K${line}\n`;
    // clear any leftover lines from a taller previous paint
    for (let i = lines.length; i < painted; i++) out += '\r\x1b[K\n';
    process.stdout.write(out);
    painted = Math.max(lines.length, painted);
  };
}

module.exports = {
  enabled, color, glyph, clearScreen, hideCursor, showCursor, banner, rule, makeRepainter,
};
