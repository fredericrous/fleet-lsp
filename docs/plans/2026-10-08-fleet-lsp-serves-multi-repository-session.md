---
status: active
branch: feat/workspace-sessions
repos: [fleet-lsp]
adrs: [decisions:ADR-0019]
---
# fleet-lsp serves a multi-repository session

## Review panel
👉 **Decide:** none — approve if per-request routing behind one proxy, with single-root sessions untouched, is the shape you want.
📍 fleet-lsp · plan reviewed, nothing built · next: Phase 1, `src/route.rs`. Panel: backend, rust, tui, unix.
**Changed by review:** shell owns initialize/shutdown and own `fleet-lsp:N` ids; per-child failure, logs, restart budget; workspace refusal texts and `doctor [PATH]`.
📄 Full reviews: [2026-10-08-fleet-lsp-serves-multi-repository-session.reviews.md](2026-10-08-fleet-lsp-serves-multi-repository-session.reviews.md)
**Verdicts:** round 1: 3 approve-with-changes, 1 rework (rust); round 2: rust and backend approve-with-changes, nothing open.
**Carried into implementation:** Phase 3 fake adds `FAKE_IGNORE_SHUTDOWN` and logs full `initialize` params; shell parses saved `initialize` once, rewrites id/rootUri/folders before `prepare_initialize`; JSON-equal params check.

## Goal
A Claude Code session started in a directory that is not a git repository
gets code intelligence for every repository it touches. The person does this
on purpose: they start in `~/Developer/Perso` and work on several
repositories in one session. Today every query there answers `not in a git
repository`. After the change, `fleet-lsp serve <lang>` picks the
repository from the file each request names, and answers with that
repository's pinned server under the same pin check and readiness barrier
as today.

This reverses a recorded non-goal of
`docs/plans/2026-10-06-fleet-lsp-pinned-ready-language-servers.md` ("Not
fixing Claude Code's single workspace root per session … fleet-lsp makes
that case say so"). Claude Code still starts one server per language per
session at the launch directory. fleet-lsp now multiplexes behind it.

## Non-goals
- No change for a session started inside a repository ("single-root
  mode"). It keeps today's path: one child, the child's own `initialize`
  answer, `Exit` ends the session, and every existing test is unchanged.
- No new pinning rule. Each child is resolved by the existing
  `resolve::resolve` for its repository (`toolchain.language-servers-pinned`).
- No cross-repository answers. `references` and `workspace/symbol` search
  one repository's server, never a merge of several.
- No per-child flow control on the shared client output lane: one child
  flooding diagnostics may slow the others. This is accepted and tested
  (see Verification).
- No Windows work.

## Behaviour
**Mode is chosen at `initialize`.** When the session root
(`relay::session_root`) has a git root, single-root mode runs, as today.
Otherwise, workspace mode runs:

1. **`initialize` is answered by the shell.** It sends a fixed capability
   set: `textDocumentSync` full (1) with open and close, hover, definition,
   references, implementation, documentSymbol, workspaceSymbol and
   callHierarchy. Full sync is valid for every server, because a change
   without a range replaces the whole text. The client's `initialize` is
   saved for the children.
2. **Routing by document.** A client message whose `params` carries
   `textDocument.uri` (or `item.uri`, for call hierarchy) selects a root:
   the file's git root and project root (`resolve::git_root`,
   `project_root`), cached per directory.
3. **Open documents are tracked**: uri → languageId, version, latest full
   text.
   - `didOpen`, `didChange` and `didClose` only update this store while
     their root has no live child, so a notification never spawns one.
   - While a child is live, they are also forwarded to it.
4. **Only a request spawns a child.** The first request for a root resolves
   it with `resolve::resolve`.
   - **On success**, the child is spawned with the saved `initialize`. Its
     `rootUri` and `workspaceFolders` are rewritten to the project root, and
     today's injections apply.
   - Then come `initialized` and a `didOpen` replay of that root's open
     documents. The triggering document is delivered only through this
     replay, so every URI reaches the child exactly once.
   - The request then waits behind the child's own readiness gate, as
     today.
   - **On refusal**, that root's requests get a workspace-mode refusal (texts
     below), and other roots are unaffected.
5. **Requests that name no document** (`workspace/symbol`) go to the child
   of the most recently used root. The session log names that repository.
