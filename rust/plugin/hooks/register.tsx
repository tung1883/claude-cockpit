// `/toc`: a pane listing what the user asked for in this session. A preview of
// the selected entry sits under the list; Enter opens its detail, where the
// arrows scroll the text, Enter jumps the conversation to the entry and Esc
// goes back; a search box narrows the list.
//
// The pane never takes more rows than it has room for, so the header, the
// preview and the key hints stay put and the arrows move the selection.
//
// The rules for what counts as an entry mirror `src/toc.rs`, judged by text
// alone (a message here has no `origin`).
import type { Register, SessionMessage } from 'claude-code'

const PANE = 'toc'
// Openings of user-role text that the user did not type.
const WRAPPERS = [
  '<system-reminder>',
  '<local-command-',
  '<bash-stdout>',
  '<bash-stderr>',
  '<task-notification>',
  '<persisted-output>',
  '<ide_',
  '<agent-message',
  '<teammate-message',
  '[Request interrupted',
  '[Image: ',
]
// How the summary that replaces compacted messages opens.
const SUMMARY = 'This session is being continued from a previous conversation'
const EDITORS = ['Edit', 'Write', 'MultiEdit', 'NotebookEdit']
// Rows the preview takes, its rule included.
const PREVIEW = 6
// With fewer rows than this the preview is left out.
const ROOMY = 16
// Behind the selected row (a dark theme's tint of the accent).
const SELECTED = '#3a2a24'
// Characters of Claude's replies kept per entry.
const REPLIES = 40_000

type Kind = 'prompt' | 'command' | 'shell'
type Entry = {
  uuid: string
  n: number
  kind: Kind
  label: string
  text: string
  // What Claude said before the next entry, in order.
  replies: string[]
  tools: number
  files: string[]
}

const MARK = {
  prompt: { glyph: '●', color: 'claude' },
  command: { glyph: '/', color: 'suggestion' },
  shell: { glyph: '$', color: 'warning' },
} as const

const oneLine = (s: string) => s.replace(/\s+/g, ' ').trim()

const tag = (s: string, name: string) => new RegExp(`<${name}>([\\s\\S]*?)</${name}>`).exec(s)?.[1]?.trim()

/** The entry a user message makes, or undefined when the user did not type it. */
const classify = (raw: string): { kind: Kind; label: string; text: string } | undefined => {
  // Blocks the harness puts in front of a typed prompt.
  const text = raw.replace(/^(\s*<(system-reminder|ide_[a-z_]+)>[\s\S]*?<\/\2>)+/, '').trim()
  if (text.startsWith('<command-name>') || text.startsWith('<command-message>')) {
    const name = tag(text, 'command-name')
    if (name === undefined || name === '/toc') return undefined
    // The marker in front of the label is the slash.
    const label = oneLine(`${name.replace(/^\//, '')} ${tag(text, 'command-args') ?? ''}`)

    return { kind: 'command', label, text: `/${label}` }
  }
  if (text.startsWith('<bash-input>')) {
    const label = oneLine(tag(text, 'bash-input') ?? '')

    return { kind: 'shell', label, text: `! ${label}` }
  }
  if (text === '' || WRAPPERS.some(w => text.startsWith(w))) return undefined
  const typed = text.replace(/<\/?pasted_content[^>]*>/g, ' ').trim()
  const label = oneLine(typed).slice(0, 200)

  return label === '' ? undefined : { kind: 'prompt', label, text: typed }
}

/** The entries since the last compaction, with what each led to. */
const outline = (messages: readonly SessionMessage[]) => {
  let isCompacted = false
  const entries: Entry[] = []
  for (const message of messages) {
    if (message.role === 'assistant') {
      const last = entries.at(-1)
      if (last === undefined) continue
      const said = message.text.trim()
      const kept = last.replies.reduce((sum, reply) => sum + reply.length, 0)
      if (said !== '' && kept < REPLIES) last.replies.push(said.slice(0, REPLIES - kept))
      for (const use of message.toolUses) {
        last.tools += 1
        const path = use.input.file_path ?? use.input.notebook_path
        if (!EDITORS.includes(use.tool) || typeof path !== 'string') continue
        const file = path.split(/[\\/]/).at(-1)!
        if (!last.files.includes(file)) last.files.push(file)
      }
      continue
    }
    if (message.uuid === undefined) continue
    if (message.text.startsWith(SUMMARY)) {
      // Nothing before the summary is on screen to jump to.
      isCompacted = true
      entries.length = 0
      continue
    }
    const made = classify(message.text)
    if (made !== undefined) {
      entries.push({ ...made, uuid: message.uuid, n: entries.length + 1, replies: [], tools: 0, files: [] })
    }
  }

  return { isCompacted, entries }
}

