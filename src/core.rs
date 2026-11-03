//! The relay's decisions, as a pure state machine:
//! `(state, input, now) → actions`.
//!
//! The I/O shell (`relay`) owns pipes, threads and queues; it feeds this
//! every client frame and every server event it observed, and executes the
//! actions it returns. Server frames are relayed by the shell directly and
//! never wait on this machine — only what they *said* reaches it.

use crate::gate::{Barrier, Effect, Gate, Signal, State};
use crate::json::{self, Json};
use crate::scan::{scan, Id, Kind, Scan};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

/// JSON-RPC / LSP error codes fleet-lsp answers with.
pub(crate) const REQUEST_CANCELLED: i64 = -32800;
pub(crate) const REQUEST_FAILED: i64 = -32803;

/// Byte bound of the held queue (plan: 16 MiB).
pub(crate) const HELD_CAP: usize = 16 << 20;

/// The ids of fleet-lsp's own `textDocument/diagnostic` pulls: a string the
/// client never sends, apart from workspace mode's `fleet-lsp:` lifecycle
/// ids. Their replies reach this machine, never the client.
const PULL_PREFIX: &str = "fleet-lsp-pull:";

pub(crate) fn is_pull_id(id: &Id) -> bool {
    matches!(id, Id::Str(s) if s.starts_with(PULL_PREFIX))
}

#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub(crate) lang: &'static str,
    pub(crate) barrier: Barrier,
    /// `Some`: the session refuses — the stub. Every request gets this text.
    pub(crate) refusal: Option<String>,
    pub(crate) ceiling: Duration,
    pub(crate) log_path: String,
    /// TypeScript: the `tsserver.path` put into `initializationOptions`.
    pub(crate) tsserver_path: Option<String>,
    /// Python: keep `python.analysis.logLevel` ≥ Information in the client's
    /// `workspace/configuration` replies, or pyright's barrier line never
    /// arrives (docs/readiness.md).
    pub(crate) guard_pyright_log_level: bool,
    /// What the child's death ends.
    pub(crate) scope: ChildScope,
    /// The project the child serves; if it is deleted under a running
    /// child, no server message can say anything useful about it.
    pub(crate) project_root: Option<std::path::PathBuf>,
    /// The server pushes no diagnostics for opened files and answers
    /// `textDocument/diagnostic` (effect-tsgo): each document it is told
    /// about is pulled, and each answer published to the client.
    pub(crate) pull_diagnostics: bool,
}

/// What a child's exit or hang ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChildScope {
    /// Single-root mode: the child is the session.
    Session,
    /// Workspace mode: one repository's child among several. Its death
    /// fails its own requests and retires it; the session goes on.
    Slot,
}

/// What the shell observed.
#[derive(Debug)]
pub(crate) enum Input {
    Client {
        body: Vec<u8>,
        scan: Scan,
    },
    ClientOversize {
        len: usize,
        scan: Scan,
    },
    /// The client's stdin ended (or its stdout broke).
    ClientGone,
    /// A server frame the shell relayed (or, per `ServerFilter`, swallowed).
    ServerResponse(Id),
    ServerRequest {
        id: Id,
        method: String,
        /// Present for the requests this machine reads (`workspace/configuration`).
        body: Option<Vec<u8>>,
    },
    ServerSignal(Signal),
    ServerOversize(Scan),
    /// The server's answer to one of fleet-lsp's diagnostic pulls.
    PullReply {
        id: Id,
        body: Vec<u8>,
    },
    /// The child exited; the text names how.
    ChildExited(String),
    /// The child stopped reading its stdin for the hung-peer limit.
    ChildHung,
    Tick,
}

/// What the shell must do, in order.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Action {
    ToChild(Vec<u8>),
    ToClient(Vec<u8>),
    /// What the child-stdout reader must swallow or answer from now on.
    SetFilter(ServerFilter),
    Log(String),
    /// Tear down and exit with this code.
    Exit(u8),
    /// `exit` was forwarded: wait for the child, then exit with this code.
    AwaitChildThenExit(u8),
    /// Workspace mode: this slot's child is gone and its requests failed.
    ChildDead,
}

/// Decided at `initialize`, applied by the child-stdout reader.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ServerFilter {
    /// Swallow `experimental/serverStatus` (fleet-lsp asked for it, the client did not).
    pub(crate) swallow_status: bool,
    /// The client lacks `window.workDoneProgress`, which fleet-lsp added:
    /// swallow `$/progress` and let this machine answer
    /// `window/workDoneProgress/create`.
    pub(crate) own_progress: bool,
}

#[derive(Debug)]
struct Held {
    id: Option<Id>,
    body: Vec<u8>,
    at: Instant,
}

#[derive(Debug)]
pub(crate) struct Core {
    cfg: Config,
    gate: Gate,
    held: VecDeque<Held>,
    held_bytes: usize,
    inflight: HashSet<Id>,
    /// Pending server `workspace/configuration` requests: their sections.
    config_requests: HashMap<Id, Vec<Option<String>>>,
    filter: ServerFilter,
    shutdown_seen: bool,
    exit_seen: bool,
    done: bool,
    pulls: Pulls,
}

/// The pull bridge (`Config::pull_diagnostics`). A pull answers for one
/// document *lifetime* (an open until its close) and one *generation*: the
/// count of changes, to it or to any other open document of the server
/// (`interFileDependencies`), that its diagnostics depend on. One pull per
/// document runs at a time; what changes meanwhile is coalesced into one
/// replacement, issued when the running pull ends.
#[derive(Debug, Default)]
struct Pulls {
    next_id: u64,
    next_lifetime: u64,
    open: BTreeMap<String, OpenDoc>,
    pending: HashMap<Id, Pull>,
    /// A `shutdown` is under way: no new pull goes out.
    stopped: bool,
}

#[derive(Debug)]
struct OpenDoc {
    lifetime: u64,
    generation: u64,
    /// The pull running for this lifetime.
    running: Option<Id>,
}

#[derive(Debug)]
struct Pull {
    uri: String,
    lifetime: u64,
    generation: u64,
    sent: Instant,
}

impl Core {
    pub(crate) fn new(cfg: Config) -> Core {
        let gate = Gate::new(cfg.barrier.clone());
        Core {
            cfg,
            gate,
            held: VecDeque::new(),
            held_bytes: 0,
            inflight: HashSet::new(),
            config_requests: HashMap::new(),
            filter: ServerFilter::default(),
            shutdown_seen: false,
            exit_seen: false,
            done: false,
            pulls: Pulls::default(),
        }
    }

    pub(crate) fn step(&mut self, input: Input, now: Instant) -> Vec<Action> {
        let mut out = Vec::new();
        if self.done {
            return out;
        }
        match input {
            Input::Client { body, scan } => self.client(body, scan, now, &mut out),
            Input::ClientOversize { len, scan } => self.client_oversize(len, &scan, &mut out),
            Input::ClientGone => {
                self.finish(
                    if self.exit_seen && self.shutdown_seen {
                        0
                    } else {
                        1
                    },
                    &mut out,
                );
            }
            Input::ServerResponse(id) => {
                self.inflight.remove(&id);
            }
            Input::ServerRequest { id, method, body } => {
                self.server_request(id, &method, body.as_deref(), &mut out)
            }
            Input::ServerSignal(sig) => self.gate_signal(&sig, &mut out),
            Input::ServerOversize(scan) => self.server_oversize(&scan, now, &mut out),
            Input::PullReply { id, body } => self.pull_done(&id, Some(&body), now, &mut out),
            Input::ChildExited(how) => self.child_exited(&how, &mut out),
            Input::ChildHung => {
                let msg = format!(
                    "fleet-lsp: {}: the server stopped reading its input",
                    self.cfg.lang
                );
                out.push(Action::Log(msg.clone()));
                self.fail_everything(&msg, &mut out);
                self.end_child(&mut out);
            }
            Input::Tick => self.tick(now, &mut out),
        }
        self.bridge(out, now)
    }

    // ---------------------------------------------------------------- client