6. **Ids.**
   - Client request ids go to their child unchanged.
   - fleet-lsp's own requests to a child use string ids `fleet-lsp:N`:
     `initialize`, `shutdown` and `exit`, for an eviction and for the
     client's own shutdown. The shell writes them straight to the slot's
     `child_in`, so they bypass `Core::step`.
   - The `initialize` rewrite is pulled out of `Core::initialize` into a
     pure `core::prepare_initialize(&Config, &[u8]) -> (Vec<u8>, ServerFilter)`,
     which today's single-root path calls unchanged. In workspace mode the
     shell calls it for the slot, sends the result as `fleet-lsp:N`, sets
     the slot reader's filter and calls `Core::adopt_filter`. Every
     injection applies: rust-analyzer's `serverStatusNotification`,
     `window.workDoneProgress` with `own_progress`, and the tsserver path.
   - The reader turns a reply to an own id into `Ev::OwnReply { slot, id }`
     for the shell, and never into `ServerResponse` or a client frame.
     `ChildDead` and `fail_everything` skip `fleet-lsp:` ids. No frame with
     such an id ever reaches the client.
   - A server→client request keeps its original id inside `Core`, so
     `config_requests` and the pyright log-level guard work unchanged. The
     reader renumbers it on the way to the client as the string
     `s<slot>:<orig>`, unique across slots. A per-slot `Arc<Mutex<IdTable>>`
     holds the map, kept apart from `ServerFilter`, which stays `Copy`. The
     shell maps the client's answer back before `Core::client_response`.
   - The shell keeps a map from client id to slot, so `$/cancelRequest`
     goes to the slot holding the id. An entry is removed when its reply
     passes.
7. **One `Core` per slot**, with one change.
   - A new `Config` flag makes `ChildExited` and `ChildHung` emit
     `Action::ChildDead` instead of `finish(1)`: that slot's pending
     requests fail with today's text, and only that slot retires.
   - In workspace mode the client's `shutdown`, `exit` and a closed client
     are the shell's to handle. They are never passed to a slot's `Core`,
     so no slot emits `Exit` or `AwaitChildThenExit` for the session.
   - A respawn builds a fresh `Core`.
8. **Shutdown and exit.**
   - The shell sends each live slot its own `shutdown` (`fleet-lsp:N`).
     It answers the client's `shutdown` once, after it has counted one
     `OwnReply` per slot or a 10 s deadline has passed.
   - On `exit` it closes every child's stdin, waits for all of them against
     one shared 10 s deadline (`TEARDOWN`), then kills whatever is left.
   - The worst case, children that answer neither `shutdown` nor `exit`,
     is about 23 s: the two 10 s deadlines plus `teardown`'s own waits.
   - The exit code is 0 only when `shutdown` came before `exit`, as today.
9. **Bounded.**
   - At most `FLEET_LSP_MAX_CHILDREN` live children per language (default
     4).
   - A new root over the cap evicts the least recently used child: it is
     sent `shutdown` and `exit`, its slot enters a non-blocking `closing
     since T` state, and `housekeeping` calls `try_wait` on it and kills it
     after `TEARDOWN`. Its documents stay in the store for a later replay.
   - A root whose child dies is respawned on its next request, at most 2
     times (matching the plugin's `maxRestarts`). After that its requests
     are refused with the name of its log file.
10. **Logs.**
    - Each child's stderr is piped. A reader thread writes it to that
      child's log, named `<utc>-<pid>-<repo>-<slot>.log` (repo basename,
      keeping only `[A-Za-z0-9._-]`).
    - The session log `<utc>-<pid>.log` records the mode and lists every
      child log, each start, stop, eviction ("evicted `<repo>` (cap 4)") and
      restart, and the repository each `workspace/symbol` went to.
    - `doctor`'s "newest log" picks the session log.
11. **Refusal texts in workspace mode**, all in today's
    `fleet-lsp: <lang>: <reason>; fix: <action>` form, each naming the
    repository:
    - `fleet-lsp: <lang>: <path> is in no git repository; fix: open a file inside a repository`
    - `fleet-lsp: <lang>: no repository chosen yet; fix: open a file of the repository first`
    - `fleet-lsp: <lang>: several <lang> projects in <repo>; fix: open a file inside one of: <paths>`
    - `fleet-lsp: <lang>: no <lang> project found in <repo>; fix: none — <repo> has none`
    - Every pin or readiness refusal keeps its text, prefixed by `<repo>:`,
      and ends with `; details: fleet-lsp doctor <repo>`.
12. **`fleet-lsp doctor [PATH]`** resolves from PATH's git root (default:
    the cwd). Run from a directory outside any repository with no PATH, it
    prints to stderr "workspace mode: doctor checks one repository; run
    `fleet-lsp doctor <repo>`" and exits 1. The README documents that exit
    code.
13. **The session-start note** (`hooks/code-intelligence.sh`) gains one
    line: "`workspaceSymbol` searches the repository of the file used most
    recently."

## Phases
- [ ] Phase 1: the pure routing layer, `src/route.rs`, with no I/O and unit
      tests. It holds the URI extraction (a nested `params` read, reusing
      `json` and `relay::file_uri_path`), the root cache, the document store
      with replay minus trigger, the MRU and cap policy, and the
      server-request id table (`s<slot>:<orig>`) and the client-id→slot
      map. `core::prepare_initialize` is extracted and
      `Core::adopt_filter` added. `Core` gains the `ChildDead` flag, and its
      existing tests are unchanged.