const matches = (entry: Entry, query: string) => {
  const q = query.trim().toLowerCase()

  return q === '' || [entry.text, ...entry.files].some(s => s.toLowerCase().includes(q))
}

const clip = (s: string, width: number) => (s.length <= width ? s : `${s.slice(0, Math.max(0, width - 1))}…`)

/** `s` without the control characters a drawing refuses. */
const printable = (s: string) => s.replace(/\t/g, '    ').replace(/[\u0000-\u0009\u000b-\u001f\u007f-\u009f]/g, '')

/** All of `s` as lines of `width`, blank lines kept. */
const flow = (s: string, width: number) => {
  const lines: string[] = []
  for (const paragraph of printable(s).split('\n')) {
    let rest = paragraph.trimEnd()
    do {
      let cut = rest.length <= width ? rest.length : rest.lastIndexOf(' ', width)
      if (cut <= width / 2) cut = Math.min(width, rest.length)
      lines.push(rest.slice(0, cut))
      rest = rest.slice(cut).trimStart()
    } while (rest !== '')
  }

  return lines
}

/** `s` as at most `rows` lines of `width`, the last one marked if text is left. */
const wrap = (s: string, width: number, rows: number) => {
  const lines: string[] = []
  for (const paragraph of printable(s).split('\n')) {
    let rest = paragraph.trimEnd()
    while (rest !== '' && lines.length <= rows) {
      let cut = rest.length <= width ? rest.length : rest.lastIndexOf(' ', width)
      if (cut <= width / 2) cut = Math.min(width, rest.length)
      lines.push(rest.slice(0, cut))
      rest = rest.slice(cut).trimStart()
    }
    if (lines.length > rows) break
  }
  if (lines.length > rows) {
    lines.length = Math.max(0, rows)
    const end = lines.length - 1
    if (end >= 0) lines[end] = `${lines[end]!.slice(0, width - 2)} …`
  }

  return lines
}

const counts = (e: Entry) =>
  [e.tools > 0 ? `${e.tools}t` : '', e.files.length > 0 ? `${e.files.length}f` : ''].filter(Boolean).join(' ')

/** "10 tools · a.rs, b.rs" */
const did = (e: Entry) =>
  e.tools === 0
    ? 'no tool calls'
    : [`${e.tools} tool${e.tools === 1 ? '' : 's'}`, e.files.join(', ')].filter(Boolean).join(' · ')

