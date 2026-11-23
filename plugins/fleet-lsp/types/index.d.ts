// The one value fleet-lsp keeps in `$.state`: whether this session's model
// already got the code-intelligence note (hooks/register.ts). Absent reads as
// not guided.

export type FleetLspGuided = boolean

declare module 'claude-code' {
  interface PluginState {
    'fleet-lsp': { guided: FleetLspGuided }
  }
}