- [ ] Phase 2: workspace mode in `relay.rs`.
      - `Shell` goes from one child to `Vec<ChildSlot>`, each with its own
        `Core`, process, queues, threads and stderr→log thread. Events are
        tagged by slot.
      - The shell answers `initialize` and `shutdown`.
      - The reader threads drop fleet-lsp's own replies and renumber
        server requests.
      - Closing slots, the shared teardown deadline and the restart budget.
      - The workspace refusal texts, `doctor [PATH]` and the hook line.
      - Single-root mode keeps its current code path.
- [ ] Phase 3: integration tests in `tests/relay.rs`.
      - `fake_server.py` learns to log `rootUri` and each `didOpen`
        uri/version, to answer with its own cwd, and to ignore `exit`
        (`FAKE_IGNORE_EXIT`).
      - Fixtures: a non-git parent holding python repos A and B, A2 (same
        basename as A, in another folder) and C (no venv).
- [ ] Phase 4: README, CHANGELOG and plugin `--min-version 0.5.0`; release
      0.5.0 (tag-release); install it the way the repository documents;
      then the live check in a fresh Claude Code session started in
      `~/Developer/Perso`.

## Decision log
- 2026-10-08: reverse the "single workspace root" non-goal, because the
  person starts sessions above their repositories on purpose. Single-root
  mode keeps its path, so where the session starts selects the behaviour.
- 2026-10-08: route by the file a request names, because Claude Code's LSP
  tool sends a file path with every operation. The one URI-less request
  goes to the most recently used child, because merged answers from several
  pinned servers have no single readiness or version to vouch for them.
- 2026-10-08: cap of 4 children per language, configurable and LRU, because
  rust-analyzer can hold over 1 GB per repository. The default is
  re-checked against the RSS measured in Verification.
- 2026-10-08: rollback is plugin `--min-version 0.4.0` plus installing
  0.4.0. Nothing persistent is written, so nothing needs migrating.

## Verification
- **Phase 1:** `cargo test route::` → URI extraction (didOpen, definition,
  callHierarchy item), cache, replay without the trigger, LRU at the cap,
  and a server-request id round trip that keeps the pyright
  `workspace/configuration` log-level guard → <result>
- **Phase 2/3:** `cargo test --test relay workspace_`, from a non-git
  parent:
  - the client gets exactly one `initialize` reply and, with 2 live fakes,
    one `shutdown` reply;
  - no frame on client stdout carries a `fleet-lsp:` id, including when a
    fake crashes before answering `initialize`, and on eviction;
  - two fakes that each send server request id 0 get distinct client ids,
    and both answers map back;
  - A and B get distinct `rootUri`s, and each sees one `didOpen` per URI;
  - A and A2 get distinct log files, and doctor's newest log is the session
    log;
  - C's requests are refused with the written text while A answers;
  - `workspace/symbol` goes to the MRU child;
  - eviction at cap 2 (env), then a replay on return;
  - killing A mid-request fails only A's ids, B answers, and the process
    stays up;
  - a fake that crashes at start is spawned at most 3 times, then refused;
  - 4 fakes ignoring `exit` shut down in under 14 s (the 10 s deadline
    plus `teardown`'s own 1 s and 2 s waits), and 4 also ignoring
    `shutdown` (`FAKE_IGNORE_SHUTDOWN`) in under 25 s, with no `<defunct>`
    left;
  - a rust-analyzer-flavoured fake spawned in workspace mode logs
    `serverStatusNotification: true` and `workDoneProgress: true` in its
    `initialize`, which matches the single-root params except `rootUri` and
    `workspaceFolders`, and its request is answered once it sends quiescent;
  - a `FAKE_FLOOD_MIB` flood on A while B still answers;
  - `fleet-lsp doctor <repo>` run from the parent

  → <result>
- **Single-root regression:** full `make check`, every existing relay test
  unchanged → <result>
- **Memory:** peak RSS of 4 rust-analyzer children on 4 real repositories
  (relais, fleet-lsp, amont-agent, one more) → the default cap kept or
  lowered → <result>
- **Live (Phase 4):** a fresh Claude Code session in `~/Developer/Perso`,
  with the LSP tool:
  - `hover` on `relais/crates/relais/src/main.rs` → an answer from relais's
    rust child;
  - `workspaceSymbol` "settle_by_agent", directly after that hover with
    nothing in between → the results include
    `relais/crates/relais/src/admission/mod.rs`, and the session log names
    relais;
  - `documentSymbol` on `fleet-lsp/src/route.rs` → an answer from a second
    rust child;
  - the session log lists two child logs

  → <result>

## Outcome

<!-- panel: repos=fleet-lsp adds= reviewers=backend,language-rust,tui,unix body-sha=733a175da239 -->