export const register: Register = on => {
  // The outline as last read; cleared when the session has moved on.
  let cache: ReturnType<typeof outline> | undefined
  let query = ''
  // The selected entry's uuid, the one shown in detail, why its jump failed,
  // the first row drawn and the first line of the detail's text drawn.
  let selected: string | undefined
  let detail: string | undefined
  let failed: string | undefined
  let top = 0
  let line = 0
  // The uuids the search lets through, as last drawn.
  let shown: string[] = []
  // Whether the focus is on the search box or its button.
  let isSearching = false

  on('session.start', async ($, e, next) => {
    await $.command.register({
      name: PANE,
      description: 'Table of contents: jump to a prompt of this session',
      immediate: true,
    })

    return next(e)
  })

  on('command.run', { command: PANE }, async $ => {
    cache = undefined
    query = ''
    selected = undefined
    detail = undefined
    failed = undefined
    isSearching = false
    await $.ui.open({ id: PANE, title: 'Contents', focus: true, closeOnEscape: true })

    return { text: 'Contents opened. Esc closes it.' }
  })

  on('turn.complete', ($, e, next) => {
    if (e.agentId === undefined) {
      cache = undefined
      $.ui.invalidate('ui.render')
    }

    return next(e)
  })

  // The selection follows the focus however it moves: arrows, Tab, a click.
  // While the detail is open an arrow scrolls its text instead, and the focus
  // stays on the entry.
  on('ui.focus', { component: 'Pane', requestId: PANE }, async ($, e, next) => {
    const order = ['search', 'query', 'before', ...shown.map(uuid => `entry:${uuid}`), 'after']
    const to = e.element === undefined ? -1 : order.indexOf(e.element)
    if (detail !== undefined && e.origin.kind === 'person' && to >= 0) {
      const step = to - order.indexOf(`entry:${detail}`)
      // An arrow moves to a neighbour; anything further is a click.
      if (Math.abs(step) === 1) {
        line += step
        $.ui.invalidate('ui.render')

        return next({ ...e, element: `entry:${detail}` })
      }
    }
    // The arrows go round: past either end of the list (a hidden button stands
    // there) is the other end. Coming down from the search box into the list
    // lands on the selected entry.
    let element = e.element
    const goTo = (uuid: string | undefined) => (uuid === undefined ? e.element : `entry:${uuid}`)
    if (element === 'after') element = goTo(shown[0])
    if (element === 'before') element = goTo(isSearching ? selected : shown.at(-1))
    isSearching = element === 'query' || element === 'search'

    const uuid = element?.startsWith('entry:') ? element.slice('entry:'.length) : undefined
    if (uuid !== undefined && uuid !== selected) {
      selected = uuid
      detail = undefined
      failed = undefined
      $.ui.invalidate('ui.render')
    }

    return element === e.element ? next(e) : next({ ...e, element })
  })

  // The pane never has more rows than fit, so only the wheel and the page keys
  // get here: they scroll the detail, or move the selection.
  on('ui.scroll', { component: 'Pane', requestId: PANE }, async ($, e, next) => {
    const at = selected === undefined ? -1 : shown.indexOf(selected)
    if (e.origin.kind !== 'person' || at < 0) return next(e)
    if (detail !== undefined) {
      line += e.by
      $.ui.invalidate('ui.render')

      return {}
    }
    const to = shown[Math.min(shown.length - 1, Math.max(0, at + e.by))]!
    if (to !== selected) {
      selected = to
      failed = undefined
      $.ui.invalidate('ui.render')
      void $.ui.focus({ requestId: PANE, key: `entry:${to}` })
    }

    return {}
  })

  // Esc (or the close mark) leaves the detail first, then closes the pane.
  on('ui.close', { id: PANE }, async ($, e, next) => {
    if (detail === undefined || e.origin.kind !== 'person') {
      detail = undefined

      return next(e)
    }
    detail = undefined
    failed = undefined
    $.ui.invalidate('ui.render')
    // The refused key still handed the keyboard back: take it again.
    void $.clock
      .sleep(100)
      .then(() => $.ui.open({ id: PANE, title: 'Contents', focus: true, closeOnEscape: true }))

    // Answering without `next` keeps the pane open.
    return { value: undefined } as never
  })

  on('ui.render', { component: 'Pane', requestId: PANE }, async ($, e) => {
    const { Box, Text, Button, Input } = $.ui.resolve(e)
    if (cache === undefined) {
      const read = await $.session.messages({ last: 4096 })
      if ('deny' in read) return <Text dimColor>Cannot read the session: {read.deny}</Text>
      cache = outline(read)
    }
    const { entries, isCompacted } = cache
    const list = entries.filter(entry => matches(entry, query))
    shown = list.map(entry => entry.uuid)
    let at = selected === undefined ? -1 : shown.indexOf(selected)
    // Nothing selected, or filtered out: the newest.
    if (at < 0) at = list.length - 1
    const current = list[at]
    selected = current?.uuid
    const isDetailed = current !== undefined && detail === current.uuid

    const width = Math.max(24, e.props.bodyColumns)
    const height = Math.max(8, e.props.scroll.bodyRows)
    // Header, search box, key hints, and the compaction note.
    const fixed = 3 + (isCompacted ? 1 : 0)
    const preview = height >= ROOMY ? PREVIEW : 0
    // The detail takes the pane, under the selected entry alone.
    const rows = isDetailed ? 1 : Math.max(1, height - fixed - preview)
    const margin = Math.min(2, Math.floor((rows - 1) / 2))
    top = Math.min(top, at - margin)
    top = Math.max(top, at - rows + 1 + margin)
    top = Math.max(0, Math.min(top, list.length - rows))

    const press = async (entry: Entry) => {
      selected = entry.uuid
      if (detail !== entry.uuid) {
        detail = entry.uuid
        failed = undefined
        line = 0
        $.ui.invalidate('ui.render')

        return
      }
      try {
        failed = (await $.ui.scroll({ to: { requestId: entry.uuid }, block: 'start' })).deny
      } catch (error) {
        failed = String(error)
      }
      if (failed === undefined) detail = undefined
      $.ui.invalidate('ui.render')
    }

    // Claude Code keeps the focus by an element's place among the drawn ones,
    // so every entry is always drawn and the ones outside the window take no
    // height. Redrawing then never moves the focus to another entry.
    //
    // The focused button is drawn inverted, as soon as the focus moves and so
    // before the redraw that moves the highlight. A row's button is therefore
    // the one cell of its accent bar, whether or not it is selected: anything
    // wider would flash.
    const row = (entry: Entry, i: number) => {
      const isOn = entry.uuid === current?.uuid
      const button = (
        <Button key={`entry:${entry.uuid}`} plain autoFocus={isOn ? true : undefined} onPress={() => press(entry)}>
          <Text color="claude"> </Text>
        </Button>
      )
      if (i < top || i >= top + rows) {
        // Nothing but the button: a drawing's text has a size limit.
        return (
          <Box key={`row:${entry.uuid}`} height={0} overflow="hidden">
            {button}
          </Box>
        )
      }
      const meta = counts(entry)
      const room = Math.max(8, width - meta.length - 10)
      const label = clip(entry.label, room)
      const sought = query.trim().toLowerCase()
      const found = sought === '' ? -1 : label.toLowerCase().indexOf(sought)
      const end = found + sought.length
      const gap = ' '.repeat(Math.max(1, room - label.length + 1))
      const behind = isOn ? SELECTED : undefined
      const quiet = entry.kind !== 'prompt' && !isOn
      const part = (text: string) => (
        <Text bold={isOn} dimColor={quiet} backgroundColor={behind}>
          {text}
        </Text>
      )
      const side = (text: string) => (
        <Text color={isOn ? 'claude' : undefined} dimColor={!isOn} backgroundColor={behind}>
          {text}
        </Text>
      )

      return (
        <Box key={`row:${entry.uuid}`} height={1} overflow="hidden">
          {button}
          <Text backgroundColor={behind} wrap="truncate-end">
            {side(`${String(entry.n).padStart(3)} `)}
            <Text color={MARK[entry.kind].color} backgroundColor={behind}>
              {`${MARK[entry.kind].glyph} `}
            </Text>
            {part(found < 0 ? label : label.slice(0, found))}
            <Text color="warning" bold backgroundColor={behind}>
              {found < 0 ? '' : label.slice(found, end)}
            </Text>
            {part(found < 0 ? '' : label.slice(end))}
            {side(`${gap}${meta} `)}
          </Text>
        </Box>
      )
    }

    // The detail's whole text, of which `view` lines from `line` are drawn.
    const body: { text: string; tone?: 'asked' | 'note' }[] = []
    if (isDetailed) {
      // Inside the border and its padding.
      const inner = width - 5
      body.push(...flow(current.text, inner).map(text => ({ text, tone: 'asked' as const })))
      for (const reply of current.replies) {
        body.push({ text: '' })
        body.push(...flow(reply, inner - 2).map((text, i) => ({ text: `${i === 0 ? '● ' : '  '}${text}` })))
      }
      body.push({ text: '' })
      body.push(...flow(did(current), inner).map(text => ({ text, tone: 'note' as const })))
    }
    // Inside the border, less a failed jump's line.
    const view = Math.max(1, height - fixed - rows - 2 - (failed === undefined ? 0 : 1))
    line = Math.max(0, Math.min(line, body.length - view))

    const panel = () => {
      if (current === undefined) return null
      if (isDetailed) {
        return (
          <Box key="detail" flexDirection="column" borderStyle="round" borderColor="claude" paddingX={1} width={width - 1}>
            {body.slice(line, line + view).map((row, i) => (
              <Text
                key={`l${i}`}
                bold={row.tone === 'asked'}
                color={row.tone === 'note' ? 'suggestion' : undefined}
                wrap="truncate-end"
              >
                {row.text === '' ? ' ' : row.text}
              </Text>
            ))}
            {failed !== undefined && (
              <Text color="warning" wrap="truncate-end">
                Could not jump: {failed}
              </Text>
            )}
          </Box>
        )
      }
      if (preview === 0) return null
      const asked = wrap(current.text, width - 3, 2)
      // The rule and the last line leave the rest to the text.
      const said = wrap(current.replies.at(-1) ?? '', width - 5, preview - 2 - asked.length)

      return (
        <Box key="preview" flexDirection="column" height={preview} overflow="hidden">
          <Text dimColor>{'─'.repeat(width - 1)}</Text>
          {asked.map((line, i) => (
            <Text key={`a${i}`} wrap="truncate-end">
              {' '}
              {line}
            </Text>
          ))}
          {said.map((line, i) => (
            <Text key={`s${i}`} dimColor wrap="truncate-end">
              {i === 0 ? ' ● ' : '   '}
              {line}
            </Text>
          ))}
          <Text color="suggestion" wrap="truncate-end">
            {' '}
            {did(current)}
          </Text>
        </Box>
      )
    }

    // The search box empties itself on Enter, so the header names the filter.
    const isFiltered = query.trim() !== ''
    const total = isFiltered
      ? `${list.length} of ${entries.length} match "${query.trim()}"`
      : `${entries.length} entries`

    return (
      <Box flexDirection="column">
        <Box justifyContent="space-between" width={width - 1}>
          <Text wrap="truncate-end">
            <Text color="claude" bold>
              Contents
            </Text>
            <Text dimColor> {total}</Text>
          </Text>
          <Button
            key="search"
            plain
            dimColor
            hotkey="s"
            label={isFiltered ? 'clear' : 'search'}
            onPress={async () => {
              if (query !== '') {
                query = ''
                selected = undefined
                detail = undefined
                failed = undefined
                $.ui.invalidate('ui.render')
              }
              // This plugin's own move: the focus hook does not hear it.
              isSearching = true
              try {
                await $.ui.focus({ requestId: PANE, key: 'query' })
              } catch {
                // The box can still be reached with the arrows.
              }
            }}
          />
        </Box>
        <Input
          key="query"
          placeholder="type to filter prompts and files"
          value={query}
          onInput={(value: string) => {
            isSearching = true
            query = value
            // Back to the newest match.
            selected = undefined
            detail = undefined
            failed = undefined
            $.ui.invalidate('ui.render')
          }}
          onSubmit={async () => {
            isSearching = false
            if (selected !== undefined) await $.ui.focus({ requestId: PANE, key: `entry:${selected}` })
          }}
        />
        {isCompacted && (
          <Text dimColor wrap="truncate-end">
            ⋯ earlier prompts compacted: see the cpit editor outline (o)
          </Text>
        )}
        {/* Where the up arrow goes from the oldest entry. */}
        <Box height={0} overflow="hidden">
          <Button key="before" plain label="·" onPress={() => {}} />
        </Box>
        <Box flexDirection="column" height={isDetailed || preview > 0 ? rows : undefined}>
          {list.length === 0 && <Text dimColor>{entries.length === 0 ? 'No prompts yet.' : 'No match.'}</Text>}
          {list.map(row)}
        </Box>
        {/* Where the down arrow goes from the newest entry. */}
        <Box height={0} overflow="hidden">
          <Button key="after" plain label="·" onPress={() => {}} />
        </Box>
        {panel()}
        <Box justifyContent="space-between" width={width - 1}>
          <Text dimColor wrap="truncate-end">
            {isDetailed
              ? '↑↓ scroll  ⏎ jump  esc back'
              : `↑↓ move  ⏎ detail  s ${isFiltered ? 'clear filter' : 'search'}  esc close`}
          </Text>
          <Text dimColor>
            {isDetailed
              ? `lines ${line + 1}–${Math.min(body.length, line + view)}/${body.length}`
              : list.length === 0
                ? ''
                : `${at + 1}/${list.length}`}
          </Text>
        </Box>
      </Box>
    )
  })
}
