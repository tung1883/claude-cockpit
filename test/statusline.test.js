const test = require('node:test');
const assert = require('node:assert/strict');
const { statusline } = require('../src/statusline');

test('renders a compact two-line statusline', () => {
  const output = statusline({
    cwd: process.cwd(),
    model: { display_name: 'Opus 4.7' },
    context_window: { used_percentage: 84 },
    cost: { total_cost_usd: 8.42 },
    session_id: '123456789',
    session_start_time: new Date().toISOString(),
    rate_limits: { five_hour: { used_percentage: 24 }, seven_day: { used_percentage: 17 } },
    total_tokens: 164300,
  });
  const plain = output.replace(/\x1b\[[0-9;]*m/g, '');
  assert.match(plain, /Opus 4\.7/);
  assert.match(plain, /ctx 84%/);
  assert.match(plain, /5h 24%/);
  assert.match(plain, /7d 17%/);
  assert.match(plain, /\$8\.42/);
  assert.match(plain, /164\.3k/);
});
