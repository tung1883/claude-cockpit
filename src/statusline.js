const { execFileSync } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');

function number(...values) {
  for (const value of values) {
    const parsed = Number(value);
    if (Number.isFinite(parsed)) return parsed;
  }
  return null;
}

function git(args, cwd) {
  try { return execFileSync('git', args, { cwd, encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] }).trim(); }
  catch { return ''; }
}

function gitStats(cwd) {
  const root = git(['rev-parse', '--show-toplevel'], cwd);
  if (!root) return { project: path.basename(cwd), branch: '', changes: 0, age: '', added: 0, removed: 0 };
  const status = git(['status', '--porcelain'], cwd);
  const lastCommit = git(['log', '-1', '--format=%ct'], cwd);
  const age = lastCommit ? formatAge(Date.now() - Number(lastCommit) * 1000) : '';
  let added = 0, removed = 0;
  for (const line of git(['diff', '--numstat', 'HEAD'], cwd).split(/\r?\n/)) {
    const [a, d] = line.split(/\s+/);
    if (/^\d+$/.test(a)) added += Number(a);
    if (/^\d+$/.test(d)) removed += Number(d);
  }
  return { project: path.basename(root), branch: git(['branch', '--show-current'], cwd), changes: status ? status.split(/\r?\n/).length : 0, age, added, removed };
}

function formatAge(ms) {
  if (!Number.isFinite(ms) || ms < 0) return '';
  const days = Math.floor(ms / 86400000);
  if (days) return `${days}d`;
  const hours = Math.floor(ms / 3600000);
  if (hours) return `${hours}h`;
  return `${Math.max(1, Math.floor(ms / 60000))}m`;
}

function duration(start) {
  const value = Date.parse(start || '') || number(start) * (String(start).length < 13 ? 1000 : 1);
  return value ? formatAge(Date.now() - value) : '';
}

function pct(value) {
  const n = number(value);
  return n === null ? '--' : `${Math.round(n)}%`;
}

function compactTokens(value) {
  const n = number(value);
  if (n === null) return '--';
  if (n >= 1000000) return `${(n / 1000000).toFixed(1)}m`;
  if (n >= 1000) return `${(n / 1000).toFixed(1)}k`;
  return String(Math.round(n));
}

// Claude's signature orange (#D97757), with a 256-color fallback.
const orange = /^(truecolor|24bit)$/i.test(process.env.COLORTERM || '') ? '\x1b[38;2;217;119;87m' : '\x1b[38;5;173m';

const themes = {
  // Claude vibe: one warm accent, everything else quiet.
  default: { reset: '\x1b[0m', gray: '\x1b[90m', cyan: orange, blue: '\x1b[90m', green: '\x1b[32m', yellow: '\x1b[33m', red: '\x1b[31m', magenta: orange, orange },
  ocean: { reset: '\x1b[0m', gray: '\x1b[90m', cyan: '\x1b[96m', blue: '\x1b[94m', green: '\x1b[92m', yellow: '\x1b[93m', red: '\x1b[91m', magenta: '\x1b[95m' },
  dracula: { reset: '\x1b[0m', gray: '\x1b[90m', cyan: '\x1b[96m', blue: '\x1b[95m', green: '\x1b[92m', yellow: '\x1b[93m', red: '\x1b[91m', magenta: '\x1b[95m' },
  nord: { reset: '\x1b[0m', gray: '\x1b[37m', cyan: '\x1b[96m', blue: '\x1b[94m', green: '\x1b[92m', yellow: '\x1b[93m', red: '\x1b[91m', magenta: '\x1b[95m' },
  mono: { reset: '\x1b[0m', gray: '\x1b[90m', cyan: '\x1b[37m', blue: '\x1b[37m', green: '\x1b[37m', yellow: '\x1b[97m', red: '\x1b[97m', magenta: '\x1b[97m' },
};

const ansi = themes[process.env.CLAUDE_COCKPIT_THEME || 'default'] || themes.default;
ansi.orange = ansi.orange || ansi.cyan;
const mark = process.env.NO_COLOR ? '*' : '✻';

function color(value, code) {
  if (process.env.NO_COLOR || !value) return String(value ?? '');
  return `${code}${value}${ansi.reset}`;
}

