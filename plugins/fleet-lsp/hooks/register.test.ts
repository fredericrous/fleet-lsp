import type { On } from 'claude-code'
import { expect, test } from 'claude-code/testing'
import { GUIDANCE, markerPath, SERVED } from './register'

// What the tools answer beneath the plugin, and where the marker would be.
// `marker` is the path the binary's SessionStart would have written.
const KEPT = [{ role: 'user', text: 'kept', toolUses: [] }]

// `compacting`, when set, holds a compaction open until it resolves.
type World = { marker?: string; erroredRead?: boolean; compacting?: Promise<void> }

function world(on: On, state: World = {}) {
  on('tool.call', () =>
    state.erroredRead === true
      ? ({ result: 'boom', isError: true } as never)
      : ({ result: 'ok' } as never),
  )
  on('session.id', () => ({ value: 's1' }))
  on('env.get', (_, e) => ({ value: e.name === 'XDG_STATE_HOME' ? '/state' : undefined }))
  on('fs.exists', (_, e) => ({ value: e.path === state.marker }))
  on('session.compact', async () => {
    await state.compacting
    return { messages: KEPT } as never
  })
}

const read = (file_path: string, extra: Record<string, unknown> = {}) =>
  ({ tool: 'Read', file_path, ...extra }) as never

const guided = (r: { context?: readonly string[] }) =>
  (r.context ?? []).filter(c => c === GUIDANCE).length

test('the first served file guides once; the next one does not', async ($, on) => {
  world(on)
  expect(guided(await $.tool.call(read('/x/a.rs')))).toBe(1)
  expect(guided(await $.tool.call(read('/x/b.ts')))).toBe(0)
})

test('a file no server serves never guides and never uses up the note', async ($, on) => {
  world(on)
  expect(guided(await $.tool.call(read('/x/README.md')))).toBe(0)
  expect(guided(await $.tool.call(read('/x/a.go')))).toBe(1)
})

test("a subagent's Read gives nothing and leaves the main session's note", async ($, on) => {
  world(on)
  expect(guided(await $.tool.call(read('/x/a.rs', { agentId: 'a1' })))).toBe(0)
  expect(guided(await $.tool.call(read('/x/a.rs')))).toBe(1)
})

test('an errored Read gives nothing and keeps the note for the next', async ($, on) => {
  const tools: World = { erroredRead: true }
  world(on, tools)
  expect(guided(await $.tool.call(read('/x/a.rs')))).toBe(0)
  tools.erroredRead = false
  expect(guided(await $.tool.call(read('/x/a.rs')))).toBe(1)
})

test("SessionStart's marker means the session already has the note", async ($, on) => {
  world(on, { marker: '/state/fleet-lsp/guided/s1' })
  expect(guided(await $.tool.call(read('/x/a.rs')))).toBe(0)
})

test('an applied compaction brings the note back; a precompute does not', async ($, on) => {
  world(on)
  expect(guided(await $.tool.call(read('/x/a.rs')))).toBe(1)
  await $.session.compact({ trigger: 'precompute', messages: KEPT } as never)
  expect(guided(await $.tool.call(read('/x/a.rs')))).toBe(0)
  await $.session.compact({ trigger: 'manual', messages: KEPT } as never)
  expect(guided(await $.tool.call(read('/x/a.rs')))).toBe(1)
})

test('after a compaction, a fresh SessionStart marker still counts', async ($, on) => {
  const tools: World = {}
  world(on, tools)
  expect(guided(await $.tool.call(read('/x/a.rs')))).toBe(1)
  await $.session.compact({ trigger: 'auto', messages: KEPT } as never)
  tools.marker = '/state/fleet-lsp/guided/s1'
  expect(guided(await $.tool.call(read('/x/a.rs')))).toBe(0)
})

test('parallel first Reads guide once between them', async ($, on) => {
  world(on)
  const all = await Promise.all([
    $.tool.call(read('/x/a.rs')),
    $.tool.call(read('/x/b.rs')),
    $.tool.call(read('/x/c.py')),
  ])
  expect(all.map(guided).reduce((a, b) => a + b, 0)).toBe(1)
})

test('the pattern serves exactly the plugin extensions', () => {
  for (const ext of ['rs', 'go', 'py', 'pyi', 'ts', 'tsx', 'js', 'jsx', 'mts', 'cts', 'mjs', 'cjs']) {
    expect(SERVED.test(`x.${ext}`)).toBe(true)
  }
  for (const name of ['x.mtsx', 'x.cjsx', 'x.rsx', 'x.json', 'x.pyc', 'x.tsbuildinfo', 'x.md', 'x.rs.bak']) {
    expect(SERVED.test(name)).toBe(false)
  }
})

test('the marker path follows the binary: absolute XDG, else HOME, safe ids only', () => {
  expect(markerPath('s1', '/x', '/h')).toBe('/x/fleet-lsp/guided/s1')
  expect(markerPath('s1', 'rel', '/h')).toBe('/h/.local/state/fleet-lsp/guided/s1')
  expect(markerPath('s1', undefined, undefined)).toBeUndefined()
  for (const id of ['', '.', '..', 'a/b', '../x']) expect(markerPath(id, '/x', '/h')).toBeUndefined()
})

// FALSIFY: reset `guided` without `ifVersion` in the compact hook.
test('a note claimed while a compaction runs survives it', async ($, on) => {
  let release = () => {}
  const tools: World = { compacting: new Promise<void>(done => (release = done)) }
  world(on, tools)
  const compaction = $.session.compact({ trigger: 'manual', messages: KEPT } as never)
  expect(guided(await $.tool.call(read('/x/a.rs')))).toBe(1)
  release()
  await compaction
  expect(guided(await $.tool.call(read('/x/b.rs')))).toBe(0)
})