    fn client(&mut self, body: Vec<u8>, scan: Scan, now: Instant, out: &mut Vec<Action>) {
        let method = scan.method.as_deref().unwrap_or("");
        match scan.kind() {
            Kind::Request => match method {
                "initialize" => self.initialize(body, &scan, out),
                "shutdown" => {
                    self.shutdown_seen = true;
                    if self.cfg.refusal.is_some() {
                        out.push(Action::ToClient(result(scan.id.as_ref(), "null")));
                    } else {
                        self.drain_held_for_shutdown(out);
                        self.forward_request(body, scan.id, out);
                    }
                }
                _ => self.request(body, scan.id, method, now, out),
            },
            Kind::Notification => match method {
                "exit" => {
                    self.exit_seen = true;
                    let code = if self.shutdown_seen { 0 } else { 1 };
                    if self.cfg.refusal.is_some() {
                        self.finish(code, out);
                    } else {
                        self.drain_held_for_shutdown(out);
                        out.push(Action::ToChild(body));
                        self.done = true;
                        out.push(Action::AwaitChildThenExit(code));
                    }
                }
                "$/cancelRequest" => self.cancel(body, out),
                "initialized" => {
                    if self.cfg.barrier == Barrier::Tsgo && self.cfg.refusal.is_none() {
                        // What the gate waits on, so never held behind a request.
                        out.push(Action::ToChild(body));
                    } else {
                        self.notification(body, out);
                    }
                    self.gate_signal(&Signal::Initialized, out);
                }
                _ => self.notification(body, out),
            },
            Kind::Response => self.client_response(body, scan.id.as_ref(), out),
            Kind::Other => self.notification(body, out),
        }
    }

    fn initialize(&mut self, body: Vec<u8>, scan: &Scan, out: &mut Vec<Action>) {
        if let Some(refusal) = &self.cfg.refusal {
            out.push(Action::Log(format!("refusing: {refusal}")));
            out.push(Action::ToClient(result(
                scan.id.as_ref(),
                &stub_capabilities(),
            )));
            return;
        }
        let Some(msg) = std::str::from_utf8(&body)
            .ok()
            .and_then(|t| json::parse(t).ok())
        else {
            out.push(Action::ToChild(body));
            return;
        };
        let (msg, filter) = prepare_initialize(&self.cfg, msg);
        self.filter = filter;
        out.push(Action::SetFilter(filter));
        out.push(Action::ToChild(msg.to_string().into_bytes()));
        if let Some(id) = scan.id.clone() {
            self.inflight.insert(id);
        }
    }

    /// Workspace mode: the shell sent this slot's `initialize` itself (see
    /// `prepare_initialize`); this is the filter it decided.
    pub(crate) fn adopt_filter(&mut self, filter: ServerFilter) {
        self.filter = filter;
    }

    /// Nothing is held, in flight or waiting on a configuration reply: the
    /// child owes the client nothing, so stopping it fails no request.
    pub(crate) fn is_quiet(&self) -> bool {
        self.held.is_empty() && self.inflight.is_empty() && self.config_requests.is_empty()
    }

    /// Workspace mode, when the shell ends a slot itself (an eviction or an
    /// idle stop that ended, a refused `initialize`): every request still
    /// held or in flight is failed with `msg`. A slot whose child died has
    /// none left.
    pub(crate) fn fail_all(&mut self, msg: &str) -> Vec<Action> {
        let mut out = Vec::new();
        self.fail_everything(msg, &mut out);
        out
    }

    /// Workspace mode, before the shell's own `shutdown` reaches the child:
    /// held notifications are returned for the child, held requests failed,
    /// and no diagnostic pull is issued or awaited any more.
    pub(crate) fn drain_for_shutdown(&mut self) -> Vec<Action> {
        let mut out = Vec::new();
        self.drain_held_for_shutdown(&mut out);
        self.pulls.stopped = true;
        self.pulls.forget_pending();
        out
    }

    /// Workspace mode: the child answered the shell's `initialize`. `sent`
    /// is what goes to it now, in order and as it is: `initialized`, the
    /// open documents replayed as `didOpen`, then what waited for the reply.
    /// The gate then hears `initialized`, and each replayed document is
    /// pulled, as in single-root mode.
    pub(crate) fn started(&mut self, sent: Vec<Vec<u8>>, now: Instant) -> Vec<Action> {
        let mut out: Vec<Action> = sent.into_iter().map(Action::ToChild).collect();
        self.gate_signal(&Signal::Initialized, &mut out);
        self.bridge(out, now)
    }

    fn request(
        &mut self,
        body: Vec<u8>,
        id: Option<Id>,
        method: &str,
        now: Instant,
        out: &mut Vec<Action>,
    ) {
        if let Some(refusal) = &self.cfg.refusal {
            out.push(Action::Log(format!("answered {method}: refused")));
            out.push(Action::ToClient(error(
                id.as_ref(),
                REQUEST_FAILED,
                refusal,
            )));
            return;
        }
        if let Some(root) = self.cfg.project_root.as_ref().filter(|r| !r.exists()) {
            let msg = crate::route::refusal::root_gone(self.cfg.lang, &root.display().to_string());
            out.push(Action::Log(format!("answered {method}: project root gone")));
            out.push(Action::ToClient(error(id.as_ref(), REQUEST_FAILED, &msg)));
            self.fail_everything(&msg, out);
            if self.cfg.scope == ChildScope::Slot {
                self.end_child(out);
            }
            return;
        }
        match self.gate.state().clone() {
            State::Failed(msg) => {
                out.push(Action::Log(format!("answered {method}: gate failed")));
                out.push(Action::ToClient(error(id.as_ref(), REQUEST_FAILED, &msg)))
            }
            State::Open if self.held.is_empty() => self.forward_request(body, id, out),
            _ => self.hold(Held { id, body, at: now }, out),
        }
    }

    fn notification(&mut self, body: Vec<u8>, out: &mut Vec<Action>) {
        if self.cfg.refusal.is_some() {
            return;
        }
        if self.held.is_empty() {
            out.push(Action::ToChild(body));
        } else {
            self.hold(
                Held {
                    id: None,
                    body,
                    at: Instant::now(),
                },
                out,
            );
        }
    }

    fn client_response(&mut self, body: Vec<u8>, id: Option<&Id>, out: &mut Vec<Action>) {
        if self.cfg.refusal.is_some() {
            return;
        }
        let sections = id.and_then(|id| self.config_requests.remove(id));
        let body = match sections {
            Some(sections) if self.cfg.guard_pyright_log_level => {
                guard_log_level(&body, &sections).unwrap_or(body)
            }
            _ => body,
        };
        // Never held: a server waiting on this reply may be what the gate waits on.
        out.push(Action::ToChild(body));
    }

    fn cancel(&mut self, body: Vec<u8>, out: &mut Vec<Action>) {
        if self.cfg.refusal.is_some() {
            return;
        }
        let target = std::str::from_utf8(&body)
            .ok()
            .and_then(|t| json::parse(t).ok())
            .and_then(|m| m.get("params").and_then(|p| p.get("id")).cloned())
            .and_then(|v| match v {
                Json::Int(n) => Some(Id::Int(n)),
                Json::Str(s) => Some(Id::Str(s)),
                _ => None,
            });
        if let Some(pos) = target
            .as_ref()
            .and_then(|t| self.held.iter().position(|h| h.id.as_ref() == Some(t)))
        {
            let h = self.remove_held(pos);
            out.push(Action::ToClient(error(
                h.id.as_ref(),
                REQUEST_CANCELLED,
                "cancelled",
            )));
            self.flush_front(out);
        } else {
            out.push(Action::ToChild(body));
        }
    }

    fn client_oversize(&mut self, len: usize, scan: &Scan, out: &mut Vec<Action>) {
        let msg = format!(
            "fleet-lsp: frame too large ({} MiB, limit {} MiB)",
            len >> 20,
            crate::frame::MAX_FRAME >> 20
        );
        out.push(Action::Log(msg.clone()));
        if scan.kind() == Kind::Request {
            out.push(Action::ToClient(error(
                scan.id.as_ref(),
                REQUEST_FAILED,
                &msg,
            )));
        } else {
            // Dropping a notification would desync the server's view of a file.
            self.fail_everything(&msg, out);
            self.finish(1, out);
        }
    }

    // ---------------------------------------------------------------- held queue

    fn hold(&mut self, h: Held, out: &mut Vec<Action>) {
        if self.held_bytes + h.body.len() > HELD_CAP {
            let mib = self.held_bytes >> 20;
            let msg = format!("fleet-lsp: overload: {mib} MiB held before the server was ready");
            out.push(Action::Log(msg.clone()));
            // Fail every held request; notifications go on in order, none dropped.
            for h in std::mem::take(&mut self.held) {
                if h.id.is_some() {
                    out.push(Action::ToClient(error(h.id.as_ref(), REQUEST_FAILED, &msg)));
                } else {
                    out.push(Action::ToChild(h.body));
                }
            }
            self.held_bytes = 0;
        }
        // A notification with nothing held ahead of it has nothing to wait for.
        if h.id.is_none() && self.held.is_empty() {
            out.push(Action::ToChild(h.body));
            return;
        }
        self.held_bytes += h.body.len();
        self.held.push_back(h);
    }

