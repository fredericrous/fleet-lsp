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
// src/hook.rs; a unit test there checks this pattern against that list).
export const SERVED = /\.(rs|go|pyi?|[jt]sx?|[mc][jt]s)$/

// A session id the binary would use in a marker path (`hook::marker`).
const SESSION_ID = /^[A-Za-z0-9._-]+$/

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
    if (await sessionStartGuided()) {
      await $.state.set(GUIDED, true, { ifVersion: held.version })
      return done
    }
    // One claim wins among parallel calls: the others see the version move.
    const claim = await $.state.set(GUIDED, true, { ifVersion: held.version })
    if (!claim.isSet) return done
    return { ...done, context: [...(done.context ?? []), GUIDANCE] }

    async function sessionStartGuided(): Promise<boolean> {
      const id = await $.session.id()
      if (!SESSION_ID.test(id) || id === '.' || id === '..') return false
      const xdg = await $.env.get('XDG_STATE_HOME')
      const home = await $.env.get('HOME')
      const base = xdg?.startsWith('/')
        ? xdg
        : home?.startsWith('/')
          ? `${home}/.local/state`
          : undefined
      if (base === undefined) return false
      return $.fs.exists(`${base}/fleet-lsp/guided/${id}`)
    }
  })

  // A compaction drops the earlier copy of the note: the next served file
  // brings it again. A precompute installs nothing, and a skipped one keeps
  // the conversation as it was.
  on('session.compact', async ($, e, next) => {
    const compacted = await next(e)
    if (e.agentId === undefined && e.trigger !== 'precompute' && !('skip' in compacted)) {
      await $.state.set(GUIDED, false)
    }
    return compacted
  })
}