function level(value) {
  const n = number(value);
  if (n === null) return ansi.gray;
  if (n >= 80) return ansi.red;
  if (n >= 50) return ansi.yellow;
  return ansi.green;
}

function resetClock(value) {
  const n = number(value);
  if (n === null) return '';
  return new Intl.DateTimeFormat('en-GB', { timeZone: process.env.CLAUDE_COCKPIT_TZ || 'Asia/Bangkok', hour: '2-digit', minute: '2-digit', hour12: false }).format(new Date(n * 1000));
}

function formatDurationMs(value) {
  const n = number(value);
  if (n === null || n < 0) return '';
  const totalMinutes = Math.floor(n / 60000);
  return `${String(Math.floor(totalMinutes / 60)).padStart(2, '0')}:${String(totalMinutes % 60).padStart(2, '0')}`;
}

function statusline(input = {}) {
  const cwd = input.workspace?.current_dir || input.cwd || process.cwd();
  const gitInfo = gitStats(cwd);
  const model = input.model?.display_name || input.model?.name || input.model || '--';
  const context = input.context_window || input.context || {};
  const cost = input.cost || {};
  const rate = input.rate_limits || input.rateLimits || {};
  const five = rate.five_hour || rate['5h'] || rate.fiveHour || {};
  const seven = rate.seven_day || rate['7d'] || rate.sevenDay || {};
  const tokens = number(input.total_tokens, input.tokens, cost.total_tokens,
    number(context.total_input_tokens) + number(context.total_output_tokens),
    number(input.usage?.input_tokens) + number(input.usage?.output_tokens));
  const session = input.session_start_time || input.session_start_timestamp || input.sessionStartTime;
  const clock = new Intl.DateTimeFormat('en-GB', { timeZone: process.env.CLAUDE_COCKPIT_TZ || 'Asia/Bangkok', hour: '2-digit', minute: '2-digit', hour12: false, timeZoneName: 'short' }).format(new Date());
  const elapsed = formatDurationMs(cost.total_duration_ms) || duration(session);
  const sep = color(' · ', ansi.gray);
  const line1 = `${color(mark, ansi.orange)} ` + [
    color(gitInfo.project, ansi.orange),
    color(gitInfo.branch, ansi.gray),
    gitInfo.changes ? color(`${gitInfo.changes}±`, ansi.yellow) : '',
    color(gitInfo.age, ansi.gray),
    input.session_name ? color(`@${input.session_name}`, ansi.gray) : '',
    color(model, ansi.orange),
    color(`${clock}${elapsed ? ` ${elapsed}` : ''}`, ansi.gray),
  ].filter(Boolean).join(sep);
  const added = number(cost.total_lines_added, gitInfo.added) || 0;
  const removed = number(cost.total_lines_removed, gitInfo.removed) || 0;
  const fiveReset = resetClock(five.resets_at ?? five.reset_at);
  const contextPct = context.used_percentage ?? context.usedPercent ?? context.percentage;
  const fivePct = five.used_percentage ?? five.usedPercent ?? five.percentage;
  const sevenPct = seven.used_percentage ?? seven.usedPercent ?? seven.percentage;
  const costText = `$${number(cost.total_cost_usd, cost.totalCostUsd)?.toFixed(2) || '0.00'}`;
  const meter = (label, value) => `${color(label, ansi.gray)} ${color(pct(value), level(value))}`;
  const line2 = `  ` + [
    meter('ctx', contextPct),
    `${meter('5h', fivePct)}${fiveReset ? ` ${color(fiveReset, ansi.gray)}` : ''}`,
    meter('7d', sevenPct),
    color(costText, ansi.gray),
    `${color(`+${added}`, ansi.green)} ${color(`-${removed}`, ansi.red)}`,
    color(compactTokens(tokens), ansi.gray),
  ].join(sep);
  return `${line1}\n${line2}`;
}

function main() {
  let raw = '';
  try { raw = fs.readFileSync(0, 'utf8'); } catch {}
  let input = {};
  try { input = raw.trim() ? JSON.parse(raw) : {}; } catch {}
  process.stdout.write(statusline(input));
}

module.exports = { statusline, gitStats, main };
if (require.main === module) main();
