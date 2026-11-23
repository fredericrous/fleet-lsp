// fleet-lsp's first-contact guidance, in process: the first Read, Edit or
// Write of a file a fleet-lsp server serves brings the code-intelligence
// note once per session, beside that tool's result. Nothing is spawned per
// call (the 0.7.0 PostToolUse command hook started a shell and the binary on
// every such call). SessionStart and SubagentStart stay command hooks
// (`fleet-lsp hook …`), one spawn each.

import type { Register } from 'claude-code'

// `hook::GUIDANCE` in src/hook.rs, line for line: a unit test there compares
// the two. Single-quoted lines, never a template literal: the text holds
// backticks.
const GUIDANCE_LINES = [
  'Code intelligence (fleet-lsp, .rs .go .py .ts/.js), wherever the LSP tool is available (named only as a deferred tool? load it first: ToolSearch select:LSP):',
  '- Navigate with the LSP tool, not grep/rg/cat or a whole-file Read: workspaceSymbol to find a name, then goToDefinition, findReferences, hover, incomingCalls; Read only the range LSP points to. Grep/Glob are for text: comments, strings, config.',
  '- Before a rename or a signature change: findReferences for the call sites, then Grep for what LSP cannot see (strings, docs, dynamic calls).',
  '- After an edit, diagnostics arrive on their own: fix each type error or missing import before moving on.',
  '- Each repository you query gets its own server (worktrees included); workspaceSymbol searches the repository of the file opened or used most recently, so open a file (documentSymbol, hover) first when the session did not start in a repository.',
  '- A slow first answer is the server loading. An error that names a fix (`fleet-lsp doctor` explains it): apply the fix or tell the person; do not fall back to Grep without saying so.',
]

export const GUIDANCE = GUIDANCE_LINES.join('\n') + '\n'

// The extensions plugin.json's lspServers serve (`hook::SERVED` in
// src/hook.rs). Its unit test `the_mod_serves_the_same_extensions` holds this
// literal and the list it spells: change all three together.
export const SERVED = /\.(rs|go|pyi?|[jt]sx?|[mc][jt]s)$/

// A session id the binary would use in a marker path (`hook::marker`).
const SESSION_ID = /^[A-Za-z0-9._-]+$/

// Where the binary's SessionStart leaves its marker for session `id` (the
// state directory as `log::state_dir()` picks it: an absolute XDG_STATE_HOME,
// else HOME's), or undefined when it would leave none.
export function markerPath(
  id: string,
  xdg: string | undefined,
  home: string | undefined,
): string | undefined {
  if (!SESSION_ID.test(id) || id === '.' || id === '..') return undefined
  if (xdg?.startsWith('/')) return `${xdg}/fleet-lsp/guided/${id}`
  if (home?.startsWith('/')) return `${home}/.local/state/fleet-lsp/guided/${id}`
  return undefined
}

const GUIDED = { plugin: 'fleet-lsp', key: 'guided' } as const

export const register: Register = on => {
  on('tool.call', { tool: ['Read', 'Edit', 'Write'], file_path: SERVED }, async ($, e, next) => {
    const done = await next(e)
    // A subagent's guidance is SubagentStart's; a plugin's own call delivers
    // no context to the model, so it must not use up the session's one note.
    if (e.agentId !== undefined || next.origin.tier !== 'core') return done
    if (done.deny !== undefined || done.isError === true) return done
    const held = await $.state.get(GUIDED)
    if (held.value === true) return done
    // The binary's SessionStart gave the note when the session started in a
    // served project, and left this marker.
    const marker = markerPath(
      await $.session.id(),
      await $.env.get('XDG_STATE_HOME'),
      await $.env.get('HOME'),
    )
    if (marker !== undefined && (await $.fs.exists(marker))) {
      await $.state.set(GUIDED, true, { ifVersion: held.version })
      return done
    }
    // One claim wins among parallel calls: the others see the version move.
    const claim = await $.state.set(GUIDED, true, { ifVersion: held.version })
    if (!claim.isSet) return done
    return { ...done, context: [...(done.context ?? []), GUIDANCE] }
  })

  // A compaction drops the earlier copy of the note: the next served file
  // brings it again. A precompute installs nothing, and a skipped one keeps
  // the conversation as it was. The reset is read before the compaction and
  // loses to a claim made while it ran: that claim's note is in the new
  // conversation.
  on('session.compact', async ($, e, next) => {
    const before = await $.state.get(GUIDED)
    const compacted = await next(e)
    if (e.agentId === undefined && e.trigger !== 'precompute' && compacted.skip === undefined) {
      await $.state.set(GUIDED, false, { ifVersion: before.version })
    }
    return compacted
  })
}
