import { expect, test } from 'claude-code/testing'

const PANE = {
  plugin: 'cpit',
  component: 'Pane',
  requestId: 'toc',
  surface: 'terminal',
  props: {
    title: 'Contents',
    isFocused: false,
    bodyColumns: 60,
    placement: 'dock',
    scroll: { offset: 0, bodyRows: 24 },
    view: {},
  },
} as const

const user = (uuid: string, text: string) => ({ role: 'user' as const, text, toolUses: [], uuid })
const claude = (text: string, ...files: string[]) => ({
  role: 'assistant' as const,
  text,
  uuid: `a-${text}`,
  toolUses: files.map((file_path, i) => ({ tool_use_id: `${text}${i}`, tool: 'Edit', input: { file_path } })),
})

const entries = async (ui: any) =>
  (await ui.findAll({ type: 'Button' })).filter((b: any) => String(b.key).startsWith('entry:'))

test('lists what the user typed, since the last compaction', async ($, on) => {
  on('session.messages', () => ({
    value: [
      user('a', 'before compaction'),
      user('b', 'This session is being continued from a previous conversation that ran out of context.'),
      user('c', '<system-reminder>x</system-reminder>\nfix   the\nbug'),
      user('d', '<task-notification>done</task-notification>'),
      user('e', '<command-message>review</command-message>\n<command-name>/review</command-name>\n<command-args>main</command-args>'),
      user('f', '<bash-input>cargo test</bash-input>'),
      user('g', '[Request interrupted by user]'),
      user('h', ''),
      claude('sure'),
    ],
  }))

  const ui = await $.ui.mount(PANE)
  const rows = await entries(ui)

  expect(rows.map((b: any) => b.key)).toEqual(['entry:c', 'entry:e', 'entry:f'])
  for (const label of [/fix the bug/, /review main/, /cargo test/]) {
    expect(await ui.find({ type: 'Text', text: label })).toBeDefined()
  }
  // A row's button is one cell, so that focusing it cannot flash the row.
  expect(rows.map((b: any) => b.text)).toEqual([' ', ' ', ' '])
  expect(await ui.find({ type: 'Text', text: /compacted/ })).toBeDefined()
  await ui.unmount()
})

test('the first press opens the detail, the second jumps', async ($, on) => {
  on('session.messages', () => ({
    value: [user('a', 'add a flag'), claude('Looking.', '/x/main.rs'), claude('The flag is in.', '/x/main.rs', '/x/cli.rs')],
  }))

  const ui = await $.ui.mount(PANE)
  // The preview already shows what it led to.
  expect(await ui.find({ type: 'Text', text: /3 tools · main\.rs, cli\.rs/ })).toBeDefined()
  expect(await ui.find({ type: 'Text', text: /esc back/ })).toBeUndefined()

  await ui.press({ key: 'entry:a' })
  await ui.redraw()
  expect(await ui.find({ type: 'Text', text: /esc back/ })).toBeDefined()
  // Every reply, not only the last.
  expect(await ui.find({ type: 'Text', text: /● Looking\./ })).toBeDefined()
  expect(await ui.find({ type: 'Text', text: /● The flag is in\./ })).toBeDefined()

  // Nothing here answers `ui.scroll`, so the jump fails and says so.
  await ui.press({ key: 'entry:a' })
  await ui.redraw()
  expect(await ui.find({ type: 'Text', text: /Could not jump: / })).toBeDefined()
  await ui.unmount()
})

test('a long reply is drawn a screenful at a time', async ($, on) => {
  const reply = Array.from({ length: 200 }, (_, i) => `line ${i}`).join('\n')
  on('session.messages', () => ({ value: [user('a', 'explain'), claude(reply)] }))

  const ui = await $.ui.mount(PANE)
  await ui.press({ key: 'entry:a' })
  await ui.redraw()

  // 24 rows less header, search box, hints, the entry and the border; the
  // prompt, the reply, two blanks and the closing note make 204 lines.
  expect(await ui.find({ type: 'Text', text: /lines 1–18\/204/ })).toBeDefined()
  expect(await ui.find({ type: 'Text', text: /line 15$/ })).toBeDefined()
  expect(await ui.find({ type: 'Text', text: /line 100$/ })).toBeUndefined()
  await ui.unmount()
})

test('search narrows the list by prompt or edited file', async ($, on) => {
  on('session.messages', () => ({
    value: [user('a', 'fix the parser'), claude('ok'), user('b', 'tidy up'), claude('done', '/x/lexer.rs')],
  }))

  const ui = await $.ui.mount(PANE)
  for (const [query, keys] of [
    ['PARSER', ['entry:a']],
    ['lexer', ['entry:b']],
    ['zzz', []],
    ['', ['entry:a', 'entry:b']],
  ] as const) {
    await ui.input({ key: 'query', text: query, kind: 'change' })
    await ui.redraw()
    expect((await entries(ui)).map((b: any) => b.key)).toEqual([...keys])
  }
  await ui.input({ key: 'query', text: 'lexer', kind: 'change' })
  await ui.redraw()
  expect(await ui.find({ type: 'Text', text: /1 of 2 match "lexer"/ })).toBeDefined()
  // `s` drops the filter.
  await ui.press({ key: 'search' })
  await ui.redraw()
  expect((await entries(ui)).length).toBe(2)
  await ui.unmount()
})

test('a long list shows what fits, ending at the newest', async ($, on) => {
  on('session.messages', () => ({
    value: Array.from({ length: 1000 }, (_, i) => user(`u${i}`, `prompt ${i} ${'words '.repeat(30)}`)),
  }))

  const ui = await $.ui.mount(PANE)
  // Every entry is there to take the focus; 24 rows less the header, search
  // box, hints and a 6-row preview have text: the newest 15.
  expect((await entries(ui)).length).toBe(1000)
  expect(await ui.find({ type: 'Text', text: /prompt 999/ })).toBeDefined()
  expect(await ui.find({ type: 'Text', text: /prompt 985 / })).toBeDefined()
  expect(await ui.find({ type: 'Text', text: /prompt 984 / })).toBeUndefined()
  await ui.unmount()
})