    fn remove_held(&mut self, pos: usize) -> Held {
        let h = self.held.remove(pos).expect("position is in range");
        self.held_bytes -= h.body.len();
        h
    }

    /// Notifications at the front go out until the next held request.
    fn flush_front(&mut self, out: &mut Vec<Action>) {
        while self.held.front().is_some_and(|h| h.id.is_none()) {
            let h = self.remove_held(0);
            out.push(Action::ToChild(h.body));
        }
    }

    fn release_all(&mut self, out: &mut Vec<Action>) {
        while let Some(h) = self.held.pop_front() {
            self.held_bytes -= h.body.len();
            match h.id {
                Some(id) => self.forward_request(h.body, Some(id), out),
                None => out.push(Action::ToChild(h.body)),
            }
        }
    }

    /// Requests answered with `msg`; notifications forwarded, in order.
    fn answer_held(&mut self, msg: &str, out: &mut Vec<Action>) {
        while let Some(h) = self.held.pop_front() {
            self.held_bytes -= h.body.len();
            match h.id {
                Some(_) => out.push(Action::ToClient(error(h.id.as_ref(), REQUEST_FAILED, msg))),
                None => out.push(Action::ToChild(h.body)),
            }
        }
    }

    /// Before `shutdown`/`exit`: notifications flushed, then requests failed.
    fn drain_held_for_shutdown(&mut self, out: &mut Vec<Action>) {
        let held: Vec<Held> = self.held.drain(..).collect();
        self.held_bytes = 0;
        let (requests, notes): (Vec<Held>, Vec<Held>) =
            held.into_iter().partition(|h| h.id.is_some());
        for h in notes {
            out.push(Action::ToChild(h.body));
        }
        for h in requests {
            out.push(Action::ToClient(error(
                h.id.as_ref(),
                REQUEST_CANCELLED,
                "server is shutting down",
            )));
        }
    }

    fn forward_request(&mut self, body: Vec<u8>, id: Option<Id>, out: &mut Vec<Action>) {
        if let Some(id) = id {
            self.inflight.insert(id);
        }
        out.push(Action::ToChild(body));
    }

    fn tick(&mut self, now: Instant, out: &mut Vec<Action>) {
        let ceiling = self.cfg.ceiling;
        let mut i = 0;
        let mut removed = false;
        while i < self.held.len() {
            let h = &self.held[i];
            if h.id.is_some() && now.saturating_duration_since(h.at) >= ceiling {
                let h = self.remove_held(i);
                let msg = format!(
                    "fleet-lsp: {}: server not ready after {}s; log: {}",
                    self.cfg.lang,
                    ceiling.as_secs(),
                    self.cfg.log_path
                );
                out.push(Action::Log(msg.clone()));
                out.push(Action::ToClient(error(h.id.as_ref(), REQUEST_FAILED, &msg)));
                removed = true;
            } else {
                i += 1;
            }
        }
        if removed {
            self.flush_front(out);
        }
        self.expire_pulls(now, out);
    }

    // ---------------------------------------------------------------- pulls

    /// `out` with a diagnostic pull after each `didOpen`/`didChange` it
    /// sends the child, and an empty set published for each `didClose`.
    fn bridge(&mut self, out: Vec<Action>, now: Instant) -> Vec<Action> {
        if !self.cfg.pull_diagnostics {
            return out;
        }
        let mut all = Vec::with_capacity(out.len());
        for action in out {
            let doc = match &action {
                Action::ToChild(b) => document_event(b),
                _ => None,
            };
            all.push(action);
            if let Some((method, uri)) = doc {
                self.on_document(method, &uri, now, &mut all);
            }
        }
        all
    }

    fn on_document(&mut self, method: &str, uri: &str, now: Instant, out: &mut Vec<Action>) {
        match method {
            "textDocument/didOpen" => {
                // A new lifetime: what a pull of an earlier one answers is stale.
                let lifetime = self.pulls.next_lifetime;
                self.pulls.next_lifetime += 1;
                self.pulls.open.insert(
                    uri.to_string(),
                    OpenDoc {
                        lifetime,
                        generation: 0,
                        running: None,
                    },
                );
                self.pull(uri, now, out);
            }
            "textDocument/didChange" => {
                // effect-tsgo declares `interFileDependencies`: an edit can
                // change any open document's diagnostics.
                let mut idle = Vec::new();
                for (u, doc) in &mut self.pulls.open {
                    doc.generation += 1;
                    if doc.running.is_none() {
                        idle.push(u.clone());
                    }
                }
                for u in idle {
                    self.pull(&u, now, out);
                }
            }
            _ => {
                self.pulls.open.remove(uri);
                out.push(Action::ToClient(publish(uri, Json::Arr(Vec::new()))));
            }
        }
    }

    /// One pull of `uri`'s current lifetime and generation.
    fn pull(&mut self, uri: &str, now: Instant, out: &mut Vec<Action>) {
        if self.pulls.stopped || self.shutdown_seen || self.exit_seen {
            return;
        }
        let Some(doc) = self.pulls.open.get_mut(uri) else {
            return;
        };
        let id = Id::Str(format!("{PULL_PREFIX}{}", self.pulls.next_id));
        self.pulls.next_id += 1;
        doc.running = Some(id.clone());
        self.pulls.pending.insert(
            id.clone(),
            Pull {
                uri: uri.to_string(),
                lifetime: doc.lifetime,
                generation: doc.generation,
                sent: now,
            },
        );
        let msg = crate::route::with_id(
            Json::obj()
                .set("jsonrpc", "2.0")
                .set("method", "textDocument/diagnostic")
                .set(
                    "params",
                    Json::obj().set("textDocument", Json::obj().set("uri", uri)),
                ),
            &id,
        );
        out.push(Action::ToChild(msg.to_string().into_bytes()));
    }

    /// A pull ended: answered (`body`), or failed without a report. Its
    /// document is free for the next pull; an answer for the document's
    /// current lifetime and generation is published, an obsolete one is
    /// dropped and replaced.
    fn pull_done(&mut self, id: &Id, body: Option<&[u8]>, now: Instant, out: &mut Vec<Action>) {
        let Some(pull) = self.pulls.pending.remove(id) else {
            return;
        };
        let Some(doc) = self
            .pulls
            .open
            .get_mut(&pull.uri)
            .filter(|d| d.lifetime == pull.lifetime)
        else {
            return;
        };
        if doc.running.as_ref() == Some(id) {
            doc.running = None;
        }
        if doc.generation != pull.generation {
            self.pull(&pull.uri, now, out);
            return;
        }
        match body.map(diagnostic_report) {
            Some(Some(Report::Full(items))) => {
                out.push(Action::ToClient(publish(&pull.uri, items)));
            }
            // The client keeps the set it was last sent.
            Some(Some(Report::Unchanged)) | None => {}
            Some(None) => out.push(Action::Log(format!(
                "diagnostic pull of {} got no report",
                pull.uri
            ))),
        }
    }

    /// Pulls unanswered for the ceiling are given up (and cancelled at the
    /// server); a replacement coalesced behind one goes out now.
    fn expire_pulls(&mut self, now: Instant, out: &mut Vec<Action>) {
        let ceiling = self.cfg.ceiling;
        let mut late: Vec<Id> = self
            .pulls
            .pending
            .iter()
            .filter(|(_, p)| now.saturating_duration_since(p.sent) >= ceiling)
            .map(|(id, _)| id.clone())
            .collect();
        late.sort_by_key(Id::to_json);
        for id in late {
            let Some(pull) = self.pulls.pending.get(&id) else {
                continue;
            };
            out.push(Action::Log(format!(
                "diagnostic pull of {} unanswered after {}s; dropped",
                pull.uri,
                ceiling.as_secs()
            )));
            let cancel = Json::obj()
                .set("jsonrpc", "2.0")
                .set("method", "$/cancelRequest")
                .set("params", crate::route::with_id(Json::obj(), &id));
            out.push(Action::ToChild(cancel.to_string().into_bytes()));
            self.pull_done(&id, None, now, out);
        }
    }

    // ---------------------------------------------------------------- server

    fn gate_signal(&mut self, signal: &Signal, out: &mut Vec<Action>) {
        for effect in self.gate.on(signal) {
            self.gate_effect(effect, out);
        }
    }

    fn gate_effect(&mut self, effect: Effect, out: &mut Vec<Action>) {
        match effect {
            Effect::Now(State::Open) => {
                out.push(Action::Log("gate open".into()));
                self.release_all(out);
            }
            Effect::Now(State::Closed) => out.push(Action::Log("gate closed".into())),
            Effect::Now(State::Failed(msg)) => {
                out.push(Action::Log(format!("gate failed: {msg}")));
                self.answer_held(&msg, out);
            }
            Effect::Warn(msg) => {
                out.push(Action::Log(format!("warning: {msg}")));
                let mut text = String::new();
                Json::obj()
                    .set("jsonrpc", "2.0")
                    .set("method", "window/showMessage")
                    .set("params", Json::obj().set("type", 2).set("message", msg))
                    .write(&mut text);
                out.push(Action::ToClient(text.into_bytes()));
            }
        }
    }

    fn server_request(&mut self, id: Id, method: &str, body: Option<&[u8]>, out: &mut Vec<Action>) {
        match method {
            "window/workDoneProgress/create" if self.filter.own_progress => {
                out.push(Action::ToChild(result(Some(&id), "null")));
            }
            "workspace/configuration" if self.cfg.guard_pyright_log_level => {
                let sections = body
                    .and_then(|b| std::str::from_utf8(b).ok())
                    .and_then(|t| json::parse(t).ok())
                    .and_then(|m| {
                        m.get("params")
                            .and_then(|p| p.get("items"))
                            .and_then(|i| i.as_arr().map(|a| a.to_vec()))
                    })
                    .map(|items| {
                        items
                            .iter()
                            .map(|it| it.get("section").and_then(Json::as_str).map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                self.config_requests.insert(id, sections);
            }
            _ => {}
        }
    }

    fn server_oversize(&mut self, scan: &Scan, now: Instant, out: &mut Vec<Action>) {
        let msg = format!(
            "fleet-lsp: server frame over {} MiB dropped",
            crate::frame::MAX_FRAME >> 20
        );
        out.push(Action::Log(msg.clone()));
        if let Some(id) = scan.id.as_ref().filter(|id| is_pull_id(id)) {
            // fleet-lsp's own pull: nobody to answer; the document is freed.
            self.pull_done(id, None, now, out);
            return;
        }
        match scan.kind() {
            Kind::Response => {
                if let Some(id) = &scan.id {
                    self.inflight.remove(id);
                }
                out.push(Action::ToClient(error(
                    scan.id.as_ref(),
                    REQUEST_FAILED,
                    &msg,
                )));
            }
            Kind::Request => out.push(Action::ToChild(error(
                scan.id.as_ref(),
                REQUEST_FAILED,
                &msg,
            ))),
            _ => {}
        }
    }

    fn child_exited(&mut self, how: &str, out: &mut Vec<Action>) {
        out.push(Action::Log(format!("server exited: {how}")));
        if self.exit_seen {
            self.finish(if self.shutdown_seen { 0 } else { 1 }, out);
            return;
        }
        let msg = format!("fleet-lsp: {}: the server exited ({how})", self.cfg.lang);
        self.fail_everything(&msg, out);
        self.end_child(out);
    }

    /// The child is gone: the session ends with it, or only its slot.
    fn end_child(&mut self, out: &mut Vec<Action>) {
        self.pulls.forget_pending();
        match self.cfg.scope {
            ChildScope::Session => self.finish(1, out),
            ChildScope::Slot => {
                self.done = true;
                out.push(Action::ChildDead);
            }
        }
    }

    fn fail_everything(&mut self, msg: &str, out: &mut Vec<Action>) {
        self.answer_held(msg, out);
        let mut ids: Vec<Id> = self.inflight.drain().collect();
        ids.sort_by_key(Id::to_json);
        for id in ids {
            out.push(Action::ToClient(error(Some(&id), REQUEST_FAILED, msg)));
        }
    }

    fn finish(&mut self, code: u8, out: &mut Vec<Action>) {
        self.done = true;
        out.push(Action::Exit(code));
    }
}

impl Pulls {
    /// No pending pull is awaited any more (the server exited, hung or is
    /// shutting down): their documents are free, and no replacement follows.
    fn forget_pending(&mut self) {
        self.pending.clear();
        for doc in self.open.values_mut() {
            doc.running = None;
        }
    }
}

/// What a `textDocument/diagnostic` answer reported.
#[derive(Debug, PartialEq)]
enum Report {
    /// `kind: "full"`: its `items`.
    Full(Json),
    /// `kind: "unchanged"`.
    Unchanged,
}

fn diagnostic_report(body: &[u8]) -> Option<Report> {
    let msg = json::parse(std::str::from_utf8(body).ok()?).ok()?;
    let result = msg.get("result")?;
    match result.get("kind")?.as_str()? {
        "full" => match result.get("items")? {
            items @ Json::Arr(_) => Some(Report::Full(items.clone())),
            _ => None,
        },
        "unchanged" => Some(Report::Unchanged),
        _ => None,
    }
}

/// `textDocument/publishDiagnostics` of `items` for `uri`.
fn publish(uri: &str, items: Json) -> Vec<u8> {
    Json::obj()
        .set("jsonrpc", "2.0")
        .set("method", "textDocument/publishDiagnostics")
        .set(
            "params",
            Json::obj().set("uri", uri).set("diagnostics", items),
        )
        .to_string()
        .into_bytes()
}

/// The method and document of a `didOpen`, `didChange` or `didClose`.
fn document_event(body: &[u8]) -> Option<(&'static str, String)> {
    let method = match scan(body).method.as_deref()? {
        "textDocument/didOpen" => "textDocument/didOpen",
        "textDocument/didChange" => "textDocument/didChange",
        "textDocument/didClose" => "textDocument/didClose",
        _ => return None,
    };
    let msg = json::parse(std::str::from_utf8(body).ok()?).ok()?;
    let uri = crate::route::document_uri(&msg)?.to_string();
    Some((method, uri))
}

/// The client's `initialize`, rewritten for the child: rust-analyzer's
/// status notifications, `window.workDoneProgress` (answered by fleet-lsp
/// when the client lacks it) and the tsserver path. The filter tells the
/// child-stdout reader what fleet-lsp asked for that the client did not.
pub(crate) fn prepare_initialize(cfg: &Config, mut msg: Json) -> (Json, ServerFilter) {
    let mut filter = ServerFilter::default();
    {
        let params = msg.obj_at(&["params"]);
        let params = params
            .entry("capabilities".into())
            .or_insert_with(Json::obj);
        if cfg.barrier == Barrier::RustAnalyzer {
            params
                .obj_at(&["experimental"])
                .insert("serverStatusNotification".into(), Json::Bool(true));
            filter.swallow_status = true;
        }
        let window = params.obj_at(&["window"]);
        if window.get("workDoneProgress") != Some(&Json::Bool(true)) {
            window.insert("workDoneProgress".into(), Json::Bool(true));
            filter.own_progress = true;
        }
    }
    if let Some(path) = &cfg.tsserver_path {
        msg.obj_at(&["params", "initializationOptions", "tsserver"])
            .insert("path".into(), Json::Str(path.clone()));
    }
    (msg, filter)
}

/// The capabilities the refusal stub advertises: the ones Claude Code's LSP
/// tool calls, so its requests reach the stub and get the refusal text
/// instead of being rejected locally.
fn stub_capabilities() -> String {
    capabilities("fleet-lsp (refusing)")
}

/// What fleet-lsp answers `initialize` with when it answers it itself (the
/// refusal stub, workspace mode): the operations Claude Code's LSP tool
/// sends, with full document sync, which every server accepts.
pub(crate) fn capabilities(server_name: &str) -> String {
    let caps = Json::obj()
        .set("textDocumentSync", 1)
        .set("definitionProvider", true)
        .set("referencesProvider", true)
        .set("hoverProvider", true)
        .set("documentSymbolProvider", true)
        .set("workspaceSymbolProvider", true)
        .set("implementationProvider", true)
        .set("callHierarchyProvider", true);
    Json::obj()
        .set("capabilities", caps)
        .set(
            "serverInfo",
            Json::obj()
                .set("name", server_name)
                .set("version", env!("CARGO_PKG_VERSION")),
        )
        .to_string()
}

fn id_json(id: Option<&Id>) -> String {
    id.map_or_else(|| "null".to_string(), Id::to_json)
}

pub(crate) fn result(id: Option<&Id>, result_json: &str) -> Vec<u8> {
    format!(
        r#"{{"jsonrpc":"2.0","id":{},"result":{}}}"#,
        id_json(id),
        result_json
    )
    .into_bytes()
}

pub(crate) fn error(id: Option<&Id>, code: i64, message: &str) -> Vec<u8> {
    let mut msg = String::new();
    Json::Str(message.to_string()).write(&mut msg);
    format!(
        r#"{{"jsonrpc":"2.0","id":{},"error":{{"code":{},"message":{}}}}}"#,
        id_json(id),
        code,
        msg
    )
    .into_bytes()
}

/// Rewrites a client's `workspace/configuration` reply so pyright keeps
/// logging at Information: its readiness line is an Information log.
/// `None` when nothing needed changing (the original bytes are kept).
fn guard_log_level(body: &[u8], sections: &[Option<String>]) -> Option<Vec<u8>> {
    let mut msg = json::parse(std::str::from_utf8(body).ok()?).ok()?;
    let Json::Obj(top) = &mut msg else {
        return None;
    };
    let Some(Json::Arr(items)) = top.get_mut("result") else {
        return None;
    };
    let mut changed = false;
    for (item, section) in items.iter_mut().zip(sections) {
        let target = match section.as_deref() {
            Some("python.analysis") => Some(item),
            Some("python") => match item {
                Json::Obj(m) => m.get_mut("analysis"),
                _ => None,
            },
            _ => None,
        };
        if let Some(Json::Obj(m)) = target {
            let quiet = matches!(
                m.get("logLevel").and_then(Json::as_str),
                Some("Error") | Some("Warning")
            );
            if quiet {
                m.insert("logLevel".into(), Json::Str("Information".into()));
                changed = true;
            }
        }
    }
    changed.then(|| msg.to_string().into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::scan;

    fn cfg(barrier: Barrier) -> Config {
        Config {
            lang: "rust",
            barrier,
            refusal: None,
            ceiling: Duration::from_secs(370),
            log_path: "/log".into(),
            tsserver_path: None,
            guard_pyright_log_level: false,
            scope: ChildScope::Session,
            project_root: None,
            pull_diagnostics: false,
        }
    }

    fn client(body: &str) -> Input {
        Input::Client {
            body: body.as_bytes().to_vec(),
            scan: scan(body.as_bytes()),
        }
    }

    fn texts(actions: &[Action]) -> Vec<String> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::ToChild(b) => Some(format!("child {}", String::from_utf8_lossy(b))),
                Action::ToClient(b) => Some(format!("client {}", String::from_utf8_lossy(b))),
                _ => None,
            })
            .collect()
    }

    fn quiescent() -> Input {
        Input::ServerSignal(Signal::Status {
            health: "ok".into(),
            quiescent: true,
            message: None,
        })
    }

    const REQ1: &str = r#"{"jsonrpc":"2.0","id":1,"method":"textDocument/references"}"#;
    const REQ2: &str = r#"{"jsonrpc":"2.0","id":2,"method":"textDocument/hover"}"#;
    const CHANGE: &str = r#"{"jsonrpc":"2.0","method":"textDocument/didChange"}"#;

    #[test]
    fn a_core_is_quiet_only_with_nothing_held_or_in_flight() {
        let mut c = Core::new(cfg(Barrier::RustAnalyzer));
        let t = Instant::now();
        assert!(c.is_quiet(), "new");
        c.step(client(REQ1), t);
        assert!(!c.is_quiet(), "a request held behind the barrier");
        c.step(quiescent(), t);
        assert!(!c.is_quiet(), "the request is in flight at the server");
        c.step(Input::ServerResponse(Id::Int(1)), t);
        assert!(c.is_quiet(), "answered");
    }

    #[test]
    fn held_until_quiescent_and_order_kept() {
        let mut c = Core::new(cfg(Barrier::RustAnalyzer));
        let t = Instant::now();
        assert!(texts(&c.step(client(REQ1), t)).is_empty());
        assert!(
            texts(&c.step(client(CHANGE), t)).is_empty(),
            "notification behind a held request waits"
        );
        let out = texts(&c.step(quiescent(), t));
        assert_eq!(
            out,
            vec![format!("child {REQ1}"), format!("child {CHANGE}")]
        );
        // Open now: straight through.
        assert_eq!(
            texts(&c.step(client(REQ2), t)),
            vec![format!("child {REQ2}")]
        );
    }

    #[test]
    fn notification_with_nothing_held_is_not_held() {
        let mut c = Core::new(cfg(Barrier::RustAnalyzer));
        assert_eq!(
            texts(&c.step(client(CHANGE), Instant::now())),
            vec![format!("child {CHANGE}")]
        );
    }

    #[test]
    fn replies_and_cancel_and_lifecycle_are_never_held() {
        let mut c = Core::new(cfg(Barrier::RustAnalyzer));
        let t = Instant::now();
        c.step(client(REQ1), t);
        let reply = r#"{"jsonrpc":"2.0","id":0,"result":[null]}"#;
        assert_eq!(
            texts(&c.step(client(reply), t)),
            vec![format!("child {reply}")]
        );
        let initialized = r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#;
        // `initialized` is a notification; with a request held it waits behind it
        // — but it is never sent after one in practice. Cancel of an unheld id passes.
        let cancel = r#"{"jsonrpc":"2.0","method":"$/cancelRequest","params":{"id":99}}"#;
        assert_eq!(
            texts(&c.step(client(cancel), t)),
            vec![format!("child {cancel}")]
        );
        let _ = initialized;
    }

    #[test]
    fn cancel_of_a_held_request_answers_it_and_frees_what_was_behind() {
        let mut c = Core::new(cfg(Barrier::RustAnalyzer));
        let t = Instant::now();
        c.step(client(REQ1), t);
        c.step(client(CHANGE), t);
        let cancel = r#"{"jsonrpc":"2.0","method":"$/cancelRequest","params":{"id":1}}"#;
        let out = texts(&c.step(client(cancel), t));
        assert!(
            out[0].starts_with("client ")
                && out[0].contains("-32800")
                && out[0].contains(r#""id":1"#)
        );
        assert_eq!(out[1], format!("child {CHANGE}"));
    }

    #[test]
    fn cancel_matches_decoded_string_ids() {
        let mut c = Core::new(cfg(Barrier::RustAnalyzer));
        let t = Instant::now();
        c.step(client(r#"{"id":"abc","method":"m"}"#), t);
        let out = texts(&c.step(
            client(r#"{"method":"$/cancelRequest","params":{"id":"abc"}}"#),
            t,
        ));
        assert!(out[0].contains("-32800"), "{out:?}");
    }

    #[test]
    fn ceiling_answers_with_an_error_naming_n_and_the_log() {
        let mut c = Core::new(cfg(Barrier::RustAnalyzer));
        let t = Instant::now();
        c.step(client(REQ1), t);
        c.step(client(CHANGE), t);
        assert!(texts(&c.step(Input::Tick, t + Duration::from_secs(369))).is_empty());
        let out = texts(&c.step(Input::Tick, t + Duration::from_secs(370)));
        assert!(
            out[0].contains("not ready after 370s; log: /log"),
            "{out:?}"
        );
        assert_eq!(out[1], format!("child {CHANGE}"));
    }

    #[test]
    fn shutdown_flushes_notifications_then_fails_requests() {
        let mut c = Core::new(cfg(Barrier::RustAnalyzer));
        let t = Instant::now();
        c.step(client(REQ1), t);
        c.step(client(CHANGE), t);
        let out = texts(&c.step(client(r#"{"jsonrpc":"2.0","id":9,"method":"shutdown"}"#), t));
        assert_eq!(out[0], format!("child {CHANGE}"));
        assert!(out[1].contains("-32800") && out[1].contains(r#""id":1"#));
        assert!(out[2].contains("shutdown"));
    }

    #[test]
    fn exit_after_shutdown_waits_for_the_child_with_code_0() {
        let mut c = Core::new(cfg(Barrier::None));
        let t = Instant::now();
        c.step(client(r#"{"jsonrpc":"2.0","id":9,"method":"shutdown"}"#), t);
        let out = c.step(client(r#"{"jsonrpc":"2.0","method":"exit"}"#), t);
        assert!(out.contains(&Action::AwaitChildThenExit(0)));
        let mut c = Core::new(cfg(Barrier::None));
        let out = c.step(client(r#"{"jsonrpc":"2.0","method":"exit"}"#), t);
        assert!(out.contains(&Action::AwaitChildThenExit(1)));
    }

    #[test]
    fn health_error_answers_held_and_later_requests() {
        let mut c = Core::new(cfg(Barrier::RustAnalyzer));
        let t = Instant::now();
        c.step(client(REQ1), t);
        let out = texts(&c.step(
            Input::ServerSignal(Signal::Status {
                health: "error".into(),
                quiescent: true,
                message: Some("Failed to load workspaces.".into()),
            }),
            t,
        ));
        assert!(out[0].contains("workspace did not load"), "{out:?}");
        let out = texts(&c.step(client(REQ2), t));
        assert!(out[0].contains("workspace did not load"));
    }

    #[test]
    fn health_warning_shows_a_message_once() {
        let mut c = Core::new(cfg(Barrier::RustAnalyzer));
        let warn = || {
            Input::ServerSignal(Signal::Status {
                health: "warning".into(),
                quiescent: true,
                message: Some("no matching package named `serde`".into()),
            })
        };
        let t = Instant::now();
        let out = texts(&c.step(warn(), t));
        assert!(out
            .iter()
            .any(|m| m.contains("window/showMessage") && m.contains("cargo fetch")));
        assert!(texts(&c.step(warn(), t)).is_empty());
    }

    #[test]
    fn overload_fails_held_requests_and_keeps_every_notification() {
        let mut c = Core::new(cfg(Barrier::RustAnalyzer));
        let t = Instant::now();
        c.step(client(REQ1), t);
        // Fill the held queue to 20 bytes short of its cap, so REQ2 overflows it.
        let frame = |n: usize| {
            format!(
                r#"{{"method":"textDocument/didChange","params":"{}"}}"#,
                "x".repeat(n)
            )
        };
        let overhead = frame(0).len();
        let big = frame(HELD_CAP - 20 - REQ1.len() - overhead);
        assert!(texts(&c.step(client(&big), t)).is_empty(), "fits, so held");
        let out = texts(&c.step(client(REQ2), t));
        assert!(out[0].contains("overload"), "{}", &out[0][..80]);
        assert!(out[1].starts_with("child ") && out[1].contains("didChange"));
        // REQ2 is held now, in the emptied queue.
        let out = texts(&c.step(quiescent(), t));
        assert_eq!(out, vec![format!("child {REQ2}")]);
    }

    #[test]
    fn child_exit_before_shutdown_fails_inflight_and_held() {
        let mut c = Core::new(cfg(Barrier::None));
        let t = Instant::now();
        c.step(client(REQ1), t);
        let out = c.step(Input::ChildExited("signal 9".into()), t);
        assert!(texts(&out)[0].contains("the server exited (signal 9)"));
        assert!(out.contains(&Action::Exit(1)));
    }

    /// FALSIFY: map `ChildScope::Slot` to `finish(1)` in `end_child`.
    #[test]
    fn a_slot_child_dying_retires_the_slot_not_the_session() {
        let mut cf = cfg(Barrier::None);
        cf.scope = ChildScope::Slot;
        let mut c = Core::new(cf);
        let t = Instant::now();
        c.step(client(REQ1), t);
        let out = c.step(Input::ChildExited("signal 9".into()), t);
        assert!(texts(&out)[0].contains("the server exited (signal 9)"));
        assert!(out.contains(&Action::ChildDead));
        assert!(!out.iter().any(|a| matches!(a, Action::Exit(_))));
        let hung = Core::new(Config {
            scope: ChildScope::Slot,
            ..cfg(Barrier::None)
        })
        .step(Input::ChildHung, t);
        assert!(hung.contains(&Action::ChildDead));
        assert!(!hung.iter().any(|a| matches!(a, Action::Exit(_))));
    }

    /// FALSIFY: drop the `project_root` check in `request`.
    #[test]
    fn a_deleted_project_root_is_named_and_retires_a_slot() {
        let gone = std::path::PathBuf::from("/nonexistent/fleet-lsp-test-root");
        let t = Instant::now();
        let mut cf = cfg(Barrier::RustAnalyzer);
        cf.project_root = Some(gone);
        let out = Core::new(cf.clone()).step(client(REQ1), t);
        let all = texts(&out).join("\n");
        assert!(all.contains("no longer exists"), "{all}");
        assert!(!all.contains("cargo metadata"), "{all}");
        assert!(out.contains(&Action::Log(
            "answered textDocument/references: project root gone".into()
        )));
        assert!(!out.contains(&Action::ChildDead));
        cf.scope = ChildScope::Slot;
        let out = Core::new(cf).step(client(REQ1), t);
        assert!(out.contains(&Action::ChildDead));
        let mut live = cfg(Barrier::None);
        live.project_root = Some(std::env::temp_dir());
        let out = Core::new(live).step(client(REQ1), t);
        assert!(!texts(&out).join("").contains("no longer exists"));
    }

    #[test]
    fn prepare_initialize_asks_for_what_the_gate_needs() {
        let msg = json::parse(
            r#"{"id":"fleet-lsp:1","method":"initialize","params":{"capabilities":{}}}"#,
        )
        .expect("json");
        let (msg, filter) = prepare_initialize(&cfg(Barrier::RustAnalyzer), msg);
        let text = msg.to_string();
        assert!(
            text.contains(r#""serverStatusNotification":true"#),
            "{text}"
        );
        assert!(text.contains(r#""workDoneProgress":true"#), "{text}");
        assert!(text.contains(r#""id":"fleet-lsp:1""#), "{text}");
        assert_eq!(
            filter,
            ServerFilter {
                swallow_status: true,
                own_progress: true
            }
        );
    }

    #[test]
    fn stub_answers_initialize_requests_shutdown_and_exit() {
        let mut cf = cfg(Barrier::None);
        cf.refusal = Some("python: no pinned pyright in the venv; fix: uv sync".into());
        let mut c = Core::new(cf);
        let t = Instant::now();
        let out = texts(&c.step(client(r#"{"id":0,"method":"initialize","params":{}}"#), t));
        assert!(out[0].contains("referencesProvider") && out[0].contains("callHierarchyProvider"));
        let out = texts(&c.step(client(REQ1), t));
        assert!(out[0].contains("uv sync") && out[0].contains("-32803"));
        assert!(texts(&c.step(client(CHANGE), t)).is_empty());
        let out = texts(&c.step(client(r#"{"id":5,"method":"shutdown"}"#), t));
        assert_eq!(
            out,
            vec![r#"client {"jsonrpc":"2.0","id":5,"result":null}"#.to_string()]
        );
        let out = c.step(client(r#"{"method":"exit"}"#), t);
        assert!(out.contains(&Action::Exit(0)));
    }

    #[test]
    fn initialize_injects_status_and_progress_and_tsserver_path() {
        let mut cf = cfg(Barrier::RustAnalyzer);
        cf.tsserver_path = Some("/r/node_modules/typescript/lib".into());
        let mut c = Core::new(cf);
        let out = c.step(
            client(
                r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"capabilities":{}}}"#,
            ),
            Instant::now(),
        );
        assert!(out.contains(&Action::SetFilter(ServerFilter {
            swallow_status: true,
            own_progress: true
        })));
        let sent = texts(&out).join("");
        assert!(sent.contains(r#""serverStatusNotification":true"#));
        assert!(sent.contains(r#""workDoneProgress":true"#));
        assert!(sent.contains(r#""path":"/r/node_modules/typescript/lib""#));
    }

    #[test]
    fn client_progress_support_means_no_own_progress() {
        let mut c = Core::new(cfg(Barrier::None));
        let out = c.step(
            client(r#"{"id":0,"method":"initialize","params":{"capabilities":{"window":{"workDoneProgress":true}}}}"#),
            Instant::now(),
        );
        assert!(out.contains(&Action::SetFilter(ServerFilter::default())));
    }

    #[test]
    fn own_progress_answers_create() {
        let mut c = Core::new(cfg(Barrier::None));
        let t = Instant::now();
        c.step(
            client(r#"{"id":0,"method":"initialize","params":{"capabilities":{}}}"#),
            t,
        );
        let out = texts(&c.step(
            Input::ServerRequest {
                id: Id::Int(4),
                method: "window/workDoneProgress/create".into(),
                body: None,
            },
            t,
        ));
        assert_eq!(
            out,
            vec![r#"child {"jsonrpc":"2.0","id":4,"result":null}"#.to_string()]
        );
    }

    #[test]
    fn pyright_log_level_kept_at_information() {
        let mut cf = cfg(Barrier::PyrightEnumeration);
        cf.guard_pyright_log_level = true;
        let mut c = Core::new(cf);
        let t = Instant::now();
        c.step(
            Input::ServerRequest {
                id: Id::Int(3),
                method: "workspace/configuration".into(),
                body: Some(
                    br#"{"id":3,"method":"workspace/configuration","params":{"items":[{"section":"python"},{"section":"python.analysis"}]}}"#
                        .to_vec(),
                ),
            },
            t,
        );
        let reply = r#"{"jsonrpc":"2.0","id":3,"result":[{"analysis":{"logLevel":"Error"}},{"logLevel":"Warning","x":1}]}"#;
        let out = texts(&c.step(client(reply), t));
        assert_eq!(out.len(), 1);
        assert!(
            !out[0].contains("Error") && !out[0].contains("Warning"),
            "{}",
            out[0]
        );
        assert_eq!(out[0].matches("Information").count(), 2);
        // An unrelated reply passes byte for byte.
        let other = r#"{"jsonrpc":"2.0","id":8,"result":[{"logLevel":"Error"}]}"#;
        assert_eq!(
            texts(&c.step(client(other), t)),
            vec![format!("child {other}")]
        );
    }

    #[test]
    fn oversize_client_request_is_answered_and_notification_tears_down() {
        let mut c = Core::new(cfg(Barrier::None));
        let t = Instant::now();
        let s = scan(br#"{"id":4,"method":"m"}"#);
        let out = texts(&c.step(
            Input::ClientOversize {
                len: 33 << 20,
                scan: s,
            },
            t,
        ));
        assert!(out[0].contains("frame too large"));
        let s = scan(br#"{"method":"textDocument/didChange"}"#);
        let out = c.step(
            Input::ClientOversize {
                len: 33 << 20,
                scan: s,
            },
            t,
        );
        assert!(out.contains(&Action::Exit(1)));
    }

    #[test]
    fn oversize_server_response_becomes_an_error_for_its_id() {
        let mut c = Core::new(cfg(Barrier::None));
        let out = texts(&c.step(
            Input::ServerOversize(scan(br#"{"id":12,"result":[]}"#)),
            Instant::now(),
        ));
        assert!(out[0].starts_with("client ") && out[0].contains(r#""id":12"#));
    }

    // ------------------------------------------------------------ pull bridge

    fn tsgo() -> Core {
        let mut cf = cfg(Barrier::Tsgo);
        cf.lang = "typescript";
        cf.pull_diagnostics = true;
        Core::new(cf)
    }

    fn open(uri: &str, version: i64) -> Input {
        client(&format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"{uri}","languageId":"typescript","version":{version},"text":"x"}}}}}}"#
        ))
    }

    fn change(uri: &str, version: i64) -> Input {
        client(&format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didChange","params":{{"textDocument":{{"uri":"{uri}","version":{version}}},"contentChanges":[{{"text":"y"}}]}}}}"#
        ))
    }

    fn close(uri: &str) -> Input {
        client(&format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didClose","params":{{"textDocument":{{"uri":"{uri}"}}}}}}"#
        ))
    }

    /// The pulls `actions` send the child: (id, uri), in order.
    fn pulls(actions: &[Action]) -> Vec<(Id, String)> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::ToChild(b) => {
                    let m = json::parse(std::str::from_utf8(b).ok()?).ok()?;
                    (m.get("method")?.as_str()? == "textDocument/diagnostic").then_some(())?;
                    let id = crate::route::id_of(m.get("id")?)?;
                    let uri = crate::route::document_uri(&m)?.to_string();
                    Some((id, uri))
                }
                _ => None,
            })
            .collect()
    }

    /// The `(uri, diagnostics)` of each `publishDiagnostics` sent the client.
    fn published(actions: &[Action]) -> Vec<(String, String)> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::ToClient(b) => {
                    let m = json::parse(std::str::from_utf8(b).ok()?).ok()?;
                    (m.get("method")?.as_str()? == "textDocument/publishDiagnostics")
                        .then_some(())?;
                    let p = m.get("params")?;
                    Some((
                        p.get("uri")?.as_str()?.to_string(),
                        p.get("diagnostics")?.to_string(),
                    ))
                }
                _ => None,
            })
            .collect()
    }

    fn full(id: &Id, message: &str) -> Input {
        Input::PullReply {
            id: id.clone(),
            body: format!(
                r#"{{"jsonrpc":"2.0","id":{},"result":{{"kind":"full","items":[{{"message":"{message}"}}]}}}}"#,
                id.to_json()
            )
            .into_bytes(),
        }
    }

    fn items(message: &str) -> String {
        format!(r#"[{{"message":"{message}"}}]"#)
    }

    /// A Tsgo core past `initialized`, with `uris` opened and their first
    /// pulls returned.
    fn opened(c: &mut Core, uris: &[&str], t: Instant) -> Vec<(Id, String)> {
        c.step(
            client(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#),
            t,
        );
        let mut all = Vec::new();
        for u in uris {
            all.extend(pulls(&c.step(open(u, 1), t)));
        }
        all
    }

    /// FALSIFY: drop the `bridge` call at the end of `step`.
    #[test]
    fn a_didopen_is_pulled_after_it_and_its_full_report_published() {
        let mut c = tsgo();
        let t = Instant::now();
        c.step(
            client(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#),
            t,
        );
        let out = c.step(open("file:///a.ts", 1), t);
        let sent = texts(&out);
        assert!(sent[0].contains("didOpen"), "{sent:?}");
        assert!(sent[1].contains("textDocument/diagnostic"), "{sent:?}");
        let p = pulls(&out);
        assert_eq!(p.len(), 1);
        assert!(is_pull_id(&p[0].0), "{:?}", p[0].0);
        assert!(!crate::route::is_own(&p[0].0), "apart from lifecycle ids");
        let out = c.step(full(&p[0].0, "globalDate"), t);
        assert_eq!(
            published(&out),
            vec![("file:///a.ts".into(), items("globalDate"))]
        );
        assert!(
            !texts(&out).iter().any(|m| m.contains(PULL_PREFIX)),
            "the reply never reaches the client"
        );
        assert!(c.is_quiet());
    }

    #[test]
    fn without_the_bridge_nothing_is_pulled_or_published() {
        let mut c = Core::new(cfg(Barrier::None));
        let t = Instant::now();
        for input in [
            open("file:///a.ts", 1),
            change("file:///a.ts", 2),
            close("file:///a.ts"),
        ] {
            let out = c.step(input, t);
            assert!(pulls(&out).is_empty() && published(&out).is_empty());
        }
    }

    /// FALSIFY: open `Barrier::Tsgo` before `initialized`.
    #[test]
    fn initialized_opens_the_tsgo_gate_and_releases_what_it_held() {
        let mut c = tsgo();
        let t = Instant::now();
        assert!(texts(&c.step(client(REQ1), t)).is_empty(), "held");
        let out = texts(&c.step(
            client(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#),
            t,
        ));
        assert!(out[0].contains("initialized"), "{out:?}");
        assert_eq!(out[1], format!("child {REQ1}"));
    }

    /// FALSIFY: answer every pending pull in `fail_everything`.
    #[test]
    fn a_pull_is_never_answered_to_the_client_when_the_server_dies() {
        let mut c = tsgo();
        let t = Instant::now();
        opened(&mut c, &["file:///a.ts"], t);
        c.step(client(REQ1), t);
        let out = c.step(Input::ChildExited("signal 9".into()), t);
        let sent = texts(&out);
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert!(sent[0].contains(r#""id":1"#), "{sent:?}");
        assert!(!sent.iter().any(|m| m.contains(PULL_PREFIX)), "{sent:?}");
        assert!(out.contains(&Action::Exit(1)));
    }

    #[test]
    fn a_pull_pending_when_a_slot_dies_is_dropped_with_its_replacement() {
        let mut cf = cfg(Barrier::Tsgo);
        cf.pull_diagnostics = true;
        cf.scope = ChildScope::Slot;
        let mut c = Core::new(cf);
        let t = Instant::now();
        let p = opened(&mut c, &["file:///a.ts"], t);
        c.step(change("file:///a.ts", 2), t);
        let out = c.step(Input::ChildHung, t);
        assert!(out.contains(&Action::ChildDead));
        assert!(pulls(&out).is_empty() && published(&out).is_empty());
        // A late answer after the slot is gone does nothing.
        assert!(c.step(full(&p[0].0, "late"), t).is_empty());
    }

    /// FALSIFY: compare only document versions, not generations.
    #[test]
    fn a_pending_pull_of_a_is_obsolete_after_an_edit_to_b_and_replaced() {
        let mut c = tsgo();
        let t = Instant::now();
        let first = opened(&mut c, &["file:///a.ts", "file:///b.ts"], t);
        let (a1, b1) = (first[0].0.clone(), first[1].0.clone());
        // B answers; A's pull is still running when B is edited.
        assert_eq!(published(&c.step(full(&b1, "b1"), t)).len(), 1);
        let out = c.step(change("file:///b.ts", 2), t);
        let p = pulls(&out);
        assert_eq!(
            p.iter().map(|(_, u)| u.as_str()).collect::<Vec<_>>(),
            vec!["file:///b.ts"],
            "A is coalesced behind its running pull"
        );
        let b2 = p[0].0.clone();
        // A's answer predates the edit to B: dropped, and A pulled again.
        let out = c.step(full(&a1, "a-before-b-changed"), t);
        assert!(published(&out).is_empty(), "{:?}", texts(&out));
        let again = pulls(&out);
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].1, "file:///a.ts");
        let out = c.step(full(&again[0].0, "a2"), t);
        assert_eq!(published(&out), vec![("file:///a.ts".into(), items("a2"))]);
        assert_eq!(
            published(&c.step(full(&b2, "b2"), t)),
            vec![("file:///b.ts".into(), items("b2"))]
        );
    }

    #[test]
    fn edits_while_a_pull_runs_are_coalesced_into_one_replacement() {
        let mut c = tsgo();
        let t = Instant::now();
        let a1 = opened(&mut c, &["file:///a.ts"], t)[0].0.clone();
        for v in 2..6 {
            assert!(pulls(&c.step(change("file:///a.ts", v), t)).is_empty());
        }
        let out = c.step(full(&a1, "stale"), t);
        assert!(published(&out).is_empty());
        assert_eq!(pulls(&out).len(), 1, "one replacement for four edits");
    }

    /// FALSIFY: re-pull only the edited document.
    #[test]
    fn an_edit_re_pulls_every_open_document() {
        let mut c = tsgo();
        let t = Instant::now();
        for (id, _) in opened(&mut c, &["file:///a.ts", "file:///b.ts", "file:///c.ts"], t) {
            c.step(full(&id, "ok"), t);
        }
        let out = c.step(change("file:///b.ts", 2), t);
        let mut uris: Vec<String> = pulls(&out).into_iter().map(|(_, u)| u).collect();
        uris.sort();
        assert_eq!(uris, vec!["file:///a.ts", "file:///b.ts", "file:///c.ts"]);
    }

    /// FALSIFY: key a pull's freshness on the document version alone.
    #[test]
    fn a_close_and_reopen_with_the_same_version_starts_a_new_lifetime() {
        let mut c = tsgo();
        let t = Instant::now();
        let old = opened(&mut c, &["file:///a.ts"], t)[0].0.clone();
        let out = c.step(close("file:///a.ts"), t);
        assert_eq!(
            published(&out),
            vec![("file:///a.ts".into(), "[]".into())],
            "a close publishes an empty set"
        );
        let new = pulls(&c.step(open("file:///a.ts", 1), t));
        assert_eq!(new.len(), 1, "the new lifetime is pulled at once");
        let out = c.step(full(&old, "from-the-closed-lifetime"), t);
        assert!(published(&out).is_empty() && pulls(&out).is_empty());
        assert_eq!(
            published(&c.step(full(&new[0].0, "fresh"), t)),
            vec![("file:///a.ts".into(), items("fresh"))]
        );
    }

    #[test]
    fn an_unchanged_report_keeps_the_last_published_set() {
        let mut c = tsgo();
        let t = Instant::now();
        let a1 = opened(&mut c, &["file:///a.ts"], t)[0].0.clone();
        c.step(full(&a1, "kept"), t);
        let a2 = pulls(&c.step(change("file:///a.ts", 2), t))[0].0.clone();
        let out = c.step(
            Input::PullReply {
                id: a2.clone(),
                body: format!(
                    r#"{{"jsonrpc":"2.0","id":{},"result":{{"kind":"unchanged","resultId":"1"}}}}"#,
                    a2.to_json()
                )
                .into_bytes(),
            },
            t,
        );
        assert!(published(&out).is_empty() && pulls(&out).is_empty());
        assert!(c.is_quiet());
    }

    /// FALSIFY: keep a timed-out pull pending, or skip its replacement.
    #[test]
    fn a_pull_never_answered_is_dropped_at_the_ceiling_and_the_edit_published() {
        let mut c = tsgo();
        let t = Instant::now();
        let lost = opened(&mut c, &["file:///a.ts"], t)[0].0.clone();
        let edit = t + Duration::from_secs(100);
        assert!(pulls(&c.step(change("file:///a.ts", 2), edit)).is_empty());
        assert!(pulls(&c.step(Input::Tick, t + Duration::from_secs(369))).is_empty());
        let at = t + Duration::from_secs(370);
        let out = c.step(Input::Tick, at);
        let sent = texts(&out);
        assert!(
            sent.iter()
                .any(|m| m.contains("$/cancelRequest") && m.contains(PULL_PREFIX)),
            "{sent:?}"
        );
        let replacement = pulls(&out);
        assert_eq!(replacement.len(), 1, "the coalesced edit goes out");
        let out = c.step(full(&replacement[0].0, "after-edit"), at);
        assert_eq!(
            published(&out),
            vec![("file:///a.ts".into(), items("after-edit"))]
        );
        assert!(
            at < edit + Duration::from_secs(370),
            "within the ceiling of the edit"
        );
        // The lost pull's late answer is not published.
        assert!(published(&c.step(full(&lost, "late"), at)).is_empty());
        assert!(c.is_quiet());
    }

    #[test]
    fn an_oversize_pull_reply_frees_the_document_and_answers_no_one() {
        let mut c = tsgo();
        let t = Instant::now();
        let a1 = opened(&mut c, &["file:///a.ts"], t)[0].0.clone();
        c.step(change("file:///a.ts", 2), t);
        let frame = format!(r#"{{"id":{},"result":{{}}}}"#, a1.to_json());
        let out = c.step(Input::ServerOversize(scan(frame.as_bytes())), t);
        assert!(
            !texts(&out).iter().any(|m| m.starts_with("client ")),
            "{:?}",
            texts(&out)
        );
        assert_eq!(pulls(&out).len(), 1, "the coalesced edit is pulled");
    }

    #[test]
    fn no_pull_goes_out_once_shutdown_began() {
        let mut c = tsgo();
        let t = Instant::now();
        opened(&mut c, &["file:///a.ts"], t);
        c.step(client(r#"{"jsonrpc":"2.0","id":9,"method":"shutdown"}"#), t);
        assert!(pulls(&c.step(open("file:///b.ts", 1), t)).is_empty());
        let mut w = tsgo();
        opened(&mut w, &["file:///a.ts"], t);
        w.drain_for_shutdown();
        assert!(pulls(&w.step(change("file:///a.ts", 2), t)).is_empty());
    }

    /// FALSIFY: push `initialized` and the replay to the outbox, bypassing
    /// `Core::started`.
    #[test]
    fn a_slot_start_sends_initialized_replay_then_what_waited_and_pulls_once() {
        let mut cf = cfg(Barrier::Tsgo);
        cf.pull_diagnostics = true;
        cf.scope = ChildScope::Slot;
        let mut c = Core::new(cf);
        let t = Instant::now();
        // A request sent while the child started is held by the closed gate.
        assert!(texts(&c.step(client(REQ1), t)).is_empty());
        let initialized = r#"{"jsonrpc":"2.0","method":"initialized"}"#.to_string();
        let replay = |u: &str| {
            format!(
                r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"{u}","languageId":"typescript","version":1,"text":"x"}}}}}}"#
            )
        };
        let waited = r#"{"jsonrpc":"2.0","id":0,"result":null}"#.to_string();
        let out = c.started(
            vec![
                initialized.clone().into_bytes(),
                replay("file:///a.ts").into_bytes(),
                replay("file:///b.ts").into_bytes(),
                waited.clone().into_bytes(),
            ],
            t,
        );
        let sent = texts(&out);
        assert_eq!(sent[0], format!("child {initialized}"));
        assert!(sent[1].contains("a.ts") && sent[1].contains("didOpen"));
        assert!(sent[2].contains("a.ts") && sent[2].contains("textDocument/diagnostic"));
        assert!(sent[3].contains("b.ts") && sent[3].contains("didOpen"));
        assert!(sent[4].contains("b.ts") && sent[4].contains("textDocument/diagnostic"));
        assert_eq!(sent[5], format!("child {waited}"));
        assert_eq!(sent[6], format!("child {REQ1}"), "released by initialized");
        assert_eq!(sent.len(), 7, "{sent:?}");
        assert_eq!(pulls(&out).len(), 2, "one pull per replayed document");
    }
}
